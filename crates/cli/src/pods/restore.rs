//! `pods restore`: push a local backup back onto one pod (never deletes).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use arena_core::provider::Provider;
use arena_core::remote::{describe_error, Remote, PROBE_TIMEOUT};
use arena_core::selector::Naming;
use arena_core::ssh::SshTarget;
use arena_core::Config;

use crate::confirm;
use crate::pods::pull::pull_roots;

/// `pods restore`'s switches, resolved (the backup dir and budget defaulted).
#[derive(Debug, Clone)]
pub(crate) struct RestoreOpts {
    pub from: Option<String>,
    pub path: Option<String>,
    pub dir: String,
    pub timeout: Duration,
    pub overwrite_newer: bool,
    pub with_git: bool,
    pub dry_run: bool,
    pub yes: bool,
}

/// `pods restore <pod>`: push one of the pod's local backups back onto it (see
/// `arena_core::restore` for why each rsync flag is there — never `--delete`, files newer on
/// the pod left alone, replaced files kept (on the volume when there is one), written through
/// the repo's link onto the volume, no `.git` from a snapshot). Everything that can be refused
/// is refused before the pod is touched: a stopped pod (its listed endpoint is stale), no such
/// backup, an empty one, a `--path` that isn't in it, a pod without an endpoint. The pod is then asked (read-only, bounded) where its repo
/// lives, whether its volume is mounted and when its container was created, so the
/// confirmation can say where the work lands, where replaced files are kept, and whether the
/// snapshot was written after the pod's disk was reset. The rsync runs over the pod's direct
/// endpoint within `opts.timeout` (stopped like a timed-out ssh); after one that pushed the
/// repo's `.git`, the repo is checked (bounded). `rsync` is the program (a stub in tests),
/// `now` the UNIX time naming the kept-files dir.
pub(crate) async fn handle_restore(
    provider: &dyn Provider,
    remote: &Arc<dyn Remote>,
    cfg: &Config,
    target: &str,
    opts: RestoreOpts,
    rsync: &str,
    now: u64,
) -> Result<()> {
    use arena_core::{pull, restore, volume};
    let policy = arena_core::retry::RetryPolicy::default();
    let pods = arena_core::retry::retrying(&policy, || provider.list_pods()).await.context("listing pods")?;
    let pod = pods[arena_core::selector::resolve_one(&Naming::from_config(cfg), &pods, target)?].clone();
    // A stopped pod's listed endpoint belongs to nothing now: whatever answers there may be
    // another pod, and the push would write this pod's backup into a stranger's home. Refused
    // before anything else, dry run included (it probes the pod), as every SSH command
    // refuses one (live finding #15).
    if arena_core::status::is_stopped(&pod.status) {
        anyhow::bail!("refusing to restore {}: it is stopped ({}) — `arena pods start {}` brings it back", pod.name, pod.status, pod.name);
    }
    let base = std::path::Path::new(&opts.dir);
    // Two volumes (`LOCAL_BACKUP_BIG_DIR`): the all-files tier is on its own root.
    let (big_root, two_volume) = pull_roots(cfg, &opts.dir);
    let (label, src) = match opts.from.as_deref() {
        Some(restore::BIG) if two_volume => restore::resolve_big(std::path::Path::new(&big_root), &pod.name),
        from => restore::resolve_source(base, std::path::Path::new(&big_root), &pod.name, from),
    }
    .map_err(|e| anyhow::anyhow!("refusing to restore {}: {e}", pod.name))?;
    let sub = opts.path.as_deref().map(restore::check_subpath).transpose().map_err(|e| anyhow::anyhow!(e))?;
    let what = sub.as_ref().map_or_else(|| src.clone(), |p| src.join(p));
    if std::fs::symlink_metadata(&what).is_err() {
        anyhow::bail!("refusing to restore {}: {} isn't in that backup", pod.name, what.display());
    }
    let stats = restore::tree_stats(&what).with_context(|| format!("reading {}", what.display()))?;
    if stats.files == 0 {
        anyhow::bail!("refusing to restore {}: {} holds no files", pod.name, what.display());
    }
    let ssh = SshTarget::from_pod(&pod, cfg).with_context(|| format!("refusing to restore {}: no SSH endpoint", pod.name))?;
    let repo = arena_core::backup::repo_path(cfg);
    let probe = match remote.exec(&ssh, &volume::probe_command(&repo), Some(PROBE_TIMEOUT)).await {
        Ok(out) if out.success => volume::parse_probe(&out.stdout),
        _ => None,
    };
    let stamp = restore::stamp(now);
    // .git from the big tier (every file); from a snapshot only when asked.
    let git = label == restore::BIG || opts.with_git;
    let flags = restore::RestoreFlags {
        kept: restore::kept_dir(probe.as_ref().is_some_and(|p| p.volume_mounted), &stamp),
        overwrite_newer: opts.overwrite_newer,
        git,
    };
    let args = restore::restore_args(&ssh, &src, sub.as_deref(), &flags);
    let kept = restore::kept_dir_display(&flags.kept);
    let tier = if label == restore::BIG { "the all-files tier" } else { "a snapshot: files under the size cap" };
    let into = sub.as_ref().map_or_else(|| "~/".to_string(), |p| format!("~/{p}"));
    let backup = RestoreSource { base, pod: &pod.name, label: &label, src: &src, sub: sub.as_deref(), last_written: stats.last_written };
    let note = restore_notes(&repo, &ssh.user, probe.as_ref(), &backup, git).iter().map(|n| format!("\n         {n}")).collect::<String>();
    let newer = if opts.overwrite_newer {
        "files on the pod newer than the backup's ARE replaced"
    } else {
        "files on the pod newer than the backup's stay"
    };
    let plan = format!(
        "restore {} (id={}, {}):\n  from:  {} ({label}, {tier}) — {} file(s), {}\n  to:    {}:{into} over {}:{}{note}\n  \
         never deletes; {newer}; files it replaces are kept on the pod under {kept} (pulls don't back that up); \
         ~/.ssh, the shell rc files, ~/.name and .claude* are not pushed",
        pod.name,
        pod.id,
        pod.provider,
        what.display(),
        stats.files,
        pull::human_bytes(stats.bytes),
        pod.name,
        ssh.host,
        ssh.port,
    );
    if opts.dry_run {
        // Shell-quoted, so the line pastes as exactly this argv (the `.claude*` exclude unquoted
        // would expand against the operator's directory).
        println!("[dry-run] would {plan}\n  {}", pull::display_argv(rsync, &args));
        return Ok(());
    }
    if !confirm(opts.yes, &format!("Will {plan}"))? {
        println!("aborted.");
        return Ok(());
    }
    let what_run = format!("rsync to {}", pod.name);
    let out = match arena_core::remote::run_local(rsync, &args, &what_run, Some(opts.timeout)).await {
        Ok(out) if out.success => out,
        Ok(out) => anyhow::bail!(
            "restoring {} failed (rsync {}): {} — nothing on the pod was deleted; re-run to finish",
            pod.name,
            out.exit_label(),
            out.stderr.trim()
        ),
        Err(e) => anyhow::bail!(
            "restoring {}: {} — nothing on the pod was deleted; re-run to finish",
            pod.name,
            describe_error(&e)
        ),
    };
    let moved = pull::parse_rsync_stats(&out.stdout)
        .map_or_else(|| "done".into(), |s| format!("{} file(s), {} sent", s.files_sent, pull::human_bytes(s.bytes_sent)));
    println!("✓ restored {} from {label} ({moved}); anything it replaced is under {kept} on the pod", pod.name);
    let kept_newer = restore::newer_on_pod(&out.stdout);
    if !kept_newer.is_empty() {
        let shown: Vec<&str> = kept_newer.iter().take(10).map(String::as_str).collect();
        let more = if kept_newer.len() > shown.len() { format!(" (+{} more)", kept_newer.len() - shown.len()) } else { String::new() };
        println!(
            "  {} file(s) on the pod are newer than the backup's and were left as they are{more}: {} — --overwrite-newer replaces them",
            kept_newer.len(),
            shown.join(", ")
        );
    }
    if restore::pushes_repo_git(&src, sub.as_deref(), &repo_rel(&repo, &ssh.user), git) {
        match remote.exec(&ssh, &restore::git_check_command(&repo), Some(restore::GIT_CHECK_TIMEOUT)).await {
            Ok(o) if o.success && restore::parse_git_check(&o.stdout) == Some(true) => {}
            Ok(o) if o.success && restore::parse_git_check(&o.stdout) == Some(false) => anyhow::bail!(
                "restored {}, but git can't read {repo} on it any more (the backup's .git lacks objects its refs need?): the \
                 .git files it replaced are under {kept} — copy them back, or restore --from big",
                pod.name
            ),
            other => {
                let why = match other {
                    Ok(o) => o.exit_label(),
                    Err(e) => describe_error(&e),
                };
                println!("  ⚠ couldn't check {repo} on {} after pushing its .git ({why}) — run `git -C {repo} fsck` there", pod.name);
            }
        }
    }
    Ok(())
}

/// The backup a restore pushes, for [`restore_notes`].
pub(crate) struct RestoreSource<'a> {
    /// The backups dir (`--dir`).
    pub base: &'a std::path::Path,
    pub pod: &'a str,
    pub label: &'a str,
    /// `<base>/<label>/<pod>`.
    pub src: &'a std::path::Path,
    /// `--path`.
    pub sub: Option<&'a str>,
    /// When a pull last wrote into what's restored ([`arena_core::restore::TreeStats`]).
    pub last_written: Option<u64>,
}

/// The confirmation's notes for a restore (reads the backups dir; pure otherwise): where the
/// repo lands ([`restore_note`]); a warning when the backup was written after the pod's disk was
/// last reset and an older snapshot predates that ([`arena_core::restore::clobber_warning`]); and
/// that a snapshot's `.git` stays behind unless `git`.
pub(crate) fn restore_notes(repo: &str, user: &str, probe: Option<&arena_core::volume::RepoProbe>, b: &RestoreSource, git: bool) -> Vec<String> {
    use arena_core::restore;
    let mut notes: Vec<String> = restore_note(repo, user, probe, b.src, b.sub).into_iter().collect();
    let container = probe.and_then(|p| p.container_created);
    if let Some(reset) = container.filter(|&c| b.last_written.is_some_and(|w| w > c)) {
        let earlier = restore::snapshot_written_before(b.base, b.pod, reset);
        notes.extend(restore::clobber_warning(b.label, b.last_written, container, earlier.as_deref()).map(|w| format!("⚠ {w}")));
    }
    if !git && restore::pushes_repo_git(b.src, b.sub, &repo_rel(repo, user), true) {
        notes.push("(.git isn't restored from a snapshot — it can lack objects over the size cap; --with-git or --from big)".into());
    }
    notes
}

/// The repo's path relative to the login home of `user` (the place it has in a backup), or
/// the configured path itself when it isn't under the home.
fn repo_rel(repo: &str, user: &str) -> String {
    arena_core::volume::repo_pull(repo, user, None).map_or_else(|| repo.trim_start_matches('/').to_string(), |p| p.rel)
}

/// The confirmation's line about the repo, when the restore carries it (pure, so tested): the
/// pod's repo is linked onto the volume → the work lands in the volume copy; the pod has a
/// volume but the repo isn't on it → say `pods setup` puts it there first; the pod didn't
/// answer → a link there is written through either way. `None` when there's nothing to say.
pub(crate) fn restore_note(
    repo: &str,
    user: &str,
    probe: Option<&arena_core::volume::RepoProbe>,
    src: &std::path::Path,
    sub: Option<&str>,
) -> Option<String> {
    use arena_core::volume::{repo_pull, RepoProbe};
    let rel = repo_pull(repo, user, None)?.rel; // the repo's place in the backup (home-relative)
    let carried = sub.is_none_or(|p| p == rel || p.starts_with(&format!("{rel}/")) || rel.starts_with(&format!("{p}/")));
    if !carried || !src.join(&rel).exists() {
        return None;
    }
    match probe {
        None => Some(format!("(couldn't ask the pod where {rel} lives — a link there, e.g. onto /workspace, is written through)")),
        Some(p @ RepoProbe { real: Some(real), .. }) => match repo_pull(repo, user, Some(p)) {
            Some(link) => Some(format!("({rel}/ lands in {} — the volume copy, through the pod's link)", link.source.trim_end_matches('/'))),
            None if p.volume_mounted && real.trim_end_matches('/') == repo.trim_end_matches('/') => {
                Some("(the pod has a /workspace volume but its repo isn't on it — `arena pods setup` first puts it there)".into())
            }
            None => None,
        },
        Some(_) => None,
    }
}
