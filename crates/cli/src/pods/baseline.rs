//! `pods baseline`: capture the week's shared baseline from one fresh pod, for the dated snapshots
//! to hard-link to.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use arena_core::provider::Provider;
use arena_core::remote::{describe_error, Remote, PROBE_TIMEOUT};
use arena_core::selector::Naming;
use arena_core::ssh::SshTarget;
use arena_core::Config;

use crate::cron::resolve_week_day;
use crate::{confirm, disk};

/// The oldest a pod's container may be for `pods baseline` to capture the fleet baseline from
/// it: the pod should come straight from `up`, before anyone has used it.
const BASELINE_MAX_AGE: Duration = Duration::from_secs(24 * 3600);

/// The budget for listing what was copied onto a baseline's source pod
/// ([`arena_core::pull::copied_in_command`]: one `find` over its home).
const COPIED_IN_TIMEOUT: Duration = Duration::from_secs(120);

/// Whether a pod can be the fleet baseline's source, from what it said about itself
/// ([`arena_core::volume::parse_probe`]) at `now` (UNIX seconds): `Ok(container creation time)`
/// — the line [`arena_core::pull::prune_baseline`] keeps the image's files by — or why not.
/// Fails closed: no answer, or no creation time, is a refusal.
fn baseline_source(probe: Option<&arena_core::volume::RepoProbe>, now: u64, pod: &str) -> std::result::Result<u64, String> {
    let Some(created) = probe.and_then(|p| p.container_created) else {
        return Err(format!(
            "{pod} didn't say when its container was created (no /.dockerenv, or no answer) — so which of its files \
             came with the image can't be told; pick another pod"
        ));
    };
    let age = now.saturating_sub(created);
    if age > BASELINE_MAX_AGE.as_secs() {
        return Err(format!(
            "{pod}'s container was created {}h ago — a baseline must come from a pod `up` made today, before anyone \
             used it (bring one up, capture, terminate it)",
            age / 3600
        ));
    }
    Ok(created)
}

/// `pods baseline <pod>`: capture the week's fleet baseline (see the command's help and
/// [`arena_core::pull::prune_baseline`] for why only files older than the container are
/// kept — and none copied onto the pod since with an old mtime, as `pods replace` and `pods
/// restore` do). Everything refusable is refused before the rsync: an existing baseline without
/// `--replace`, a pod that can't say when its container was created or whose container is over
/// [`BASELINE_MAX_AGE`] old, one whose copied-in files can't be listed. The rsync goes into a partial dir next to the destination
/// (bounded like a pull: `BACKUP_TIMEOUT_SECS`, 5 min without I/O), is pruned, and only then
/// takes the destination's place — a failed or stopped capture leaves no half baseline.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_baseline(
    provider: &dyn Provider,
    remote: &Arc<dyn Remote>,
    cfg: &Config,
    target: &str,
    week: Option<u32>,
    dir: &str,
    replace: bool,
    dry_run: bool,
    yes: bool,
    // The rsync to run: [`RSYNC`] for real, a stub in tests.
    rsync: &str,
    now: u64,
) -> Result<()> {
    use arena_core::{pull, volume};
    let (week, _) = resolve_week_day(cfg, week, Some(1))?;
    let budget = pull::backup_timeout(cfg.get("BACKUP_TIMEOUT_SECS"))?;
    let floor = disk::floor(cfg)?;
    let policy = arena_core::retry::RetryPolicy::default();
    let pods = arena_core::retry::retrying(&policy, || provider.list_pods()).await.context("listing pods")?;
    let pod = pods[arena_core::selector::resolve_one(&Naming::from_config(cfg), &pods, target)?].clone();
    // A stopped pod's listed endpoint belongs to nothing now — whatever answers there may be
    // another pod, whose home every snapshot would then link to — so it's refused before any
    // probe, as every SSH command refuses one (live finding #15).
    if arena_core::status::is_stopped(&pod.status) {
        anyhow::bail!("refusing the baseline: {} is stopped ({}) — `arena pods start {}` brings it back", pod.name, pod.status, pod.name);
    }
    let ssh = SshTarget::from_pod(&pod, cfg).with_context(|| format!("{}: no SSH endpoint", pod.name))?;
    let base = std::path::Path::new(dir);
    let dest = pull::baseline_dir(base, week);
    let exists = std::fs::read_dir(&dest).is_ok_and(|mut e| e.next().is_some());
    if exists && !replace {
        anyhow::bail!(
            "{} already holds week {week}'s baseline — pass --replace to capture it again (the old one is moved aside, \
             not deleted)",
            dest.display()
        );
    }
    let repo = arena_core::backup::repo_path(cfg);
    let probe = match remote.exec(&ssh, &volume::probe_command(&repo), Some(PROBE_TIMEOUT)).await {
        Ok(out) if out.success => volume::parse_probe(&out.stdout),
        _ => None,
    };
    let created = baseline_source(probe.as_ref(), now, &pod.name).map_err(|e| anyhow::anyhow!("refusing the baseline: {e}"))?;
    // What was written onto it since with an older mtime — a `pods replace`/`migrate` copy of
    // another pod's home, a `pods restore`, an archive unpacked at setup: not the image's, though
    // the mtime prune alone would keep it (fail closed: no listing, no capture).
    let copied = match remote.exec(&ssh, &pull::copied_in_command(), Some(COPIED_IN_TIMEOUT)).await {
        Ok(out) if out.success => pull::parse_copied_in(&out.stdout),
        Ok(out) => Err(format!("{}: {}", out.exit_label(), out.stderr.trim())),
        Err(e) => Err(describe_error(&e)),
    }
    .map_err(|e| {
        anyhow::anyhow!(
            "refusing the baseline: couldn't list what was copied onto {} since its container was created ({e})",
            pod.name
        )
    })?;
    let copied_note = if copied.is_empty() {
        String::new()
    } else {
        format!(
            " — and none of the {} file(s) copied onto it since with an older mtime (`pods replace`/`restore`, setup)",
            copied.len()
        )
    };
    let threshold =
        cfg.get("BACKUP_MAX_SIZE").filter(|s| !s.is_empty()).map(String::from).unwrap_or_else(|| "50M".to_string());
    let mut pc = pull::PullConfig::small_tier(threshold.as_str());
    // Last week's baseline: what didn't change in the image is linked, not copied again.
    if week > 0 {
        if let Some(prev) = pull::newest_baseline(base, Some(week - 1)).and_then(|p| std::fs::canonicalize(p).ok()) {
            pc.link_dest.push(prev.display().to_string());
        }
    }
    let mut note = String::new();
    if let Ok(Some(split)) = volume::repo_pull_from("", &repo, &ssh.user, probe.as_ref()) {
        pc.excludes.push(split.home_exclude());
        note = format!("; its repo is on the /workspace volume, so it's left out ({} isn't in the baseline)", split.rel);
    }
    let partial = dest.with_file_name(format!(".fleet.partial-{}", std::process::id()));
    let partial_dir = format!("{}/", partial.display());
    let passes: Vec<Vec<String>> = pc.passes().iter().map(|p| pull::rsync_args(&ssh, p, &partial_dir)).collect();
    let age = now.saturating_sub(created);
    let plan = format!(
        "capture week {week}'s fleet baseline from {} (id={}, container created {}, {}h{:02}m ago) into {}: rsync its home \
         (the snapshot tier's filters, .git complete{note}), then keep only the files older than the container — the \
         image's own{copied_note} — for every pod's snapshots to hard-link to{}",
        pod.name,
        pod.id,
        arena_core::restore::stamp(created),
        age / 3600,
        age % 3600 / 60,
        dest.display(),
        if exists { format!(" (the current one is moved aside to {}/fleet.replaced-<time>/)", dest.parent().unwrap_or(base).display()) } else { String::new() },
    );
    if dry_run {
        println!("[dry-run] would {plan}");
        for args in &passes {
            println!("  {}", pull::display_argv(rsync, args));
        }
        return Ok(());
    }
    if !confirm(yes, &format!("Will {plan}."))? {
        println!("aborted.");
        return Ok(());
    }
    // Room on this box first (a fresh pod's home, not measured: a first copy), held against
    // other runs' copies until the capture is done.
    let _room = match floor {
        None => disk::Hold::default(),
        Some(floor) => {
            let (skipped, hold) = disk::judge_copies(cfg, floor, &[(pod.name.clone(), vec![(partial.clone(), arena_core::disk::Size::First)])], true);
            if let Some((_, skip)) = skipped.first() {
                disk::note_skips(cfg, "baseline", &[(pod.name.clone(), skip.to_string())]);
                anyhow::bail!("refusing the baseline from {}: {skip}", pod.name);
            }
            hold
        }
    };
    std::fs::create_dir_all(&partial).with_context(|| format!("creating {}", partial.display()))?;
    let started = tokio::time::Instant::now();
    // The floor stays a hard line while it writes (`disk::Watch`: stopped under it).
    let watch = disk::Watch::new(floor, &[partial.as_path()]);
    for args in passes {
        let left = budget.saturating_sub(started.elapsed()).max(Duration::from_secs(1));
        let what = format!("rsync of {} [baseline]", pod.name);
        let failed = match disk::watched(watch.as_ref(), arena_core::remote::run_local(rsync, &args, &what, Some(left))).await {
            Ok(Ok(o)) if o.success => None,
            Ok(Ok(o)) => Some(format!("rsync {}: {}", o.exit_label(), o.stderr.trim())),
            Ok(Err(e)) => Some(describe_error(&e)),
            Err(skip) => {
                disk::note_skips(cfg, "baseline", &[(pod.name.clone(), skip.to_string())]);
                Some(skip.to_string())
            }
        };
        if let Some(why) = failed {
            // Only the partial dir this run made; the week's baseline (if any) is untouched.
            let _ = std::fs::remove_dir_all(&partial);
            anyhow::bail!("capturing the baseline from {} failed ({why}) — nothing changed under {}", pod.name, dest.display());
        }
    }
    let kept = match pull::prune_baseline(&partial, created, &copied) {
        Ok(k) => k,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&partial);
            anyhow::bail!("pruning the baseline failed ({e}) — nothing changed under {}", dest.display());
        }
    };
    if exists {
        let aside = dest.with_file_name(format!("fleet.replaced-{}", arena_core::restore::stamp(now)));
        std::fs::rename(&dest, &aside).with_context(|| format!("moving the old baseline aside to {}", aside.display()))?;
        println!("(the previous baseline is now {} — delete it yourself when you like)", aside.display());
    }
    std::fs::rename(&partial, &dest).with_context(|| format!("moving the baseline into {}", dest.display()))?;
    println!(
        "✓ week {week}'s baseline from {}: {} files, {} that came with the image (in {}); left out {} files, {} written \
         or copied onto it since the container was created. Snapshots from now on hard-link to it.",
        pod.name,
        kept.kept_files,
        pull::human_bytes(kept.kept_bytes),
        dest.display(),
        kept.removed_files,
        pull::human_bytes(kept.removed_bytes),
    );
    Ok(())
}
