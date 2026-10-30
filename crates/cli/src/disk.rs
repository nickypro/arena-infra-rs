//! The control box's disk, as the commands use it (`arena_core::disk` has the rules and words):
//! the floor every copy onto this machine keeps free, the record of skips the daily health check
//! reports, and the rows `config check`, that health check and `teardown --check` show — how full
//! each filesystem arena writes to is, the biggest of arena's dirs on a low one, and what arena
//! left on the box (a replace/migrate staging copy, …), with how to look at it and remove it.

use std::path::{Path, PathBuf};

use anyhow::Result;
use arena_core::disk::{self as core_disk, Floor, Leftover, Level, Size, Skip};
use arena_core::Config;

/// Whether this run looks at the real disk. Always, but in this crate's unit tests only when
/// the test's config sets `CONTROL_MIN_FREE_GB`: a test's copies, checks and reports must never
/// depend on how full the developer's (or CI's) disk is, nor list another test's temp dirs.
pub(crate) fn live(cfg: &Config) -> bool {
    !cfg!(test) || cfg.get(core_disk::FLOOR_KEY).is_some()
}

/// The floor every copy onto this box keeps free (`CONTROL_MIN_FREE_GB`; `None` where
/// [`live`] says not to look). A bad value fails the run before anything is copied.
pub(crate) fn floor(cfg: &Config) -> Result<Option<Floor>> {
    if !live(cfg) {
        return Ok(None);
    }
    core_disk::floor_bytes(cfg).map(|b| Some(Floor::control(b))).map_err(|e| anyhow::anyhow!(e))
}

/// replace/migrate's look at [`floor`] before they create anything: their copy via local staging
/// needs it, and a bad value found only there — after the new pod was created and set up — left
/// that pod billing (review #42). `Some(why)` when it's invalid; nothing to say otherwise.
pub(crate) fn floor_refusal(cfg: &Config, what: &str) -> Option<String> {
    floor(cfg).err().map(|e| format!("{e} — not starting the {what}: its copy via local staging needs it (fix it, or unset it for the default)"))
}

/// replace/migrate's via-local copy refused (or stopped) for room: the skip's words with arena's
/// biggest dirs on that filesystem named — one bounded look, which a run's one refusal can
/// afford (a backup's line per pod points at `config check` instead) — then another filesystem
/// arena writes to (the backups' volume, …) with room for `need` above the floor, to point
/// `TMPDIR` at for the run (`need` 0: not sized yet).
pub(crate) fn staging_advice(cfg: &Config, skip: &Skip, need: u64, floor: Floor) -> String {
    let Some(short) = &skip.short else { return staging_words(skip, None) };
    let left = leftovers(cfg, arena_core::snapshot::unix_now()).leftovers;
    let named = skip.clone().naming(&dir_sizes(&core_disk::biggest(&arena_dirs(cfg, &left), short.dev, 3)));
    // Another filesystem with room: a dir on it to suggest, checked to be on it (a `/tmp` on
    // its own tmpfs isn't `/`).
    let other = places(cfg)
        .into_iter()
        .filter_map(|(_, p)| core_disk::reading(&p).ok())
        .filter(|fs| fs.dev != short.dev && fs.free >= floor.bytes.saturating_add(need))
        .max_by_key(|fs| fs.free)
        .map(|fs| (fs.mount.join("arena-tmp"), fs))
        .filter(|(dir, fs)| core_disk::reading(dir).is_ok_and(|at| at.dev == fs.dev));
    staging_words(&named, other.as_ref().map(|(dir, fs)| (dir.as_path(), fs.free)))
}

/// [`staging_advice`]'s words (pure): the skip, then where else to stage — `other` (a dir, and
/// what its filesystem has free) when one is known.
fn staging_words(skip: &Skip, other: Option<(&Path, u64)>) -> String {
    let elsewhere = match other {
        Some((dir, free)) => format!("`TMPDIR={} arena …` ({} free there)", dir.display(), core_disk::gb(free)),
        None => "`TMPDIR=<a dir on a disk with room> arena …`".to_string(),
    };
    format!("{skip}; or stage it on another filesystem for this run: {elsewhere}")
}

/// `/tmp/arena-replace-x 3.5 GB · /srv/backup/w1d1 over 2.0 GB`: dirs with their sizes ([`core_disk::biggest`]).
fn dir_sizes(big: &[(PathBuf, core_disk::TreeUse)]) -> String {
    big.iter().map(|(p, t)| format!("{} {}{}", p.display(), if t.capped { "over " } else { "" }, core_disk::gb(t.bytes))).collect::<Vec<_>>().join(" · ")
}

/// Judge a run's copies onto this box together ([`core_disk::Ledger`]): each copy (a pod) with
/// the destinations it writes and their sizes, against what other arena runs' copies under way
/// hold ([`core_disk::Reservations`], in the state root: every cohort's runs as this user). The
/// ones that don't fit, with why — in order — and the admitted ones' reservation, held until the
/// returned [`Hold`] is dropped (when the copies end). `reserve: false` (a dry run) holds nothing.
/// What other runs hold that can't be known — their notes' lock held for [`core_disk::RECORD_LOCK_WAIT`],
/// the notes unreadable — or this run's own that can't be noted, skips every copy (review r3: a
/// warning there let two runs count the same free space); a file it can't parse (arena writes it
/// atomically: another version's) is replaced, said, as the [`Watch`] stops what overruns anyway.
pub(crate) fn judge_copies(cfg: &Config, floor: Floor, copies: &[(String, Vec<(PathBuf, Size)>)], reserve: bool) -> (Vec<(String, Skip)>, Hold) {
    let refuse = |why: String| -> (Vec<(String, Skip)>, Hold) {
        (copies.iter().map(|(name, _)| (name.clone(), Skip::others_unknown(&why))).collect(), Hold::default())
    };
    let path = match reservations_path(cfg) {
        None => return (judge_copies_with(core_disk::Ledger::new(floor), copies, core_disk::reading).0, Hold::default()),
        Some(Err(e)) => return refuse(e),
        Some(Ok(path)) => path,
    };
    // Under the state root's lock: no other run reads or writes the notes between this one's
    // reading and its own.
    let lock = match core_disk::lock_state_for(&path, core_disk::RECORD_LOCK_WAIT) {
        Ok(lock) => lock,
        Err(e) => return refuse(format!("{}: {e}", path.display())),
    };
    let now = arena_core::snapshot::unix_now();
    let mut held = match core_disk::Reservations::load(&path) {
        Ok((held, warning)) => {
            warning.iter().for_each(|w| eprintln!("{w}"));
            held
        }
        Err(e) => return refuse(e),
    };
    held.prune(now, core_disk::arena_alive);
    let (skipped, taken) = judge_copies_with(core_disk::Ledger::with_others(floor, held.by_dev()), copies, core_disk::reading);
    if !reserve || taken.is_empty() {
        return (skipped, Hold::default());
    }
    let token = now.wrapping_mul(1_000_003) ^ u64::from(std::process::id()) ^ u64::from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos()));
    held.hold(std::process::id(), token, &taken, now);
    if let Err(e) = held.save(&path) {
        return refuse(format!("noting this run's copies in {}: {e}", path.display()));
    }
    drop(lock);
    (skipped, Hold { path: Some(path), token })
}

/// [`judge_copies`]' judging with `ledger` and the filesystem readings from `read` (tests hand in
/// their own): the skipped copies, and what the admitted ones take per device.
fn judge_copies_with(
    mut ledger: core_disk::Ledger,
    copies: &[(String, Vec<(PathBuf, Size)>)],
    read: impl Fn(&Path) -> std::result::Result<core_disk::Fs, String>,
) -> (Vec<(String, Skip)>, std::collections::BTreeMap<u64, u64>) {
    let mut out = Vec::new();
    for (name, parts) in copies {
        let read: std::result::Result<Vec<_>, Skip> =
            parts.iter().map(|(dest, size)| read(dest).map(|fs| (fs, *size)).map_err(|e| Skip::unknown(dest, &e))).collect();
        if let Err(skip) = read.and_then(|parts| ledger.admit(&parts)) {
            out.push((name.clone(), skip));
        }
    }
    (out, ledger.taken())
}

/// A run's reservation of the space its admitted copies may take ([`judge_copies`]): let go when
/// dropped — the copies ended — under the state dir's lock. Best effort: one that can't be let go
/// of goes with its process (or its age).
#[derive(Debug, Default)]
pub(crate) struct Hold {
    path: Option<PathBuf>,
    token: u64,
}

impl Drop for Hold {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else { return };
        let Ok(lock) = core_disk::lock_state_for(&path, core_disk::RECORD_LOCK_WAIT) else { return };
        let Ok((mut held, _)) = core_disk::Reservations::load(&path) else { return };
        held.release(std::process::id(), self.token);
        let _ = held.save(&path);
        drop(lock);
    }
}

/// How often a run's copies look at the free space under them ([`Watch`]).
pub(crate) const WATCH_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

/// The floor as a hard line while a run's copies write (review r3): a copy is admitted on an
/// estimate — a pod's later copy counts 0.1 GB a tier, and can bring a 12 GB file —, so its run
/// looks at the filesystems it writes to every [`WATCH_EVERY`] and stops its copies once one has
/// less than the floor free, as a copy over its time budget is stopped: what it copied stays, the
/// pod's copy fails, said and recorded for the health check. A filesystem that can't be read
/// meanwhile doesn't stop them (it was read to admit them).
#[derive(Debug, Clone)]
pub(crate) struct Watch {
    floor: Floor,
    dirs: Vec<PathBuf>,
}

impl Watch {
    /// A watch over the filesystems of `dirs` — `None` without a floor, or with a zero one (only
    /// the copy itself must fit).
    pub(crate) fn new(floor: Option<Floor>, dirs: &[&Path]) -> Option<Watch> {
        floor.filter(|f| f.bytes > 0).map(|floor| Watch { floor, dirs: dirs.iter().map(|d| d.to_path_buf()).collect() })
    }

    /// Why the copies must stop now, from the readings `read` gives (pure but for them).
    fn under(&self, read: impl Fn(&Path) -> std::result::Result<core_disk::Fs, String>) -> Option<Skip> {
        self.dirs.iter().filter_map(|d| read(d).ok()).find(|fs| fs.free < self.floor.bytes).map(|fs| Skip::under_floor_now(&fs, self.floor))
    }
}

/// Resolves once `watch` finds one of its filesystems under the floor (never, for `None`).
pub(crate) async fn breached(watch: Option<&Watch>) -> Skip {
    let Some(w) = watch else { return std::future::pending().await };
    loop {
        if let Some(skip) = w.under(core_disk::reading) {
            return skip;
        }
        tokio::time::sleep(WATCH_EVERY).await;
    }
}

/// `copy` (onto this box) unless `watch` breaches first: then `Err` with why, once the copy is
/// dropped and its rsync stopped (`arena_core::remote::run_local`: SIGTERM, then SIGKILL) —
/// [`children_stopped`] — so nothing writes on after this returns.
pub(crate) async fn watched<T>(watch: Option<&Watch>, copy: impl std::future::Future<Output = T>) -> std::result::Result<T, Skip> {
    tokio::select! {
        biased;
        out = copy => Ok(out),
        skip = breached(watch) => {
            children_stopped().await;
            Err(skip)
        }
    }
}

/// Wait for the copies a watch stopped to have ended ([`arena_core::remote::children_done`]):
/// rsync's own stop takes ~0.4 s, a SIGKILL comes after its grace.
pub(crate) async fn children_stopped() {
    let wait = arena_core::remote::TERM_GRACE + std::time::Duration::from_secs(1);
    if !arena_core::remote::children_done(wait).await {
        eprintln!("warning: a copy stopped for space was still running after {}s", wait.as_secs());
    }
}

/// Where the reservations live: the state root (`ARENA_STATE_DIR`, the dir that holds each
/// cohort's), so two cohorts' runs on one box count each other's copies (review r3); `None` in
/// this crate's unit tests unless the test's config sets `ARENA_STATE_DIR`.
fn reservations_path(cfg: &Config) -> Option<std::result::Result<PathBuf, String>> {
    if cfg!(test) && cfg.get("ARENA_STATE_DIR").is_none() {
        return None;
    }
    Some(arena_core::snapshot::state_dir_for(cfg).map(|cohort| cohort.parent().unwrap_or(&cohort).join(core_disk::RESERVATIONS_FILE)))
}

/// What a copy of `pod` into `dest` may take, under the snapshot root `base`: a later copy — only
/// what changed — when the pod was copied whole into this backup before ([`core_disk::was_copied`])
/// and there's a copy of it to build on (its files in `dest` — the big tier's dir, or a snapshot
/// label it's pulled into again —, or for the snapshot tier a `wNdM` snapshot of it to link
/// against); else a first one. An interrupted first copy leaves files but no mark, so its retry
/// still counts as a first copy.
pub(crate) fn tier_size(base: &Path, pod: &str, snapshot: bool, dest: &Path) -> Size {
    let holds = std::fs::read_dir(dest).is_ok_and(|mut e| e.next().is_some());
    let builds_on = holds || (snapshot && !arena_core::restore::snapshot_labels(base, pod).is_empty());
    // A backup from before marking (none yet: a dry run, which doesn't start it) is taken as a real
    // run would take it once it has ([`core_disk::seed_copied`]).
    let marked = core_disk::was_copied(base, pod) || (builds_on && !base.join(core_disk::COPIED_DIR).exists());
    if builds_on && marked {
        Size::Later
    } else {
        Size::First
    }
}

/// Start marking whole copies in the backup at `base` before a run's copies are judged
/// ([`core_disk::seed_copied`]: once per backup, its pods from before marking taken as copied
/// whole) — said when it marked any; a failure is a warning (those pods then count as first copies).
pub(crate) fn seed_copied(base: &Path) {
    match core_disk::seed_copied(base) {
        Ok(0) => {}
        Ok(n) => eprintln!("note: {n} pod(s) already backed up in {} marked as copied whole (their next copies count as later ones)", base.display()),
        Err(e) => eprintln!("warning: couldn't start marking whole copies in {} ({e}) — every pod's copy counts as a first one", base.display()),
    }
}

/// Where the skip record lives: the fleet's state dir. `None` in this crate's unit tests
/// unless the test's config sets `ARENA_STATE_DIR` (a test never writes the developer's real
/// `~/.local/state`).
fn skips_path(cfg: &Config) -> Option<std::result::Result<PathBuf, String>> {
    if cfg!(test) && cfg.get("ARENA_STATE_DIR").is_none() {
        return None;
    }
    Some(arena_core::snapshot::state_file_path_for(cfg, core_disk::SKIPS_FILE))
}

/// Record copies skipped for space (`(pod, why)`, all of kind `what`) for the daily health check
/// — best effort: a warning line, never an error (the skips were said, and failed the run).
pub(crate) fn note_skips(cfg: &Config, what: &str, skipped: &[(String, String)]) {
    if skipped.is_empty() {
        return;
    }
    let path = match skips_path(cfg) {
        None => return,
        Some(Ok(p)) => p,
        Some(Err(e)) => return eprintln!("warning: the skipped copies aren't recorded for the health check: {e}"),
    };
    let new: Vec<(String, String, String)> = skipped.iter().map(|(pod, why)| (pod.clone(), what.to_string(), reason(why))).collect();
    match core_disk::record_skips(&path, &new, arena_core::snapshot::unix_now(), core_disk::RECORD_LOCK_WAIT) {
        Ok(warning) => warning.iter().for_each(|w| eprintln!("{w}")),
        Err(e) => eprintln!("warning: couldn't record the skipped copies in {} ({e})", path.display()),
    }
}

/// The reason in a skip's line, without its `skipped: ` / `stopped: ` and its advice (the
/// record's words).
fn reason(line: &str) -> String {
    let r = line.strip_prefix("skipped: ").or_else(|| line.strip_prefix("stopped: ")).unwrap_or(line);
    r.split(" — nothing copied").next().and_then(|r| r.split(" — what it copied so far stays").next()).unwrap_or(r).to_string()
}

/// The places arena writes to on this box, `(what, where)`: the backup tiers, the staging dir
/// replace/migrate copy through, the fleet's state.
fn places(cfg: &Config) -> Vec<(String, PathBuf)> {
    let dir = crate::local_backup_dir(cfg);
    let abs = |p: &str| std::path::absolute(p).unwrap_or_else(|_| PathBuf::from(p));
    let mut out = vec![("backups".to_string(), abs(&dir))];
    let (big, two_volume) = crate::pull_roots(cfg, &dir);
    if two_volume {
        out.push(("all-files tier".to_string(), abs(&big)));
    }
    out.push(("staging".to_string(), std::env::temp_dir()));
    if let Ok(state) = arena_core::snapshot::state_dir_for(cfg) {
        out.push(("state".to_string(), state));
    }
    out
}

/// The backup roots leftovers are looked for in (the snapshot root and the big tier's).
fn backup_roots(cfg: &Config) -> Vec<PathBuf> {
    let dir = crate::local_backup_dir(cfg);
    let (big, _) = crate::pull_roots(cfg, &dir);
    [dir, big].iter().map(|p| std::path::absolute(p).unwrap_or_else(|_| PathBuf::from(p))).collect()
}

/// What arena left on this box ([`core_disk::find_leftovers`]), and where it couldn't look.
pub(crate) fn leftovers(cfg: &Config, now: u64) -> core_disk::Found {
    core_disk::find_leftovers(&std::env::temp_dir(), &backup_roots(cfg), now)
}

/// Where arena's own dirs are, to name the biggest on a low filesystem: every entry of the backup
/// roots (labels, `big`, `baseline`, `reaped`), the leftovers, the state dir and the cron logs —
/// none inside another (a parent's size already holds it).
fn arena_dirs(cfg: &Config, left: &[Leftover]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in backup_roots(cfg) {
        if let Ok(es) = std::fs::read_dir(&root) {
            out.extend(es.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()));
        }
    }
    out.extend(left.iter().map(|l| l.path.clone()));
    if let Ok(state) = arena_core::snapshot::state_dir_for(cfg) {
        out.push(state);
    }
    if let Ok(home) = std::env::var("HOME") {
        out.push(PathBuf::from(home).join(".local/state/arena-cron"));
    }
    out.sort();
    let mut kept: Vec<PathBuf> = Vec::new();
    for p in out {
        if !kept.iter().any(|k| p.starts_with(k)) {
            kept.push(p);
        }
    }
    kept
}

/// The rows about this box's disk (`config check`, the health check): one per filesystem arena
/// writes to — what's free, of what, and which of arena's places are there; ⚠ under twice the
/// floor, ⚠ under it (every copy onto it is skipped), each with arena's biggest dirs there —,
/// then what arena left on the box. Indented two spaces, as `config check`'s rows.
pub(crate) fn rows(cfg: &Config, floor: Floor, now: u64) -> Vec<String> {
    let core_disk::Found { leftovers: left, unread } = leftovers(cfg, now);
    let mut by_fs: Vec<(core_disk::Fs, Vec<String>)> = Vec::new();
    let mut out = Vec::new();
    for (what, path) in places(cfg) {
        match core_disk::reading(&path) {
            Ok(fs) => match by_fs.iter_mut().find(|(f, _)| f.dev == fs.dev) {
                Some((_, names)) => names.push(format!("{what} {}", path.display())),
                None => by_fs.push((fs, vec![format!("{what} {}", path.display())])),
            },
            Err(e) => out.push(format!("  ? {what} {}: free space unknown ({e})", path.display())),
        }
    }
    let mut dirs: Option<Vec<PathBuf>> = None;
    for (fs, names) in by_fs {
        let head = format!("{} — {} free of {}", fs.mount.display(), core_disk::gb(fs.free), core_disk::gb(fs.total));
        let on = names.join(", ");
        let floor_words = format!("{} ({})", core_disk::gb(floor.bytes), floor.key);
        match core_disk::level(fs.free, floor.bytes) {
            Level::Ok => out.push(format!("  ✓ {head}: {on}")),
            low => {
                out.push(match low {
                    Level::Below => format!("  ⚠ {head}, UNDER the floor {floor_words}: every copy onto it is skipped until there's room — {on}"),
                    _ => format!("  ⚠ {head}, under twice the floor {floor_words}: the next big copy may be skipped — {on}"),
                });
                let dirs = dirs.get_or_insert_with(|| arena_dirs(cfg, &left));
                let big = core_disk::biggest(dirs, fs.dev, 5);
                if !big.is_empty() {
                    out.push(format!("      └ arena's biggest dirs there: {}", dir_sizes(&big)));
                }
            }
        }
    }
    for l in &left {
        for (i, line) in l.lines(now).into_iter().enumerate() {
            out.push(if i == 0 { format!("  {line}") } else { format!("    {line}") });
        }
    }
    out.extend(unread.iter().map(|u| format!("  ? couldn't look for what arena left in {u}")));
    out
}

/// `config check`'s look at this box's disk, after the report ([`rows`]), under its heading.
/// Advice only: it never fails the check (an invalid `CONTROL_MIN_FREE_GB` is the report's ✗).
pub(crate) fn check_rows(cfg: &Config) -> Vec<String> {
    let Ok(Some(floor)) = floor(cfg) else { return Vec::new() };
    let mut out = vec!["\nThis box's disk (every copy of a pod lands here — CONTROL_MIN_FREE_GB):".to_string()];
    out.extend(rows(cfg, floor, arena_core::snapshot::unix_now()));
    out
}

/// What the daily health check (`pods test --deep` over the cohort) adds about this box: the
/// disk ([`rows`]) and the copies skipped for space in the last day — a failure of the check —
/// read from the record the copies left.
pub(crate) struct Health {
    pub lines: Vec<String>,
    /// How many pods had a copy skipped in the last day.
    pub skipped: usize,
    /// The floor itself is invalid: every copy onto this box fails (a failure of the check too).
    pub invalid: Option<String>,
}

pub(crate) fn health(cfg: &Config, now: u64) -> Option<Health> {
    let mut lines = vec!["\nThis box (where the backups and copies land):".to_string()];
    let mut invalid = None;
    match floor(cfg) {
        Ok(None) => return None,
        Ok(Some(floor)) => lines.extend(rows(cfg, floor, now)),
        Err(e) => {
            lines.push(format!("  ✗ {e} — every copy onto this box fails until it's fixed"));
            invalid = Some(e.to_string());
        }
    }
    let mut skipped = 0;
    if let Some(Ok(path)) = skips_path(cfg) {
        let (skips, warning) = core_disk::Skips::load(&path);
        lines.extend(warning.map(|w| format!("  {w}")));
        let recent = skips.recent(now);
        skipped = recent.iter().map(|e| e.pod.as_str()).collect::<std::collections::BTreeSet<_>>().len();
        lines.extend(recent.iter().map(|e| format!("  {}", e.line(now))));
    }
    Some(Health { lines, skipped, invalid })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A replace/migrate staging refused for room: how much to free where (with what's there,
    /// when named), then another filesystem to stage on — a real one with its free space when
    /// one has room, else said in general.
    #[test]
    fn a_staging_refusal_says_what_to_free_and_where_else_to_stage() {
        let g = core_disk::GB;
        let fs = core_disk::Fs { dev: 1, mount: PathBuf::from("/"), free: 3_100_000_000, total: 75 * g };
        let skip = core_disk::judge(&fs, 0, &Size::Measured(3_900_000_000).into(), Floor::control(5 * g)).unwrap_err();
        let named = skip.clone().naming("/tmp/arena-replace-leh3rc2hdcf31i 3.5 GB · /root/.local/state/arena 0.1 GB");
        assert_eq!(
            staging_words(&named, Some((Path::new("/mnt/volume-nbg1-1/arena-tmp"), 331 * g))),
            "skipped: only 3.1 GB free on / (floor 5 GB, CONTROL_MIN_FREE_GB): this copy (~3.9 GB, measured) + the floor = 8.9 GB \
             needed — nothing copied; free at least 5.8 GB on / (arena's biggest dirs there: /tmp/arena-replace-leh3rc2hdcf31i \
             3.5 GB · /root/.local/state/arena 0.1 GB); or stage it on another filesystem for this run: \
             `TMPDIR=/mnt/volume-nbg1-1/arena-tmp arena …` (331 GB free there)"
        );
        assert!(
            staging_words(&skip, None).ends_with(
                "free at least 5.8 GB on / (`arena config check` names arena's biggest dirs there); or stage it on another \
                 filesystem for this run: `TMPDIR=<a dir on a disk with room> arena …`"
            ),
            "{}",
            staging_words(&skip, None)
        );
        let big = [(PathBuf::from("/tmp/a"), core_disk::TreeUse { bytes: 3_500_000_000, capped: false, changed: None }),
                   (PathBuf::from("/b"), core_disk::TreeUse { bytes: 2 * g, capped: true, changed: None })];
        assert_eq!(dir_sizes(&big), "/tmp/a 3.5 GB · /b over 2 GB");
        assert_eq!(dir_sizes(&[]), "");
    }

    #[test]
    fn a_skips_reason_is_its_line_without_the_advice() {
        assert_eq!(
            reason("skipped: only 3.1 GB free on / (floor 5 GB, CONTROL_MIN_FREE_GB) — nothing copied; free at least 1.9 GB on / (…)"),
            "only 3.1 GB free on / (floor 5 GB, CONTROL_MIN_FREE_GB)"
        );
        assert_eq!(
            reason("stopped: only 4.9 GB free on / while copying (floor 5 GB, CONTROL_MIN_FREE_GB) — what it copied so far stays; free at least 0.1 GB on / (…)"),
            "only 4.9 GB free on / while copying (floor 5 GB, CONTROL_MIN_FREE_GB)"
        );
        assert_eq!(reason("something else"), "something else");
    }

    /// The floor as a hard line while copies write: a watch over their filesystems stops them once
    /// one has less than the floor free (a copy writing more than its estimate); none without a
    /// floor or with a zero one; a filesystem it can't read doesn't stop them.
    #[tokio::test]
    async fn a_copy_is_stopped_once_its_disk_goes_under_the_floor() {
        let g = core_disk::GB;
        assert!(Watch::new(None, &[Path::new("/")]).is_none() && Watch::new(Some(Floor::control(0)), &[Path::new("/")]).is_none());
        let w = Watch::new(Some(Floor::control(5 * g)), &[Path::new("/b"), Path::new("/big")]).unwrap();
        let read = |free_big: u64| {
            move |p: &Path| match p.to_str() {
                Some("/b") => Ok(core_disk::Fs { dev: 1, mount: PathBuf::from("/"), free: 9 * g, total: 75 * g }),
                Some("/big") if free_big > 0 => Ok(core_disk::Fs { dev: 2, mount: PathBuf::from("/mnt/big"), free: free_big, total: 75 * g }),
                _ => Err("No such device".to_string()),
            }
        };
        assert_eq!(w.under(read(5 * g)), None, "at the floor: room");
        assert_eq!(w.under(read(0)), None, "unreadable: not a reason to stop");
        let skip = w.under(read(4_900_000_000)).expect("under it");
        assert!(skip.stopped);
        assert_eq!(skip.reason, "only 4.9 GB free on /mnt/big while copying (floor 5 GB, CONTROL_MIN_FREE_GB)");
        assert!(skip.to_string().starts_with("stopped: only 4.9 GB free on /mnt/big while copying") && skip.to_string().contains("— what it copied so far stays; free at least 0.1 GB on /mnt/big"), "{skip}");

        // Live: a copy that would never end is dropped (its rsync stopped) once the disk is under
        // the floor — here a floor no disk has; with no watch, it runs to its end.
        let huge = Watch::new(Some(Floor::control(100_000 * g)), &[&std::env::temp_dir()]);
        let stopped = tokio::time::timeout(std::time::Duration::from_secs(10), watched(huge.as_ref(), std::future::pending::<()>())).await;
        assert!(stopped.expect("stopped at once").unwrap_err().reason.contains("while copying (floor 100000 GB"));
        assert_eq!(watched(None, async { 7 }).await.unwrap(), 7);
    }

    /// A copy the watch stops has ended by the time `watched` returns: nothing of it writes on while
    /// its run goes on — or ends, which would SIGKILL an rsync mid-stop and orphan its receiver.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_stopped_copy_has_ended_when_the_watch_returns() {
        let huge = Watch::new(Some(Floor::control(100_000 * core_disk::GB)), &[&std::env::temp_dir()]);
        let arg = format!("{}.{}", 40 + std::process::id() % 7, std::process::id());
        let running = || {
            let want = format!("sleep\0{arg}\0");
            std::fs::read_dir("/proc").into_iter().flatten().flatten().any(|e| std::fs::read(e.path().join("cmdline")).is_ok_and(|c| c == want.as_bytes()))
        };
        let started = std::time::Instant::now();
        let copy = arena_core::remote::run_local("sleep", std::slice::from_ref(&arg), "a long copy", None);
        let stopped = watched(huge.as_ref(), copy).await;
        assert!(stopped.is_err_and(|s| s.stopped));
        assert!(started.elapsed() < std::time::Duration::from_secs(8), "{:?}", started.elapsed());
        assert!(!running(), "the stopped copy is gone");
    }

    /// What other runs hold that can't be read skips every copy (never counting free space another
    /// run may count); the notes are box-wide — another cohort's run on this box counts them.
    #[test]
    fn copies_are_judged_against_every_cohorts_runs_or_not_at_all() {
        let d = std::env::temp_dir().join(format!("arena-disk-cohorts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let cfg = |prefix: &str| Config::parse(&format!("MACHINE_NAME_PREFIX={prefix}\nARENA_STATE_DIR={}\nCONTROL_MIN_FREE_GB=0\n", d.display()));
        let floor = Floor::control(0);
        let copy = |pod: &str, bytes: u64| [(pod.to_string(), vec![(d.clone(), Size::Measured(bytes))])];
        std::fs::create_dir_all(&d).unwrap();
        let free = core_disk::reading(&d).unwrap().free;
        // arena8's run holds all but 2 GB of what's free; arena9's 3 GB copy doesn't fit (whatever
        // else the disk does meanwhile, short of a GB written or freed).
        let g = core_disk::GB;
        let (skipped, held) = judge_copies(&cfg("arena8"), floor, &copy("arena8-a", free.saturating_sub(2 * g)), true);
        assert!(skipped.is_empty(), "{skipped:?}");
        let (skipped, _) = judge_copies(&cfg("arena9"), floor, &copy("arena9-b", 3 * g), true);
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert!(skipped[0].1.reason.contains("for other copies under way"), "{}", skipped[0].1);
        drop(held);
        // Notes that can't be read (a dir where the file goes): every copy skipped, said why.
        let path = d.join(core_disk::RESERVATIONS_FILE);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir_all(&path).unwrap();
        let (skipped, _) = judge_copies(&cfg("arena9"), floor, &[copy("arena9-b", 1)[0].clone(), copy("arena9-c", 1)[0].clone()], true);
        assert_eq!(skipped.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["arena9-b", "arena9-c"]);
        assert!(skipped[0].1.to_string().starts_with("skipped: other arena runs' copies onto this box unknown (can't read "), "{}", skipped[0].1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_disk_is_looked_at_in_tests_only_when_asked() {
        assert!(!live(&Config::parse("")));
        assert!(floor(&Config::parse("")).unwrap().is_none());
        assert_eq!(floor(&Config::parse("CONTROL_MIN_FREE_GB=7\n")).unwrap(), Some(Floor::control(7 * core_disk::GB)));
        assert!(floor(&Config::parse("CONTROL_MIN_FREE_GB=x\n")).is_err());
        assert!(check_rows(&Config::parse("")).is_empty() && health(&Config::parse(""), 0).is_none());
        // An invalid floor: said, and a failure of the health check (every copy fails up front).
        let h = health(&Config::parse("CONTROL_MIN_FREE_GB=5GB\n"), 0).unwrap();
        assert!(h.invalid.as_deref().is_some_and(|e| e.starts_with("CONTROL_MIN_FREE_GB must be")), "{:?}", h.invalid);
        assert!(h.lines.iter().any(|l| l.starts_with("  ✗ CONTROL_MIN_FREE_GB must be") && l.ends_with("every copy onto this box fails until it's fixed")));
    }

    /// A pod's copy is a later one only onto its own earlier copy here AND once it was copied whole
    /// here (an interrupted first copy leaves files but no mark); its tiers are judged together,
    /// a run's copies adding up, and a reading that fails is a skip.
    #[test]
    fn a_pods_copy_is_sized_by_what_is_already_there_and_judged_whole() {
        let d = std::env::temp_dir().join(format!("arena-disk-size-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let base = d.join("backup");
        let (snap, big) = (base.join("w1d2/devtest-a"), base.join("big/devtest-a"));
        let sizes = |pod: &str, snap: &Path, big: &Path| (tier_size(&base, pod, true, snap), tier_size(&base, pod, false, big));
        assert_eq!(sizes("devtest-a", &snap, &big), (Size::First, Size::First));
        for dir in [base.join("w1d1/devtest-a"), big.clone()] {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("x.py"), "x").unwrap();
        }
        assert_eq!(sizes("devtest-a", &snap, &big), (Size::Later, Size::Later), "a backup from before marking: as a run takes it");
        std::fs::create_dir_all(base.join(core_disk::COPIED_DIR)).unwrap();
        assert_eq!(sizes("devtest-a", &snap, &big), (Size::First, Size::First), "files there, but never copied whole");
        core_disk::mark_copied(&base, "devtest-a").unwrap();
        assert_eq!(sizes("devtest-a", &snap, &big), (Size::Later, Size::Later));
        core_disk::mark_copied(&base, "devtest-b").unwrap();
        let (snap_b, big_b) = (base.join("w1d2/devtest-b"), base.join("big/devtest-b"));
        assert_eq!(sizes("devtest-b", &snap_b, &big_b), (Size::First, Size::First), "marked, but nothing of it here to build on");
        // A label of its own (`--label mine`), pulled into again: its files there to build on.
        let mine = base.join("mine/devtest-b");
        std::fs::create_dir_all(&mine).unwrap();
        std::fs::write(mine.join("x.py"), "x").unwrap();
        assert_eq!(tier_size(&base, "devtest-b", true, &mine), Size::Later);
        let _ = std::fs::remove_dir_all(&d);

        // 8 GB free, floor 5: a first copy of both tiers (~6 GB) doesn't fit, later ones do.
        let g = core_disk::GB;
        let read = |p: &Path| {
            if p.starts_with("/gone") {
                return Err("No such device".to_string());
            }
            Ok(core_disk::Fs { dev: 1, mount: PathBuf::from("/"), free: 8 * g, total: 75 * g })
        };
        let pod = |name: &str, size: Size| (name.to_string(), vec![(PathBuf::from("/b/w1d2"), size), (PathBuf::from("/b/big"), size)]);
        let copies = [pod("fresh", Size::First), pod("old1", Size::Later), pod("old2", Size::Later), ("lost".into(), vec![(PathBuf::from("/gone/x"), Size::Later)])];
        let (skipped, taken) = judge_copies_with(core_disk::Ledger::new(Floor::control(5 * g)), &copies, read);
        let names: Vec<&str> = skipped.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["fresh", "lost"]);
        assert!(skipped[0].1.reason.contains("this copy (~6 GB, a first copy, not measured) would leave less"), "{}", skipped[0].1);
        assert!(skipped[1].1.to_string().starts_with("skipped: free space on /gone/x unknown (No such device)"), "{}", skipped[1].1);
        assert_eq!(taken.get(&1), Some(&(4 * core_disk::LATER_COPY)), "what the admitted ones reserve");
    }

    /// Two runs at once count each other's copies: an admission reserves its space in the state
    /// dir until its hold goes; a dry run reserves nothing.
    #[test]
    fn a_runs_admitted_copies_are_held_for_other_runs_until_they_end() {
        let d = std::env::temp_dir().join(format!("arena-disk-hold-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let cfg = Config::parse(&format!("MACHINE_NAME_PREFIX=devtest\nARENA_STATE_DIR={}\nCONTROL_MIN_FREE_GB=0\n", d.display()));
        let path = d.join(core_disk::RESERVATIONS_FILE);
        let copy = [("devtest-a".to_string(), vec![(d.clone(), Size::Measured(1_000))])];
        let floor = Floor::control(0);
        let (skipped, hold) = judge_copies(&cfg, floor, &copy, false);
        assert!(skipped.is_empty() && !path.exists(), "a dry run holds nothing");
        drop(hold);
        let (skipped, hold) = judge_copies(&cfg, floor, &copy, true);
        assert!(skipped.is_empty());
        let (held, _) = core_disk::Reservations::load(&path).unwrap();
        assert_eq!(held.held.iter().map(|r| (r.pid, r.bytes)).collect::<Vec<_>>(), [(std::process::id(), 1_000)]);
        // Another run sees it as taken: here, all but 500 bytes of what's free is held elsewhere
        // too, and a 1 GB copy doesn't fit (whatever else the disk does meanwhile, short of
        // freeing a GB).
        let now_free = core_disk::reading(&d).unwrap();
        let mut r = held.clone();
        r.hold(std::process::id(), 42, &[(now_free.dev, now_free.free.saturating_sub(500))].into_iter().collect(), arena_core::snapshot::unix_now());
        r.save(&path).unwrap();
        let big = [("devtest-b".to_string(), vec![(d.clone(), Size::Measured(core_disk::GB))])];
        let (skipped, other) = judge_copies(&cfg, floor, &big, true);
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert!(skipped[0].1.reason.contains("for other copies under way"), "{}", skipped[0].1);
        drop(other);
        drop(hold);
        let (left, _) = core_disk::Reservations::load(&path).unwrap();
        assert_eq!(left.held.iter().map(|r| r.token).collect::<Vec<_>>(), [42], "its own let go of, the other's kept");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `config check`'s look at this box: one row per filesystem arena writes to, ⚠ under the
    /// floor with arena's biggest dirs there, ✓ with room; then what arena left on it — a
    /// replace/migrate staging copy with its size, age and the commands to look at it and remove
    /// it (nothing removed).
    #[test]
    fn the_rows_say_how_full_each_disk_is_and_what_arena_left_on_it() {
        let d = std::env::temp_dir().join(format!("arena-disk-rows-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let backup = d.join("backup");
        for tier in ["w1d1/devtest-a", "big/devtest-a"] {
            std::fs::create_dir_all(backup.join(tier)).unwrap();
            std::fs::write(backup.join(tier).join("notes.py"), vec![b'x'; 1 << 20]).unwrap();
        }
        let stage = core_disk::staging_dir(&std::env::temp_dir(), &format!("diskrows{}", std::process::id()));
        std::fs::create_dir_all(stage.join("ARENA_materials")).unwrap();
        std::fs::write(stage.join("ARENA_materials/work.py"), "work").unwrap();
        let cfg = |floor: &str| {
            Config::parse(&format!(
                "MACHINE_NAME_PREFIX=devtest\nLOCAL_BACKUP_DIR={}\nARENA_STATE_DIR={}\nCONTROL_MIN_FREE_GB={floor}\n",
                backup.display(),
                d.join("state").display()
            ))
        };
        let rows = check_rows(&cfg("100000"));
        assert_eq!(rows[0], "\nThis box's disk (every copy of a pod lands here — CONTROL_MIN_FREE_GB):");
        let low = rows.iter().find(|r| r.contains(&format!("backups {}", backup.display()))).expect("the backups' row");
        assert!(
            low.starts_with("  ⚠ ") && low.contains("UNDER the floor 100000 GB (CONTROL_MIN_FREE_GB): every copy onto it is skipped until there's room"),
            "{rows:#?}"
        );
        assert!(rows.iter().any(|r| r.starts_with("      └ arena's biggest dirs there: ")), "{rows:#?}");
        let at = rows.iter().position(|r| r.starts_with(&format!("  ⚠ {} — ", stage.display()))).expect("the staging copy");
        assert!(rows[at].contains("staged by `pods replace`/`migrate`"), "{}", rows[at]);
        assert!(rows[at + 1..].iter().any(|r| r == &format!("      └ remove: rm -rf {}", stage.display())), "{rows:#?}");
        // With room: ✓, no list of dirs.
        let rows = check_rows(&cfg("0"));
        assert!(rows.iter().any(|r| r.starts_with("  ✓ ") && r.contains(&format!("backups {}", backup.display()))), "{rows:#?}");
        assert!(!rows.iter().any(|r| r.contains("biggest dirs")), "{rows:#?}");
        assert!(stage.join("ARENA_materials/work.py").exists(), "nothing removed");
        let _ = std::fs::remove_dir_all(&stage);
        let _ = std::fs::remove_dir_all(&d);
    }
}
