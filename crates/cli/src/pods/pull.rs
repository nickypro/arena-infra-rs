//! `pods pull`: rsync each pod's home here; and the per-pod backup locks that `pull`, `backup`,
//! `set-branch` and the reaper take.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use arena_core::provider::Provider;
use arena_core::remote::{describe_error, Remote, PROBE_TIMEOUT};
use arena_core::selector::Naming;
use arena_core::ssh::SshTarget;
use arena_core::Config;

use crate::cli::Select;
use crate::cron::resolve_week_day;
use crate::pods::exec_each_pod;
use crate::pods::selection::{say, select, Selected, Unscoped};
use crate::{confirm, disk};

/// `pods pull`: rsync each pod's home directory to `<dir>/<label>/<pod-name>/`. The file
/// backup (legacy `backup.sh`), complementing the git autocommit `backup`. It spawns
/// `rsync` itself rather than going through a [`Remote`]: rsync drives its own ssh
/// transport (`-e`), which is neither an exec nor a single-file copy.
///
/// Bounded twice, so one wedged pod can't hold the backup — or the */15 cron behind it —
/// forever: rsync's own `--timeout` gives up after [`pull::RSYNC_IO_TIMEOUT_SECS`] without
/// any I/O, and each rsync runs within `BACKUP_TIMEOUT_SECS` of wall clock
/// ([`pull::backup_timeout`], default 2h), stopped like a timed-out ssh
/// ([`arena_core::remote::run_local`]). A pod's jobs (two tiers, each with its repo split
/// out when it's linked) run concurrently, so that is also the pod's budget. A pod that runs
/// out is a named failure; the others finish.
///
/// A repo that setup moved onto the `/workspace` volume is a symlink in the home, and rsync
/// `-a` copies a symlink as a symlink — so each pod is first asked (read-only, bounded, over
/// `remote`) where its repo really is, and a symlinked one inside the pull (the home, or a
/// `--remote-path` / `BACKUP_REMOTE_PATH` holding or naming it) is pulled from its real path
/// into the same place in the backup (`arena_core::volume::repo_pull_from`) — under the same
/// two bounds.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_pull(
    // How the pods are asked where their repo is: `SshRemote` for real, `FakeRemote` in tests.
    remote: &Arc<dyn Remote>,
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
    // The rsync to run: [`RSYNC`] for real, a recording stub in tests.
    rsync: &str,
) -> Result<()> {
    let args = PullRun { label, max_size, remote_path, no_git, no_big, dry_run, yes };
    pull_locked(remote, cfg, dir, args, sel, rsync, LockMode::Take(POD_LOCK_WAIT)).await
}

/// `pods pull`'s options, as [`handle_pull`] takes them.
pub(crate) struct PullRun {
    pub label: Option<String>,
    pub max_size: Option<String>,
    pub remote_path: Option<String>,
    pub no_git: bool,
    pub no_big: bool,
    pub dry_run: bool,
    pub yes: bool,
}

/// How a pull takes the per-pod backup locks: each name's once, waiting up to this long for
/// one another backup holds, each released when that pod's own rsyncs end (RR3). (`pods
/// backup` takes them for its git push and releases each as that pod's push ends; the pull
/// then takes them again.)
#[derive(Debug, Clone, Copy)]
pub(crate) enum LockMode {
    /// Take each pod's lock, waiting up to this long for one another backup holds.
    Take(Duration),
    /// The caller holds each pod's lock already (a branch switch holds it from the pod's backup
    /// to the end of its switch, [`switch_guarded`]): none is taken here.
    Held,
}

/// How long `pods pull` / `pods backup` wait for a pod's backup lock when another backup of
/// it (the reaper's probe — seconds — or its final backup, or another pull) holds it. Past
/// that the pod is a failure for this run (`busy: …`), the others carry on.
pub(crate) const POD_LOCK_WAIT: Duration = Duration::from_secs(120);

/// The per-pod backup locks a run holds ([`arena_core::pull::pod_lock_path`]): one per
/// distinct NAME — two listed pods sharing a name share it, so a run never waits on its own
/// lock (RR1) — each released as soon as the last piece of this run's work under it ends
/// ([`PodLocks::done`]), not when the fleet's slowest pod does (RR3); and the names another
/// backup held for the whole wait.
#[derive(Debug, Default)]
pub(crate) struct PodLocks {
    /// Held (until released or this is dropped), with how many pieces of this run's work are
    /// still to end under each.
    pub held: std::collections::HashMap<String, (arena_core::pull::PodLock, usize)>,
    /// Names whose lock stayed busy: skipped, a failure.
    pub busy: Vec<String>,
}

impl PodLocks {
    /// `name` has one more piece of work under its lock (a lock this run doesn't hold: nothing).
    pub(crate) fn expect(&mut self, name: &str) {
        if let Some((_, left)) = self.held.get_mut(name) {
            *left += 1;
        }
    }

    /// One piece of `name`'s work ended: the lock goes with the last.
    pub(crate) fn done(&mut self, name: &str) {
        if let Some((_, left)) = self.held.get_mut(name) {
            *left = left.saturating_sub(1);
            if *left == 0 {
                self.held.remove(name);
            }
        }
    }

    /// Release every lock no work was announced under ([`PodLocks::expect`]).
    pub(crate) fn release_idle(&mut self) {
        self.held.retain(|_, (_, left)| *left > 0);
    }

    /// Whether this run holds `name`'s lock.
    #[cfg(test)]
    pub(crate) fn holds(&self, name: &str) -> bool {
        self.held.contains_key(name)
    }
}

/// Take the backup lock of each distinct name in `names` in `dir` — once per name (RR1) —
/// waiting up to `wait` for the busy ones; nothing is announced under them yet (the caller
/// [`PodLocks::expect`]s its work, then releases the idle ones). A lock that can't even be
/// opened (the backup dir not writable here) is warned about once and gone without — the run
/// then fails or succeeds as it always did; only a lock another backup HOLDS keeps a pod out.
/// A relative `dir` takes no locks: the reaper (the backup these locks keep out of each
/// other's way) refuses one, and a relative path means a different place to every cwd — the
/// legacy `./backup` setup is unchanged. Nor does a `dir` that doesn't exist yet (RR5): the
/// locks never create it — the run goes as it did before them.
pub(crate) async fn take_pod_locks(dir: &str, names: &[String], wait: Duration) -> PodLocks {
    let mut out = PodLocks::default();
    if pod_locks_off(dir).is_some() {
        return out;
    }
    let mut names: Vec<String> = names.to_vec();
    names.sort();
    names.dedup();
    let mut pending: Vec<(String, std::fs::File)> = Vec::new();
    let mut warned = false;
    for name in &names {
        match arena_core::pull::open_pod_lock(dir, name) {
            Ok(f) => pending.push((name.clone(), f)),
            Err(e) if !warned => {
                warned = true;
                eprintln!("warning: can't take the per-pod backup locks in {dir}/.arena-locks ({e}) — going ahead without them");
            }
            Err(_) => {}
        }
    }
    let deadline = tokio::time::Instant::now() + wait;
    let mut said = false;
    loop {
        let mut still = Vec::new();
        for (name, f) in pending {
            match arena_core::pull::try_lock(f) {
                Ok(Ok(lock)) => {
                    out.held.insert(name, (lock, 0));
                }
                Ok(Err(f)) => still.push((name, f)),
                Err(e) => {
                    eprintln!("warning: locking {name}'s backup failed ({e}) — going ahead without its lock");
                }
            }
        }
        pending = still;
        if pending.is_empty() || tokio::time::Instant::now() >= deadline {
            break;
        }
        if !said {
            said = true;
            let busy: Vec<&str> = pending.iter().map(|(n, _)| n.as_str()).collect();
            eprintln!("waiting up to {}s: another backup of {} is running", wait.as_secs(), busy.join(", "));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    out.busy = pending.into_iter().map(|(n, _)| n).collect();
    out
}

/// Why a run takes no per-pod backup locks in `dir` — [`take_pod_locks`]' rule, kept in one
/// place so what a command says about its locks can't drift from what it does — as `(what's
/// wrong, what fixes it)`; `None` when it takes them. A relative `dir` means a different place
/// to every cwd (the legacy `./backup`), and the locks never create a `dir` that doesn't exist.
fn pod_locks_off(dir: &str) -> Option<(&'static str, &'static str)> {
    let path = std::path::Path::new(dir);
    if !path.is_absolute() {
        Some(("is relative", "set an absolute path"))
    } else if !path.is_dir() {
        Some(("doesn't exist", "create it"))
    } else {
        None
    }
}

/// What a branch switch (`set-branch`, `init-branches`) says, in one line, when it runs without
/// the per-pod backup locks ([`pod_locks_off`]): then nothing keeps the cron's backup, a pull or
/// the reaper off a pod mid-switch, and "under the pod's backup lock" would be untrue (live
/// bsec B1: the sandbox's `./backup` switched a pod whose lock a test held). `None` when the
/// locks are taken.
pub(crate) fn switch_unlocked_line(cfg: &Config) -> Option<String> {
    let dir = local_backup_dir(cfg);
    let (why, fix) = pod_locks_off(&dir)?;
    Some(format!("(no per-pod backup locks: LOCAL_BACKUP_DIR ({dir}) {why} — {fix} to keep other backups out during the switch)"))
}

/// The line for a pod whose backup lock stayed busy for the whole `wait`.
pub(crate) fn busy_line(name: &str, wait: Duration) -> String {
    format!("busy: another backup of {name} is running (waited {}s for its lock) — skipped this run", wait.as_secs())
}

/// Where a pull writes its big tier, and whether it's in two-volume mode: `LOCAL_BACKUP_BIG_DIR`
/// set and the pull going to the configured `LOCAL_BACKUP_DIR` — then both roots must carry
/// their tier marker ([`arena_core::pull::TIER_MARKER`]). An explicit `--dir` elsewhere is a
/// self-contained tree (`<dir>/big`), as before. Pure but for the caller's config.
pub(crate) fn pull_roots(cfg: &Config, dir: &str) -> (String, bool) {
    let big = cfg.get("LOCAL_BACKUP_BIG_DIR").map(str::trim).filter(|d| !d.is_empty());
    let two_volume = big.is_some() && dir.trim_end_matches('/') == local_backup_dir(cfg).trim_end_matches('/');
    let root = arena_core::pull::big_root(dir, if two_volume { big } else { None });
    (root, two_volume)
}

/// [`handle_pull`] with the locking decided by the caller ([`LockMode`]).
pub(crate) async fn pull_locked(
    remote: &Arc<dyn Remote>,
    cfg: &Config,
    dir: &str,
    args: PullRun,
    sel: &Selected,
    rsync: &str,
    locking: LockMode,
) -> Result<()> {
    pull_pods(remote, cfg, dir, args, sel, rsync, locking).await?.result()
}

/// What a pull did, for a caller that reports per pod (the backup's fallback copy): which pods
/// it couldn't copy, and where the others went. Nothing failed = an empty `failed`.
#[derive(Debug, Default)]
pub(crate) struct Pulled {
    /// Each failed rsync job (or busy lock), as the summary names it: `<pod> [<tier>] …`.
    pub failures: Vec<String>,
    /// The same, by pod: `(pod, what failed)`.
    pub failed: Vec<(String, String)>,
    /// Where each pod's copy is: `<dir>/<label>/<pod>` (the snapshot) and, unless `--no-big`,
    /// `<big root>/<pod>` — the pod's name is appended.
    pub snapshot_root: String,
    pub big_root: Option<String>,
    /// The pods copied whole: every rsync planned for them, in both tiers — the snapshot's
    /// uncapped `.git` pass among them — succeeded. Only such a copy may hold an operation's state
    /// before it is finished (`resolve`): a pod left out for any reason (its lock busy, no
    /// endpoint, a timeout, a tier or a pass that failed, not pulled at all) isn't here. Empty for a
    /// pull with `--no-big` or `--no-git`.
    pub whole: Vec<String>,
}

impl Pulled {
    /// The pull as one result: an error naming every failed job, as `pods pull` reports it.
    pub(crate) fn result(self) -> Result<()> {
        if self.failures.is_empty() {
            return Ok(());
        }
        anyhow::bail!("{} rsync job(s) failed: {}", self.failures.len(), self.failures.join(", "))
    }

    /// Where `pod`'s files went (`None`: its copy failed — [`Pulled::failed`] says how).
    pub(crate) fn copied_to(&self, pod: &str) -> Option<String> {
        if self.failed.iter().any(|(p, _)| p == pod) {
            return None;
        }
        Some(match &self.big_root {
            Some(big) => format!("{}/{pod} (and {big}/{pod})", self.snapshot_root),
            None => format!("{}/{pod}", self.snapshot_root),
        })
    }
}

/// [`pull_locked`], said per pod ([`Pulled`]). `Err` = nothing was copied (a bad label or
/// budget, a backup volume missing).
pub(crate) async fn pull_pods(
    remote: &Arc<dyn Remote>,
    cfg: &Config,
    dir: &str,
    args: PullRun,
    sel: &Selected,
    rsync: &str,
    locking: LockMode,
) -> Result<Pulled> {
    use arena_core::pull::{self, PullConfig};
    let PullRun { label, max_size, remote_path, no_git, no_big, dry_run, yes } = args;

    // Read (and validate) the budget first: a bad value fails before anything is asked or copied.
    let budget = pull::backup_timeout(cfg.get("BACKUP_TIMEOUT_SECS"))?;
    // So is the free space every copy onto this box keeps (CONTROL_MIN_FREE_GB).
    let floor = disk::floor(cfg)?;

    // Label: explicit, else the computed wNdM iteration. Never one of the backup dir's own
    // folders (the all-files tier, the baselines) or a path.
    let label = match label {
        Some(l) => l,
        None => {
            let (w, d) = resolve_week_day(cfg, None, None)?;
            format!("w{w}d{d}")
        }
    };
    if label.is_empty()
        || label.contains('/')
        || [".", "..", arena_core::restore::BIG, pull::BASELINE_DIR, arena_core::restore::REAPED].contains(&label.as_str())
    {
        anyhow::bail!("`{label}` can't be a snapshot label (a name like w1d3; not `big`, `baseline`, `reaped` or a path)");
    }
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
    // rsync's temp files go to the tier's own `.rsync-tmp/` (`<dir>/<label>/.rsync-tmp`,
    // `<dir>/big/.rsync-tmp`), outside every tree the backup archive's push reads — on the
    // destination's filesystem, checked per job below (`pull::PullConfig::temp_dir`).

    let src = if remote_path.is_empty() { "~/ (home)".to_string() } else { remote_path.clone() };
    let (big_root, two_volume) = pull_roots(cfg, dir);
    let big_note = if no_big {
        "big tier skipped".to_string()
    } else {
        format!("all files -> {big_root}/<pod>/")
    };
    // Two volumes: each root must carry its tier's marker, or a volume that didn't mount
    // would have this pull fill the empty mountpoint on `/` (and "succeed").
    if two_volume {
        let mut bad = vec![pull::check_tier_marker(std::path::Path::new(dir), pull::Tier::Snapshot)];
        if !no_big {
            bad.push(pull::check_tier_marker(std::path::Path::new(&big_root), pull::Tier::Big));
        }
        let bad: Vec<String> = bad.into_iter().filter_map(|r| r.err()).collect();
        if !bad.is_empty() {
            if dry_run {
                bad.iter().for_each(|b| println!("⚠ {b} — a real pull would refuse"));
            } else {
                anyhow::bail!("refusing to pull (LOCAL_BACKUP_BIG_DIR is set, so both backup volumes must be there): {}", bad.join("; "));
            }
        }
    }
    println!(
        "Source {src} · snapshot < {threshold} -> {dir}/{label}/<pod>/ · {big_note} · {} · {}\n",
        if no_git { "no .git" } else { "incl .git" },
        pull_budget_note(budget),
    );

    let targets = sel.named_targets(cfg)?;
    // A pod with no endpoint isn't pulled: a failure for a caller that reports per pod (the
    // backup's copy of a pod git didn't back up), though not a failed rsync for `pods pull`.
    let unreachable: Vec<(String, String)> = sel
        .pods
        .iter()
        .filter(|p| !targets.iter().any(|(n, _)| *n == p.name))
        .map(|p| (p.name.clone(), "no SSH endpoint to pull from".to_string()))
        .collect();
    if targets.is_empty() {
        println!("(no pods with an SSH endpoint to pull)");
        return Ok(Pulled { failures: sel.endpoint_failures().into_iter().map(|n| format!("{n}: no SSH endpoint after 10 minutes running")).collect(), failed: unreachable, ..Pulled::default() });
    }

    // Per-pod tiers: (tier name, dest, config). `big` is dropped when --no-big. The snapshot
    // hard-links what didn't change to the pod's previous snapshot and the fleet baseline.
    let base = std::path::Path::new(dir);
    let tiers_for = |name: &str| -> Vec<(&'static str, String, PullConfig)> {
        let mut snapshot = pull::snapshot_tier_for(base, &label, name, &small);
        snapshot.temp_dir = Some(pull::temp_dir_for(&absolute_path(&base.join(&label))));
        let mut v = vec![("snapshot", pull::local_dest(dir, &label, name), snapshot)];
        if !no_big {
            // The big tier's temp files beside its pods, under its own root — on its own volume
            // when LOCAL_BACKUP_BIG_DIR is set (a temp dir on the other volume would make rsync
            // copy every file across instead of renaming it into place).
            let big = PullConfig { temp_dir: Some(pull::temp_dir_for(&absolute_path(std::path::Path::new(&big_root)))), ..big.clone() };
            v.push(("big", pull::big_dest_in(&big_root, name), big));
        }
        v
    };
    let repo = arena_core::backup::repo_path(cfg);
    // Room on this box (CONTROL_MIN_FREE_GB, `arena_core::disk`): each pod's tiers judged
    // together, against what this run's other copies and other runs' copies under way may take,
    // before anything of it is made or asked — a pod that doesn't fit is skipped whole, never half
    // copied. A pod copied whole here before counts as a later copy where it has one to build on
    // (`disk::tier_size`), anything else as a first one. `reserve`: hold the admitted ones' space
    // for this run (until the returned hold goes, after the copies).
    let room = |names: &[String], reserve: bool| -> (Vec<(String, arena_core::disk::Skip)>, disk::Hold) {
        let Some(floor) = floor else { return (Vec::new(), disk::Hold::default()) };
        if reserve {
            disk::seed_copied(base);
        }
        let copies: Vec<(String, Vec<(PathBuf, arena_core::disk::Size)>)> = names
            .iter()
            .map(|name| {
                let parts = tiers_for(name)
                    .into_iter()
                    .map(|(tier, dest, _)| {
                        let size = disk::tier_size(base, name, tier == "snapshot", std::path::Path::new(&dest));
                        (PathBuf::from(dest), size)
                    })
                    .collect();
                (name.clone(), parts)
            })
            .collect();
        disk::judge_copies(cfg, floor, &copies, reserve)
    };

    if dry_run {
        let n = if no_big { 1 } else { 2 };
        println!("Dry-run — would rsync {} pod(s), {n} tier(s) each:\n", targets.len());
        for (name, t) in &targets {
            println!("# {name}");
            for (tier, dest, pc) in tiers_for(name) {
                for pass in pc.passes() {
                    let tier = if pass.git_only { format!("{tier} .git") } else { tier.to_string() };
                    println!("  [{tier}] {}", pull::display_rsync(t, &pass, &dest));
                }
            }
            println!();
        }
        for (name, skip) in room(&targets.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(), false).0 {
            println!("⚠ {name}, if pulled now: {skip}");
        }
        // What the pods would be asked, judged from the configured paths alone.
        let guessed: Vec<_> =
            targets.iter().map(|(_, t)| arena_core::volume::repo_pull_from(&remote_path, &repo, &t.user, None)).collect();
        if guessed.iter().any(|g| matches!(g, Ok(Some(_)))) {
            println!(
                "(each pod is first asked where {repo} really is: if it's a link — onto the /workspace \
                 volume — the link is left out of the pull and its tree pulled into the same place)"
            );
        }
        if let Some(Err(why)) = guessed.iter().find(|g| g.is_err()) {
            println!("⚠ {why}: where {repo} is a link (setup links it onto the /workspace volume), this pull would hold only the link");
        }
        println!("Preview only — run without --dry-run to copy.");
        return Ok(Pulled { failures: sel.endpoint_failures().into_iter().map(|n| format!("{n}: no SSH endpoint after 10 minutes running")).collect(), ..Pulled::default() });
    }
    let dest_msg = if no_big {
        format!("the dated snapshot {dir}/{label}/")
    } else if two_volume {
        format!("{dir}/ (dated snapshot {label}) + {big_root}/ (all files)")
    } else {
        format!("{dir}/ (dated snapshot {label} + the all-files big/)")
    };
    let names = targets.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ");
    if !confirm(yes, &format!("Will rsync {} pod home(s) into {dest_msg}: {names}", targets.len()))? {
        println!("aborted.");
        return Ok(Pulled::default());
    }
    // The run record (`arena backups`): started now, each pod's copy as the run ends.
    let run = crate::backups::Run::start(cfg, arena_core::backups::Job::Copy, sel.pods.len());
    // Each pod's backup lock, unless the caller holds them: a pod another backup holds for
    // the whole wait is skipped — a failure of this run, named (as is one there's no room for).
    let mut not_pulled: Vec<String> = Vec::new();
    // Every failure by pod too (`Pulled::failed`), for a caller that reports per pod.
    let mut by_pod: Vec<(String, String)> = unreachable;
    let mut locks = match locking {
        LockMode::Held => PodLocks::default(),
        LockMode::Take(wait) => {
            let names: Vec<String> = targets.iter().map(|(n, _)| n.clone()).collect();
            let locks = take_pod_locks(dir, &names, wait).await;
            for b in &locks.busy {
                println!("✗ {b}: {}", busy_line(b, wait));
                not_pulled.push(format!("{b} (busy)"));
                by_pod.push((b.clone(), busy_line(b, wait)));
            }
            locks
        }
    };
    let targets: Vec<(String, SshTarget)> =
        targets.into_iter().filter(|(n, _)| !not_pulled.iter().any(|f| f.starts_with(&format!("{n} ")))).collect();
    // The pods there's no room for on this box: said, recorded for the health check, failures.
    // The others' space is held until this run's copies end (`_room`, dropped on return).
    let (skipped, _room) = room(&targets.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(), true);
    for (name, skip) in &skipped {
        say(&format!("✗ {name}: {skip}"));
        not_pulled.push(format!("{name} ({})", skip.reason));
        by_pod.push((name.clone(), skip.to_string()));
    }
    disk::note_skips(cfg, "copy", &skipped.iter().map(|(n, s)| (n.clone(), s.to_string())).collect::<Vec<_>>());
    let targets: Vec<(String, SshTarget)> = targets.into_iter().filter(|(n, _)| !skipped.iter().any(|(s, _)| s == n)).collect();

    let total_pods = targets.len();
    // Where each pod's repo really is: rsync -a copies a symlink as a symlink, so a repo setup
    // linked onto the volume would be backed up as a dangling link wherever it sits in the pull
    // — a home pull, `BACKUP_REMOTE_PATH=~/` or `/root`, the repo named without a trailing
    // slash (`arena_core::volume::repo_pull_from`). `None` = the pod didn't answer: the repo is
    // then pulled through its configured path, which follows a link if there is one.
    let mut splits: std::collections::HashMap<String, arena_core::volume::RepoPull> = Default::default();
    let probe = arena_core::volume::probe_command(&repo);
    let jobs = targets.iter().map(|(name, t)| ((name.clone(), t.user.clone()), t.clone(), probe.clone())).collect();
    exec_each_pod(remote, jobs, PROBE_TIMEOUT, |_, _, (name, user), call| {
        let answer = match &call {
            Ok(out) if out.success => arena_core::volume::parse_probe(&out.stdout),
            _ => None,
        };
        match arena_core::volume::repo_pull_from(&remote_path, &repo, user, answer.as_ref()) {
            Ok(Some(split)) => {
                splits.insert(name.clone(), split);
            }
            Ok(None) => {}
            Err(why) => {
                let linked = answer.as_ref().and_then(|p| p.real.as_deref()).filter(|r| r.trim_end_matches('/') != repo.trim_end_matches('/'));
                if let Some(real) = linked {
                    println!(
                        "⚠ {name}: {repo} is a link to {real} and {why} — this pull may hold only the link: \
                         pull the home (no --remote-path), or --remote-path {real}/"
                    );
                }
            }
        }
    })
    .await;
    if total_pods > 0 {
        println!("Pulling {total_pods} pod(s) into {dest_msg}…");
    }
    // Every rsync: per pod, per tier, the home job — plus the repo job when it's split out.
    let mut planned = Vec::new();
    // How many rsync jobs each pod has, and how many succeeded: all of them = copied whole.
    let mut jobs_of: std::collections::HashMap<String, (usize, usize)> = Default::default();
    for (name, t) in &targets {
        for (tier, dest, pc) in tiers_for(name) {
            for job in arena_core::volume::pull_jobs(&pc, &dest, splits.get(name)) {
                let tier = match (job.repo, tier) {
                    (false, tier) => tier,
                    (true, "big") => "big repo",
                    (true, _) => "snapshot repo",
                };
                jobs_of.entry(name.clone()).or_default().0 += 1;
                planned.push((name, t, tier, job));
            }
        }
    }
    let mut set = tokio::task::JoinSet::new();
    // Each running job's pod, by task: its lock goes when the pod's last job ends (RR3).
    let mut job_pod: std::collections::HashMap<tokio::task::Id, String> = Default::default();
    // The day a directory in the way of a symlink is held under (`pull::rsync_making_way`).
    let day = pull::aside_day(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()));
    let (mut jobs, mut failed) = (0, 0);
    // Which jobs failed, by name: the summary (and the cron log) must say which pod to look at.
    let mut failures: Vec<String> = Vec::new();
    for (name, t, tier, job) in planned {
        let dest_owned = job.dest.clone();
        let dest = dest_owned.as_str();
        if job.repo {
            match clear_stale_link(dest) {
                Ok(false) => {}
                Ok(true) => println!("({name}: the backed-up link at {dest} is replaced by the repo's tree)"),
                Err(e) => {
                    eprintln!("[FAILED] {name} [{tier}]: {dest} is a symlink and couldn't be removed: {e}");
                    failed += 1;
                    failures.push(format!("{name} [{tier}] (removing the stale link {dest})"));
                    by_pod.push((name.clone(), format!("[{tier}] removing the stale link {dest}: {e}")));
                    continue;
                }
            }
        }
        // rsync needs the destination directory to exist.
        if let Err(e) = std::fs::create_dir_all(dest) {
            eprintln!("[FAILED] {name} [{tier}]: creating {dest}: {e}");
            failed += 1;
            failures.push(format!("{name} [{tier}] (creating {dest})"));
            by_pod.push((name.clone(), format!("[{tier}] creating {dest}: {e}")));
            continue;
        }
        // The temp dir, owner-only (what's in flight is a pod's files — its rc files carry its API
        // keys) — and only on the destination's filesystem: across filesystems rsync's rename
        // fails and it rewrites the destination in place, half-written for as long as it copies
        // (AD7). Then rsync's own default (beside each file, renamed atomically) is the safe one.
        let mut job = job;
        if let Some(tmp) = job.config.temp_dir.clone() {
            match same_filesystem(std::path::Path::new(&tmp), std::path::Path::new(dest)) {
                Ok(true) => {}
                Ok(false) => {
                    println!(
                        "({name} [{tier}]: {dest} is on another filesystem than {tmp} — rsync keeps its temp files beside \
                         each file there, so an archive push running at the same time can see one)"
                    );
                    job.config.temp_dir = None;
                }
                Err(e) => {
                    eprintln!("[FAILED] {name} [{tier}]: creating {tmp}: {e}");
                    failed += 1;
                    failures.push(format!("{name} [{tier}] (creating {tmp})"));
                    by_pod.push((name.clone(), format!("[{tier}] creating {tmp}: {e}")));
                    continue;
                }
            }
        }
        // The repo job's config is the tier's with the repo's paths swapped in
        // (`volume::pull_jobs`), so it carries the tier's `--timeout` too. A job's passes (the
        // snapshot's files, then its `.git`) run one after the other, within one budget.
        let passes: Vec<(bool, Vec<String>)> =
            job.config.passes().iter().map(|pc| (pc.git_only, pull::rsync_args(t, pc, dest))).collect();
        let (name, rsync, day, into) = (name.clone(), rsync.to_string(), day.clone(), std::path::PathBuf::from(dest));
        jobs += 1;
        locks.expect(&name);
        let pod = name.clone();
        let handle = set.spawn(async move {
            let started = tokio::time::Instant::now();
            let mut stats: Option<pull::RsyncStats> = None;
            // Directories moved out of the way of what the pod has now: said on the job's line.
            let mut made = pull::MadeWay::default();
            for (i, (git, args)) in passes.into_iter().enumerate() {
                let what = format!("rsync of {name} [{tier}{}]", if git { ", .git" } else { "" });
                // The first pass gets the whole budget; a later one what's left of it (whole
                // seconds, at least one). A pass that has to move a directory aside retries
                // within its own share.
                let left = if i == 0 {
                    budget
                } else {
                    Duration::from_secs(budget.saturating_sub(started.elapsed()).as_secs().max(1))
                };
                let (out, m) = pull::rsync_making_way(&rsync, &args, &into, &what, left, &day).await;
                made.extend(m);
                match out {
                    Ok(o) if o.success => {
                        let s = pull::parse_rsync_stats(&o.stdout);
                        stats = match (stats, s) {
                            (Some(a), Some(b)) => Some(a.add(b)),
                            (a, b) => a.or(b),
                        };
                    }
                    failed => return (name, tier, made, Err((git, failed))),
                }
            }
            (name, tier, made, Ok(stats))
        });
        job_pod.insert(handle.id(), pod);
    }
    // A pod none of whose jobs started holds its lock no longer.
    locks.release_idle();
    let missing = sel.endpoint_failures();
    failed += missing.len();
    failures.extend(missing.into_iter().map(|n| format!("{n}: no SSH endpoint after 10 minutes running")));
    let (mut ok, mut done) = (0, 0);
    // The floor as a hard line while the copies write (`disk::Watch`): under it, every job still
    // running is stopped, its pod's copy a failure — said, and recorded for the health check.
    let watched_dirs: Vec<&std::path::Path> =
        if no_big { vec![std::path::Path::new(dir)] } else { vec![std::path::Path::new(dir), std::path::Path::new(&big_root)] };
    let watch = disk::Watch::new(floor, &watched_dirs);
    let mut stopped: Option<arena_core::disk::Skip> = None;
    // The pods stopped, and each stopped job's pod (its lock goes once its rsync has ended).
    let (mut stopped_pods, mut stopped_jobs): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
    loop {
        let joined = tokio::select! {
            biased;
            joined = set.join_next_with_id() => match joined {
                Some(joined) => joined,
                None => break,
            },
            skip = disk::breached(watch.as_ref()), if stopped.is_none() && !set.is_empty() => {
                say(&format!("✗ {} — stopping this run's copies", skip.reason));
                stopped = Some(skip);
                set.abort_all();
                continue;
            }
        };
        done += 1;
        let (name, tier, made, out) = match joined {
            Ok((id, r)) => {
                if let Some(pod) = job_pod.remove(&id) {
                    locks.done(&pod);
                }
                r
            }
            // Stopped by the watch: its rsync with it (`remote::run_local`, on drop). Said once per pod.
            Err(e) if e.is_cancelled() && stopped.is_some() => {
                let skip = stopped.as_ref().map(ToString::to_string).unwrap_or_default();
                if let Some(pod) = job_pod.remove(&e.id()) {
                    if !stopped_pods.contains(&pod) {
                        say(&format!("[{done}/{jobs}] ✗ {pod}: {skip}"));
                        failures.push(format!("{pod} ({})", stopped.as_ref().map(|s| s.reason.as_str()).unwrap_or_default()));
                        by_pod.push((pod.clone(), skip));
                        stopped_pods.push(pod.clone());
                    }
                    stopped_jobs.push(pod);
                }
                failed += 1;
                continue;
            }
            // A crashed task still counts — as a failure, never silently.
            Err(e) => {
                if let Some(pod) = job_pod.remove(&e.id()) {
                    locks.done(&pod);
                    by_pod.push((pod, format!("an rsync task crashed: {e}")));
                }
                println!("[{done}/{jobs}] ✗ an rsync task crashed: {e}");
                failed += 1;
                failures.push("(a crashed task)".into());
                continue;
            }
        };
        // A directory moved aside (or refused) is said on the job's line, whatever the outcome.
        let moved = made.note();
        match out {
            Ok(stats) => {
                // Report what actually moved, so a pull isn't "silent".
                let summary = stats.map_or_else(|| "done".into(), |s| s.summary());
                say(&format!("[{done}/{jobs}] ✓ {name} [{tier}] ({summary}){moved}"));
                ok += 1;
                jobs_of.entry(name.clone()).or_default().1 += 1;
            }
            Err((git, Ok(o))) => {
                let pass = if git { ", .git" } else { "" };
                say(&format!("[{done}/{jobs}] ✗ {name} [{tier}{pass}] (rsync {}): {}{moved}", o.exit_label(), o.stderr.trim()));
                failed += 1;
                failures.push(format!("{name} [{tier}]"));
                by_pod.push((name.clone(), format!("[{tier}{pass}] rsync {}", o.exit_label())));
            }
            // Timed out (`timed out after 7200s` — what was copied so far stays, and the next
            // run continues incrementally), or rsync couldn't be started.
            Err((git, Err(e))) => {
                let pass = if git { ", .git" } else { "" };
                say(&format!("[{done}/{jobs}] ✗ {name} [{tier}{pass}]: {}{moved}", describe_error(&e)));
                failed += 1;
                failures.push(format!("{name} [{tier}] ({})", describe_error(&e)));
                by_pod.push((name.clone(), format!("[{tier}{pass}] {}", describe_error(&e))));
            }
        }
    }
    if let Some(skip) = &stopped {
        // Nothing of a stopped copy writes on once this run goes on (or ends), nor does its pod's
        // lock go before.
        disk::children_stopped().await;
        for pod in &stopped_jobs {
            locks.done(pod);
        }
        disk::note_skips(cfg, "copy", &stopped_pods.iter().map(|p| (p.clone(), skip.to_string())).collect::<Vec<_>>());
    }
    // What the new snapshots really take: hard-linked files are shared with the earlier
    // snapshots and the baseline, so `du` of one snapshot alone overstates it.
    let mut used = Vec::new();
    let mut sizes: std::collections::HashMap<String, pull::DiskUse> = Default::default();
    for (name, _) in &targets {
        if let Ok(d) = pull::disk_use(std::path::Path::new(&pull::local_dest(dir, &label, name))) {
            if d.files > 0 {
                sizes.insert(name.clone(), d);
                used.push(format!(
                    "  {name}: {} in {} files, {} of it new on disk{}",
                    pull::human_bytes(d.bytes),
                    d.files,
                    pull::human_bytes(d.own_bytes),
                    if d.own_bytes < d.bytes { " (the rest hard-linked to an earlier snapshot or the baseline)" } else { "" }
                ));
            }
        }
    }
    if !used.is_empty() {
        println!("\nSnapshot {label} on disk (`du` over every snapshot counts a hard-linked file once):\n{}", used.join("\n"));
    }
    let no_room = if skipped.is_empty() { String::new() } else { format!("; {} pod(s) skipped — no room on this box", skipped.len()) };
    println!("\nDone: {ok} rsync job(s) ok, {failed} failed across {total_pods} pod(s){no_room}.");
    failures.splice(0..0, not_pulled.iter().cloned());
    let whole: Vec<String> = if no_big || no_git {
        Vec::new()
    } else {
        targets
            .iter()
            .map(|(n, _)| n.clone())
            .filter(|n| jobs_of.get(n).is_some_and(|(all, ok)| *all > 0 && ok == all) && !by_pod.iter().any(|(p, _)| p == n))
            .collect()
    };
    // What makes this pod's next copy here a later one (`disk::tier_size`): its whole home copied
    // (`~/` as the cron's BACKUP_REMOTE_PATH says, not one dir of it, after which a copy of the
    // home would still bring most of it). Best effort: without it, the next copy counts as a first one.
    for (pod, t) in targets.iter().filter(|(n, _)| whole.contains(n)) {
        if arena_core::volume::whole_home(&remote_path, &t.user) {
            let _ = arena_core::disk::mark_copied(base, pod);
        }
    }
    run.finish(copy_events(sel, &targets, &by_pod, &sizes, &label), copy_summary(sel, &by_pod, &label));
    Ok(Pulled { failures, failed: by_pod, snapshot_root: format!("{dir}/{label}"), big_root: (!no_big).then_some(big_root), whole })
}

/// Each pod's copy for the run record (`arena backups`): failed with every reason it failed for,
/// else copied — with what the snapshot holds (`w3d5: 1.8G in 6754 files, 12.0M new`). Pure.
fn copy_events(
    sel: &Selected,
    copied: &[(String, SshTarget)],
    failed: &[(String, String)],
    sizes: &std::collections::HashMap<String, arena_core::pull::DiskUse>,
    label: &str,
) -> Vec<(String, arena_core::backups::record::Event)> {
    use arena_core::backups::{record::Outcome, Job};
    let mut names: Vec<&str> = sel.pods.iter().map(|p| p.name.as_str()).chain(failed.iter().map(|(p, _)| p.as_str())).collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .filter_map(|name| {
            let mut why: Vec<&str> = failed.iter().filter(|(p, _)| p == name).map(|(_, w)| w.as_str()).collect();
            why.dedup();
            let ev = if !why.is_empty() {
                crate::backups::event(Job::Copy, Outcome::Failed, why.join("; "))
            } else if copied.iter().any(|(n, _)| n == name) {
                let d = sizes.get(name);
                let detail = d.map_or_else(
                    || label.to_string(),
                    |d| format!("{label}: {} in {} files, {} new", arena_core::pull::human_bytes(d.bytes), d.files, arena_core::pull::human_bytes(d.own_bytes)),
                );
                crate::backups::event(Job::Copy, Outcome::Copied, detail).with_bytes(d.map(|d| d.bytes))
            } else {
                return None;
            };
            Some((name.to_string(), ev))
        })
        .collect()
}

/// The copy run in a few words: `41 copied, 1 failed (w3d5)`. Pure.
fn copy_summary(sel: &Selected, failed: &[(String, String)], label: &str) -> String {
    let mut bad: Vec<&str> = failed.iter().map(|(p, _)| p.as_str()).collect();
    bad.sort();
    bad.dedup();
    let ok = sel.pods.iter().filter(|p| !bad.contains(&p.name.as_str())).count();
    format!("{ok} copied, {} failed ({label})", bad.len())
}

/// `p` absolute: canonical once it exists, else made absolute against the working directory (the
/// same place, before the first pull creates it).
pub(crate) fn absolute_path(p: &std::path::Path) -> std::path::PathBuf {
    std::fs::canonicalize(p).or_else(|_| std::path::absolute(p)).unwrap_or_else(|_| p.to_path_buf())
}

/// Create the temp dir `tmp` (owner-only) and say whether it is on `dest`'s filesystem.
fn same_filesystem(tmp: &std::path::Path, dest: &std::path::Path) -> std::io::Result<bool> {
    create_private_dir(tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(std::fs::metadata(tmp)?.dev() == std::fs::metadata(dest)?.dev())
    }
    #[cfg(not(unix))]
    Ok(true)
}

/// `mkdir -p` with mode 0700 for any directory it creates (an existing one is left as it is).
fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)
}

/// `pods pull`'s arguments sorted out ([`pull_label_and_targets`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PullArgs {
    /// The snapshot label (`None` = the computed `wNdM`).
    pub label: Option<String>,
    /// The targets, for the selector (positional ones first, then the old `-t` ones).
    pub targets: Vec<String>,
    /// The deprecation note for an old-style positional label.
    pub note: Option<String>,
}

/// `pods pull`'s positionals are pods, like every other command's; the snapshot label goes
/// behind `--label`. They used to be the label — and `pods pull bravo` pulled EVERY pod into
/// `backup/bravo/` (live: ~18G on the host the cohort shares). An old invocation keeps working
/// where it can only mean the label: a lone positional that looks like one (`w1d3`, see
/// [`arena_core::restore::parse_label`]) and names no pod — none listed (`pods`), no
/// `MACHINE_NAME_LIST` entry — is taken as the label, with a note. One that could be either is
/// refused rather than guessed; so are two labels. `-t` (the old target flag) adds targets.
pub(crate) fn pull_label_and_targets(
    label: Option<String>,
    positional: &[String],
    flag_targets: &[String],
    naming: &Naming,
    pods: &[arena_core::Pod],
) -> std::result::Result<PullArgs, String> {
    let mut out = PullArgs { label, targets: Vec::new(), note: None };
    for raw in positional {
        let word = raw.trim();
        if arena_core::restore::parse_label(word).is_none() {
            out.targets.push(raw.clone());
            continue;
        }
        let a_pod = pods.iter().any(|p| naming.matches(p, word)) || naming.names_list_entry(word);
        match (a_pod, &out.label) {
            // `--label` given: every positional is a pod.
            (_, Some(_)) if out.note.is_none() => out.targets.push(raw.clone()),
            (true, _) => {
                return Err(format!(
                    "`{word}` is both a snapshot label and a pod's name — write `--label {word}` for the label, or the \
                     pod's full name or id for the pod"
                ))
            }
            (false, Some(first)) => {
                return Err(format!("two snapshot labels (`{first}`, `{word}`): pass one, with --label"));
            }
            (false, None) => {
                out.note = Some(format!(
                    "note: reading `{word}` as the snapshot label (the old spelling) — `pods pull` takes pods as its \
                     arguments now; write `--label {word}`"
                ));
                out.label = Some(word.to_string());
            }
        }
    }
    out.targets.extend(flag_targets.iter().cloned());
    Ok(out)
}

/// `pods pull`'s label, pods and deprecation note, as the command resolves them: the fleet
/// listing (only when a positional looks like a label — it's what tells one from a pod),
/// [`pull_label_and_targets`], then the selector. Returned rather than acted on, so a test sees
/// exactly what an invocation means.
pub(crate) async fn resolve_pull(
    provider: &dyn Provider,
    cfg: &Config,
    label: Option<String>,
    mut sel: Select,
    target_flag: &[String],
) -> Result<(Option<String>, Selected, Option<String>)> {
    let pods = if sel.targets.iter().any(|t| arena_core::restore::parse_label(t.trim()).is_some()) {
        let policy = arena_core::retry::RetryPolicy::default();
        arena_core::retry::retrying(&policy, || provider.list_pods()).await.context("listing pods")?
    } else {
        Vec::new()
    };
    let naming = Naming::from_config(cfg);
    let split = pull_label_and_targets(label, &sel.targets, target_flag, &naming, &pods).map_err(|e| anyhow::anyhow!(e))?;
    let positional = !sel.targets.is_empty();
    sel.targets = split.targets;
    let chosen = select(provider, cfg, &sel.args(), Unscoped::All).await.map_err(|e| {
        if positional && split.label.is_none() {
            e.context("`pods pull` takes pods as its arguments now (like every command); a snapshot label goes behind --label")
        } else {
            e
        }
    })?;
    Ok((split.label, chosen, split.note))
}

/// How `pods pull` says it's bounded, on its header line: the per-pod wall clock and the
/// I/O-silence cut-off (see [`handle_pull`]).
fn pull_budget_note(budget: Duration) -> String {
    format!(
        "each pod ≤ {} (BACKUP_TIMEOUT_SECS), ≤ {}s without I/O",
        arena_core::error::human_duration(&budget),
        arena_core::pull::RSYNC_IO_TIMEOUT_SECS
    )
}

/// Where a repo's tree is about to be rsynced locally (`dest`, a directory path): if an older
/// run left a symlink there (it copied the repo's link itself, before repos moved onto
/// volumes), remove that link — never what it points at — so the tree lands here and not
/// through the link into some path on this machine. Whether a link was removed.
pub(crate) fn clear_stale_link(dest: &str) -> std::io::Result<bool> {
    let link = dest.trim_end_matches('/');
    match std::fs::symlink_metadata(link) {
        Ok(m) if m.file_type().is_symlink() => std::fs::remove_file(link).map(|()| true),
        _ => Ok(false),
    }
}

/// The local directory `pods pull` rsyncs into: config `LOCAL_BACKUP_DIR`, else `./backup`.
pub(crate) fn local_backup_dir(cfg: &Config) -> String {
    cfg.get("LOCAL_BACKUP_DIR").filter(|s| !s.is_empty()).unwrap_or("./backup").to_string()
}
