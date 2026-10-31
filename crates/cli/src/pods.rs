//! `arena pods …`: the dispatch over `PodCmd`, the per-call SSH budgets and the run-on-every-pod
//! helpers. Each command family is a submodule (`up`, `jobs`, `ask`, `reap`, `resolve` predate this).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use arena_core::provider::Provider;
use arena_core::remote::{describe_error, Remote};
use arena_core::ssh::SshTarget;
use arena_core::Config;

use crate::cli::{PodCmd, Select};
use crate::cron::resolve_week_day;
use crate::keys::{check_csv_keys, handle_copy_keys, keys_dir, list_for_check, mint_missing, openrouter_client, openrouter_keys_csv, plan_mints, up_keys_stage, KeyBook};
use crate::pods::backup::{handle_full_backup, RSYNC};
use crate::pods::baseline::handle_baseline;
use crate::pods::branch::{handle_init_branches, set_branch_and_init, SetBranchRun};
use crate::pods::copy::handle_copy;
use crate::pods::create::{check_create_body, create_extra, create_policy_line, create_with_retry, hand_keys_over, image_flag, owner_plan, pin_vm_image, plan_create, print_create_body, replacement_rounds, resolve_explicit_names, resolve_want, sizing_flags, spec_with_overrides, warn_no_volume, CreateFailed, Made, RecordingProvider, SpecOverrides, NO_STREAM};
use crate::pods::health::handle_deep_test;
use crate::pods::lifecycle::{disk_fate, handle_reimage, handle_restart, handle_start, handle_terminate, names_of, no_stop_left, no_stop_refusal, repo_sites, split_no_stop, stop_lines, wipe_refusal, DiskOp, RestartOpts, TerminateOpts, RESTART_SETTLE};
use crate::pods::list::list_with;
use crate::pods::capable::CreateFlags;
use crate::pods::lock::{handle_lock, print_would_refuse};
use crate::pods::migrate::{handle_migrate_copy, handle_migrate_cutover, handle_migrate_finish, handle_migrate_revert, handle_migrate_status, CopyHow, CutoverOpts, FinishOpts, MigrateCmd};
use crate::pods::placement::{checked_gpu, describe_create, first_create_spec, place_names, placement_prompt, placement_request, plan_placement, print_balance_preflight, print_placement_preview, single_option_plan};
use crate::pods::pull::{handle_pull, local_backup_dir, resolve_pull};
use crate::pods::rename::{handle_rename, RenameRequest};
use crate::pods::replace::{handle_replace, short_of, ReplaceOpts};
use crate::pods::restore::{handle_restore, RestoreOpts};
use crate::pods::run::{handle_run, misplaced_run_flags, pod_name_as_command};
use crate::pods::selection::{cohort_only, hard_non_cohort_refusal, refresh_missing_endpoints, select, Unscoped, ENDPOINT_RETRY};
use crate::pods::setup::{handle_setup, setup_config, stage_hetzner_script};
use crate::proxy::{proxy_deployable, sync_proxy};
use crate::{ask, confirm, jobs, money, reap, spend, up, Interrupt};

pub(crate) mod adopt;
pub(crate) mod backup;
pub(crate) mod baseline;
pub(crate) mod branch;
pub(crate) mod capable;
pub(crate) mod copy;
pub(crate) mod copy_home;
pub(crate) mod create;
pub(crate) mod health;
pub(crate) mod home_copy;
pub(crate) mod lifecycle;
pub(crate) mod list;
pub(crate) mod lock;
pub(crate) mod migrate;
pub(crate) mod placement;
pub(crate) mod pull;
pub(crate) mod rename;
pub(crate) mod replace;
pub(crate) mod restore;
pub(crate) mod run;
pub(crate) mod selection;
pub(crate) mod setup;
pub(crate) mod spare;

// Per-call SSH budgets (PLAN 1.B). Every pod-SSH call is bounded so one wedged pod can
// never hang a fleet command: it reports `✗ <name>: timed out after Ns`, counts as a
// failure, and the others carry on. Quick probes share `arena_core::remote::PROBE_TIMEOUT`.

/// `pods test`: a cold `import torch` (CUDA libs off a cold disk) takes 10–30s, so 90s
/// means the pod is wedged — and a read-only health check must not hang on it.
pub(crate) const TEST_TIMEOUT: Duration = Duration::from_secs(90);

/// `pods run`'s default per-pod budget (`--timeout` overrides). Arbitrary commands can be
/// long (a download, a test suite), so it's generous — but never unbounded: a fleet run
/// must end even when one pod wedges. 30 min.
pub(crate) const RUN_TIMEOUT_SECS: u64 = 30 * 60;

/// `pods backup`: git add + commit + push of a participant's tree — seconds normally, but
/// a first push of notebooks/outputs can take minutes. 5 min.
pub(crate) const BACKUP_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// `pods set-branch` / `init-branches`: a fetch + checkout (+ fast-forward or push) against
/// GitHub — seconds normally, so 2 min means stuck (a hung fetch, a held lock).
pub(crate) const BRANCH_TIMEOUT: Duration = Duration::from_secs(2 * 60);

// set-branch's switch and init-branches bound themselves on the pod (`timeout`, then a KILL) inside
// that budget, so their pod side has ended before arena reports a timeout and releases the pod's
// lock (review ai-review claude #13).
const _: () = assert!(arena_core::backup::SWITCH_BOUND_SECS + arena_core::backup::SWITCH_KILL_AFTER_SECS < BRANCH_TIMEOUT.as_secs());

/// `pods cp`'s scp budget floor: connection setup, a small file, a slow start — 10 min.
/// What's copied adds to it ([`cp_timeout`]). Its `mkdir -p` and post-copy size check are
/// quick probes.
pub(crate) const CP_BASE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The slowest operator uplink `pods cp` budgets for: 1 MB/s (≈8 Mbit/s, a tethered or
/// hotel connection). Every pod's scp starts at once from the operator's machine, so the
/// copies share it — the budget is for detecting a wedged copy, not a slow one.
const CP_MIN_BYTES_PER_SEC: u64 = 1_000_000;

/// `pods cp`'s per-pod scp budget, proportional to what's copied: [`CP_BASE_TIMEOUT`] plus
/// the time to send `bytes` to all `pods` at [`CP_MIN_BYTES_PER_SEC`], capped at a day
/// (the `--timeout` maximum). A fixed budget killed big fleet-wide copies part-way: 500 MB
/// to 30 pods over a 20 MB/s uplink needs ~750s, so every pod would fail at 600s.
pub(crate) fn cp_timeout(bytes: u64, pods: usize) -> Duration {
    let sending = bytes.saturating_mul(pods as u64) / CP_MIN_BYTES_PER_SEC;
    Duration::from_secs(
        CP_BASE_TIMEOUT.as_secs().saturating_add(sending).min(arena_core::setup::MAX_STEP_TIMEOUT_SECS),
    )
}

/// What `pods cp` sends for `path`: a file's size, or a tree's total (as `scp -r` sends
/// it). Best-effort, for [`cp_timeout`] only: unreadable entries count 0 and symlinked
/// dirs inside the tree aren't followed (no loops) — the base budget absorbs the slack.
pub(crate) fn local_size(path: &std::path::Path) -> u64 {
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
/// must give the operator their terminal back instead of waiting forever. Each leg of the
/// via-local fallback ([`CopyLeg`]) gets the same budget, for all of its rsyncs together.
pub(crate) const POD_COPY_TIMEOUT: Duration = Duration::from_secs(2 * 3600);

/// How one pod's SSH call ended, ready for its report line: the command's output (which
/// may still be a non-zero exit), or why there is none — `timed out after Ns`, a spawn
/// error, a crashed task.
pub(crate) type PodCall = std::result::Result<arena_core::ssh::SshOutput, String>;

/// Run one job per pod concurrently and hand each pod's result to `on_done(done, total,
/// key, result)` as it finishes. Finishing order is the point: healthy pods report at
/// once, and a pod whose job is stuck reports when its budget runs out — never holding the
/// others, or the command, hostage (jobs bound their own SSH calls). A job that panicked is
/// still reported, by its key, as `Err` — never silently dropped from the tally.
///
/// `key` is usually the pod's name (all a report line needs). A caller that matches results
/// back to pods uses something unique instead — two pods can share a name (a double
/// create, a leftover), and a name-keyed map would hand one pod the other's result.
pub(crate) async fn each_pod<K, T, F>(
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
pub(crate) async fn exec_each_pod<K>(
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
pub(crate) fn failure_line(done: usize, total: usize, name: &str, call: &PodCall) -> String {
    match call {
        Ok(out) => format!("[{done}/{total}] ✗ {name} ({}): {}", out.exit_label(), out.stderr.trim()),
        Err(why) => format!("[{done}/{total}] ✗ {name}: {why}"),
    }
}

pub(crate) async fn handle_pods(
    cmd: PodCmd,
    provider: &dyn Provider,
    // How we reach pods (one for the whole command): `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    yes: bool,
) -> Result<()> {
    handle_pods_with(cmd, provider, remote, cfg, yes, Interrupt::ctrl_c()).await
}

/// [`handle_pods`] with the command's Ctrl+C handed in — one latch for every phase of a
/// `create`/`up` (tests press a manual one).
pub(crate) async fn handle_pods_with(
    cmd: PodCmd,
    provider: &dyn Provider,
    remote: Arc<dyn Remote>,
    cfg: &Config,
    yes: bool,
    interrupt: Interrupt,
) -> Result<()> {
    handle_pods_bal(cmd, provider, remote, cfg, yes, interrupt, &money::BalanceCtx::from_config(cfg)).await
}

/// [`handle_pods_with`] with the balance reads handed in (`pods list`'s footer, the
/// `create`/`up` preflight) — the real ones, or a fake in tests.
async fn handle_pods_bal(
    cmd: PodCmd,
    provider: &dyn Provider,
    remote: Arc<dyn Remote>,
    cfg: &Config,
    yes: bool,
    interrupt: Interrupt,
    bal: &money::BalanceCtx,
) -> Result<()> {
    // Every create, stop, start and terminate below is recorded in the spend ledger as it
    // succeeds (`arena spend`), with `--owner` for the instances `create`/`up` make; a
    // `replace`/`migrate` keeps the owner of the machine it replaces.
    let tracking = spend::SpendCtx::from_config(cfg);
    let owner = match &cmd {
        PodCmd::Create { owner, .. } | PodCmd::Up { owner, .. } => owner.owner.as_deref(),
        _ => None,
    };
    let replaces = matches!(cmd, PodCmd::Replace { .. } | PodCmd::Migrate { .. });
    let tracked = spend::Tracked::new(provider, &tracking, spend::owner_rule(owner, replaces));
    let provider: &dyn Provider = &tracked;
    match cmd {
        PodCmd::List { json, probe, no_probe } => {
            // Probe GPU by default for the human table; JSON stays fast/scriptable unless
            // asked. `--no-probe` always wins.
            let (out, warnings) = list_with(provider, &remote, cfg, bal, json, !no_probe && (probe || !json)).await?;
            warnings.iter().for_each(|w| eprintln!("{w}"));
            print!("{out}");
        }

        PodCmd::Create { names, count, add, gpu, gpus, server_type, cloud, max_price, order, disk, volume, image, bootstrap, api_json, reap, owner, skip_proxy, dry_run, keep_trying, retry_mins, retry_secs } => {
            // A reap policy asked for: resolved (one end time for every pod) and its state
            // checked before anything is created; then every create records it.
            let reaping = reap.resolve(cfg, provider.name())?;
            let recorder;
            let provider: &dyn Provider = match &reaping {
                Some((policy, store)) => {
                    recorder = RecordingProvider { inner: provider, policy: *policy, store: store.clone(), set_by: "create" };
                    &recorder
                }
                None => provider,
            };
            if let (Some((policy, _)), true) = (&reaping, dry_run) {
                println!("[dry-run] each pod would get a reap policy: {}", create_policy_line(policy));
            }
            // Bad extra JSON (or a field arena sets itself) fails before any API call.
            let api_extra = create_extra(provider, cfg, api_json.as_deref())?;
            // Flags the create target would ignore fail here too (the capability table: `--disk`
            // on Hetzner, `--bootstrap` on Vast, `--gpu` on Hetzner, `--server-type` elsewhere)
            // — refused, never silently dropped.
            CreateFlags { cloud: cloud.as_deref(), disk, volume, image: image.as_deref(), bootstrap, max_price }.check(provider, None)?;
            let server_type = sizing_flags(provider.name(), gpu.as_deref(), gpus, server_type)?;
            let (image, vm_image) = image_flag(provider.name(), image)?;
            // A typo'd --gpu fails here, before anything is listed or created.
            let gpu = checked_gpu(cfg, provider.name(), gpu).await?;
            let ov = SpecOverrides { gpu, gpus, cloud, disk, volume, image, bootstrap, api_extra, server_type, vm_image, ..Default::default() };
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
            let (mut spec, options) = match placing {
                Some((req, base)) => {
                    let plan = plan_placement(cfg, provider.name(), &req, &base).await?;
                    bal.remember_gpu_prices(&plan);
                    (base, Some(plan))
                }
                None => (spec_with_overrides(cfg, &ov), None),
            };
            pin_vm_image(provider.name(), cfg, &mut spec).await?;
            check_create_body(provider, &first_create_spec(&spec, options.as_ref(), &names[0]))?;
            if dry_run {
                match &options {
                    Some(plan) => print_placement_preview(provider, plan, &spec, &names),
                    None => {
                        let desc = describe_create(provider, &spec);
                        for name in &names {
                            println!("[dry-run] would create {name} on {} ({desc})", provider.name());
                        }
                    }
                }
                print_create_body(provider, &first_create_spec(&spec, options.as_ref(), &names[0]));
                warn_no_volume(provider, &spec);
                print_balance_preflight(provider, cfg, bal, names.len(), options.as_ref(), &spec).await;
                owner_plan(&tracking, &owner, &names, true)?;
                println!("\nDry-run only — no pods created (this is a preview).");
                return Ok(());
            }
            let then = if skip_proxy { "" } else { ", then sync the proxy" };
            let what = match &options {
                Some(plan) => placement_prompt(provider, plan, &spec, &names, then)?,
                None => format!(
                    "Will create {} pod(s) on {} ({}){then}:\n  {}",
                    names.len(), provider.name(), describe_create(provider, &spec), names.join(", ")
                ),
            };
            // The account balance for these pods, next to the prompt: advisory (it never
            // blocks, and `--yes` changes nothing), quiet if the balance can't be read.
            print_balance_preflight(provider, cfg, bal, names.len(), options.as_ref(), &spec).await;
            let handing = owner_plan(&tracking, &owner, &names, false)?;
            if !confirm(yes, &what)? {
                println!("aborted.");
                return Ok(());
            }
            hand_keys_over(&tracking, cfg, &owner, &handing).await?;
            // Retry rounds only retry the confirmed names still missing; -n/-a also stop once
            // the provider-scoped total is met (topup_target == 0 marks the explicit case).
            let outcome = match &options {
                Some(plan) => {
                    place_names(provider, cfg, names, topup_target, &spec, plan, retry_mins, retry_secs, &interrupt, NO_STREAM)
                        .await
                }
                None => {
                    create_with_retry(
                        provider, cfg, names, topup_target, &ov, keep_trying, retry_mins, retry_secs, &interrupt, NO_STREAM,
                    )
                    .await
                }
            };
            let (created, error) = match outcome {
                Ok(created) => (created, None),
                Err(CreateFailed { created, error }) => (created, Some(error)),
            };
            // A price the create answered with is remembered for the balance (a later
            // listing without one — RunPod v1's — still prices the pod).
            bal.remember_prices(created.iter().map(|m| &m.pod));
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

        PodCmd::Up { names, count, add, gpu, gpus, server_type, cloud, max_price, order, disk, volume, image, bootstrap, api_json, reap, owner, dry_run, no_wait, keep_trying, retry_mins, retry_secs, no_setup, check, check_attempts, retry_bad_hosts, lock, timeout, endpoint_timeout, interval } => {
            // As `create`: the reap policy is resolved once, before anything is created, and
            // recorded for each pod (replacements included) the moment its create returns.
            let reaping = reap.resolve(cfg, provider.name())?;
            let recorder;
            let provider: &dyn Provider = match &reaping {
                Some((policy, store)) => {
                    recorder = RecordingProvider { inner: provider, policy: *policy, store: store.clone(), set_by: "up" };
                    &recorder
                }
                None => provider,
            };
            if let (Some((policy, _)), true) = (&reaping, dry_run) {
                println!("[dry-run] each pod would get a reap policy: {}", create_policy_line(policy));
            }
            let api_extra = create_extra(provider, cfg, api_json.as_deref())?;
            // Flags the create target would ignore, and --lock where pods can't be locked, fail
            // now, not after pods were made.
            CreateFlags { cloud: cloud.as_deref(), disk, volume, image: image.as_deref(), bootstrap, max_price }.check(provider, None)?;
            if lock {
                provider.supports(arena_core::capability::Op::Lock, None).check().map_err(|why| anyhow::anyhow!("--lock: {why}"))?;
            }
            let server_type = sizing_flags(provider.name(), gpu.as_deref(), gpus, server_type)?;
            let (image, vm_image) = image_flag(provider.name(), image)?;
            let gpu = checked_gpu(cfg, provider.name(), gpu).await?;
            // The machines earlier `up --check`s found bad: a marketplace (Vast) skips their
            // offers; RunPod can't be told, so a pod it places on one is rejected (`up`).
            let avoid_machines = if retry_bad_hosts {
                Vec::new()
            } else {
                arena_core::snapshot::avoided_machines(&up::remembered_bad_hosts(cfg, arena_core::snapshot::unix_now()).0, "vast")
            };
            let ov = SpecOverrides { gpu, gpus, cloud, disk, volume, image, bootstrap, api_extra, avoid_machines, server_type, vm_image };
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

            // The pipelines' config is checked *before* creating (billing) pods, not after: a
            // malformed SETUP_TIMEOUT_SECS / MIN_DRIVER_VERSION fails here.
            let setup_timeouts = if no_setup {
                arena_core::setup::SetupTimeouts::default()
            } else {
                arena_core::setup::SetupTimeouts::from_config(cfg, None)?
            };
            let policy = if check { Some(arena_core::health::HealthPolicy::from_config(cfg)?) } else { None };
            // --endpoint-timeout / ENDPOINT_TIMEOUT_MINS, checked before anything bills.
            let endpoint_mins = arena_core::pipeline::endpoint_timeout_mins(cfg, endpoint_timeout.map(u64::from))?;
            let (mut spec, options) = match placing {
                Some((req, base)) => {
                    let plan = plan_placement(cfg, provider.name(), &req, &base).await?;
                    bal.remember_gpu_prices(&plan);
                    (base, Some(plan))
                }
                None => (spec_with_overrides(cfg, &ov), None),
            };
            pin_vm_image(provider.name(), cfg, &mut spec).await?;
            check_create_body(provider, &first_create_spec(&spec, options.as_ref(), &names[0]))?;
            let keys = up_keys_stage(cfg, &keys_dir(cfg), !no_setup);
            // OpenRouter, when setup runs and a provisioning key is set: one listing says which
            // names have no key — `up` mints those after the prompt, so its keys stage copies them
            // on — and, before the copy, which of the keys CSV's rows are dead (`keys::checked`).
            // A name a handover gives a fresh key (an owner's) isn't minted for twice.
            let naming = arena_core::selector::Naming::from_config(cfg);
            let or = if no_setup { None } else { openrouter_client(cfg) };
            let key_api = or.as_ref().map(|a| a as &dyn arena_core::openrouter::KeyApi);
            let handed_names: Vec<String> = spend::handovers(&tracking, &names, owner.owner.as_deref(), owner.keep_key)
                .map(|h| h.into_iter().map(|h| h.name).collect())
                .unwrap_or_default();
            let key_limit = cfg.get_parsed::<f64>("OPENROUTER_KEY_LIMIT").unwrap_or(5.0);
            let mints = plan_mints(&naming, &names, &handed_names, keys.as_ref(), list_for_check(key_api).await, key_limit, dry_run);
            // Each pod's own Claude Code account, after setup — when there's a pool.
            let claude_line = if no_setup { Ok(None) } else { crate::claude::plan_line(cfg, None) };
            let per_pod = up::pipeline_text(
                !no_setup,
                check.then_some(check_attempts),
                keys.is_some() || !mints.mint.is_empty(),
                matches!(claude_line, Ok(Some(_))),
                lock,
                (endpoint_mins, check_attempts),
            );
            if dry_run {
                match &options {
                    Some(plan) => print_placement_preview(provider, plan, &spec, &names),
                    None => {
                        let desc = describe_create(provider, &spec);
                        for name in &names {
                            println!("[dry-run] would create {name} on {} ({desc})", provider.name());
                        }
                    }
                }
                print_create_body(provider, &first_create_spec(&spec, options.as_ref(), &names[0]));
                warn_no_volume(provider, &spec);
                print_balance_preflight(provider, cfg, bal, names.len(), options.as_ref(), &spec).await;
                owner_plan(&tracking, &owner, &names, true)?;
                for line in &mints.lines {
                    println!("{line}");
                }
                let retry = if retry_mins > 0 { format!(" (retrying up to {retry_mins}m for capacity)") } else { String::new() };
                println!(
                    "\nDry-run only — no pods created (preview){retry}: would create the above, then {per_pod}. \
                     Each pod waits up to {timeout}s for its endpoint."
                );
                return Ok(());
            }
            // Provisioning settings too: a config without ARENA_REPO_* must fail before the
            // create, not strand the new pods after it. Likewise the hetzner bare-VM script's
            // temp file (an unwritable temp dir): staged after the create, its failure ended
            // `up` with pods billing and no pipeline, proxy sync or summary for them. Creates
            // go to the primary backend, so its name says whether a hetzner pod can result.
            let setup = if no_setup {
                None
            } else {
                let (scfg, tokens) =
                    setup_config(cfg, None, false, false).context("provisioning settings (or pass --no-setup)")?;
                let hetzner_script = stage_hetzner_script(arena_core::setup::is_bare_vm(provider.name()))?;
                Some((up::SetupStage { scfg, timeouts: setup_timeouts, hetzner_script }, tokens))
            };

            let then = format!(
                "{}, then {per_pod}",
                if retry_mins > 0 { format!(", retrying up to {retry_mins}m") } else { String::new() },
            );
            let what = match &options {
                Some(plan) => placement_prompt(provider, plan, &spec, &names, &then)?,
                None => format!(
                    "Will create {} pod(s) on {} ({}){then}:\n  {}",
                    names.len(), provider.name(), describe_create(provider, &spec), names.join(", ")
                ),
            };
            // The account balance for these pods, next to the prompt: advisory (it never
            // blocks, and `--yes` changes nothing), quiet if the balance can't be read.
            print_balance_preflight(provider, cfg, bal, names.len(), options.as_ref(), &spec).await;
            let handing = owner_plan(&tracking, &owner, &names, false)?;
            for line in &mints.lines {
                println!("{line}");
            }
            if !confirm(yes, &what)? {
                println!("aborted.");
                return Ok(());
            }
            // The handed-over and the minted keys are in the CSV before the pipelines copy any.
            let handed = hand_keys_over(&tracking, cfg, &owner, &handing).await?;
            let minted = match key_api {
                Some(api) if !mints.mint.is_empty() => {
                    let m = mint_missing(api, &tracking, &naming, std::path::Path::new(&keys_dir(cfg)), &mints, owner.owner.as_deref()).await;
                    for l in &m.lines {
                        if l.starts_with('✗') {
                            eprintln!("{l}");
                        } else {
                            println!("{l}");
                        }
                    }
                    m
                }
                _ => Default::default(),
            };
            // A key made by another arena while this one waited at its prompt is in the CSV too.
            let reread = handed || !minted.names.is_empty() || minted.lines.iter().any(|l| l.starts_with('='));
            let mut keys = if reread { up_keys_stage(cfg, &keys_dir(cfg), !no_setup) } else { keys };
            // The other names' rows in the keys CSV, checked against the newest listing: a key
            // deleted elsewhere isn't copied on (said, with what to run).
            if let Some(k) = keys.as_mut() {
                let fresh = |n: &String| minted.names.contains(n) || handing.iter().any(|h| h.name == *n);
                let hosts: Vec<String> = names.iter().filter(|n| !fresh(n)).cloned().collect();
                let listed = minted.listed.map(Ok).or(mints.listed);
                for line in check_csv_keys(k, &hosts, listed.as_ref(), &naming) {
                    eprintln!("{line}");
                }
            }
            // What the operator confirmed: a name of these that gets no pod is in the summary
            // (and the exit status) too, not silently missing from it.
            let confirmed = names.clone();
            if no_wait {
                // No pipelines: create (retrying if asked), sync the proxy for what was made.
                let outcome = match &options {
                    Some(plan) => {
                        place_names(provider, cfg, names, topup_target, &spec, plan, retry_mins, retry_secs, &interrupt, NO_STREAM)
                            .await
                    }
                    None => {
                        create_with_retry(
                            provider, cfg, names, topup_target, &ov, keep_trying, retry_mins, retry_secs, &interrupt, NO_STREAM,
                        )
                        .await
                    }
                };
                let (created, error) = match outcome {
                    Ok(created) => (created, None),
                    Err(CreateFailed { created, error }) => (created, Some(error)),
                };
                // As `create`: the prices the creates answered with are remembered for the
                // balance — those made before a failure too.
                bal.remember_prices(created.iter().map(|m| &m.pod));
                if !created.is_empty() {
                    sync_proxy(cfg, provider, "up").await;
                }
                if let Some(e) = error {
                    return Err(e);
                }
                if created.is_empty() {
                    anyhow::bail!("no pods were created — nothing to bring up");
                }
                println!("\n--no-wait: not polling. Run `arena proxy apply` once endpoints are assigned.");
                return Ok(());
            }

            // One independent pipeline per pod (see `up`), each started the moment its create
            // returns — while placement may still be retrying for the other names (finding
            // #14: a `--retry-mins` window used to hold every placed pod idle, billing, until it
            // ended). Replacements draw from the confirmed options — on the single-spec path,
            // that one spec as an option.
            let plan_for_run = options.clone().unwrap_or_else(|| single_option_plan(provider.name(), &spec));
            let setup = setup.map(|(stage, tokens)| {
                println!("{tokens}");
                stage
            });
            match &keys {
                Some(k) if k.csv.is_empty() => println!(
                    "API keys: none generated in {}/ — each pod gets the backup archive's access once it's set up",
                    keys_dir(cfg)
                ),
                Some(k) => println!("API keys: {} — copied to each pod once it's set up", k.csv.join(", ")),
                None if setup.is_some() => println!(
                    "API keys: none generated in {}/ — skipping (run `arena keys gen --all` or drop in \
                     <provider>_api_keys.csv, then `pods copy-keys`).",
                    keys_dir(cfg)
                ),
                None => {}
            }
            match &claude_line {
                Ok(Some(line)) => println!("{line}"),
                Ok(None) => {}
                Err(e) => println!("Claude Code: ✗ {e} — skipped (fix it, then `arena claude push`)"),
            }
            let run = up::UpRun {
                provider,
                remote: remote.clone(),
                cfg,
                options: plan_for_run,
                base: spec.clone(),
                rounds: replacement_rounds(keep_trying, retry_mins, retry_secs),
                timeout: Duration::from_secs(timeout),
                interval: Duration::from_secs(interval.max(1)),
                proxy: proxy_deployable(cfg).await,
                setup,
                check: policy.map(|policy| up::CheckStage {
                    policy,
                    attempts: check_attempts,
                    cmd: arena_core::health::deep_check_command(Some(cfg.get("CONDA_ENV").unwrap_or("arena-env"))),
                }),
                keys,
                // With setup, always: with no pool it only delivers a pending removal (review r3).
                claude: !no_setup,
                lock,
                retry_bad_hosts,
                balance: Some(bal),
                reap_notice: reaping.is_some(),
                placements: check_attempts,
                endpoint_timeout: Duration::from_secs(endpoint_mins * 60),
            };
            println!(
                "\nEach pod is brought up on its own the moment it's created (endpoint wait ≤{timeout}s per pod, one up \
                 without an endpoint for {endpoint_mins} min is replaced; Ctrl+C stops — nothing is terminated by it)…"
            );
            let (made_tx, made_rx) = tokio::sync::mpsc::unbounded_channel::<Made>();
            let creating = async {
                // Owned here, so the channel closes — and the pipelines can finish — once the
                // create phase is over.
                let made_tx = made_tx;
                let on_made = move |m: &Made| {
                    // Its price, for the balance, the moment it exists (as a replacement's,
                    // in `up::UpRun`): the pipeline after it may run for many minutes.
                    bal.remember_prices(std::iter::once(&m.pod));
                    let _ = made_tx.send(m.clone());
                };
                match &options {
                    Some(plan) => {
                        place_names(provider, cfg, names, topup_target, &spec, plan, retry_mins, retry_secs, &interrupt, &on_made)
                            .await
                    }
                    None => {
                        create_with_retry(
                            provider, cfg, names, topup_target, &ov, keep_trying, retry_mins, retry_secs, &interrupt, &on_made,
                        )
                        .await
                    }
                }
            };
            let (outcome, mut rows) =
                tokio::join!(creating, up::run_up_streaming(&run, made_rx, interrupt.wait(), &up::console));
            // A create that failed part-way still had its pods brought up (and the proxy synced
            // by the pipelines' final sync); the failure is reported after their summary.
            let (created, error) = match outcome {
                Ok(created) => (created, None),
                Err(CreateFailed { created, error }) => (created, Some(error)),
            };
            if created.is_empty() {
                if let Some(e) = error {
                    return Err(e);
                }
                anyhow::bail!("no pods were created — nothing to bring up");
            }
            let made: Vec<String> = created.iter().map(|m| m.pod.name.clone()).collect();
            rows.extend(up::not_created_rows(&confirmed, &made, interrupt.is_set()));
            let concluded = up::conclude(&rows, &up::console);
            if let Some(e) = error {
                return Err(e);
            }
            concluded?;
        }

        PodCmd::Stop { sel, wipe_ok, dry_run } => {
            let sel = select(provider, cfg, &sel.args(), Unscoped::Refuse("stop")).await?;
            // Every selected pod that's billing is stopped — not just RUNNING: RunPod v2
            // reports real states, and a PROVISIONING/STARTING pod bills too (one status rule
            // for this, the $/H column and the fleet total: `status::is_billing`). A selected
            // pod that isn't billing (already stopped, or a state we don't know) is said, not
            // hidden, and left alone.
            let billing = |p: &arena_core::Pod| arena_core::status::is_billing(&p.status);
            for p in sel.pods.iter().filter(|p| !billing(p)) {
                eprintln!("skip {} — not running or starting ({})", p.name, p.status);
            }
            let billing_pods: Vec<&arena_core::Pod> = sel.pods.iter().filter(|p| billing(p)).collect();
            // A billing pod whose backend has no stop (Shadeform: it bills until deleted) is
            // skipped and said — before the --wipe-ok gate, which would only mislead — and the
            // rest are stopped: one overflow VM must not veto `stop --all` (review SF3). Only a
            // selection with nothing else to stop is refused outright. Afterwards the exit is
            // non-zero while a skipped one still bills (see `no_stop_left`).
            let (no_stop, pods) = split_no_stop(provider, arena_core::capability::Op::Stop, billing_pods);
            if pods.is_empty() && !no_stop.is_empty() {
                anyhow::bail!("{}", no_stop_refusal(&no_stop, "stop"));
            }
            for (p, why) in &no_stop {
                eprintln!("skip {} — {why}", p.name);
            }
            if pods.is_empty() {
                println!("(no running or starting pods to stop)");
                return Ok(());
            }
            // What each stop costs (a RunPod pod keeps no data across stop → start), and
            // which pods would lose everything — those need --wipe-ok, even with --yes.
            let mut fates = Vec::with_capacity(pods.len());
            for p in &pods {
                fates.push(disk_fate(provider, p).await);
            }
            let repo = arena_core::backup::repo_path(cfg);
            // A locked pod refuses a stop (RunPod itself does): the whole batch is refused up
            // front, before anything stops, rather than half-run.
            let locked = arena_core::lock::refusal("stop", &pods, "");
            if let (Some(r), false) = (&locked, dry_run) {
                anyhow::bail!("{r}");
            }
            // Where each pod's repo really is — asked only of pods with a volume, each
            // bounded, all at once.
            let sites = repo_sites(&remote, cfg, &pods, &fates, &repo).await;
            let lines = stop_lines(&pods, &fates, &sites);
            let blocked: Vec<&str> = pods
                .iter()
                .zip(fates.iter().zip(&sites))
                .filter(|(_, (f, site))| f.needs_wipe_ok(site))
                .map(|(p, _)| p.name.as_str())
                .collect();
            if dry_run {
                println!("[dry-run] would stop {} pod(s):\n{}", pods.len(), lines.join("\n"));
                if let Some(r) = &locked {
                    print_would_refuse(r);
                }
                if !blocked.is_empty() && !wipe_ok {
                    println!("(would be refused without --wipe-ok: {})", blocked.join(", "));
                }
                if !no_stop.is_empty() {
                    println!("(would skip, still billing — no stop on their provider: {})", names_of(&no_stop).join(", "));
                }
                println!("\nDry-run only — would stop {} pod(s).", pods.len());
                return Ok(());
            }
            if !blocked.is_empty() && !wipe_ok {
                anyhow::bail!("{}", wipe_refusal(DiskOp::Stop, false, &blocked, &repo));
            }
            if !confirm(yes, &format!("Will stop {} running/starting pod(s):\n{}", pods.len(), lines.join("\n")))? {
                println!("aborted.");
                return Ok(());
            }
            // The fleet provider routes each stop to the backend that owns the pod id. A Hetzner
            // VM shuts down gracefully; one that had to be powered off after all is said.
            let (mut ok, total) = (0, pods.len());
            for p in &pods {
                match provider.stop_pod_outcome(&p.id).await {
                    Ok(outcome) => {
                        println!("[stopped] {}", p.name);
                        if let Some(note) = outcome.note() {
                            eprintln!("  ⚠ {}: {note}", p.name);
                        }
                        ok += 1;
                    }
                    Err(e) => eprintln!("[FAILED] {}: {}", p.name, arena_core::lock::explain(&p.name, &e)),
                }
            }
            println!("\nstopped {ok}/{total}");
            if ok < total {
                anyhow::bail!("{} pod(s) failed to stop", total - ok);
            }
            if let Some(left) = no_stop_left(&no_stop) {
                anyhow::bail!("{left}");
            }
        }

        PodCmd::InitBranches { sel, cohort, week, day, dry_run } => {
            let sel = select(provider, cfg, &sel.args(), Unscoped::All).await?;
            let sel = cohort_only(cfg, sel, cohort);
            handle_init_branches(remote, cfg, week, day, &sel, dry_run, yes, RSYNC).await?;
        }

        PodCmd::Pull { label, dir, max_size, remote_path, no_git, no_big, target_flag, cohort, sel, dry_run } => {
            let dir = dir.unwrap_or_else(|| local_backup_dir(cfg));
            let (label, sel, note) = resolve_pull(provider, cfg, label, sel, &target_flag).await?;
            if let Some(note) = &note {
                eprintln!("{note}");
            }
            let mut sel = cohort_only(cfg, sel, cohort);
            refresh_missing_endpoints(provider, &mut sel, ENDPOINT_RETRY).await;
            handle_pull(&remote, cfg, label, &dir, max_size, remote_path, no_git, no_big, &sel, dry_run, yes, RSYNC).await?;
        }

        PodCmd::Baseline { target, week, dir, replace, dry_run } => {
            let dir = dir.unwrap_or_else(|| local_backup_dir(cfg));
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            handle_baseline(provider, &remote, cfg, &target, week, &dir, replace, dry_run, yes, RSYNC, now).await?;
        }

        PodCmd::Restore { target, from, path, dir, timeout, overwrite_newer, with_git, dry_run } => {
            let dir = dir.unwrap_or_else(|| local_backup_dir(cfg));
            let timeout = timeout.map_or(arena_core::restore::DEFAULT_TIMEOUT, Duration::from_secs);
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let opts = RestoreOpts { from, path, dir, timeout, overwrite_newer, with_git, dry_run, yes };
            handle_restore(provider, &remote, cfg, &target, opts, RSYNC, now).await?;
        }

        PodCmd::CopyKeys { sel, keys_dir: dir, hf_token, cc_token, dry_run } => {
            let sel = select(provider, cfg, &sel.args(), Unscoped::All).await?;
            let dir = dir.unwrap_or_else(|| keys_dir(cfg));
            handle_copy_keys(remote, cfg, &dir, hf_token, cc_token, &sel, dry_run, yes).await?;
        }

        PodCmd::Cp { file, dest, recursive, sel, timeout, dry_run } => {
            let timeout = timeout.map(Duration::from_secs);
            let sel = select(provider, cfg, &sel.args(), Unscoped::All).await?;
            handle_copy(remote, cfg, &file, dest.as_deref(), recursive, &sel, timeout, dry_run, yes).await?;
        }

        PodCmd::Restart { target, wipe_ok, no_setup, skip_proxy, dry_run } => {
            let opts = RestartOpts { wipe_ok, no_setup, skip_proxy, dry_run, yes };
            handle_restart(provider, remote, cfg, &target, opts, RESTART_SETTLE, &keys_dir(cfg)).await?;
        }

        PodCmd::Start { no_setup, skip_proxy, dry_run, sel } => {
            let sel = select(provider, cfg, &sel.args(), Unscoped::Refuse("start")).await?;
            let opts = RestartOpts { wipe_ok: false, no_setup, skip_proxy, dry_run, yes };
            handle_start(provider, remote, cfg, &sel, opts, RESTART_SETTLE, &keys_dir(cfg)).await?;
        }

        PodCmd::Rename { target, new_name, from_prefix, skip_proxy, dry_run } => {
            let or = openrouter_client(cfg);
            let book = KeyBook::new(openrouter_keys_csv(cfg), or.as_ref());
            let request = match (from_prefix, target, new_name) {
                (Some(from), _, _) => RenameRequest::FromPrefix(from),
                (None, Some(old), Some(new)) => RenameRequest::One { old, new },
                _ => anyhow::bail!("pass `<old> <new>` or `--from-prefix <prefix>`"),
            };
            handle_rename(provider, remote, cfg, &book, request, skip_proxy, dry_run, yes).await?;
        }

        PodCmd::Lock { dry_run, sel } => {
            let sel = select(provider, cfg, &sel.args(), Unscoped::Refuse("lock")).await?;
            handle_lock(provider, &sel, true, dry_run, yes).await?;
        }

        PodCmd::Unlock { dry_run, sel } => {
            let sel = select(provider, cfg, &sel.args(), Unscoped::Refuse("unlock")).await?;
            handle_lock(provider, &sel, false, dry_run, yes).await?;
        }

        PodCmd::Reimage { sel, image, wipe_ok, no_setup, skip_proxy, dry_run } => {
            let sel = select(provider, cfg, &sel.args(), Unscoped::Refuse("reimage")).await?;
            let opts = RestartOpts { wipe_ok, no_setup, skip_proxy, dry_run, yes };
            handle_reimage(provider, remote, cfg, &sel, image, opts, RESTART_SETTLE, &keys_dir(cfg)).await?;
        }

        PodCmd::Replace { target, gpu, gpus, cloud, disk, volume, image, place, onto, force, keep_old, skip_proxy, dry_run } => {
            let ov = SpecOverrides { gpu, gpus, cloud, disk, volume, image, bootstrap: false, api_extra: None, ..Default::default() };
            let opts = ReplaceOpts { keep_old, skip_proxy, dry_run, yes, force };
            handle_replace(remote, provider, cfg, &target, &ov, &place, onto.target(), opts, &interrupt, bal).await?;
        }

        PodCmd::Migrate { cmd } => match cmd {
            MigrateCmd::Copy { target, gpu, gpus, cloud, disk, volume, image, bootstrap, place, onto, force, dry_run } => {
                let ov = SpecOverrides { gpu, gpus, cloud, disk, volume, image, bootstrap, api_extra: None, ..Default::default() };
                let how = CopyHow { source: onto.target(), force, dry_run, yes };
                handle_migrate_copy(remote, cfg, &target, &ov, &place, how, &interrupt, bal).await?;
            }
            MigrateCmd::Cutover { target, yes: y, force, skip_proxy, no_final_sync, dry_run } => {
                let opts = CutoverOpts { skip_proxy, no_final_sync, dry_run, yes: yes || y, force };
                handle_migrate_cutover(remote, provider, cfg, &target, opts).await?;
            }
            MigrateCmd::Finish { target, yes: y, discard_stopped, dry_run } => {
                let opts = FinishOpts { discard_stopped, dry_run, yes: yes || y };
                handle_migrate_finish(remote.as_ref(), provider, cfg, &target, opts).await?;
            }
            MigrateCmd::Revert { target, yes: y, skip_proxy, dry_run } => {
                handle_migrate_revert(remote.as_ref(), provider, cfg, &target, skip_proxy, dry_run, yes || y).await?;
            }
            MigrateCmd::Status { target } => {
                handle_migrate_status(cfg, &target).await?;
            }
        },

        PodCmd::Spare(cmd) => {
            crate::pods::spare::handle_spare(cmd, cfg, yes, &interrupt).await?;
        }

        PodCmd::CopyHome { from, to, overwrite, force, dry_run } => {
            let opts = crate::pods::copy_home::CopyHomeOpts { overwrite, force, dry_run, yes };
            crate::pods::copy_home::handle_copy_home(remote, cfg, &from, &to, opts).await?;
        }

        PodCmd::Terminate { targets, all, unlock, revoke_key, skip_proxy, dry_run } => {
            let or = openrouter_client(cfg);
            let book = KeyBook::new(openrouter_keys_csv(cfg), or.as_ref());
            let named: Vec<&str> = targets.iter().map(String::as_str).collect();
            let targets = match (all, named.is_empty()) {
                (true, _) => None,
                (false, false) => Some(named.as_slice()),
                (false, true) => anyhow::bail!("specify the pods (names or ids) to terminate, or pass --all"),
            };
            let opts = TerminateOpts { unlock, revoke_key, skip_proxy, dry_run, yes };
            handle_terminate(provider, cfg, &book, targets, opts).await?;
        }

        PodCmd::Backup { sel, cohort, no_pull, dry_run, message } => {
            // Resolved once, before the prompt (a typo fails before asking), and shared by the
            // git push and the pull.
            let sel = select(provider, cfg, &sel.args(), Unscoped::All).await?;
            let mut sel = cohort_only(cfg, sel, cohort);
            refresh_missing_endpoints(provider, &mut sel, ENDPOINT_RETRY).await;
            handle_full_backup(remote, cfg, &sel, no_pull, message, dry_run, yes, RSYNC).await?;
        }
        PodCmd::Setup { sel, dry_run, force, hf_token, cc_token, zsh_install, no_vscode, timeout } => {
            // Budgets first: a malformed SETUP_TIMEOUT_SECS fails before anything else.
            let timeouts = arena_core::setup::SetupTimeouts::from_config(cfg, timeout)?;
            // Likewise the VS Code warm-up's settings (VSCODE_PREINSTALL / VSCODE_EXTENSIONS).
            arena_core::vscode::expected_extensions(cfg)?;
            let vscode = !no_vscode && arena_core::vscode::preinstall_enabled(cfg.get("VSCODE_PREINSTALL"))?;
            // Resolved before the prompt, so a typo fails before asking and the prompt names
            // exactly the pods that will be provisioned.
            let sel = select(provider, cfg, &sel.args(), Unscoped::All).await?;
            if !dry_run
                && !sel.pods.is_empty()
                && !confirm(
                    yes,
                    &format!(
                        "Will provision {} pod(s) over SSH (deploy key, ~/.name, repo{}): {}",
                        sel.pods.len(),
                        if vscode { ", VS Code warm-up" } else { "" },
                        sel.names()
                    ),
                )?
            {
                println!("aborted.");
                return Ok(());
            }
            handle_setup(remote, cfg, !dry_run, force, hf_token, cc_token, zsh_install, no_vscode, timeouts, &sel, &keys_dir(cfg), Some(provider))
                .await?;
        }
        PodCmd::SetBranch { branch, sel, cohort, hard, dry_run, init_branches } => {
            let sel = sel.args();
            if let Some(why) = hard_non_cohort_refusal(hard, cohort, sel.names_pods()) {
                anyhow::bail!("{why}");
            }
            let sel = select(provider, cfg, &sel, Unscoped::Refuse("switch")).await?;
            let sel = cohort_only(cfg, sel, cohort);
            // The day `--init-branches` would init, before anything switches: a missing start
            // date fails here, not after the reset.
            if init_branches {
                resolve_week_day(cfg, None, None)?;
            }
            let how = SetBranchRun { branch: &branch, hard, dry_run, yes, init_branches };
            set_branch_and_init(remote, cfg, &sel, &how, RSYNC).await?;
        }
        PodCmd::Run { sel, command, timeout, dry_run, background } => {
            if let Some(why) = misplaced_run_flags(&command) {
                anyhow::bail!("{why}");
            }
            let cmd = command.join(" ");
            let budget = Duration::from_secs(timeout);
            let mut sel = select(provider, cfg, &sel.args(), Unscoped::All).await?;
            let names: Vec<(&str, &str)> = sel.pods.iter().map(|p| (p.name.as_str(), short_of(cfg, &p.name))).collect();
            if let Some(why) = pod_name_as_command(&command, &names) {
                anyhow::bail!("{why}");
            }
            refresh_missing_endpoints(provider, &mut sel, ENDPOINT_RETRY).await;
            if background {
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
                let id = arena_core::jobs::JobId::generate(now, &cmd);
                let ask = &mut |what: &str| confirm(yes, what);
                jobs::handle_start(remote, cfg, &sel, &cmd, id, dry_run, ask, &mut |l| println!("{l}")).await?;
            } else {
                handle_run(remote, cfg, &sel, &cmd, budget, dry_run, yes, /*confirm*/ true, /*compact*/ false).await?;
            }
        }
        PodCmd::Jobs { kill, sel } => {
            // Ids are checked before anything is listed: one goes into a remote path.
            let (targets, only) = arena_core::jobs::split_job_arg(&sel.targets).map_err(anyhow::Error::msg)?;
            let kill = kill.as_deref().map(arena_core::jobs::JobId::parse).transpose().map_err(anyhow::Error::msg)?;
            if let (Some(k), Some(o)) = (&kill, &only) {
                if k != o {
                    anyhow::bail!("--kill {k} and job {o}: name one job");
                }
            }
            let sel = select(provider, cfg, &Select { targets, opts: sel.opts }.args(), Unscoped::All).await?;
            match kill {
                Some(id) => {
                    let ask = &mut |what: &str| confirm(yes, what);
                    jobs::handle_kill(remote, cfg, &sel, &id, ask, &mut |l| println!("{l}")).await?
                }
                None => jobs::handle_list(remote, cfg, &sel, only.as_ref(), &mut |l| println!("{l}")).await?,
            }
        }
        PodCmd::Logs { tail, follow, sel } => {
            let (targets, job) = arena_core::jobs::split_job_arg(&sel.targets).map_err(anyhow::Error::msg)?;
            let sel = select(provider, cfg, &Select { targets, opts: sel.opts }.args(), Unscoped::All).await?;
            // Ctrl+C is only taken over once following starts; until then it ends the command.
            let stop = || interrupt.wait();
            let tail = usize::try_from(tail).unwrap_or(usize::MAX);
            jobs::handle_logs(remote, cfg, &sel, job.as_ref(), tail, follow, jobs::FOLLOW_INTERVAL, stop, &mut |l| println!("{l}"))
                .await?;
        }
        PodCmd::Reap(cmd) => {
            let env = reap::ReapEnv {
                provider,
                remote: remote.clone(),
                cfg,
                yes,
                clock: reap::real::clock(),
                ntp: &reap::real::ntp,
                tz: reap::real::tz(),
                rsync: RSYNC.to_string(),
                nonce: reap::real::nonce(),
                resolve_local: &reap::real::resolve_local,
                out: &|l: &str| println!("{l}"),
                sync_proxy: true,
                config_file: cfg.source().map(std::path::Path::to_path_buf),
            };
            reap::handle_reap(cmd, &env).await?;
        }
        PodCmd::Ask { ai, model, effort, timeout, dry_run, question, cohort, sel } => {
            // Read-only on the pods, but it sends what it reads off this machine: `ASK_AI=off`
            // (or a malformed value) refuses before anything is listed or read.
            let (targets, question) = ask::split_question(&sel.targets, question).map_err(anyhow::Error::msg)?;
            let pick = arena_core::ask::ai::pick(cfg, ai, model.as_deref(), effort).map_err(anyhow::Error::msg)?;
            let sel = select(provider, cfg, &Select { targets, opts: sel.opts }.args(), Unscoped::Refuse("ask about")).await?;
            let sel = cohort_only(cfg, sel, cohort);
            let run = ask::AskRun { pick, timeout: Duration::from_secs(timeout), dry_run, question };
            let env = arena_core::ask::AiEnv::from_process();
            let stop = || ask::stop_signals(interrupt.wait());
            ask::handle_ask(remote, cfg, &sel, run, &env, stop, &mut |l| println!("{l}"), &mut |l| eprintln!("{l}")).await?;
        }
        PodCmd::Idle { hours, json, sel } => {
            // Read-only: no confirm. The report prints commands; it never runs them.
            let sel = select(provider, cfg, &sel.args(), Unscoped::All).await?;
            money::handle_idle(provider, remote, cfg, &sel, hours, json, &mut |l| println!("{l}")).await?;
        }
        PodCmd::Test { deep: true, sel, json, verbose, cohort } => {
            // Read-only: no confirm. Config first: a malformed MIN_DRIVER_VERSION fails before
            // the fleet is even listed. Fleet-wide = this cohort's pods: the check runs CUDA, NCCL
            // and a download on each GPU, never on a staff box's (the daily cron line's run).
            arena_core::health::HealthPolicy::from_config(cfg)?;
            let sel = cohort_only(cfg, select(provider, cfg, &sel.args(), Unscoped::All).await?, cohort);
            handle_deep_test(provider, remote, cfg, &sel, json, verbose).await?;
        }
        PodCmd::Test { deep: false, sel, cohort, .. } => {
            // Read-only: no confirm, compact one-line-per-pod output.
            let sel = cohort_only(cfg, select(provider, cfg, &sel.args(), Unscoped::All).await?, cohort);
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

/// Scenario tests for target selection (PLAN 1.C) at the command layer, driven by a fake
/// `Provider` that records every mutation (no real API/SSH): the shared selector picks
/// exactly the pods each command family acts on, typos fail before anything is touched,
/// commands that change pods demand names or `--all`, and every old spelling still parses.
#[cfg(test)]
mod selection_tests;

/// Every pod-SSH path over a scripted `FakeRemote` (PLAN 1.B): each call carries its
/// budget, partial failures are reported per pod and fail the command, and one wedged pod
/// never holds up the others — on a paused clock, so budgets elapse instantly and exactly.
#[cfg(test)]
pub(crate) mod remote_tests;

/// PLAN 1.D + restart safety, end to end against fakes: `rename` brings `~/.name`, the
/// OpenRouter key mapping and the proxy along; `terminate --revoke-key` revokes keys and
/// drops CSV rows without ever blocking a terminate; `restart`/`stop` say what a container
/// reset destroys, refuse it without `--wipe-ok` when nothing would survive, and a restart
/// comes back set up and proxied. No network: `FakeRemote` for SSH, a fake provider and key
/// API, temp dirs for the keys CSV and the (write-only) proxy file.
#[cfg(test)]
pub(crate) mod lifecycle_tests;

/// Moving a Hetzner VM (`replace`/`migrate` with `--server-type`, `--onto`, `--onto-spare`, the
/// "anyone on it?" checks, a VM's own identity) over a fake Hetzner backend and `FakeRemote`.
#[cfg(test)]
mod move_tests;
