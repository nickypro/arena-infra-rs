//! `pods migrate copy|cutover|revert|finish|status`: the staged move to a fresh host.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use arena_core::capability::Op;
use arena_core::provider::Provider;
use arena_core::remote::{Remote, PROBE_TIMEOUT};
use arena_core::spare::SpareCloud;
use arena_core::ssh::SshTarget;
use arena_core::Config;
use clap::Subcommand;

use crate::cli::{OntoFlags, PlaceFlags};
use crate::keys::keys_dir;
use crate::pods::adopt::{self, Source};
use crate::pods::create::{sizing_flags, SpecOverrides};
use crate::pods::home_copy::{copy_pod_files, remove_staging_for, verify_replacement, CopyOpts};
use crate::pods::lifecycle::proxied_name;
use crate::pods::lock::{print_would_refuse, LOCKED_REPLACEMENT_NOTE};
use crate::pods::replace::{replacement_flags, after_swap, clean_marker, copy_scope_line, create_replacement, locked_original_note, maintenance_window, plan_replacement, print_replacement_plan, replace_cleanup_blocker, short_of, stopped_source_refusal, wait_for_endpoint, wait_for_stable_endpoint, write_name_file, write_only_proxy};
use crate::pods::selection::{just_these, resolve_target_any};
use crate::pods::setup::handle_setup;
use crate::proxy::{sync_proxy, ProxySync};
use crate::ssh_config::ssh_config_after;
use crate::{confirm, disk, money, reap, Interrupt};

#[derive(Subcommand)]
pub(crate) enum MigrateCmd {
    /// Build `<name>-new` and sync the pod's files onto it
    ///
    /// Repeatable. Never renames `<name>` or touches the proxy: the participant keeps using
    /// `<name>` and can SSH the new pod directly to test it. --onto / --onto-spare take a
    /// machine arena already has as `<name>-new` instead of creating one.
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
        #[command(flatten)]
        onto: OntoFlags,
        /// With --onto: take a cohort machine (someone's) anyway
        #[arg(long)]
        force: bool,
        /// Preview only.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Final sync, swap names, re-point the proxy, verify
    ///
    /// `<name>` becomes `<name>-old` and `<name>-new` becomes `<name>`. If the new pod isn't
    /// reachable through the proxy, it all reverts automatically. `<name>-old` is kept until
    /// `migrate finish`. Refuses while someone is on `<name>` (SSH, VS Code) unless --force:
    /// what they write after the final sync would stay behind on `<name>-old`.
    #[command(verbatim_doc_comment)]
    Cutover {
        /// Machine name (or id) being migrated.
        target: String,
        /// Skip the confirmation prompt.
        #[arg(short, long)]
        yes: bool,
        /// Cut over even with someone on the original
        #[arg(long)]
        force: bool,
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
pub(crate) async fn proxy_reaches_pod(remote: &dyn Remote, cfg: &Config, provider: &str, name: &str, expected_id: &str) -> Result<bool> {
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
    // The machine's own id (RunPod: RUNPOD_POD_ID in PID 1's env; a Hetzner VM: its server id,
    // cloud-init's instance id). A backend whose machines show none can't be confirmed.
    let Some(probe) = arena_core::replica::id_probe(provider) else { return Ok(false) };
    match remote.exec(&target, &probe, Some(PROBE_TIMEOUT)).await {
        Ok(o) if o.success => {
            let got = o.stdout.trim();
            // FAIL CLOSED: require the pod's own id to be present AND match. An empty read
            // (probe failed, or a recycled ip:port landed on a pod that doesn't expose it) must
            // NOT pass — the entire point of this gate is to catch the proxy reaching the
            // wrong/foreign pod. Empty == "couldn't confirm" == not safe to cut over.
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
pub(crate) async fn handle_migrate_copy(
    // How we reach pods: `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    target: &str,
    ov: &SpecOverrides,
    place: &PlaceFlags,
    how: CopyHow,
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
    // `-new` is recorded in the spend ledger as it's made (or taken), under `<name>`'s owner.
    let tracking = crate::spend::SpendCtx::from_config(cfg);
    let owner = crate::spend::Tracked::new(owner.as_ref(), &tracking, arena_core::spend::OwnerRule::Inherit);
    let spares = adopt::spares_for(&owner, cfg);
    let cloud = crate::pods::datavol::cloud_for(cfg);
    let CopyHow { source, force, dry_run, yes } = how;
    let keys = keys_dir(cfg);
    let job = MigrateCopyJob {
        setup_timeouts,
        ov,
        place,
        dry_run,
        yes,
        interrupt,
        keys_dir: &keys,
        balance: Some(bal),
        source,
        force,
        spares: spares.as_deref(),
        volumes: cloud.as_ref().map(|c| c as &dyn arena_core::datavol::VolumeCloud),
    };
    migrate_copy_on(remote, cfg, &owner, &canonical, &pods, job).await
}

/// `migrate copy`'s switches besides the spec: where `-new` comes from, and the confirms.
#[derive(Debug, Clone)]
pub(crate) struct CopyHow {
    pub source: Source,
    /// With `--onto`: take a cohort machine (someone's) anyway.
    pub force: bool,
    pub dry_run: bool,
    pub yes: bool,
}

/// Everything [`migrate_copy_on`] needs besides the pods and providers.
pub(crate) struct MigrateCopyJob<'a> {
    pub setup_timeouts: arena_core::setup::SetupTimeouts,
    pub ov: &'a SpecOverrides,
    pub place: &'a PlaceFlags,
    pub dry_run: bool,
    pub yes: bool,
    pub interrupt: &'a Interrupt,
    /// Where the per-host API-key CSVs live (`keys_dir`; a temp dir in tests).
    pub keys_dir: &'a str,
    /// Where `-new`'s price is remembered for the balance ([`create_replacement`]); `None`:
    /// not remembered.
    pub balance: Option<&'a money::BalanceCtx>,
    /// Where `-new` comes from when there's none yet: created, or `--onto` / `--onto-spare`.
    pub source: Source,
    /// `--onto` a cohort machine (someone's) anyway.
    pub force: bool,
    /// The owner's capacity spares (Hetzner), for `--onto-spare` / `--onto <spare>`.
    pub spares: Option<&'a dyn SpareCloud>,
    /// The fleet's Hetzner volumes: the original's are copied onto `-new` now when it's in
    /// another location (else they move at the cutover).
    pub volumes: Option<&'a dyn arena_core::datavol::VolumeCloud>,
}

/// [`handle_migrate_copy`] once the owner and its listing are known ([`resolve_migration`])
/// — the seam tests drive with a fake owner.
pub(crate) async fn migrate_copy_on(
    remote: Arc<dyn Remote>,
    cfg: &Config,
    owner: &dyn Provider,
    canonical: &str,
    pods: &[arena_core::Pod],
    job: MigrateCopyJob<'_>,
) -> Result<()> {
    let MigrateCopyJob { setup_timeouts, ov, place, dry_run, yes, interrupt, keys_dir, balance, source, force, spares, volumes } = job;
    let canonical = canonical.to_string();
    let src = pods.iter().find(|p| p.name == canonical).expect("the canonical pod is listed").clone();
    let new_name = format!("{canonical}-new");
    let existing_new = pods.iter().find(|p| p.name == new_name).cloned();
    // Its volumes (Hetzner), carried along: listed now, so a listing that fails stops here.
    let src_volumes = crate::pods::datavol::source_volumes(volumes, &src, cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena")).await?;
    // A provider the migration can't run on (the capability table) is refused before anything is
    // read or created — dry run or not; so is a flag the new pod's provider would ignore.
    if let Some(r) = crate::pods::capable::refusal(owner, Op::Migrate, "migrate", &[&src]) {
        anyhow::bail!("{r}");
    }
    replacement_flags(ov, place).check(owner, Some(&src))?;
    sizing_flags(&src.provider, ov.gpu.as_deref(), ov.gpus, None)?;
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
    // `--onto` / `--onto-spare` pick the machine a migration starts on: with a `-new` there
    // already, the copy goes onto that one, as ever.
    if let (Some(np), Source::Onto(_) | Source::Spare) = (&existing_new, &source) {
        anyhow::bail!(
            "{new_name} exists already (id={}) — `arena pods migrate copy {canonical}` re-syncs onto it; {} only picks the \
             machine a migration starts on (terminate {new_name} first to start on another)",
            np.id,
            source.flag()
        );
    }
    // The machine to take, found and checked now — before anything is asked or changed.
    let types = crate::pods::replace::server_types(owner.name(), place.server_type.as_deref())?;
    let onto = match &source {
        Source::Create => None,
        other => {
            crate::pods::replace::refuse_shaping_with_onto(other, ov, place)?;
            let onto = adopt::find(owner, spares, cfg, &canonical, pods, other, &types).await?;
            if let Some(r) = adopt::onto_refusal(&src, &onto) {
                anyhow::bail!("{r}");
            }
            Some(onto)
        }
    };
    let someones = match (&onto, force) {
        (Some(o), false) if adopt::listed_machine(cfg, &o.pod.name) && !o.spare => {
            let probe = adopt::probe(remote.as_ref(), cfg, &o.pod).await;
            adopt::someones_refusal(cfg, &canonical, o, &probe)
        }
        _ => None,
    };
    if let (Some(r), false) = (&someones, dry_run) {
        anyhow::bail!("{r}");
    }

    if dry_run {
        for r in [&stopped, &reaping, &bad_floor, &someones].into_iter().flatten() {
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
        } else if let Some(o) = &onto {
            println!("  - {}", adopt::take_line(o, &new_name));
            println!("  - set it up (~/.name={} until the cutover)", short_of(cfg, &new_name));
            println!("  - sync {canonical} → {new_name}: {}", copy_scope_line());
            if !o.spare {
                println!(
                    "    (what the sync replaces or deletes of {}'s own files is moved aside to ~/{}/ there, never deleted)",
                    o.pod.name,
                    arena_core::replica::KEPT_DIR
                );
            }
        } else {
            let rep = plan_replacement(cfg, owner, &src, ov, place).await?;
            println!("  - create {new_name}:");
            print_replacement_plan(owner, &rep, &new_name);
            println!("  - wait for it to come up, then set it up (~/.name={} until the cutover)", short_of(cfg, &new_name));
            println!("  - sync {canonical} → {new_name}: {}", copy_scope_line());
        }
        if let Some(n) = &lock_note {
            println!("  {n}");
        }
        if let Some(l) = crate::pods::datavol::plan_line(&src_volumes) {
            println!("  - {l}");
        }
        println!("  (no rename of {canonical}, no proxy change)\n[dry-run] nothing changed.");
        return Ok(());
    }

    let settle = crate::pods::replace::settle_secs(owner.name());
    let (new_id, fresh) = if let Some(np) = existing_new {
        println!("Re-using existing {new_name} (id={}) — incremental re-sync.", np.id);
        let shaping = ov.gpu.is_some() || ov.gpus.is_some() || ov.cloud.is_some() || ov.disk.is_some() || ov.volume.is_some()
            || ov.image.is_some() || ov.bootstrap || place.shapes_a_create();
        if shaping {
            eprintln!(
                "note: the spec/placement flags only shape a NEW pod — {new_name} exists and is re-synced as it is \
                 (terminate it first to build it differently)"
            );
        }
        (np.id, false)
    } else {
        let (new_id, fresh) = match &onto {
            Some(o) => {
                println!("migrate copy {canonical} → {new_name}:");
                println!("  {}", adopt::take_line(o, &new_name));
                if let Some(n) = &lock_note {
                    println!("  {n}");
                }
                if !confirm(
                    yes,
                    &format!(
                        "Will take {} as {new_name}, set it up and sync {canonical}'s files onto it — no rename of {canonical}, \
                         no proxy change. ({})",
                        o.pod.name,
                        copy_scope_line()
                    ),
                )? {
                    println!("aborted.");
                    return Ok(());
                }
                println!("[1/3] taking {} as {new_name}…", o.pod.name);
                adopt::take(owner, spares, cfg, o, &new_name).await?;
                println!("      {}", adopt::taken_line(o, &new_name));
                (o.pod.id.clone(), o.spare)
            }
            None => {
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
                (created.id, true)
            }
        };
        println!("[2/3] waiting for {new_name} to come up{}…", if settle > 0 { " and stabilize" } else { "" });
        let up = wait_for_endpoint(owner, &new_id, 900).await.with_context(|| format!("{new_name} never came up"))?;
        if let Some(n) = crate::pods::replace::moved_note(&src, &up, &new_name, onto.is_some()) {
            println!("      {n}");
        }
        wait_for_stable_endpoint(owner, &new_id, settle, 600)
            .await
            .with_context(|| format!("{new_name} never stabilized"))?;
        println!("      provisioning {new_name} (its per-host API keys follow at the cutover, under {canonical})…");
        // By id (a fresh listing, for the settled endpoint): the pod we just made (or took).
        let new = just_these(owner, std::slice::from_ref(&new_id)).await?;
        handle_setup(remote.clone(), cfg, true, false, None, None, false, false, setup_timeouts, &new, keys_dir, Some(owner))
            .await
            .with_context(|| {
                format!(
                    "provisioning {new_name} — it's left as it is: `arena pods setup {new_name}` sets it up again, then \
                     `arena pods migrate copy {canonical}` syncs onto it; or `arena pods terminate {new_name}`"
                )
            })?;
        (new_id, fresh)
    };

    println!("[3/3] syncing {canonical} → {new_name}…");
    copy_pod_files(cfg, owner, remote.as_ref(), &src.id, &new_id, &CopyOpts::new(fresh))
        .await
        .with_context(|| format!("syncing {canonical} -> {new_name}"))?;
    clean_marker(owner, remote.as_ref(), &new_id, cfg).await;
    // The original's volumes: copied now onto volumes of `-new`'s when it's in another location
    // (a volume can't leave its own); in the same one they move at the cutover.
    if let (Some(cloud), false) = (volumes, src_volumes.is_empty()) {
        let new_pod = owner.list_pods().await.context("listing pods")?.into_iter().find(|p| p.id == new_id).context("-new vanished")?;
        // What the cutover needs to know if the original is down by then: where each volume is
        // mounted, and which copies finished.
        let mut record = crate::pods::datavol::load_record(cfg, &canonical, &src.id);
        for v in &src_volumes {
            let at = crate::pods::datavol::mount_point(remote.as_ref(), cfg, owner, &src, v).await?;
            record.mounts.insert(v.id, at);
        }
        match crate::pods::datavol::carry(src_volumes.clone(), new_pod.location.as_deref()) {
            crate::pods::datavol::Carry::Copy(vols) => {
                println!("      copying {canonical}'s volume(s) onto volumes of {new_name}'s (another location)…");
                // Not "finished" while it runs (review): a sync cut short leaves a copy half new.
                for v in &vols {
                    record.copied.remove(&v.id);
                }
                crate::pods::datavol::save_record(cfg, &canonical, &record)?;
                let done = crate::pods::datavol::copy_volumes(remote.as_ref(), cfg, owner, cloud, &src, &new_pod, &canonical, &vols, &record.mounts)
                    .await
                    .with_context(|| format!("syncing {canonical}'s volume(s) -> {new_name}"))?;
                record.copied.extend(done);
                crate::pods::datavol::save_record_or_say(cfg, &canonical, &record);
            }
            crate::pods::datavol::Carry::Move(vols) => {
                crate::pods::datavol::save_record_or_say(cfg, &canonical, &record);
                println!(
                    "      {canonical}'s volume(s) {} move to {new_name} at the cutover (same location: detached, attached)",
                    vols.iter().map(|v| v.name.as_str()).collect::<Vec<_>>().join(", ")
                );
            }
            crate::pods::datavol::Carry::Nothing => {}
        }
    }

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
#[derive(Clone, Copy, Default)]
pub(crate) struct CutoverOpts<'a> {
    /// The fleet's Hetzner volumes: the original's move to the new pod at the swap (same
    /// location), or their copies get a last sync.
    pub volumes: Option<&'a dyn arena_core::datavol::VolumeCloud>,
    pub skip_proxy: bool,
    /// Cut over to the copy already on `-new` without syncing from the original first.
    pub no_final_sync: bool,
    pub dry_run: bool,
    pub yes: bool,
    /// Cut over with someone on the original.
    pub force: bool,
}

/// `migrate cutover`: final delta sync, swap names, re-point + VERIFY the proxy, auto-revert
/// if the new pod isn't reachable through the proxy. Keeps `<name>-old`.
pub(crate) async fn handle_migrate_cutover(
    // How we reach pods: `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    // The fleet (every configured provider), for the proxy syncs.
    fleet: &dyn Provider,
    cfg: &Config,
    target: &str,
    opts: CutoverOpts<'_>,
) -> Result<()> {
    let (owner, canonical, pods) = resolve_migration(cfg, target).await?;
    let cloud = crate::pods::datavol::cloud_for(cfg);
    let opts = CutoverOpts { volumes: opts.volumes.or(cloud.as_ref().map(|c| c as &dyn arena_core::datavol::VolumeCloud)), ..opts };
    migrate_cutover_on(remote, fleet, cfg, owner.as_ref(), &canonical, &pods, opts, &keys_dir(cfg)).await
}

/// The cutover's check that no one is on the original (`--force` skips it): what they write
/// after the final sync would stay behind on `<name>-old`. A stopped original has no one on it
/// (and no final sync); `None` = go on.
async fn cutover_presence_refusal(remote: &dyn Remote, cfg: &Config, src: &arena_core::Pod, force: bool) -> Option<String> {
    if force || arena_core::status::is_stopped(&src.status) {
        return None;
    }
    let presence = arena_core::idle::presence(&adopt::probe(remote, cfg, src).await);
    crate::pods::replace::presence_refusal(
        "cut over",
        &src.name,
        &presence,
        &format!(
            "What they write after the final sync would stay behind on {name}-old. Re-run once they're off (`arena pods \
             migrate copy {name}` meanwhile keeps the copy fresh).",
            name = src.name
        ),
    )
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
pub(crate) fn sync_time_label(at: u64, now: u64) -> String {
    let day = arena_core::schedule::ymd_string((at / 86_400) as i64);
    let ago = Duration::from_secs(now.saturating_sub(at));
    format!("{day} {:02}:{:02} UTC ({} ago)", at % 86_400 / 3600, at % 3600 / 60, arena_core::pipeline::fmt_elapsed(ago))
}

/// [`handle_migrate_cutover`] once the owner and its listing are known ([`resolve_migration`])
/// — the seam tests drive with a fake owner (and fleet).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn migrate_cutover_on(
    remote: Arc<dyn Remote>,
    fleet: &dyn Provider,
    cfg: &Config,
    owner: &dyn Provider,
    canonical: &str,
    pods: &[arena_core::Pod],
    opts: CutoverOpts<'_>,
    // Where the per-host API-key CSVs live (`keys_dir`; a temp dir in tests).
    keys_dir: &str,
) -> Result<()> {
    let CutoverOpts { volumes, skip_proxy, no_final_sync, dry_run, yes, force } = opts;
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
    let src_volumes = crate::pods::datavol::source_volumes(volumes, &src, cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena")).await?;
    // Asked again here: the cutover is the step a provider may not get through (Shadeform's).
    if let Some(r) = crate::pods::capable::refusal(owner, Op::Migrate, "cut over", &[&src]) {
        anyhow::bail!("{r}");
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
    // Nor while someone is on the original (an SSH session, a VS Code server), unless --force.
    let busy = cutover_presence_refusal(remote.as_ref(), cfg, &src, force).await;
    if let (Some(r), false) = (&busy, dry_run) {
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
        for r in [&locked, &reaping, &bad_floor, &busy].into_iter().flatten() {
            print_would_refuse(r);
        }
        println!("migrate cutover {canonical} (dry-run):");
        println!("  1. {sync_line}");
        let names = if arena_core::setup::hostname_is_machine_name(owner.name()) { "~/.name and hostname" } else { "~/.name" };
        println!("  2. rename {canonical} → {old_name}, then {new_name} → {canonical} (+ {names} on both, API keys under {canonical})");
        if skip_proxy {
            println!("  3. proxy SKIPPED — run `arena proxy apply` yourself");
        } else {
            println!("  3. {verify_line}");
        }
        if let Some(l) = crate::pods::datavol::plan_line(&src_volumes) {
            println!("  {l}");
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
    // The original's volumes: their copies get a last sync (another location), or they move
    // now, before the names (same location) — one that doesn't is put back, nothing swapped.
    let new_pod = pods.iter().find(|p| p.id == new_id).cloned().context("-new vanished")?;
    let mut record = crate::pods::datavol::load_record(cfg, canonical, &src_id);
    // What an earlier cutover of this migration moved, accounted for before anything (review r3):
    // on `-new` already (pending: moved, to be mounted), back on the original (moved again with
    // the rest), anywhere else — refused, nothing done.
    let mut pending: Vec<arena_core::archive::cloud::CloudVolume> = Vec::new();
    let mut back_on_src: Vec<arena_core::archive::cloud::CloudVolume> = Vec::new();
    if let (Some(cloud), true) = (volumes, record.moved_to.as_deref() == Some(new_id.as_str()) && !record.moved.is_empty()) {
        let all = cloud.all_volumes().await.context("listing the volumes")?;
        for id in &record.moved {
            match all.iter().find(|v| v.id == *id) {
                Some(v) if v.server.is_some_and(|s| s.to_string() == new_id) => pending.push(v.clone()),
                // Back on the original (from this fresh listing — the inventory above may predate
                // it: fix-check): carried with the rest.
                Some(v) if v.server.is_some_and(|s| s.to_string() == src_id) => back_on_src.push(v.clone()),
                Some(v) => anyhow::bail!(
                    "volume {} (an earlier cutover of {canonical} moved it) is {} — put it back on {canonical} first (the Hetzner \
                     console, then `arena pods volume mount {canonical}`); nothing was swapped",
                    v.name,
                    v.server.map_or_else(|| "detached".to_string(), |s| format!("attached to server {s}"))
                ),
                None => anyhow::bail!("volume id {id} (an earlier cutover of {canonical} moved it) is gone — `arena pods volume list`; nothing was swapped"),
            }
        }
    }
    // Every recorded volume is in the inventory (fresh entries replace stale ones), or refused.
    let mut carry_set = src_volumes;
    for v in pending.iter().chain(back_on_src.iter()) {
        carry_set.retain(|x| x.id != v.id);
        carry_set.push(v.clone());
    }
    let carry = crate::pods::datavol::carry(carry_set, new_pod.location.as_deref());
    // Copies (another location): their last sync — the long step — before the last presence check.
    if let (Some(cloud), crate::pods::datavol::Carry::Copy(vols)) = (volumes, &carry) {
        if no_final_sync {
            // Only onto copies that finished (review), each mounted where it belongs (review r3).
            crate::pods::datavol::copies_ready(cloud, &record, &new_pod, vols).await.context("nothing was swapped")?;
            let all = cloud.all_volumes().await.context("listing the volumes")?;
            for v in vols {
                let copy = record.copied.get(&v.id).and_then(|c| all.iter().find(|x| x.id == *c)).context("a copy vanished")?;
                let at = record.mounts.get(&v.id).cloned().unwrap_or_else(|| arena_core::datavol::DEFAULT_MOUNT.to_string());
                crate::pods::datavol::ensure_mounted(remote.as_ref(), cfg, owner, &new_pod, copy, &at).await.context("nothing was swapped")?;
            }
            println!("      {canonical}'s volume(s): their copies on {new_name} are as of its last `migrate copy` (--no-final-sync)");
        } else {
            // Not "finished" while the final sync runs (review): saved before it starts.
            for v in vols {
                record.copied.remove(&v.id);
            }
            crate::pods::datavol::save_record(cfg, canonical, &record).context("nothing was swapped")?;
            let done = crate::pods::datavol::copy_volumes(remote.as_ref(), cfg, owner, cloud, &src, &new_pod, canonical, vols, &record.mounts)
                .await
                .with_context(|| format!("the final sync of {canonical}'s volume(s) — nothing was swapped"))?;
            record.copied.extend(done);
            crate::pods::datavol::save_record_or_say(cfg, canonical, &record);
        }
    }
    // Asked again right before anything moves or is renamed (reviews r2, r3): someone who came on
    // during the prompt or a sync would leave what they write next behind on the parked original.
    if let Some(r) = cutover_presence_refusal(remote.as_ref(), cfg, &src, force).await {
        anyhow::bail!("NOT swapping — {r} (they came on during the final sync; {new_name} has the copy, nothing was renamed)");
    }
    // Volumes in the same location move now, before the names: the intent and every mount point
    // saved first (reviews r2, r3); one that doesn't move is put back, nothing swapped.
    let moved = match (volumes, &carry) {
        (Some(cloud), crate::pods::datavol::Carry::Move(all_moved)) => {
            let to_move: Vec<arena_core::archive::cloud::CloudVolume> =
                all_moved.iter().filter(|v| !pending.iter().any(|p| p.id == v.id)).cloned().collect();
            if !arena_core::status::is_stopped(&src.status) {
                for v in &to_move {
                    let at = crate::pods::datavol::mount_point(remote.as_ref(), cfg, owner, &src, v).await.context("nothing was swapped")?;
                    record.mounts.insert(v.id, at);
                }
            }
            record.moved = all_moved.iter().map(|v| v.id).collect();
            record.moved_to = Some(new_id.clone());
            crate::pods::datavol::save_record(cfg, canonical, &record).context("nothing was swapped (no volume moved)")?;
            if let Err(e) = crate::pods::datavol::move_volumes(remote.as_ref(), cfg, owner, cloud, &src, &new_pod, &to_move, &record.mounts).await {
                // What isn't back on the original, mounted, stays recorded (review r3).
                crate::pods::datavol::keep_unresolved(remote.as_ref(), cfg, owner, cloud, &mut record, &src).await;
                crate::pods::datavol::save_record_or_say(cfg, canonical, &record);
                return Err(e).context("nothing was swapped");
            }
            for v in &pending {
                let at = record.mounts.get(&v.id).cloned().unwrap_or_else(|| arena_core::datavol::DEFAULT_MOUNT.to_string());
                crate::pods::datavol::ensure_mounted(remote.as_ref(), cfg, owner, &new_pod, v, &at).await.context("nothing was swapped")?;
            }
            Some((cloud, all_moved.clone(), record.mounts.clone()))
        }
        _ => None,
    };
    let move_back = || async {
        if let Some((cloud, vols, mounts)) = &moved {
            let back = crate::pods::datavol::move_volumes(remote.as_ref(), cfg, owner, *cloud, &new_pod, &src, vols, mounts).await;
            let mut r = crate::pods::datavol::load_record(cfg, canonical, &src_id);
            crate::pods::datavol::keep_unresolved(remote.as_ref(), cfg, owner, *cloud, &mut r, &src).await;
            crate::pods::datavol::save_record_or_say(cfg, canonical, &r);
            if let Err(me) = back {
                eprintln!("      moving the volume(s) back to {canonical}: {me:#}");
            }
        }
    };

    // 3. swap names (old-first so the canonical name is never on two pods).
    println!("[3/4] swapping names…");
    let policy = arena_core::retry::RetryPolicy::default();
    if let Err(e) = arena_core::retry::retrying(&policy, || owner.rename_pod(&src_id, &old_name)).await {
        move_back().await;
        return Err(e).with_context(|| format!("renaming {canonical} -> {old_name}"));
    }
    if let Err(e) =
        arena_core::retry::retrying(&policy, || owner.rename_pod(&new_id, canonical)).await
    {
        eprintln!("      promote failed ({e}); rolling back {old_name} -> {canonical}");
        let _ = owner.rename_pod(&src_id, canonical).await;
        move_back().await;
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
            let left = cutover_revert(remote.as_ref(), cfg, fleet, owner, canonical, &new_id, &src_id, true, volumes).await;
            anyhow::bail!("cutover reverted: {why}. {canonical} is back on the original.{}", left.iter().map(|l| format!(" ⚠ {l}.")).collect::<String>());
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
                if proxy_reaches_pod(remote.as_ref(), cfg, owner.name(), canonical, &new_id).await.unwrap_or(false) {
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
                let left = cutover_revert(remote.as_ref(), cfg, fleet, owner, canonical, &new_id, &src_id, true, volumes).await;
                anyhow::bail!(
                    "cutover reverted: the new pod wasn't reachable through the proxy (it passed its own SSH \
                     health check, so look at the proxy host first). {canonical} is back on the original \
                     (untouched); retry once the proxy answers.{}",
                    left.iter().map(|l| format!(" ⚠ {l}.")).collect::<String>()
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
pub(crate) async fn cutover_revert(
    remote: &dyn Remote,
    cfg: &Config,
    fleet: &dyn Provider,
    owner: &dyn Provider,
    canonical: &str,
    new_id: &str,
    old_id: &str,
    apply_px: bool,
    // The fleet's Hetzner volumes: the ones the cutover moved go back with the names.
    volumes: Option<&dyn arena_core::datavol::VolumeCloud>,
) -> Vec<String> {
    let mut problems = Vec::new();
    let new_name = format!("{canonical}-new");
    // Demote the (failed) new pod, then restore the original to the canonical name. Order
    // matters so the canonical name is never held by two pods.
    if owner.rename_pod(new_id, &new_name).await.is_ok() {
        reap::renamed(cfg, owner.name(), new_id, &new_name);
    }
    match owner.rename_pod(old_id, canonical).await {
        Ok(()) => reap::renamed(cfg, owner.name(), old_id, canonical),
        Err(e) => eprintln!("  REVERT WARNING: couldn't restore {canonical} ({e}) — fix names manually!"),
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
    if let Some(cloud) = volumes {
        let mut record = crate::pods::datavol::load_record(cfg, canonical, old_id);
        if !record.moved.is_empty() {
            let pods = owner.list_pods().await.unwrap_or_default();
            match (pods.iter().find(|p| p.id == new_id), pods.iter().find(|p| p.id == old_id)) {
                (Some(new), Some(old)) => match crate::pods::datavol::revert_moves(remote, cfg, owner, cloud, new, old, &mut record).await {
                    // Forgotten only once they're back and mounted (review): a failed one stays.
                    Ok(()) => {
                        record.moved.clear();
                        record.moved_to = None;
                        crate::pods::datavol::save_record_or_say(cfg, canonical, &record);
                    }
                    Err(e) => {
                        crate::pods::datavol::save_record_or_say(cfg, canonical, &record);
                        eprintln!("  REVERT WARNING: the volume(s) didn't all go back to {canonical}: {e:#}");
                        problems.push(format!("the volume(s) didn't all go back to {canonical} ({e:#}) — `migrate revert {canonical}` tries again"));
                    }
                },
                _ => problems.push("couldn't list the two pods to move the volume(s) back — `arena pods volume list` shows where they are".into()),
            }
        }
    }
    if apply_px && !matches!(sync_proxy(cfg, fleet, "migrate revert").await, ProxySync::Synced { .. }) {
        eprintln!("  REVERT WARNING: the proxy wasn't re-pointed — run `arena proxy apply`.");
    }
    problems
}

/// `migrate revert`: manual rollback of a cutover (swap `<name>` ↔ `<name>-old` + proxy).
/// Work done on the new pod since the cutover stays on it (now `<name>-new`) — the confirm
/// and the summary say so (live finding #7); a later `migrate copy` keeps (in
/// `~/arena-sync-kept/` there), never destroys, whatever of it the copy replaces.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_migrate_revert(
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
    let cloud = crate::pods::datavol::cloud_for(cfg);
    let volumes = cloud.as_ref().map(|c| c as &dyn arena_core::datavol::VolumeCloud);
    let left = cutover_revert(remote, cfg, fleet, owner.as_ref(), &canonical, &new_id, &old_id, !skip_proxy, volumes).await;
    if !left.is_empty() {
        anyhow::bail!("{canonical}'s names are back on the original, but {}", left.join("; "));
    }
    println!("✓ Reverted: {canonical} is the original again; the new pod is parked as {new_name}.");
    println!("{}", revert_followup(&canonical, &new_name));
    Ok(())
}

/// What `migrate revert` says about the new pod's work (pure, tested): it stays there, and
/// how to get it — `pods pull <new>` backs up THAT pod only (`pull`'s positionals are pods,
/// like every command's; it once read one as the backup's label and pulled every pod's home
/// into a folder named after it, onto the disk the live cohort shares).
pub(crate) fn revert_followup(canonical: &str, new_name: &str) -> String {
    format!(
        "  Its work since the cutover stays there — copy what's wanted back to {canonical} by hand (back it up \
         first with `arena pods pull {new_name}`)."
    )
}

/// `migrate finish`'s switches, passed through as one value.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FinishOpts {
    /// Terminate a STOPPED `-old` anyway.
    pub discard_stopped: bool,
    pub dry_run: bool,
    pub yes: bool,
}

/// `migrate finish`: terminate the parked `<name>-old` pod, once the proxy routes the
/// participant's port to the live pod ([`migrate_finish_on`]).
pub(crate) async fn handle_migrate_finish(remote: &dyn Remote, fleet: &dyn Provider, cfg: &Config, target: &str, opts: FinishOpts) -> Result<()> {
    let (owner, canonical, pods) = resolve_migration(cfg, target).await?;
    // The terminate goes into the spend ledger as it succeeds (its end is exact).
    let tracking = crate::spend::SpendCtx::from_config(cfg);
    let owner = crate::spend::Tracked::new(owner.as_ref(), &tracking, arena_core::spend::OwnerRule::None);
    // `-old`'s volumes (Hetzner: the originals a move to another location copied) outlive it.
    let old = pods.iter().find(|p| p.name == format!("{canonical}-old")).cloned();
    let cloud = crate::pods::datavol::cloud_for(cfg);
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena").to_string();
    let kept = match (&cloud, &old) {
        (Some(c), Some(o)) => crate::pods::datavol::source_volumes(Some(c), o, &prefix).await.unwrap_or_default(),
        _ => Vec::new(),
    };
    let dry_run = opts.dry_run;
    migrate_finish_on(remote, fleet, cfg, &owner, &canonical, &pods, opts).await?;
    if dry_run {
        return Ok(());
    }
    crate::pods::datavol::clear_record(cfg, &canonical);
    let gone = match &old {
        Some(o) => owner.list_pods().await.is_ok_and(|ps| !ps.iter().any(|p| p.id == o.id)),
        None => false,
    };
    if gone {
        for v in &kept {
            println!("  kept: volume {} ({} GB), detached — it bills until `arena pods volume delete {}`", v.name, v.size_gb, v.name);
        }
    }
    Ok(())
}

/// Why `migrate finish` keeps a STOPPED `-old` (pure, tested): nothing was synced from it
/// since the live pod's last sync (`last_sync`, as [`last_sync_label`] says it) — after a
/// `cutover --no-final-sync` the participant's changes since are still on it, on its volume
/// (`volume_gb`: `Some(0)` none, `None` unknown) — and the terminate would delete them.
pub(crate) fn stopped_old_refusal(old_name: &str, status: &str, canonical: &str, volume_gb: Option<u32>, last_sync: &str) -> String {
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
pub(crate) async fn migrate_finish_on(
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
pub(crate) async fn handle_migrate_status(cfg: &Config, target: &str) -> Result<()> {
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
