//! The home-dir copy `replace` and `migrate` run: pod to pod, or through a local staging copy,
//! then verified.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use arena_core::provider::Provider;
use arena_core::remote::{describe_error, Remote, PROBE_TIMEOUT};
use arena_core::ssh::SshTarget;
use arena_core::Config;

use crate::disk;
use crate::pods::backup::RSYNC;
use crate::pods::pull::clear_stale_link;
use crate::pods::replace::{copy_marker_path, copy_marker_token, fresh_target, marker_present, target_is_pod, wait_for_endpoint};
use crate::pods::{local_size, POD_COPY_TIMEOUT};

/// How [`copy_pod_files`] runs: whether the destination is a pod this command just created
/// (its replica baseline starts now — [`arena_core::replica::receiver_prepare_command`]) and, so
/// tests can drive the via-local copy with the real rsync, which rsync runs and where its
/// staging dir goes.
pub(crate) struct CopyOpts {
    pub fresh: bool,
    pub rsync: String,
    pub stage_root: PathBuf,
}

impl CopyOpts {
    pub(crate) fn new(fresh: bool) -> Self {
        Self { fresh, rsync: RSYNC.to_string(), stage_root: std::env::temp_dir() }
    }
}

/// How often a via-local copy tries its staging dir's lock again, 200 ms apart, before it
/// takes the dir for another run's (a look at the box holds it only for an instant).
const STAGE_LOCK_TRIES: u32 = 10;

/// The via-local copy's staging dir for source pod `src_id` under `root`.
pub(crate) fn staging_dir(root: &std::path::Path, src_id: &str) -> PathBuf {
    arena_core::disk::staging_dir(root, src_id)
}

/// Remove a leftover via-local staging copy of `src_id` (the participant's whole home), once
/// it can't be needed again: the source pod is gone. Under the stage's lock, held through the
/// removal: one another run is copying through is left alone, said. Says so when there was one.
pub(crate) fn remove_staging_for(root: &std::path::Path, src_id: &str) {
    let stage = staging_dir(root, src_id);
    let _held = match arena_core::disk::lock_dir(&stage) {
        Ok(held) => held,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            return eprintln!("      the local staging copy {} is in use by another arena run — left as it is", stage.display())
        }
        Err(e) => return eprintln!("      warning: couldn't lock the local staging copy {} to remove it ({e}) — left as it is", stage.display()),
    };
    let size = arena_core::pull::human_bytes(local_size(&stage));
    match std::fs::remove_dir_all(&stage) {
        Ok(()) => println!("      removed the leftover local staging copy {} ({size})", stage.display()),
        Err(e) => eprintln!("      warning: couldn't remove the local staging copy {} ({size}): {e}", stage.display()),
    }
}

/// Whether `dir` holds any file (at any depth; links not followed) — a staging dir with none
/// has nothing worth keeping.
fn holds_files(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries.flatten().any(|e| match e.file_type() {
            Ok(t) if t.is_dir() => holds_files(&e.path()),
            Ok(_) => true,
            Err(_) => true,
        })
    })
}

/// The last non-empty stderr line of a failed command, clipped — what a fallback's note
/// quotes as the reason.
fn last_line(stderr: &str) -> String {
    let line = stderr.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("no error output");
    if line.chars().count() > 160 {
        format!("{}…", line.chars().take(159).collect::<String>())
    } else {
        line.to_string()
    }
}

/// Why an rsync over ssh failed, for the fallback's note (pure, tested): the last stderr line
/// that isn't rsync's own epilogue — its `rsync error: … (code 255)` summary and "connection
/// unexpectedly closed" say only that the transport died, and live R printed just that for a
/// missing pod-to-pod key, hiding ssh's `Permission denied (publickey)` above it. ssh's
/// known-hosts notice isn't a reason either. With nothing else there, rsync's own last line.
pub(crate) fn copy_failure_reason(stderr: &str) -> String {
    let generic = |l: &str| {
        l.starts_with("rsync error:")
            || l.contains("connection unexpectedly closed")
            || l.starts_with("Warning: Permanently added")
    };
    match stderr.lines().rev().map(str::trim).find(|l| !l.is_empty() && !generic(l)) {
        Some(line) => last_line(line),
        None => last_line(stderr),
    }
}

/// The identity check that every replica step on pod `id` runs first, in the same session
/// ([`arena_core::replica::pod_identity`]): on RunPod, PID 1's `RUNPOD_POD_ID` must be `id`; on
/// a Hetzner VM its own server id (cloud-init's instance id) — an address a deleted server held
/// can be another customer's server by now. Providers whose machines expose no id: `None` (as
/// [`target_is_pod`]).
pub(crate) fn pod_guard(provider: &dyn Provider, id: &str) -> Option<String> {
    arena_core::replica::pod_identity(provider.name(), id)
}

/// Why a replica sync was refused before anything was transferred, for the operator: what
/// the reset guard found ([`arena_core::replica::mirror_refusal`]) and what's still possible.
pub(crate) fn mirror_refusal_text(r: &arena_core::replica::MirrorRefusal, src: &str, dest: &str) -> String {
    use arena_core::replica::MirrorRefusal;
    // To the second: a reset and a sync in the same minute read as contradictory to the
    // minute ("container from 14:54, sync from 14:54" — live R, 2026-10-09).
    let when = |t: u64| {
        let day = arena_core::schedule::ymd_string((t / 86_400) as i64);
        format!("{day} {:02}:{:02}:{:02} UTC", t % 86_400 / 3600, t % 3600 / 60, t % 60)
    };
    let why = match r {
        MirrorRefusal::SourceReset { container, last_sync } => format!(
            "{src} was reset since {dest}'s last sync (its container dates from {}, the sync from {}): its home is \
             the image again",
            when(*container),
            when(*last_sync)
        ),
        MirrorRefusal::NoRecord { last_sync } => format!(
            "{src} has no record of {dest}'s last sync ({}) — it was reset or rebuilt since (the record goes with its \
             home), or {dest} was synced by an older arena",
            when(*last_sync)
        ),
    };
    format!(
        "NOT syncing {src} -> {dest}: {why}. A sync mirrors deletions, so it would delete from {dest} every file \
         of the participant's that {src} no longer has. Nothing was copied; both pods are untouched. {dest} keeps \
         its copy from that sync — cut over to it as it is (`arena pods migrate cutover <name> --no-final-sync`; \
         what changed on {src} since stays there), or terminate {dest} and copy afresh."
    )
}

/// Copy the source pod's home onto the destination pod (`replace`, `migrate copy` /
/// `cutover`). Tries a **direct** pod-to-pod rsync (run on the source, pushing to the dest
/// endpoint with the deploy key both pods hold); on failure falls back to **via-local** (pull
/// the source home into a control-side staging dir, then push it up to the dest). Both use
/// the replication filters (`arena_core::pull::PullConfig::replication`: caches, HF models,
/// `.claude`, `.ssh`, shell rc files, `~/.name` stay behind), and both carry the ARENA repo as
/// its tree (`arena_core::volume::pod_to_pod_copy` / `push_jobs`): on a pod with a volume the
/// repo is a link onto /workspace, and copying the link alone would leave the work behind.
///
/// The copy is a REPLICA ([`arena_core::replica`]): a file deleted on the source is deleted
/// on the destination too (live finding #20), but whatever the sync overwrites or deletes on
/// the destination is moved aside there (`~/arena-sync-kept/<stamp>/`, a visible folder the
/// backups carry), never destroyed — the destination may hold work of its own (live finding
/// #7: a cutover, work, a revert, then a copy that overwrote it). Before the sync the
/// destination lists what changed on it since its last one (by ctime); after a VERIFIED sync
/// it drops only the moved files on neither side of that line, and keeps and lists the rest;
/// after one that wasn't verified it drops nothing. Never `--delete` on the source, which is
/// only read, nor on any backup.
///
/// Correctness guards, learned the hard way against churning pods:
/// 1. **Both pods must be up** — a stopped source is said as such (not as a "reassigned
///    endpoint", which read as "re-run" — live finding #16); so is a stopped destination,
///    whose listed endpoint is stale.
/// 2. **Every step runs on the right pod** — on RunPod each one (the source's prepare and
///    the copy run on it; the destination's prepare, the transfer's remote rsync
///    (`--rsync-path`) and the tidy-up) first checks `RUNPOD_POD_ID` in the same session and
///    does nothing elsewhere: a recycled ip:port would otherwise copy a STRANGER's home, or
///    mirror onto one and move its files aside.
/// 3. **A mirror only from an unchanged source** — onto a destination that was synced
///    before, the source must still be what that sync copied: not reset since (its container
///    older than the destination's stamp) and holding its record of that sync (which a reset
///    takes with the home). Otherwise the sync is refused before anything moves: mirroring a
///    reset pod's bare image would delete the participant's files from the copy.
/// 4. **Fresh endpoints per transfer** — `src`/`dest` SSH ip:port are re-resolved
///    immediately before each rsync. An endpoint captured before a multi-GB pull can go
///    stale mid-pull, sending the push to a since-reassigned port.
/// 5. **Delivery is verified, not assumed** — a marker is planted on the source, carried by
///    the copy, and read back from a freshly-resolved dest. A push that "succeeded" against
///    a stale endpoint fails this check, so we never swap in a pod that didn't get the data.
///
/// The direct copy is skipped when both pods share a public IP — a pod can't reach its
/// neighbour through it, and the hairpin connect just hangs (live finding #30) — and its
/// ssh gives up connecting after seconds; when it fails, the reason printed is its own.
///
/// The via-local copy checks first (an rsync dry run) that the staging fits on the local
/// disk with room to spare — this box is shared with the live cohort — keeps the stage
/// private (0700: it is a participant's home), and removes it once the copy has landed; a
/// failed copy keeps it (a re-run continues from it), saying where and how big (live
/// finding #13: 3.1 GB per pod was left in /tmp, forever).
///
/// The probes and the direct copy go through `remote`, each with a budget (the copy gets
/// [`POD_COPY_TIMEOUT`]). The via-local legs spawn `rsync` itself ([`CopyLeg`], the same
/// budget per leg): rsync runs its own ssh transport, which is neither a `Remote` exec nor a copy.
pub(crate) async fn copy_pod_files(
    cfg: &Config,
    provider: &dyn Provider,
    remote: &dyn Remote,
    src_id: &str,
    dest_id: &str,
    opts: &CopyOpts,
) -> Result<()> {
    use arena_core::pull::{self, Mirror, PullConfig};
    use arena_core::replica::{self, SettleMode, BOOKKEEPING_TIMEOUT, WRONG_POD_EXIT};
    use arena_core::volume;
    let remote_key = cfg.get("GIT_SSH_KEY_REMOTE").unwrap_or("/root/.ssh/id_ed25519").to_string();
    let user = cfg.get("SSH_USER").unwrap_or("root").to_string();
    // What the via-local staging must leave free on this box (CONTROL_MIN_FREE_GB): a bad value
    // fails here, before anything is copied — and replace/migrate refuse it before they create
    // anything (`disk::floor_refusal`): found only here, it left the new pod billing.
    let floor = disk::floor(cfg)?;
    // The repo's place in the home, carried as a TREE: on a pod with a volume it is a link
    // onto /workspace (setup put it there), and a plain home copy would carry only the link —
    // leaving the work on the old pod's volume, which goes with it.
    let repo = arena_core::backup::repo_path(cfg);
    let rel = volume::repo_pull(&repo, &user, None).map(|p| p.rel);
    let (src_guard, dest_guard) = (pod_guard(provider, src_id), pod_guard(provider, dest_id));

    // 1. Both pods must be up: the source to be read, the destination to be written — a
    // stopped one is listed with the endpoint it HAD, which may be another pod's by now.
    if let Ok(listed) = provider.list_pods().await {
        if let Some(p) = listed.iter().find(|p| p.id == src_id).filter(|p| arena_core::status::is_stopped(&p.status)) {
            anyhow::bail!("the source {} is stopped ({}) — there is nothing running to sync from", p.name, p.status);
        }
        if let Some(p) = listed.iter().find(|p| p.id == dest_id).filter(|p| arena_core::status::is_stopped(&p.status)) {
            anyhow::bail!(
                "the destination {} is stopped ({}) — start it first (`arena pods start {}`); nothing was copied",
                p.name,
                p.status,
                p.name
            );
        }
    }
    // Identity-check the SOURCE before reading from it: if its ip:port was reassigned to a
    // different pod, we'd otherwise copy a STRANGER's home onto the new pod (and the dest
    // marker check would still pass, since the marker rides along). Fail closed.
    let src_target = fresh_target(provider, src_id, cfg).await.context("resolving source endpoint")?;
    if !target_is_pod(remote, &src_target, src_id, provider).await {
        anyhow::bail!(
            "the source {src_id}'s SSH endpoint doesn't answer as that pod (it's down, or its ip:port was \
             reassigned to another pod) — refusing to copy from it"
        );
    }
    // Plant a delivery marker on the source (a dotfile the copy carries, not matched by any
    // exclude) — verified on the dest after the copy; the dest copy is left in place for the
    // caller's persistence re-check (then cleaned up there) — and read what the reset guard
    // needs: the source's record of its syncs, and its container's age.
    let token = copy_marker_token(dest_id);
    let marker = copy_marker_path();
    let prep = replica::source_prepare_command(src_guard.as_deref(), marker, &token, dest_id, volume::CONTAINER_MARK);
    let source = match remote.exec(&src_target, &prep, Some(PROBE_TIMEOUT)).await {
        Ok(o) if o.success => replica::parse_source_state(&o.stdout),
        Ok(o) if o.code == Some(WRONG_POD_EXIT) => anyhow::bail!(
            "the source {src_id}'s endpoint answered as a different pod — refusing to copy the wrong pod's data. Re-run."
        ),
        _ => None,
    }
    .ok_or_else(|| anyhow::anyhow!("couldn't prepare the source {src_id} for the copy (planting its marker) — nothing was copied"))?;
    let unmark = |landed: bool| replica::source_finish_command(src_guard.as_deref(), marker, dest_id, landed);

    // The destination, in one session behind its identity check: its baseline when this
    // command just made it (what's on it is image + setup), its last sync, and the list of
    // what changed on it since — what the tidy-up afterwards may never drop.
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let stamp = replica::sync_stamp(now);
    let dest_pod = wait_for_endpoint(provider, dest_id, 120).await.context("resolving dest endpoint")?;
    let prepared = match SshTarget::from_pod(&dest_pod, cfg) {
        Ok(t) => match remote.exec(&t, &replica::receiver_prepare_command(dest_guard.as_deref(), &stamp, rel.as_deref(), opts.fresh), Some(BOOKKEEPING_TIMEOUT)).await {
            Ok(o) if o.success => replica::parse_prepared(&o.stdout).ok_or_else(|| "it didn't say it was ready".to_string()),
            Ok(o) if o.code == Some(WRONG_POD_EXIT) => Err(format!(
                "its endpoint {}:{} doesn't answer as {dest_id} (reassigned to another pod?)",
                dest_pod.ssh_ip.as_deref().unwrap_or("?"),
                dest_pod.ssh_port.unwrap_or(0)
            )),
            Ok(o) => Err(last_line(&o.stderr)),
            Err(e) => Err(describe_error(&e)),
        },
        Err(e) => Err(e.to_string()),
    };
    let prepared = match prepared {
        Ok(p) => p,
        Err(why) => {
            let _ = remote.exec(&src_target, &unmark(false), Some(PROBE_TIMEOUT)).await;
            anyhow::bail!("couldn't prepare {} for the sync: {why} — nothing was copied", dest_pod.name);
        }
    };
    // 3. A mirror only from a source that is still what the last sync copied.
    if let Some(r) = replica::mirror_refusal(opts.fresh, prepared.last_sync, &source) {
        let _ = remote.exec(&src_target, &unmark(false), Some(PROBE_TIMEOUT)).await;
        let src_name = provider
            .list_pods()
            .await
            .ok()
            .and_then(|ps| ps.into_iter().find(|p| p.id == src_id).map(|p| p.name))
            .unwrap_or_else(|| src_id.to_string());
        anyhow::bail!("{}", mirror_refusal_text(&r, &src_name, &dest_pod.name));
    }
    let pc = PullConfig {
        mirror: replica::home_mirror(&stamp),
        rsync_path: dest_guard.as_deref().map(replica::guarded_rsync_path),
        ..PullConfig::replication()
    };

    let transfer = async {
        // --- direct attempt: rsync ON the source, pushing to the dest's fresh endpoint ---
        let dest_ip = dest_pod.ssh_ip.clone().unwrap_or_default();
        let direct_ok = if dest_ip == src_target.host {
            eprintln!(
                "      both pods are behind {dest_ip} — a pod can't reach its neighbour through their shared IP, \
                 so copying via local staging…"
            );
            false
        } else {
            let direct = volume::pod_to_pod_copy(&dest_ip, dest_pod.ssh_port.unwrap_or(22), &user, &remote_key, &pc, rel.as_deref());
            // Behind the source's check too: it runs on whatever answers at the source's endpoint.
            let direct = match src_guard.as_deref() {
                Some(cond) => replica::guarded(Some(cond), &direct),
                None => direct,
            };
            match remote.exec(&src_target, &direct, Some(POD_COPY_TIMEOUT)).await {
                Ok(o) if o.success => true,
                Ok(o) => {
                    eprintln!("      direct pod-to-pod copy failed ({}) — copying via local staging…", copy_failure_reason(&o.stderr));
                    false
                }
                // A copy that ran out of its budget was under way (a missing pod-to-pod key fails at
                // once), so the via-local fallback would only repeat it, slower: stop here instead.
                // Nothing has been swapped, and rsync is incremental — copying onto the SAME dest
                // again continues from here. How to do that depends on the caller (`migrate copy`
                // re-uses `-new`; `replace` refuses while it exists), so the caller says it.
                Err(e @ arena_core::Error::Timeout { .. }) => anyhow::bail!(
                    "direct pod-to-pod copy {} — NOT swapping; the original is untouched. What was \
                     copied stays on the new pod: copying onto it again continues from there.",
                    describe_error(&e)
                ),
                Err(e) => {
                    eprintln!("      direct pod-to-pod copy couldn't run ({}) — copying via local staging…", describe_error(&e));
                    false
                }
            }
        };
        if direct_ok {
            println!("      copied (direct pod-to-pod)");
            return Ok(());
        }
        // Refused for room: recorded for the health check, said with how much to free where,
        // arena's biggest dirs there, and another filesystem to stage on (`need`: its size, 0
        // before it is sized).
        let refuse = |skip: &arena_core::disk::Skip, floor, need| {
            disk::note_skips(cfg, "replace/migrate staging", &[(dest_pod.name.clone(), skip.to_string())]);
            anyhow::anyhow!("not copying via local staging ({}): {}", opts.stage_root.display(), disk::staging_advice(cfg, skip, need, floor))
        };
        // Already under the floor: no copy fits, whatever its size — said before it is sized (an
        // rsync dry run over the whole home, for nothing) or anything is made.
        if let Some(floor) = floor {
            if let Some(fs) = arena_core::disk::reading(&opts.stage_root).ok().filter(|fs| fs.free < floor.bytes) {
                if let Err(skip) = arena_core::disk::judge(&fs, 0, &arena_core::disk::Size::Measured(0).into(), floor) {
                    return Err(refuse(&skip, floor, 0));
                }
            }
        }
        // pull source -> staging (fresh source), then push staging -> dest (FRESH dest, right
        // before the push — this is the endpoint most likely to have gone stale during the pull).
        let stage = staging_dir(&opts.stage_root, src_id);
        // Its lock, held for the whole copy: a second replace/migrate of this pod can't write
        // into the same stage, no cleanup removes it under this one (a cleanup holds it while it
        // removes one), and `config check` / `teardown --check` can tell it in use from one left
        // behind (their look takes the lock for an instant: tried again for a moment). Refused,
        // the other run's stage is left exactly as it is.
        let _held = {
            let mut tries = 0;
            loop {
                std::fs::create_dir_all(&stage).with_context(|| format!("creating staging dir {}", stage.display()))?;
                match arena_core::disk::lock_dir(&stage) {
                    Ok(held) => break held,
                    // Removed by a cleanup between the mkdir and the lock: made again.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound && tries < STAGE_LOCK_TRIES => tries += 1,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && tries < STAGE_LOCK_TRIES => {
                        tries += 1;
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => anyhow::bail!(
                        "another arena run is copying {src_id} through {} right now — nothing copied; let it finish \
                         (or stop it), then re-run",
                        stage.display()
                    ),
                    Err(e) => anyhow::bail!("locking the staging dir {}: {e}", stage.display()),
                }
            }
        };
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("making the staging dir {} private", stage.display()))?;
        }
        let stage_s = format!("{}/", stage.to_string_lossy());
        let kept_note = |stage: &std::path::Path| {
            format!(
                "the local staging copy is kept at {} ({}) so a re-run continues from it — `rm -rf {}` if you won't re-run",
                stage.display(),
                arena_core::pull::human_bytes(local_size(stage)),
                stage.display()
            )
        };
        let staged = async {
            let src_target = fresh_target(provider, src_id, cfg).await?;
            if !target_is_pod(remote, &src_target, src_id, provider).await {
                anyhow::bail!(
                    "the source {src_id}'s endpoint answered as a different pod before the pull — refusing to copy \
                     the wrong pod's data. Re-run."
                );
            }
            // Where the source's repo really is (read-only, bounded): a link is pulled as its tree,
            // into its place in the staging dir — as `pods pull` does. The stage is the tool's own
            // scratch copy: an exact mirror, so nothing deleted on the source rides back through it.
            let probe = match remote.exec(&src_target, &volume::probe_command(&repo), Some(PROBE_TIMEOUT)).await {
                Ok(out) if out.success => volume::parse_probe(&out.stdout),
                _ => None,
            };
            // The pulls read only the source: its remote rsync starts only there.
            let staging_pc = PullConfig {
                mirror: Mirror::Exact,
                rsync_path: src_guard.as_deref().map(replica::guarded_rsync_path),
                ..PullConfig::replication()
            };
            let jobs = volume::pull_jobs(&staging_pc, &stage_s, volume::repo_pull(&repo, &user, probe.as_ref()).as_ref());
            // Room first: what the pull would add to the stage (an rsync dry run), against the
            // free space this box keeps (CONTROL_MIN_FREE_GB, `arena_core::disk`).
            let mut needed = 0u64;
            for job in &jobs {
                let mut args = pull::rsync_args(&src_target, &job.config, &job.dest);
                args.insert(0, "--dry-run".into());
                let out = arena_core::remote::run_local(&opts.rsync, &args, "rsync", Some(POD_COPY_TIMEOUT))
                    .await
                    .context("sizing the local staging (rsync --dry-run)")?;
                if !out.success {
                    anyhow::bail!("sizing the local staging (rsync --dry-run) failed: {}", out.stderr.trim());
                }
                needed += arena_core::replica::transfer_bytes(&out.stdout)
                    .ok_or_else(|| anyhow::anyhow!("sizing the local staging: rsync --dry-run reported no size"))?;
            }
            // (Held, against other runs' copies, until the staging is done.)
            let _room = match floor {
                None => disk::Hold::default(),
                Some(floor) => {
                    let copy = [(dest_pod.name.clone(), vec![(opts.stage_root.clone(), arena_core::disk::Size::Measured(needed))])];
                    let (skipped, hold) = disk::judge_copies(cfg, floor, &copy, true);
                    if let Some((_, skip)) = skipped.first() {
                        return Err(refuse(skip, floor, needed));
                    }
                    hold
                }
            };
            for job in jobs.iter().filter(|j| j.repo) {
                clear_stale_link(&job.dest).with_context(|| format!("clearing an old link at {}", job.dest))?;
                std::fs::create_dir_all(&job.dest).with_context(|| format!("creating {}", job.dest))?;
            }
            // Each leg's rsyncs (the home, then a split-out repo) share the leg's one budget. The
            // floor stays a hard line while they write (`disk::Watch`: stopped under it).
            let leg = CopyLeg::start(POD_COPY_TIMEOUT);
            let watch = disk::Watch::new(floor, &[opts.stage_root.as_path()]);
            let pulled = disk::watched(watch.as_ref(), async {
                for job in &jobs {
                    leg.rsync(&opts.rsync, &pull::rsync_args(&src_target, &job.config, &job.dest))
                        .await
                        .context("pull source -> staging")?;
                }
                anyhow::Ok(())
            })
            .await;
            if let Err(skip) = &pulled {
                disk::note_skips(cfg, "replace/migrate staging", &[(dest_pod.name.clone(), skip.to_string())]);
            }
            pulled.map_err(|skip| {
                let advice = match floor {
                    Some(floor) => disk::staging_advice(cfg, &skip, needed, floor),
                    None => skip.to_string(),
                };
                anyhow::anyhow!("pull source -> staging ({}): {advice}", opts.stage_root.display())
            })??;
            let dest_target = fresh_target(provider, dest_id, cfg).await?;
            let staged_repo = rel.as_ref().is_some_and(|r| std::fs::symlink_metadata(stage.join(r)).is_ok_and(|m| m.is_dir()));
            let leg = CopyLeg::start(POD_COPY_TIMEOUT);
            for (src, pc) in volume::push_jobs(&pc, &stage_s, rel.as_deref(), staged_repo) {
                leg.rsync(&opts.rsync, &pull::push_rsync_args(&dest_target, &pc, &src)).await.context("push staging -> dest")?;
            }
            anyhow::Ok(())
        }
        .await;
        match staged {
            Ok(()) => {
                let size = arena_core::pull::human_bytes(local_size(&stage));
                match std::fs::remove_dir_all(&stage) {
                    Ok(()) => println!("      copied (via local staging; the staging copy, {size}, is removed)"),
                    Err(e) => eprintln!("      copied via local staging, but couldn't remove {} ({size}): {e}", stage.display()),
                }
                Ok(())
            }
            // Nothing staged (refused before the pull, or it failed at once): no copy to keep.
            Err(e) if !holds_files(&stage) => {
                let _ = std::fs::remove_dir_all(&stage);
                Err(e)
            }
            Err(e) => Err(e.context(kept_note(&stage))),
        }
    }
    .await;

    // Verify the data actually landed on the RIGHT pod (identity + marker). `marker_present`
    // re-resolves the endpoint fresh and confirms `RUNPOD_POD_ID` matches, so a push that
    // "succeeded" against a since-reassigned port (a different pod) is caught.
    let landed = transfer.is_ok() && marker_present(provider, remote, dest_id, cfg).await;
    // Clean the SOURCE marker now (and record a landed sync there, for the next one's reset
    // guard); leave the DEST marker for the caller's persistence re-check.
    let _ = remote.exec(&src_target, &unmark(landed), Some(PROBE_TIMEOUT)).await;
    // Sort what the sync moved aside on the destination — always, so even a partial sync's
    // report happens; only a verified one drops anything or moves the baseline on. Behind the
    // destination's own check: on any other pod it does nothing.
    let mode = if landed { SettleMode::Advance } else { SettleMode::Keep };
    let kept_at = format!("~/{}/{stamp}", replica::KEPT_DIR);
    match fresh_target(provider, dest_id, cfg).await {
        Ok(t) => match remote.exec(&t, &replica::settle_command(dest_guard.as_deref(), &stamp, rel.as_deref(), mode), Some(BOOKKEEPING_TIMEOUT)).await {
            Ok(o) if o.success => match replica::parse_settle(&o.stdout) {
                Some(s) => {
                    for line in replica::settle_lines(&dest_pod.name, &s, mode) {
                        eprintln!("      {line}");
                    }
                }
                None => eprintln!("      warning: couldn't read what the sync moved aside on {} (see {kept_at} there)", dest_pod.name),
            },
            Ok(o) if o.code == Some(WRONG_POD_EXIT) => eprintln!(
                "      warning: {}'s endpoint no longer answers as it — what the sync moved aside there ({kept_at}) wasn't sorted",
                dest_pod.name
            ),
            _ => eprintln!("      warning: couldn't sort what the sync moved aside on {} (it's in {kept_at} there)", dest_pod.name),
        },
        Err(_) => eprintln!("      warning: couldn't reach {} to sort what the sync moved aside ({kept_at})", dest_pod.name),
    }
    transfer?;
    if !landed {
        anyhow::bail!(
            "copy verification failed — the data didn't reach the intended new pod ({dest_id}) \
             (a churning endpoint likely got reassigned mid-transfer). NOT swapping; the \
             original is untouched. Re-run to retry."
        );
    }
    Ok(())
}

/// How long [`would_move_aside`]'s dry run may take: it walks both homes but moves nothing.
pub(crate) const DRY_RUN_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The receiver's OWN files a sync from `src_id` onto `dest_id` would move aside — the ones it
/// would overwrite or delete there ([`arena_core::replica::moved_aside`]) — by a dry run of the
/// direct copy [`copy_pod_files`] makes (same filters, same mirror, run on the source behind its
/// identity check, the remote side behind the destination's). Read-only: nothing is planted,
/// prepared or transferred. `Err`: why it couldn't tell — a stopped pod, both behind one IP (no
/// direct path), the dry run failing.
pub(crate) async fn would_move_aside(
    cfg: &Config,
    provider: &dyn Provider,
    remote: &dyn Remote,
    src_id: &str,
    dest_id: &str,
) -> std::result::Result<Vec<String>, String> {
    use arena_core::pull::PullConfig;
    use arena_core::{replica, volume};
    let remote_key = cfg.get("GIT_SSH_KEY_REMOTE").unwrap_or("/root/.ssh/id_ed25519").to_string();
    let user = cfg.get("SSH_USER").unwrap_or("root").to_string();
    let repo = arena_core::backup::repo_path(cfg);
    let rel = volume::repo_pull(&repo, &user, None).map(|p| p.rel);
    let src = fresh_target(provider, src_id, cfg).await.map_err(|e| format!("the source has no endpoint ({e:#})"))?;
    let dest = wait_for_endpoint(provider, dest_id, 120).await.map_err(|e| format!("the destination has no endpoint ({e:#})"))?;
    let dest_ip = dest.ssh_ip.clone().unwrap_or_default();
    if dest_ip == src.host {
        return Err(format!("both are behind {dest_ip}, so there is no direct path to dry-run"));
    }
    let pc = PullConfig {
        mirror: replica::home_mirror(&replica::sync_stamp(0)),
        rsync_path: pod_guard(provider, dest_id).as_deref().map(replica::guarded_rsync_path),
        dry_run: true,
        ..PullConfig::replication()
    };
    let cmd = volume::pod_to_pod_copy(&dest_ip, dest.ssh_port.unwrap_or(22), &user, &remote_key, &pc, rel.as_deref());
    let cmd = replica::guarded(pod_guard(provider, src_id).as_deref(), &cmd);
    match remote.exec(&src, &cmd, Some(DRY_RUN_TIMEOUT)).await {
        Ok(o) if o.success => Ok(replica::moved_aside(&o.stdout, rel.as_deref())),
        Ok(o) if o.code == Some(replica::WRONG_POD_EXIT) => Err("an endpoint answered as another machine".into()),
        Ok(o) => Err(format!("the dry run failed: {}", copy_failure_reason(&o.stderr))),
        Err(e) => Err(format!("the dry run couldn't run: {}", describe_error(&e))),
    }
}

/// One leg of the via-local copy — the pull into staging, or the push out of it — and its
/// clock. A leg is up to two rsyncs (the home, then a linked repo's tree, split out because a
/// home copy alone would carry only the link), and they share ONE budget, as the direct
/// copy's two rsyncs share its one exec: each gets what the ones before it left, and one that
/// would start with nothing left doesn't start. A per-rsync budget would let a split leg run
/// twice as long as the "2 h per leg" the operator is promised.
///
/// The rsyncs are deliberately not [`Remote`] calls (like `pods pull`'s): rsync drives its own
/// ssh transport (`-e`), so it is neither an exec nor a single-file copy — but it is stopped
/// the same way ([`arena_core::remote::run_local`]), and its `--timeout` gives up on a silent
/// connection long before the budget does. A stopped leg swaps nothing: the caller bails
/// before the marker check, and rsync is incremental, so a re-run continues from what was
/// copied.
pub(crate) struct CopyLeg {
    budget: Duration,
    deadline: tokio::time::Instant,
}

impl CopyLeg {
    /// Start the leg's clock: `budget` ([`POD_COPY_TIMEOUT`]) for all of its rsyncs together.
    pub(crate) fn start(budget: Duration) -> Self {
        Self { budget, deadline: tokio::time::Instant::now() + budget }
    }

    /// Run one of the leg's rsyncs (`program` + argv) within what is left of the leg's budget:
    /// an error (with stderr) on a non-zero exit, `rsync timed out after <the leg's budget>`
    /// when the leg runs out — mid-rsync, or before one that would start with nothing left.
    pub(crate) async fn rsync(&self, program: &str, args: &[String]) -> Result<()> {
        let spent = || {
            anyhow::anyhow!(
                "rsync timed out after {} (the copy leg's budget, for all its rsyncs together)",
                arena_core::error::human_duration(&self.budget)
            )
        };
        let left = self.deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return Err(spent());
        }
        match arena_core::remote::run_local(program, args, "rsync", Some(left)).await {
            Ok(out) if out.success => Ok(()),
            Ok(out) => anyhow::bail!("rsync failed: {}", out.stderr.trim()),
            Err(arena_core::Error::Timeout { .. }) => Err(spent()),
            Err(e) => Err(e.into()),
        }
    }
}

/// Pre-swap health check: the replacement must answer over SSH (and report its GPU if it
/// has one). Re-resolves the pod's endpoint fresh on each attempt and retries a few times,
/// because RunPod can reassign a pod's SSH ip/port right after provisioning — a single shot
/// against a stale endpoint spuriously reads as "Permission denied". (A precise copied-size
/// check is approximated by rsync's own success in `copy_pod_files`, plus the manual swap
/// confirm; the post-swap `~/.name` write re-resolves its own endpoint.)
pub(crate) async fn verify_replacement(provider: &dyn Provider, remote: &dyn Remote, id: &str, cfg: &Config) -> Result<()> {
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
