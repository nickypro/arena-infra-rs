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

/// Judge a run's copies onto this box together ([`core_disk::Ledger`]): each copy (a pod) with
/// the destinations it writes and their sizes, against what other arena runs' copies under way
/// hold ([`core_disk::Reservations`], in the state dir). The ones that don't fit, with why — in
/// order — and the admitted ones' reservation, held until the returned [`Hold`] is dropped (when
/// the copies end). `reserve: false` (a dry run) holds nothing.
pub(crate) fn judge_copies(cfg: &Config, floor: Floor, copies: &[(String, Vec<(PathBuf, Size)>)], reserve: bool) -> (Vec<(String, Skip)>, Hold) {
    let Some(path) = reservations_path(cfg) else {
        return (judge_copies_with(core_disk::Ledger::new(floor), copies, core_disk::reading).0, Hold::default());
    };
    // Under the state dir's lock: no other run reads or writes the reservations between this
    // one's reading and its own (a lock that can't be had: this run's copies alone, said).
    let lock = match core_disk::lock_state_for(&path, core_disk::RECORD_LOCK_WAIT) {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!("warning: other runs' copies onto this box aren't counted this time ({}: {e})", path.display());
            return (judge_copies_with(core_disk::Ledger::new(floor), copies, core_disk::reading).0, Hold::default());
        }
    };
    let now = arena_core::snapshot::unix_now();
    let (mut held, warning) = core_disk::Reservations::load(&path);
    warning.iter().for_each(|w| eprintln!("{w}"));
    held.prune(now, core_disk::arena_alive);
    let (skipped, taken) = judge_copies_with(core_disk::Ledger::with_others(floor, held.by_dev()), copies, core_disk::reading);
    if !reserve || taken.is_empty() {
        return (skipped, Hold::default());
    }
    let token = now.wrapping_mul(1_000_003) ^ u64::from(std::process::id()) ^ u64::from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos()));
    held.hold(std::process::id(), token, &taken, now);
    if let Err(e) = held.save(&path) {
        eprintln!("warning: couldn't note this run's copies in {} ({e}) — other runs won't count them", path.display());
        return (skipped, Hold::default());
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
        let (mut held, _) = core_disk::Reservations::load(&path);
        held.release(std::process::id(), self.token);
        let _ = held.save(&path);
        drop(lock);
    }
}

/// Where the reservations live: the fleet's state dir (as the skip record); `None` in this
/// crate's unit tests unless the test's config sets `ARENA_STATE_DIR`.
fn reservations_path(cfg: &Config) -> Option<PathBuf> {
    if cfg!(test) && cfg.get("ARENA_STATE_DIR").is_none() {
        return None;
    }
    arena_core::snapshot::state_file_path_for(cfg, core_disk::RESERVATIONS_FILE).ok()
}

/// What a copy of `pod` into `dest` may take, under the snapshot root `base`: a later copy — only
/// what changed — when the pod was copied whole into this backup before ([`core_disk::was_copied`])
/// and there's a copy of it to build on (for the snapshot tier, a snapshot of it under any label;
/// for the big tier, its files in `dest`); else a first one. An interrupted first copy leaves
/// files but no mark, so its retry still counts as a first copy.
pub(crate) fn tier_size(base: &Path, pod: &str, snapshot: bool, dest: &Path) -> Size {
    let builds_on = if snapshot {
        !arena_core::restore::snapshot_labels(base, pod).is_empty()
    } else {
        std::fs::read_dir(dest).is_ok_and(|mut e| e.next().is_some())
    };
    if builds_on && core_disk::was_copied(base, pod) {
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

/// The reason in a skip's line, without its `skipped: ` and its advice (the record's words).
fn reason(line: &str) -> String {
    let r = line.strip_prefix("skipped: ").unwrap_or(line);
    r.split(" — nothing copied").next().unwrap_or(r).to_string()
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
                    let list: Vec<String> = big
                        .iter()
                        .map(|(p, t)| format!("{} {}{}", p.display(), if t.capped { "over " } else { "" }, core_disk::gb(t.bytes)))
                        .collect();
                    out.push(format!("      └ arena's biggest dirs there: {}", list.join(" · ")));
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

    #[test]
    fn a_skips_reason_is_its_line_without_the_advice() {
        assert_eq!(
            reason("skipped: only 3.1 GB free on / (floor 5 GB, CONTROL_MIN_FREE_GB) — nothing copied; free space on / (…)"),
            "only 3.1 GB free on / (floor 5 GB, CONTROL_MIN_FREE_GB)"
        );
        assert_eq!(reason("something else"), "something else");
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
        assert_eq!(sizes("devtest-a", &snap, &big), (Size::First, Size::First), "files there, but never copied whole");
        core_disk::mark_copied(&base, "devtest-a").unwrap();
        assert_eq!(sizes("devtest-a", &snap, &big), (Size::Later, Size::Later));
        core_disk::mark_copied(&base, "devtest-b").unwrap();
        let (snap_b, big_b) = (base.join("w1d2/devtest-b"), base.join("big/devtest-b"));
        assert_eq!(sizes("devtest-b", &snap_b, &big_b), (Size::First, Size::First), "marked, but nothing of it here to build on");
        let _ = std::fs::remove_dir_all(&d);

        // 8 GB free, floor 5: a first copy of both tiers (~4 GB) doesn't fit, later ones do.
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
        assert!(skipped[0].1.reason.contains("this copy (~4 GB, a first copy, not measured) would leave less"), "{}", skipped[0].1);
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
        let path = d.join("devtest").join(core_disk::RESERVATIONS_FILE);
        let copy = [("devtest-a".to_string(), vec![(d.clone(), Size::Measured(1_000))])];
        let floor = Floor::control(0);
        let (skipped, hold) = judge_copies(&cfg, floor, &copy, false);
        assert!(skipped.is_empty() && !path.exists(), "a dry run holds nothing");
        drop(hold);
        let (skipped, hold) = judge_copies(&cfg, floor, &copy, true);
        assert!(skipped.is_empty());
        let (held, _) = core_disk::Reservations::load(&path);
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
        let (left, _) = core_disk::Reservations::load(&path);
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
