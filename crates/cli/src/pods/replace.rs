//! `pods replace`: move a pod to a fresh host keeping its name and files — the replacement's
//! spec, create, copy, swap and cleanup.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use arena_core::provider::Provider;
use arena_core::remote::{Remote, PROBE_TIMEOUT};
use arena_core::ssh::SshTarget;
use arena_core::{Config, PodSpec};

use crate::cli::PlaceFlags;
use crate::keys::{handle_copy_keys, KeySources, KEYS_DIR};
use crate::pods::create::{apply_spec_overrides, check_create_body, create_extra, SpecOverrides};
use crate::pods::home_copy::{copy_pod_files, pod_guard, remove_staging_for, verify_replacement, CopyOpts};
use crate::pods::lifecycle::proxied_name;
use crate::pods::list::ENRICH_TIMEOUT;
use crate::pods::lock::{print_would_refuse, LOCKED_REPLACEMENT_NOTE};
use crate::pods::placement::{checked_gpu, first_create_spec, live_gpu_catalog, plan_header, plan_placement, single_option_plan};
use crate::pods::rename::replace_stage_names;
use crate::pods::selection::{just_these, resolve_target_any};
use crate::pods::setup::handle_setup;
use crate::proxy::{sync_proxy, ProxySync};
use crate::{archive, confirm, disk, money, reap, up, Interrupt};

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
pub(crate) struct Replacement {
    /// What every option is layered on ([`build_replacement_spec`]).
    base: PodSpec,
    /// The options, in the order they're tried: one, unless `--gpu` / `--cloud` list several
    /// or `--max-price` is set — then the same plan `create`/`up` would make.
    pub plan: arena_core::placement::OptionPlan,
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
pub(crate) async fn plan_replacement(
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
pub(crate) fn print_replacement_plan(owner: &dyn Provider, rep: &Replacement, name: &str) {
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
pub(crate) async fn create_replacement(
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
pub(crate) fn short_of<'a>(cfg: &Config, name: &'a str) -> &'a str {
    arena_core::setup::short_name(cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena"), name)
}

/// Write `~/.name` on pod `id` for the machine `name`, in setup's own format
/// ([`arena_core::setup::name_file_command`], trailing newline included — live finding #26:
/// the swap hand-rolled one without it). Re-resolves the endpoint first (within
/// `wait_secs`). Never on a STOPPED pod — it's listed with the endpoint it had, which may be
/// another pod's by now — and only on the pod itself ([`pod_guard`], in the same session): a
/// write that reached another pod would label it as this machine in backups and metrics.
/// `false` = it couldn't.
pub(crate) async fn write_name_file(remote: &dyn Remote, cfg: &Config, provider: &dyn Provider, id: &str, name: &str, wait_secs: u64) -> bool {
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
pub(crate) async fn after_swap(
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
pub(crate) async fn maintenance_window(owner: &dyn Provider, id: &str) -> Option<String> {
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
pub(crate) fn write_only_proxy(cfg: &Config) -> Option<String> {
    arena_core::proxy::ProxyConfig::from_config(cfg).ok().filter(|p| p.reload_cmd.trim().is_empty()).map(|p| p.nginx_path)
}

/// The config line a plan prints for the copy: what it carries and what it leaves (#27).
pub(crate) fn copy_scope_line() -> String {
    format!("copies {}; {}", arena_core::pull::REPLICATION_KEEPS, arena_core::pull::REPLICATION_SKIPS)
}

/// The plan / confirm note on a locked original: copying only reads it, but what retires it
/// will be refused until it's unlocked (never unlocked here).
pub(crate) fn locked_original_note(src: &arena_core::Pod) -> Option<String> {
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
pub(crate) fn stopped_source_refusal(src: &arena_core::Pod) -> Option<String> {
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

/// Blue-green pod replacement: build a fresh pod from the source's spec, copy its files
/// over, then rename-swap it into the canonical machine name (parking the old one as
/// `<name>-old`). Identity-preserving — the proxy port / ssh-config (which key off the
/// canonical name) carry straight over. Same spec as the source unless `ov` overrides it.
///
/// Ordering is chosen so nothing is destroyed until the replacement is built **and**
/// verified, and there's a manual gate right before the swap. The swap renames old-first so
/// two pods never share the canonical name; if the promote fails it rolls back.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_replace(
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
pub(crate) struct ReplaceOpts {
    pub keep_old: bool,
    pub skip_proxy: bool,
    pub dry_run: bool,
    pub yes: bool,
}

/// Everything [`replace_on`] needs besides the pods and providers.
pub(crate) struct ReplaceJob<'a> {
    pub setup_timeouts: arena_core::setup::SetupTimeouts,
    pub ov: &'a SpecOverrides,
    pub place: &'a PlaceFlags,
    pub opts: ReplaceOpts,
    pub interrupt: &'a Interrupt,
    /// Where the per-host API-key CSVs live (`KEYS_DIR`; a temp dir in tests).
    pub keys_dir: &'a str,
    /// Where the new pod's price is remembered for the balance ([`create_replacement`]);
    /// `None`: not remembered.
    pub balance: Option<&'a money::BalanceCtx>,
}

/// [`handle_replace`] once the source pod (`src_id`) and the backend that owns it are
/// known — the seam tests drive with a fake owner.
pub(crate) async fn replace_on(
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
pub(crate) fn replace_cleanup_blocker(
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
pub(crate) async fn wait_for_endpoint(
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
pub(crate) async fn fresh_target(
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
pub(crate) fn copy_marker_path() -> &'static str {
    "$HOME/.arena_replace_marker"
}
pub(crate) fn copy_marker_token(dest_id: &str) -> String {
    format!("arena-replace-{dest_id}")
}

/// Is the copy marker present on pod `dest_id` *with the right token, on the right pod*?
/// Re-resolves the endpoint fresh, confirms identity via `RUNPOD_POD_ID` (from
/// `/proc/1/environ` — it's not in the non-interactive ssh env but is in PID 1's), and reads
/// the marker. Any failure (unreachable, wrong recycled-port pod, marker gone, a probe that
/// ran out of its `PROBE_TIMEOUT`) → false.
pub(crate) async fn marker_present(provider: &dyn Provider, remote: &dyn Remote, dest_id: &str, cfg: &Config) -> bool {
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
pub(crate) async fn target_is_pod(
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
pub(crate) async fn clean_marker(provider: &dyn Provider, remote: &dyn Remote, dest_id: &str, cfg: &Config) {
    if let Ok(t) = fresh_target(provider, dest_id, cfg).await {
        let _ = remote.exec(&t, &format!("rm -f \"{}\"", copy_marker_path()), Some(PROBE_TIMEOUT)).await;
    }
}

/// Wait until pod `id`'s SSH endpoint has been present and UNCHANGED for `stable_secs` — a
/// freshly-created pod's ip:port churns while RunPod places it, and a pod that's still moving
/// can migrate and reset its container disk (wiping any setup+copy). Provisioning a *settled*
/// pod avoids most of that. Polls every ~15s up to `timeout_secs`; returns the settled pod,
/// or the current pod at timeout if it at least has an endpoint (else errors).
pub(crate) async fn wait_for_stable_endpoint(
    provider: &dyn Provider,
    id: &str,
    stable_secs: u64,
    timeout_secs: u64,
) -> Result<arena_core::Pod> {
    wait_for_stable_endpoint_every(provider, id, stable_secs, timeout_secs, Duration::from_secs(15)).await
}

/// [`wait_for_stable_endpoint`], polling every `poll` (tests pass zero).
pub(crate) async fn wait_for_stable_endpoint_every(
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
pub(crate) fn replace_copy_failed(canonical: &str, new_name: &str) -> String {
    format!(
        "copying {canonical} -> {new_name}. {new_name} is left running (billed). Continue the \
         copy onto it with `arena pods migrate copy {canonical}`, then `arena pods migrate \
         cutover {canonical}`; or remove it with `arena pods terminate {new_name}`"
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn replace_keeps_the_old_pod_unless_the_proxy_routes_to_the_new_one() {
        use super::replace_cleanup_blocker;
        use crate::proxy::{ProxySync, Written};
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
}

/// `replace`/`migrate`'s replacement spec and the `--gpu` check policy, with a fake
/// provider (no network: the configs carry no RUNPOD_API_KEY, so no catalog is fetched).
#[cfg(test)]
mod replacement_spec_tests;
