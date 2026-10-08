//! Replication bookkeeping for `pods replace` / `pods migrate` — the copy of a participant's
//! home from the pod they use (`<name>`) onto its replacement (`<name>-new`).
//!
//! The copy is a **replica**, not a backup: a file the participant deleted on the original
//! must not come back on the new pod (live finding #20), so it mirrors deletions
//! ([`crate::pull::Mirror::Replica`]). But the new pod can hold work of its own — a
//! cutover, a day's work on it, then a revert and a re-copy overwrote that work in place
//! (live finding #7, data loss). So nothing on the receiver is ever destroyed by a sync:
//! whatever it overwrites or deletes there is *moved* into a per-sync folder
//! (`~/.arena-sync-replaced/<stamp>/`, rsync `--backup-dir`), and afterwards this module's
//! [`settle_command`] sorts that folder on the pod:
//!
//! - a moved file **not newer** than the pod's last-sync stamp (`~/.arena-last-sync`) was
//!   what an earlier sync (or the image / setup, before the first one) put there — the
//!   original still has it or deliberately deleted it — so it goes;
//! - a moved file **newer** than the stamp was changed on the new pod itself since that
//!   sync: it is KEPT, counted and named in the report, for a human to reconcile.
//!
//! The stamp only advances after a complete sync ([`SettleMode::Advance`]): a partial one
//! leaves the files it didn't reach in place with their (newer) times, and moving the
//! baseline past them would let the next sync's tidy-up delete them. A pod the copy has just
//! created starts its baseline before its first sync ([`baseline_command`]) — its files are
//! all image + setup then. A pod without a stamp (made by an older version of this tool)
//! keeps everything that's moved: nothing is guessed.
//!
//! Also here: the local-staging checks of the via-local copy (the fallback that pulls the
//! home onto the control machine first) — how much it would stage and whether the disk has
//! room for it, so it never fills a box it shares with a live cohort (live finding #13).

use crate::pull::Mirror;

/// The receiver's last-sync stamp (a file in its home; its mtime is the baseline).
pub const LAST_SYNC: &str = ".arena-last-sync";

/// Where a sync moves what it overwrites or deletes on the receiver, per sync:
/// `<dir>/<stamp>/` (a dot-dir, so no later sync copies or deletes it).
pub const REPLACED_DIR: &str = ".arena-sync-replaced";

/// A sync's folder name: its UTC start time, sortable (`20261008T110203Z`).
pub fn sync_stamp(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let (y, m, d) = crate::schedule::civil_from_days(days);
    let s = unix_secs % 86_400;
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", s / 3600, s % 3600 / 60, s % 60)
}

/// The mirror for the home transfer of the sync `stamp`: moved files go to
/// `~/.arena-sync-replaced/<stamp>/` (rsync's relative `--backup-dir` is the receiving dir's).
pub fn home_mirror(stamp: &str) -> Mirror {
    Mirror::Replica { backup_dir: format!("{REPLACED_DIR}/{stamp}") }
}

/// The `--backup-dir` for a transfer into `<rel>/` (the repo's tree, carried on its own —
/// [`crate::volume::pod_to_pod_copy`]) when the home transfer's is `home_backup_dir`: beside
/// the repo's REAL directory (`../`), under its name — on the same filesystem as the repo
/// (its `/workspace` volume, when it's linked there), so rsync moves the files with a rename
/// that keeps their times, which [`settle_command`] reads. `None` for an unusable `rel`.
pub fn tree_backup_dir(home_backup_dir: &str, rel: &str) -> Option<String> {
    let name = rel.trim_end_matches('/').rsplit('/').next().filter(|n| !n.is_empty() && *n != "." && *n != "..")?;
    Some(format!("../{home_backup_dir}/{name}"))
}

/// Run on a pod the copy has just created, before its first sync: the baseline the first
/// [`settle_command`] measures against (everything on it so far is image + setup).
pub fn baseline_command() -> String {
    format!("touch \"$HOME/{LAST_SYNC}\"")
}

/// Whether [`settle_command`] moves the baseline on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleMode {
    /// The sync completed: the receiver now matches the original — new baseline.
    Advance,
    /// The sync failed part-way: tidy and report, but keep the old baseline (files it
    /// didn't reach keep their place and times, and must not look old next time).
    Keep,
}

/// Single-quote for a POSIX shell.
fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The tidy-up + report run on the receiver after the sync `stamp` (see the module doc).
/// `rel` = the repo's place in the home when its tree was carried on its own (its backups
/// sit beside its real directory). Prints `arena-replica-*` lines ([`parse_settle`]).
pub fn settle_command(stamp: &str, rel: Option<&str>, mode: SettleMode) -> String {
    let home_dir = format!("\"$HOME\"/{}", sq(&format!("{REPLACED_DIR}/{stamp}")));
    let tree = match rel.filter(|r| tree_backup_dir("", r).is_some()).map(|r| r.trim_end_matches('/')) {
        // The repo's real directory (through a link), then its parent's per-sync folder.
        Some(rel) => format!(
            "r=$(cd \"$HOME\"/{} 2>/dev/null && pwd -P) && [ -n \"$r\" ] && t=\"${{r%/*}}\"/{} && [ \"$t\" != \"$1\" ] && set -- \"$@\" \"$t\"\n",
            sq(rel),
            sq(&format!("{REPLACED_DIR}/{stamp}"))
        ),
        None => String::new(),
    };
    let advance = match mode {
        SettleMode::Advance => format!("touch \"$HOME/{LAST_SYNC}\"\n"),
        SettleMode::Keep => String::new(),
    };
    format!(
        "s=\"$HOME/{LAST_SYNC}\"\nset -- {home_dir}\n{tree}\
         for d in \"$@\"; do\n\
         \x20 [ -d \"$d\" ] || continue\n\
         \x20 [ -f \"$s\" ] && find \"$d\" -type f ! -newer \"$s\" -exec rm -f {{}} + 2>/dev/null\n\
         \x20 find \"$d\" -depth -type d -empty -exec rmdir {{}} \\; 2>/dev/null\n\
         \x20 rmdir \"${{d%/*}}\" 2>/dev/null\n\
         done\n\
         {advance}\
         n=0\n\
         for d in \"$@\"; do [ -d \"$d\" ] && n=$((n + $(find \"$d\" -type f | wc -l))); done\n\
         echo \"arena-replica-kept=$n\"\n\
         for d in \"$@\"; do\n\
         \x20 [ -d \"$d\" ] || continue\n\
         \x20 echo \"arena-replica-dir=$d $(du -sh \"$d\" 2>/dev/null | cut -f1)\"\n\
         \x20 find \"$d\" -type f ! -path '*/.git/*' | head -n 5 | while IFS= read -r f; do echo \"arena-replica-file=${{f#\"$d\"/}}\"; done\n\
         done\n\
         true\n"
    )
}

/// What [`settle_command`] reported: files kept (changed on the receiver since its last
/// sync), where (`dir size` per folder), and up to five per folder by name (outside `.git`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settled {
    pub kept: u64,
    pub dirs: Vec<String>,
    pub files: Vec<String>,
}

/// Read [`settle_command`]'s output; `None` when it didn't run (no `kept` line).
pub fn parse_settle(stdout: &str) -> Option<Settled> {
    let mut out = Settled::default();
    let mut seen = false;
    for line in stdout.lines() {
        if let Some(n) = line.strip_prefix("arena-replica-kept=") {
            out.kept = n.trim().parse().ok()?;
            seen = true;
        } else if let Some(d) = line.strip_prefix("arena-replica-dir=") {
            out.dirs.push(d.trim().to_string());
        } else if let Some(f) = line.strip_prefix("arena-replica-file=") {
            out.files.push(f.to_string());
        }
    }
    seen.then_some(out)
}

/// The report lines for `pod` (the receiver): nothing when nothing was kept; else how many
/// files it held that the sync replaced or deleted and that had changed on it since its last
/// sync — kept, where, and a few by name. Pure.
pub fn settle_lines(pod: &str, s: &Settled) -> Vec<String> {
    if s.kept == 0 {
        return Vec::new();
    }
    let mut out = vec![format!(
        "⚠ {} file(s) on {pod} had changed there since its last sync and were replaced or deleted by this \
         one — KEPT, moved aside (not lost): {}",
        s.kept,
        s.dirs.join(", ")
    )];
    for f in &s.files {
        out.push(format!("    {f}"));
    }
    out.push(format!(
        "  (work done on {pod} itself — e.g. after a cutover and a revert. Compare and restore what's \
         wanted; the copy now there is the original's.)"
    ));
    out
}

/// Prints the receiver's last-sync time (`arena-last-sync=<unix secs>`), or nothing.
pub fn last_sync_command() -> String {
    format!("[ -f \"$HOME/{LAST_SYNC}\" ] && echo \"arena-last-sync=$(stat -c %Y \"$HOME/{LAST_SYNC}\")\"; true")
}

/// [`last_sync_command`]'s answer.
pub fn parse_last_sync(stdout: &str) -> Option<u64> {
    stdout.lines().find_map(|l| l.strip_prefix("arena-last-sync=")).and_then(|v| v.trim().parse().ok())
}

/// The bytes a dry-run `rsync --stats` would transfer (`Total transferred file size:`),
/// with or without `--human-readable` units (`3.10G`, `1,234,567`, `812.33K`; rsync's
/// units are powers of 1000). `None` when the line is missing or unreadable.
pub fn transfer_bytes(stats: &str) -> Option<u64> {
    let rest = stats.lines().find_map(|l| l.trim().strip_prefix("Total transferred file size:"))?;
    let token = rest.split_whitespace().next()?;
    let (num, mult) = match token.chars().last()? {
        c if c.is_ascii_digit() => (token, 1u64),
        'K' | 'k' => (&token[..token.len() - 1], 1_000),
        'M' => (&token[..token.len() - 1], 1_000_000),
        'G' => (&token[..token.len() - 1], 1_000_000_000),
        'T' => (&token[..token.len() - 1], 1_000_000_000_000),
        'P' => (&token[..token.len() - 1], 1_000_000_000_000_000),
        _ => return None,
    };
    if mult == 1 {
        // Plain (level 1): digits with thousands separators, either ',' or '.'.
        return num.chars().filter(char::is_ascii_digit).collect::<String>().parse().ok();
    }
    let v: f64 = num.replace(',', ".").parse().ok()?;
    (v.is_finite() && v >= 0.0).then(|| (v * mult as f64).round() as u64)
}

/// What the local staging must leave free on its disk: a tenth of it, at least 5 GB — room
/// for everything else on a machine the live cohort's proxy and backups share.
pub fn staging_reserve(total: u64) -> u64 {
    (total / 10).max(5_000_000_000)
}

/// Whether staging `needed` bytes fits on a disk with `free` of `total` bytes, keeping
/// [`staging_reserve`] free. `Err` says why not (in GB). Pure.
pub fn staging_room(needed: u64, free: u64, total: u64) -> Result<(), String> {
    let reserve = staging_reserve(total);
    let gb = |b: u64| format!("{:.1} GB", b as f64 / 1e9);
    if needed.saturating_add(reserve) <= free {
        return Ok(());
    }
    Err(format!(
        "staging needs ~{} but its disk has {} free and {} must stay free",
        gb(needed),
        gb(free),
        gb(reserve)
    ))
}

/// `(free, total)` bytes of the filesystem holding `path` (`statvfs`; free = what an
/// unprivileged process may use). `None` when it can't be read.
#[cfg(unix)]
pub fn disk_space(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` a properly sized out-param.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frsize = st.f_frsize as u64;
    Some((st.f_bavail as u64 * frsize, st.f_blocks as u64 * frsize))
}

#[cfg(not(unix))]
pub fn disk_space(_path: &std::path::Path) -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    #[test]
    fn stamps_are_utc_and_sortable() {
        assert_eq!(sync_stamp(0), "19700101T000000Z");
        assert_eq!(sync_stamp(1_791_457_323), "20261008T110203Z");
        assert!(sync_stamp(1_791_457_323) < sync_stamp(1_791_457_324));
    }

    #[test]
    fn backup_dirs_for_the_home_and_the_tree() {
        assert_eq!(home_mirror("S"), Mirror::Replica { backup_dir: ".arena-sync-replaced/S".into() });
        assert_eq!(tree_backup_dir(".arena-sync-replaced/S", "ARENA_materials").as_deref(), Some("../.arena-sync-replaced/S/ARENA_materials"));
        assert_eq!(tree_backup_dir("b", "work/ARENA_materials/").as_deref(), Some("../b/ARENA_materials"));
        for bad in ["", "/", "..", "a/.."] {
            assert_eq!(tree_backup_dir("b", bad), None, "{bad:?}");
        }
    }

    #[test]
    fn transfer_bytes_reads_every_rsync_number_style() {
        let line = |v: &str| format!("Number of files: 9\nTotal transferred file size: {v} bytes\nLiteral data: 0\n");
        for (v, want) in [
            ("1,234,567", Some(1_234_567)),
            ("1.234.567", Some(1_234_567)),
            ("0", Some(0)),
            ("3.10G", Some(3_100_000_000)),
            ("184.32M", Some(184_320_000)),
            ("812.33K", Some(812_330)),
            ("1,5G", Some(1_500_000_000)),
            ("lots", None),
        ] {
            assert_eq!(transfer_bytes(&line(v)), want, "{v}");
        }
        assert_eq!(transfer_bytes("nothing here"), None);
    }

    #[test]
    fn staging_keeps_a_reserve_free() {
        const G: u64 = 1_000_000_000;
        // (needed, free, total, fits)
        for (needed, free, total, fits) in [
            (3 * G, 18 * G, 75 * G, true),   // the shared box: 18 GB free, 7.5 GB reserve
            (11 * G, 18 * G, 75 * G, false), // would leave < 7.5 GB
            (G, 5 * G, 20 * G, false),       // small disk: 5 GB floor
            (0, 5 * G, 20 * G, true),
        ] {
            assert_eq!(staging_room(needed, free, total).is_ok(), fits, "{needed} {free} {total}");
        }
        let e = staging_room(11 * G, 18 * G, 75 * G).unwrap_err();
        assert!(e.contains("~11.0 GB") && e.contains("18.0 GB free") && e.contains("7.5 GB must stay free"), "{e}");
    }

    #[test]
    fn settle_and_last_sync_output_parse() {
        let out = "arena-replica-kept=3\narena-replica-dir=/root/.arena-sync-replaced/S 12K\narena-replica-file=notes.txt\narena-replica-file=ARENA_materials/x.py\n";
        let s = parse_settle(out).unwrap();
        assert_eq!(s, Settled { kept: 3, dirs: vec!["/root/.arena-sync-replaced/S 12K".into()], files: vec!["notes.txt".into(), "ARENA_materials/x.py".into()] });
        let lines = settle_lines("devtest-a-new", &s);
        assert!(lines[0].contains("3 file(s) on devtest-a-new") && lines[0].contains("KEPT") && lines[0].contains("/root/.arena-sync-replaced/S"), "{lines:?}");
        assert_eq!(lines[1], "    notes.txt");
        assert!(settle_lines("p", &Settled::default()).is_empty());
        assert_eq!(parse_settle("garbage"), None);
        assert_eq!(parse_last_sync("arena-last-sync=1791457323\n"), Some(1_791_457_323));
        assert_eq!(parse_last_sync(""), None);
    }

    /// A temp dir, removed when dropped.
    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tmp(tag: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("arena-replica-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }
    fn put(p: &Path, body: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
    /// Set a file's mtime `secs` before now (via `touch -d @…`).
    fn age(p: &Path, secs: u64) {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() - secs;
        assert!(Command::new("touch").arg("-d").arg(format!("@{t}")).arg(p).status().unwrap().success());
    }
    fn sh(cmd: &str, home: &Path) -> String {
        let out = Command::new("sh").arg("-c").arg(cmd).env("HOME", home).output().unwrap();
        assert!(out.status.success(), "{cmd}\n{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }
    fn have_rsync() -> bool {
        Command::new("rsync").arg("--version").output().is_ok()
    }

    /// The whole contract, with the real rsync and the real script on a temp-dir "pod": a
    /// replica sync propagates the original's deletions, moves everything it replaces or
    /// deletes aside instead of destroying it, and the tidy-up drops what an earlier sync put
    /// there while KEEPING (and naming) what changed on the pod itself since — the data loss
    /// of live finding #7 (a revert, then a copy that overwrote the work on `-new`) and the
    /// resurrection of #20 (a deleted file coming back) in one run.
    #[cfg(unix)]
    #[test]
    fn a_replica_sync_mirrors_deletions_and_keeps_work_done_on_the_receiver() {
        if !have_rsync() {
            eprintln!("rsync not installed — skipping");
            return;
        }
        let t = tmp("sync");
        let (src, dst) = (t.0.join("src"), t.0.join("dst"));
        // First sync: the original's files land; the receiver's baseline is older.
        put(&src.join("work.py"), "v1\n");
        put(&src.join("todelete.txt"), "bye\n");
        put(&src.join("keep.txt"), "same\n");
        for f in ["work.py", "todelete.txt", "keep.txt"] {
            age(&src.join(f), 200); // written on the original well before the first sync
        }
        std::fs::create_dir_all(&dst).unwrap();
        sh(&baseline_command(), &dst);
        age(&dst.join(LAST_SYNC), 100);
        let sync = |stamp: &str| {
            let pc = crate::pull::PullConfig { mirror: home_mirror(stamp), ..crate::pull::PullConfig::replication() };
            let mut args: Vec<String> = crate::pull::push_rsync_args(
                &crate::ssh::SshTarget { user: "u".into(), host: "h".into(), port: 1, key_paths: vec![], connect_timeout_secs: 1 },
                &pc,
                &format!("{}/", src.display()),
            );
            // A local receiver instead of `u@h:` (rsync's own transport isn't the point here).
            let e = args.iter().position(|a| a == "-e").unwrap();
            args.drain(e..e + 2);
            *args.last_mut().unwrap() = format!("{}/", dst.display());
            let out = Command::new("rsync").args(&args).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        };
        sync("S1");
        let first = parse_settle(&sh(&settle_command("S1", None, SettleMode::Advance), &dst)).unwrap();
        assert_eq!(first.kept, 0, "a fresh receiver keeps nothing: {first:?}");
        age(&dst.join(LAST_SYNC), 50);
        // Then: the participant deletes a file and edits one on the original; meanwhile work
        // was done on the receiver itself (after a cutover + revert): an edit and a new file.
        std::fs::remove_file(src.join("todelete.txt")).unwrap();
        put(&src.join("work.py"), "v2 (original)\n");
        age(&src.join("work.py"), 60); // edited on the original BEFORE the receiver's edit
        put(&dst.join("work.py"), "v3 (on the new pod)\n");
        put(&dst.join("only_new.txt"), "made on the new pod\n");
        sync("S2");
        // #20: the deleted file is gone from the receiver's tree.
        assert!(!dst.join("todelete.txt").exists());
        assert_eq!(std::fs::read_to_string(dst.join("work.py")).unwrap(), "v2 (original)\n");
        assert_eq!(std::fs::read_to_string(dst.join("keep.txt")).unwrap(), "same\n");
        let s = parse_settle(&sh(&settle_command("S2", None, SettleMode::Advance), &dst)).unwrap();
        // #7: the receiver's own work is kept and named; the stale replica copy is dropped.
        let moved = dst.join(REPLACED_DIR).join("S2");
        assert_eq!(std::fs::read_to_string(moved.join("work.py")).unwrap(), "v3 (on the new pod)\n");
        assert_eq!(std::fs::read_to_string(moved.join("only_new.txt")).unwrap(), "made on the new pod\n");
        assert!(!moved.join("todelete.txt").exists(), "an earlier sync's copy of a file the original deleted goes");
        assert_eq!(s.kept, 2, "{s:?}");
        let mut files = s.files.clone();
        files.sort();
        assert_eq!(files, ["only_new.txt", "work.py"]);
        // A third sync with nothing changed on the receiver: nothing kept, folder gone, and
        // the earlier folder (the kept work) untouched.
        age(&dst.join(LAST_SYNC), 10);
        put(&src.join("work.py"), "v4\n");
        sync("S3");
        let s3 = parse_settle(&sh(&settle_command("S3", None, SettleMode::Advance), &dst)).unwrap();
        assert_eq!(s3.kept, 0, "{s3:?}");
        assert!(!dst.join(REPLACED_DIR).join("S3").exists());
        assert!(moved.join("work.py").exists(), "kept work survives later syncs");
    }

    /// Without a stamp (a receiver from an older version) nothing moved is deleted; a
    /// partial sync (`Keep`) doesn't move the baseline; the repo's tree folder (beside its
    /// real directory, through a link) is tidied and reported too.
    #[cfg(unix)]
    #[test]
    fn the_tidy_up_guesses_nothing() {
        let t = tmp("settle");
        let home = t.0.join("home");
        let ws = t.0.join("ws");
        std::fs::create_dir_all(ws.join("ARENA_materials")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::os::unix::fs::symlink(ws.join("ARENA_materials"), home.join("ARENA_materials")).unwrap();
        // No stamp: everything moved stays.
        put(&home.join(REPLACED_DIR).join("S/old.txt"), "x");
        age(&home.join(REPLACED_DIR).join("S/old.txt"), 1000);
        put(&ws.join(REPLACED_DIR).join("S/ARENA_materials/nb.ipynb"), "y");
        let s = parse_settle(&sh(&settle_command("S", Some("ARENA_materials"), SettleMode::Keep), &home)).unwrap();
        assert_eq!(s.kept, 2, "{s:?}");
        assert_eq!(s.dirs.len(), 2, "both folders reported: {s:?}");
        assert!(s.files.contains(&"ARENA_materials/nb.ipynb".to_string()), "{s:?}");
        assert!(!home.join(LAST_SYNC).exists(), "Keep never starts a baseline");
        // With a stamp: the old one goes, the newer one stays; Advance stamps.
        sh(&baseline_command(), &home);
        age(&home.join(LAST_SYNC), 100);
        age(&ws.join(REPLACED_DIR).join("S/ARENA_materials/nb.ipynb"), 200);
        let s = parse_settle(&sh(&settle_command("S", Some("ARENA_materials"), SettleMode::Advance), &home)).unwrap();
        assert_eq!(s.kept, 0, "{s:?}");
        assert!(!home.join(REPLACED_DIR).exists() && !ws.join(REPLACED_DIR).exists(), "emptied folders removed");
        let stamped = std::fs::metadata(home.join(LAST_SYNC)).unwrap().modified().unwrap();
        assert!(stamped.elapsed().unwrap().as_secs() < 50, "baseline moved on");
        assert!(parse_last_sync(&sh(&last_sync_command(), &home)).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn disk_space_reads_the_temp_dir() {
        let (free, total) = disk_space(&std::env::temp_dir()).unwrap();
        assert!(total > 0 && free <= total);
        assert_eq!(disk_space(Path::new("/nonexistent/arena")), None);
    }
}
