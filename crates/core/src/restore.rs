//! `pods restore`: push a local backup — what `pods pull` saved — back onto a pod.
//!
//! Why: the file backups exist to get participants' work back after a pod lost it (a RunPod
//! restart/stop resets the container disk) or was replaced. This module is the pure part:
//! which backup (`<dir>/<label>/<pod>/` or `<dir>/big/<pod>/`, pull's layout), which path in
//! it, the rsync argv, and what to warn about. The CLI resolves the pod, confirms, and runs it
//! with a budget.
//!
//! The argv is safe by construction:
//! - never `--delete` (nor `--force`): whatever is on the pod and not in the backup stays;
//! - `--update`: a file on the pod NEWER than the backup's copy is left alone (a restore is
//!   for getting lost work back, not for reverting what the participant did since the
//!   backup) — `--overwrite-newer` drops it; each such file is reported (`--info=skip1`);
//! - a file the restore does replace is kept (rsync `--backup-dir`): under
//!   `/workspace/.arena-restore/<UTC stamp>/` when the pod has its volume mounted — where a
//!   restart can't wipe it; the home is the container disk, and the repo's files would be
//!   moved off the volume into it — else `~/.arena-restore/<stamp>/`. Pulls don't back that
//!   dir up (a dot-dir): the prompt says so;
//! - `--keep-dirlinks`: a symlink to a directory on the pod — the ARENA repo linked onto the
//!   `/workspace` volume by setup — is written THROUGH, so the work lands in the volume copy.
//!   Without it rsync replaces the link with a fresh directory on the container disk (verified
//!   locally): the next restart would wipe what was just restored;
//! - `--no-owner --no-group --chmod=go-w`: the pod's home keeps root's ownership and never
//!   turns group/world-writable — either would make sshd's StrictModes refuse every key;
//! - `.ssh/`, the shell rc files and histories, `~/.name` and `.claude*` are not pushed: keys,
//!   the per-pod API keys in the rc files, the machine name and Claude Code state belong to the
//!   pod (setup / copy-keys write them) — the same line `pods replace`'s copy draws;
//! - from a snapshot tier, `.git/` dirs aren't pushed (unless `--with-git`): the tier drops
//!   files over its size cap, a git pack among them, and refs pointing at objects that never
//!   arrived break the repo. The big tier carries every file; after any restore that pushed a
//!   repo's `.git`, the CLI checks the repo still reads ([`git_check_command`]);
//! - `--timeout=300`: a transfer that goes silent gives up (the CLI bounds the whole run too).

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::ssh::SshTarget;

/// `--from big`: pull's all-files tier (`<dir>/big/<pod>/`).
pub const BIG: &str = "big";

/// The default wall-clock budget for one restore (`--timeout` overrides): a full home over a
/// slow uplink takes tens of minutes; a silent transfer is already stopped by the I/O timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2 * 3600);

/// rsync `--timeout`: seconds without any I/O before the transfer gives up.
pub const IO_TIMEOUT_SECS: u64 = 300;

/// The dir (under the home, or the volume's root) where the files a restore replaced are kept.
pub const KEPT_DIR: &str = ".arena-restore";

/// The budget for the post-restore git check ([`git_check_command`]).
pub const GIT_CHECK_TIMEOUT: Duration = Duration::from_secs(120);

/// Never pushed back (see the module doc). Anchored (`/…`) = only at the home's top level.
pub const EXCLUDES: &[&str] = &[
    "/.ssh/",
    "/.bashrc",
    "/.zshrc",
    "/.zshrc.pre-oh-my-zsh",
    "/.bash_history",
    "/.zsh_history",
    "/.name",
    "/.arena-restore/",
    ".claude*",
];

/// A snapshot label `wNdM` as `(week, day)`; anything else (a custom `--label`, `big`) is `None`.
pub fn parse_label(label: &str) -> Option<(u32, u32)> {
    let rest = label.strip_prefix('w')?;
    let (w, d) = rest.split_once('d')?;
    let num = |s: &str| (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok()).flatten();
    Some((num(w)?, num(d)?))
}

/// Whether `dir` is a directory holding at least one entry (not following a symlink).
fn non_empty_dir(dir: &Path) -> bool {
    std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir())
        && std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some())
}

/// Every label under `base` holding a non-empty backup of `pod`, for "what is there" messages:
/// the `wNdM` snapshots newest first, then any other label (`pods pull --label …`) by name,
/// then `big`.
pub fn all_labels(base: &Path, pod: &str) -> Vec<String> {
    let mut others: Vec<String> = std::fs::read_dir(base)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|l| parse_label(l).is_none() && l != BIG && non_empty_dir(&base.join(l).join(pod)))
        .collect();
    others.sort();
    let mut v = snapshot_labels(base, pod);
    v.extend(others);
    if non_empty_dir(&base.join(BIG).join(pod)) {
        v.push(BIG.to_string());
    }
    v
}

/// The `wNdM` snapshot labels under `base` that hold a non-empty backup of `pod`, newest first
/// (by week, then day — not by name: `w10d1` is newer than `w9d5`).
pub fn snapshot_labels(base: &Path, pod: &str) -> Vec<String> {
    let mut found: Vec<((u32, u32), String)> = std::fs::read_dir(base)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let label = e.file_name().to_string_lossy().into_owned();
            let key = parse_label(&label)?;
            non_empty_dir(&e.path().join(pod)).then_some((key, label))
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    found.into_iter().map(|(_, l)| l).collect()
}

/// Which backup of `pod` under `base` to restore: `from` = a label (`w1d3`), `big`, or `None`
/// for the newest `wNdM` snapshot. Returns `(label, dir)`. Refuses — the message says what
/// exists instead — when the base dir is missing, the chosen backup is missing or empty, or
/// there is no snapshot to default to.
pub fn resolve_source(base: &Path, pod: &str, from: Option<&str>) -> Result<(String, PathBuf), String> {
    if !base.is_dir() {
        return Err(format!("no backups here: {} doesn't exist (pass --dir, or set LOCAL_BACKUP_DIR)", base.display()));
    }
    let labels = snapshot_labels(base, pod);
    let have = || {
        let v = all_labels(base, pod);
        if v.is_empty() {
            format!("there is no backup of {pod} under {}", base.display())
        } else {
            format!("backups of {pod}: {}", v.join(", "))
        }
    };
    let label = match from {
        Some(l) if l.is_empty() || l.contains('/') || l == "." || l == ".." => {
            return Err(format!("--from `{l}` isn't a backup label (e.g. w1d3) or `big`"))
        }
        Some(l) => l.to_string(),
        None => match labels.first() {
            Some(l) => l.clone(),
            None => return Err(format!("no wNdM snapshot of {pod} to restore — {}; pass --from <label>|big", have())),
        },
    };
    let dir = base.join(&label).join(pod);
    if !non_empty_dir(&dir) {
        let what = if dir.exists() { "is empty" } else { "doesn't exist" };
        return Err(format!("{} {what} — {}", dir.display(), have()));
    }
    Ok((label, dir))
}

/// Check `--path`: a relative path inside the backup, no `..`/`.`/empty component. Returns it
/// with any trailing `/` dropped.
pub fn check_subpath(path: &str) -> Result<String, String> {
    let p = path.trim_end_matches('/');
    if p.is_empty() || p.starts_with('/') || p.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
        return Err(format!("--path `{path}` must be a relative path inside the backup, e.g. ARENA_materials/chapter1"));
    }
    Ok(p.to_string())
}

/// What a backup tree holds: files (symlinks count, never followed), bytes, and when a pull
/// last wrote into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeStats {
    pub files: u64,
    pub bytes: u64,
    /// The newest status change (ctime, UNIX seconds) of anything in it: the last time a pull
    /// wrote, replaced or created something there (rsync keeps the pod's mtimes; it can't keep
    /// a ctime). `None` for an empty tree.
    pub last_written: Option<u64>,
}

/// A file's status-change time (UNIX seconds) — its mtime where there is no ctime.
fn changed_secs(m: &std::fs::Metadata) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        u64::try_from(m.ctime()).ok()
    }
    #[cfg(not(unix))]
    {
        m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs())
    }
}

/// [`TreeStats`] of `path` (a file counts itself).
pub fn tree_stats(path: &Path) -> std::io::Result<TreeStats> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_dir() {
        return Ok(TreeStats { files: 1, bytes: meta.len(), last_written: changed_secs(&meta) });
    }
    let mut t = TreeStats { files: 0, bytes: 0, last_written: changed_secs(&meta) };
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let m = entry.metadata()?; // DirEntry::metadata doesn't follow symlinks
            t.last_written = t.last_written.max(changed_secs(&m));
            if m.is_dir() {
                stack.push(entry.path());
            } else {
                t.files += 1;
                t.bytes += m.len();
            }
        }
    }
    Ok(t)
}

/// Files and bytes under `path` ([`tree_stats`] without the time).
pub fn tree_size(path: &Path) -> std::io::Result<(u64, u64)> {
    tree_stats(path).map(|t| (t.files, t.bytes))
}

/// The warning for a backup that a pull wrote into AFTER the pod's container was created (its
/// disk last reset), when an older snapshot was finished before that: the */15 pull of a wiped
/// pod overwrites the snapshot's copies of the work with the reset pod's files (they differ, so
/// rsync sends them; nothing is kept), so restoring it can put back the image instead of the
/// work — and say "restored". `label`/`written` = the chosen backup and its last write;
/// `container` = the pod's container creation (probe); `earlier` = the newest snapshot label
/// last written before that (see [`snapshot_written_before`]). `None` = nothing to say (no
/// reset since the oldest backup, or no older snapshot to suggest).
pub fn clobber_warning(label: &str, written: Option<u64>, container: Option<u64>, earlier: Option<&str>) -> Option<String> {
    let (written, container, earlier) = (written?, container?, earlier?);
    if written <= container || earlier == label {
        return None;
    }
    Some(format!(
        "{label} was last written {} — after the pod's container was created ({}, i.e. its disk last reset): if the \
         pod lost its files then, pulls since may have replaced the work in {label} with the reset pod's own files. \
         The newest snapshot from before that is {earlier} (--from {earlier})",
        utc(written),
        utc(container),
    ))
}

/// The newest `wNdM` snapshot of `pod` (newest first, as [`snapshot_labels`]) whose backup a
/// pull last wrote before `t` (UNIX seconds) — the last one that can't hold a post-reset pull.
/// Stops at the first such label (only that far is walked).
pub fn snapshot_written_before(base: &Path, pod: &str, t: u64) -> Option<String> {
    snapshot_labels(base, pod).into_iter().find(|l| {
        tree_stats(&base.join(l).join(pod)).ok().and_then(|s| s.last_written).is_some_and(|w| w <= t)
    })
}

/// `YYYY-MM-DD HH:MM UTC` for a UNIX time (for messages).
fn utc(unix_secs: u64) -> String {
    let s = stamp(unix_secs);
    format!("{}-{}-{} {}:{} UTC", &s[0..4], &s[4..6], &s[6..8], &s[9..11], &s[11..13])
}

/// `YYYYMMDDTHHMMSSZ` for a UNIX time — names the run's kept-files dir.
pub fn stamp(unix_secs: u64) -> String {
    let (days, rem) = ((unix_secs / 86_400) as i64, unix_secs % 86_400);
    let (y, m, d) = crate::schedule::civil_from_days(days);
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Where the files a restore replaces are kept on the pod (rsync `--backup-dir`): on the
/// volume when it's mounted there (an absolute path — a restart can't wipe it, and the repo's
/// files stay on the volume they came from), else in the home (relative to it). `stamp` names
/// the run.
pub fn kept_dir(volume_mounted: bool, stamp: &str) -> String {
    if volume_mounted {
        format!("{}/{KEPT_DIR}/{stamp}", crate::provider::runpod_v2::VOLUME_MOUNT_PATH)
    } else {
        format!("{KEPT_DIR}/{stamp}")
    }
}

/// [`kept_dir`] as a prompt names it (`~/…` for the home).
pub fn kept_dir_display(kept: &str) -> String {
    if kept.starts_with('/') {
        format!("{kept}/")
    } else {
        format!("~/{kept}/")
    }
}

/// The knobs of one restore's argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreFlags {
    /// [`kept_dir`].
    pub kept: String,
    /// Replace files on the pod that are newer than the backup's (`--overwrite-newer`); off =
    /// rsync `--update`.
    pub overwrite_newer: bool,
    /// Push `.git/` dirs (always from the big tier; from a snapshot only with `--with-git`).
    pub git: bool,
}

/// The rsync argv (after the program name) that pushes the backup at `src` — or only
/// `subpath` inside it, recreating its parents (`--relative` from `src/./`) — into
/// `target`'s home over its direct endpoint, as `flags` say. See the module doc for every
/// flag's reason.
pub fn restore_args(target: &SshTarget, src: &Path, subpath: Option<&str>, flags: &RestoreFlags) -> Vec<String> {
    let mut a: Vec<String> = [
        "-az",
        "--keep-dirlinks",
        "--no-owner",
        "--no-group",
        "--chmod=go-w",
        "--backup",
    ]
    .map(String::from)
    .into();
    a.push(format!("--backup-dir={}", flags.kept));
    if !flags.overwrite_newer {
        a.push("--update".into());
    }
    a.push(format!("--timeout={IO_TIMEOUT_SECS}"));
    a.push("--human-readable".into());
    a.push("--stats".into());
    a.push("--info=skip1".into());
    for ex in EXCLUDES {
        a.push("--exclude".into());
        a.push(ex.to_string());
    }
    if !flags.git {
        a.push("--exclude".into());
        a.push(".git/".into());
    }
    let src = src.display().to_string();
    let src = src.trim_end_matches('/');
    let source = match subpath {
        None => format!("{src}/"),
        Some(p) => {
            a.push("--relative".into());
            format!("{src}/./{p}")
        }
    };
    a.push("-e".into());
    a.push(target.rsh_command());
    a.push(source);
    a.push(format!("{}@{}:", target.user, target.host));
    a
}

/// The files the pod had NEWER than the backup, which `--update` left alone: rsync's
/// `--info=skip1` lines `<path> is newer`.
pub fn newer_on_pod(stdout: &str) -> Vec<String> {
    stdout.lines().filter_map(|l| l.trim_end_matches('\r').strip_suffix(" is newer")).map(String::from).collect()
}

/// Does this restore push `<rel>/.git` (the repo's, `rel` home-relative) — i.e. should the repo
/// be checked afterwards? `src` = the backup, `subpath` its `--path`, `git` = .git pushed at all.
pub fn pushes_repo_git(src: &Path, subpath: Option<&str>, rel: &str, git: bool) -> bool {
    let inside = |p: &str| rel == p || rel.starts_with(&format!("{p}/")) || p == format!("{rel}/.git") || p.starts_with(&format!("{rel}/.git/"));
    git && subpath.map_or(true, inside) && src.join(rel).join(".git").is_dir()
}

/// The read-only check after a restore that pushed the repo's `.git`: git can still resolve
/// HEAD and every ref's objects are there. Prints `arena-git=ok` or `arena-git=broken`.
pub fn git_check_command(repo: &str) -> String {
    let r = crate::setup::shell_quote(repo.trim_end_matches('/'));
    format!(
        "if git -C {r} rev-parse -q --verify HEAD >/dev/null 2>&1 && git -C {r} fsck --connectivity-only --no-progress >/dev/null 2>&1; \
         then echo arena-git=ok; else echo arena-git=broken; fi"
    )
}

/// [`git_check_command`]'s verdict: `Some(true)` ok, `Some(false)` broken, `None` no answer.
pub fn parse_git_check(stdout: &str) -> Option<bool> {
    stdout.lines().map(|l| l.trim_end_matches('\r')).find_map(|l| match l {
        "arena-git=ok" => Some(true),
        "arena-git=broken" => Some(false),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> SshTarget {
        SshTarget {
            user: "root".into(),
            host: "1.2.3.4".into(),
            port: 22001,
            key_paths: vec!["/k/devtest_key".into()],
            connect_timeout_secs: 10,
        }
    }

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tmp(tag: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("arena-restore-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }
    fn put(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn labels_parse_and_sort_by_week_then_day() {
        for (l, want) in [("w1d3", Some((1, 3))), ("w10d1", Some((10, 1))), ("w0d0", Some((0, 0))), ("big", None), ("w1", None), ("wxd1", None), ("w1d", None), ("w1d3x", None), ("w-1d2", None)] {
            assert_eq!(parse_label(l), want, "{l}");
        }
        let t = tmp("labels");
        for l in ["w9d5", "w10d1", "w2d3", "w10d0"] {
            put(&t.0, &format!("{l}/devtest-a/notes.txt"), "x");
        }
        put(&t.0, "custom/devtest-a/notes.txt", "x"); // not a wNdM label: never the default
        put(&t.0, "big/devtest-a/notes.txt", "x");
        std::fs::create_dir_all(t.0.join("w11d1/devtest-a")).unwrap(); // empty: not a backup
        put(&t.0, "w12d1/devtest-b/notes.txt", "x"); // another pod's
        assert_eq!(snapshot_labels(&t.0, "devtest-a"), ["w10d1", "w10d0", "w9d5", "w2d3"]);
        assert!(snapshot_labels(&t.0.join("nope"), "devtest-a").is_empty());
    }

    #[test]
    fn source_resolution_and_refusals() {
        let t = tmp("source");
        put(&t.0, "w1d1/devtest-a/notes.txt", "x");
        put(&t.0, "w1d2/devtest-a/notes.txt", "x");
        put(&t.0, "big/devtest-a/big.bin", "x");
        put(&t.0, "m2test/devtest-a/ARENA_materials/m2.txt", "x"); // a `pods pull --label m2test`
        std::fs::create_dir_all(t.0.join("w1d3/devtest-a")).unwrap();
        put(&t.0, "big/devtest-b/x", "x");
        put(&t.0, "m2test/devtest-b/x", "x");
        let ok = |from: Option<&str>| resolve_source(&t.0, "devtest-a", from).unwrap();
        assert_eq!(ok(None), ("w1d2".to_string(), t.0.join("w1d2/devtest-a")), "the newest snapshot");
        assert_eq!(ok(Some("w1d1")).1, t.0.join("w1d1/devtest-a"));
        assert_eq!(ok(Some("big")).1, t.0.join("big/devtest-a"));
        assert_eq!(ok(Some("m2test")).1, t.0.join("m2test/devtest-a"), "a custom label, named");
        let err = |pod: &str, from: Option<&str>| resolve_source(&t.0, pod, from).unwrap_err();
        // Every backup there is gets listed — custom labels too.
        let e = err("devtest-a", Some("w1d3"));
        assert!(e.contains("is empty") && e.contains("backups of devtest-a: w1d2, w1d1, m2test, big"), "{e}");
        let e = err("devtest-a", Some("w9d9"));
        assert!(e.contains("doesn't exist") && e.contains("w1d2, w1d1, m2test, big"), "{e}");
        // No wNdM snapshot: nothing to default to — says what there is and what to pass.
        let e = err("devtest-b", None);
        assert!(e.contains("no wNdM snapshot of devtest-b") && e.contains("backups of devtest-b: m2test, big") && e.contains("--from"), "{e}");
        let e = err("devtest-z", None);
        assert!(e.contains("there is no backup of devtest-z"), "{e}");
        for bad in ["", "..", ".", "w1d1/../x", "/etc"] {
            assert!(err("devtest-a", Some(bad)).contains("isn't a backup label"), "{bad}");
        }
        let e = resolve_source(&t.0.join("missing"), "devtest-a", None).unwrap_err();
        assert!(e.contains("doesn't exist") && e.contains("--dir"), "{e}");
        assert_eq!(all_labels(&t.0, "devtest-a"), ["w1d2", "w1d1", "m2test", "big"]);
    }

    #[test]
    fn subpaths_must_stay_inside_the_backup() {
        assert_eq!(check_subpath("ARENA_materials/chapter1/").unwrap(), "ARENA_materials/chapter1");
        assert_eq!(check_subpath("notes.txt").unwrap(), "notes.txt");
        for bad in ["", "/", "/root/x", "../x", "a/../b", "a//b", "./a", "a/."] {
            assert!(check_subpath(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn sizes_and_stamps() {
        let t = tmp("size");
        put(&t.0, "a/b/c.txt", "12345");
        put(&t.0, "d.txt", "123");
        #[cfg(unix)]
        std::os::unix::fs::symlink("/nonexistent/far/away", t.0.join("link")).unwrap();
        let (files, bytes) = tree_size(&t.0).unwrap();
        assert_eq!(files, if cfg!(unix) { 3 } else { 2 });
        assert!(bytes >= 8, "{bytes}");
        assert_eq!(tree_size(&t.0.join("d.txt")).unwrap(), (1, 3));
        assert!(tree_size(&t.0.join("missing")).is_err());
        // When a pull last wrote into it: just now.
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let written = tree_stats(&t.0).unwrap().last_written.unwrap();
        assert!(written.abs_diff(now) <= 5, "{written} vs {now}");
        assert_eq!(stamp(0), "19700101T000000Z");
        assert_eq!(stamp(1_791_400_000), "20261007T190640Z");
        assert_eq!(utc(1_791_400_000), "2026-10-07 19:06 UTC");
    }

    /// A snapshot a pull wrote into after the pod's disk was reset may hold the reset pod's
    /// files instead of the work: said, with the newest snapshot from before the reset.
    #[test]
    fn a_snapshot_written_after_the_reset_is_flagged_with_the_one_before_it() {
        const RESET: u64 = 1_791_400_000;
        // (label, last written, container created, earlier snapshot, warns)
        let cases: &[(&str, Option<u64>, Option<u64>, Option<&str>, bool)] = &[
            ("w1d2", Some(RESET + 900), Some(RESET), Some("w1d1"), true),
            ("big", Some(RESET + 900), Some(RESET), Some("w1d2"), true),
            // written before the reset: it holds the work
            ("w1d2", Some(RESET - 60), Some(RESET), Some("w1d2"), false),
            // no snapshot from before the reset (the pod is older than every backup): no evidence
            ("w1d2", Some(RESET + 900), Some(RESET), None, false),
            // the pod didn't say when its container was created
            ("w1d2", Some(RESET + 900), None, Some("w1d1"), false),
            ("w1d2", None, Some(RESET), Some("w1d1"), false),
        ];
        for (label, written, container, earlier, warns) in cases {
            let w = clobber_warning(label, *written, *container, *earlier);
            assert_eq!(w.is_some(), *warns, "{label} {written:?} {container:?} {earlier:?}: {w:?}");
            if let (Some(w), Some(e)) = (w, earlier) {
                assert!(w.contains(&format!("--from {e}")) && w.contains("2026-10-07 19:06 UTC"), "{w}");
            }
        }
        // Which snapshot counts as "from before": the newest one whose last write is at or
        // before the time.
        let t = tmp("before");
        for l in ["w1d1", "w1d2"] {
            put(&t.0, &format!("{l}/devtest-a/notes.txt"), "x");
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(snapshot_written_before(&t.0, "devtest-a", now + 60).as_deref(), Some("w1d2"));
        assert_eq!(snapshot_written_before(&t.0, "devtest-a", 0), None);
    }

    fn flags(kept: &str) -> RestoreFlags {
        RestoreFlags { kept: kept.into(), overwrite_newer: false, git: false }
    }

    #[test]
    fn the_argv_never_deletes_keeps_what_it_replaces_and_writes_through_links() {
        let a = restore_args(&target(), Path::new("/b/w1d2/devtest-a/"), None, &flags(&kept_dir(false, "20261008T050640Z")));
        let has = |x: &str| a.iter().any(|y| y == x);
        for flag in ["-az", "--keep-dirlinks", "--no-owner", "--no-group", "--chmod=go-w", "--backup", "--update", "--timeout=300", "--stats", "--info=skip1"] {
            assert!(has(flag), "missing {flag}: {a:?}");
        }
        assert!(has("--backup-dir=.arena-restore/20261008T050640Z"), "{a:?}");
        assert!(!a.iter().any(|x| x.starts_with("--delete") || x == "--force" || x.starts_with("--remove")), "{a:?}");
        for ex in [".claude*", "/.ssh/", "/.bashrc", "/.zshrc", "/.name", "/.zsh_history", ".git/"] {
            assert!(a.windows(2).any(|w| w[0] == "--exclude" && w[1] == ex), "missing exclude {ex}");
        }
        // Over the pod's direct endpoint (port + key in the transport), into its home.
        let e = a.iter().position(|x| x == "-e").unwrap();
        assert!(a[e + 1].contains("-p 22001") && a[e + 1].contains("-i /k/devtest_key"), "{a:?}");
        assert_eq!(&a[a.len() - 2..], ["/b/w1d2/devtest-a/", "root@1.2.3.4:"]);
        assert!(!has("--relative"));
        // Only a subpath: recreated under the home from the backup's root (`/./`). With the
        // volume mounted the kept files go there; .git and newer files as asked.
        let f = RestoreFlags { kept: kept_dir(true, "S"), overwrite_newer: true, git: true };
        let a = restore_args(&target(), Path::new("/b/big/devtest-a"), Some("ARENA_materials/ch1"), &f);
        assert!(a.contains(&"--relative".to_string()));
        assert!(a.contains(&"--backup-dir=/workspace/.arena-restore/S".to_string()), "{a:?}");
        assert!(!a.contains(&"--update".to_string()) && !a.windows(2).any(|w| w[1] == ".git/"), "{a:?}");
        assert_eq!(&a[a.len() - 2..], ["/b/big/devtest-a/./ARENA_materials/ch1", "root@1.2.3.4:"]);
        assert_eq!(kept_dir_display(&kept_dir(true, "S")), "/workspace/.arena-restore/S/");
        assert_eq!(kept_dir_display(&kept_dir(false, "S")), "~/.arena-restore/S/");
        assert_eq!(newer_on_pod("ARENA_materials/w.py is newer\r\nsent 1 bytes\nnotes.txt is newer\n"), ["ARENA_materials/w.py", "notes.txt"]);
        // The repo check, and when it's due.
        let c = git_check_command("/root/ARENA_materials/");
        assert!(c.contains("git -C '/root/ARENA_materials' fsck --connectivity-only") && c.contains("arena-git=broken"), "{c}");
        for (out, want) in [("arena-git=ok\n", Some(true)), ("noise\r\narena-git=broken\r\n", Some(false)), ("", None)] {
            assert_eq!(parse_git_check(out), want, "{out:?}");
        }
        let t = tmp("pushes-git");
        std::fs::create_dir_all(t.0.join("ARENA_materials/.git")).unwrap();
        // (subpath, .git pushed at all, check)
        for (sub, git, want) in [
            (None, true, true),
            (None, false, false),
            (Some("ARENA_materials"), true, true),
            (Some("ARENA_materials/.git/refs"), true, true),
            (Some("ARENA_materials/chapter1"), true, false),
            (Some("notes.txt"), true, false),
        ] {
            assert_eq!(pushes_repo_git(&t.0, sub, "ARENA_materials", git), want, "{sub:?} {git}");
        }
        assert!(!pushes_repo_git(&t.0.join("nope"), None, "ARENA_materials", true), "no .git in the backup");
    }

    /// Set a file's mtime `secs_ago` seconds back (the image's files, a backup's copy).
    fn age(path: &Path, secs_ago: u64) {
        let t = std::time::SystemTime::now() - Duration::from_secs(secs_ago);
        std::fs::File::options().write(true).open(path).unwrap().set_modified(t).unwrap();
    }

    /// Run the real rsync with `args` against the "pod" `home`; its stdout.
    #[cfg(unix)]
    fn rsync_stdout(args: &[String], home: &Path) -> String {
        let out = std::process::Command::new("rsync").args(args).env("FAKE_HOME", home).output().unwrap();
        assert!(out.status.success(), "rsync {args:?}:\n{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// The argv, run by the REAL rsync into a temp-dir "pod" (the transport swapped for a fake
    /// that runs rsync's server side locally): the pod's repo is a symlink onto its volume.
    #[cfg(unix)]
    #[test]
    fn a_restore_lands_in_the_volume_copy_through_the_link_and_deletes_nothing() {
        use crate::volume::fake_ssh;
        if !fake_ssh::have_rsync() {
            eprintln!("rsync not installed — skipping");
            return;
        }
        let t = tmp("real");
        let fake = fake_ssh::install(&t.0);
        // The backup (pull's layout): the repo tree, a notebook, keys/rc files that must not go back.
        let bk = t.0.join("backup/big/devtest-a");
        put(&bk, "ARENA_materials/chapter1/work.py", "mine, newer\n"); // a different size: rsync sends it
        put(&bk, "ARENA_materials/.git/HEAD", "ref: refs/heads/autocommit\n");
        put(&bk, "notes.txt", "notes\n");
        put(&bk, ".bashrc", "export OPENAI_API_KEY=old\n");
        put(&bk, ".claude/projects/x.jsonl", "{}\n");
        put(&bk, ".claude.json", "{}\n");
        put(&bk, ".name", "export MACHINE_NAME='old'\n");
        // The pod after a reset + setup: home with the repo linked onto the volume, the image's
        // files on the volume copy (older than the backup), and a file the backup doesn't know.
        let (home, ws) = (t.0.join("pod/root"), t.0.join("pod/workspace"));
        let vol = ws.join("ARENA_materials");
        put(&vol, "chapter1/work.py", "image\n");
        age(&vol.join("chapter1/work.py"), 30 * 86_400);
        put(&vol, "chapter1/only_on_pod.py", "keep me\n");
        put(&home, ".bashrc", "export OPENAI_API_KEY=current\n");
        put(&home, ".name", "export MACHINE_NAME='a'\n");
        std::os::unix::fs::symlink(&vol, home.join("ARENA_materials")).unwrap();

        // With the volume mounted, the replaced files are kept ON it (here: the fake volume).
        let kept = ws.join(".arena-restore/S1");
        let f = RestoreFlags { kept: kept.display().to_string(), overwrite_newer: false, git: true };
        let args = restore_args(&target(), &bk, None, &f);
        let out = rsync_stdout(&fake_ssh::swap_transport(&args, &fake), &home);
        assert!(newer_on_pod(&out).is_empty(), "{out}");
        let read = |p: &Path| std::fs::read_to_string(p).unwrap();
        // Written THROUGH the link into the volume copy; the link is still a link.
        assert!(std::fs::symlink_metadata(home.join("ARENA_materials")).unwrap().file_type().is_symlink());
        assert_eq!(read(&vol.join("chapter1/work.py")), "mine, newer\n");
        assert_eq!(read(&vol.join(".git/HEAD")), "ref: refs/heads/autocommit\n");
        assert_eq!(read(&home.join("notes.txt")), "notes\n");
        // Nothing deleted; what it replaced is kept — on the volume, not the container disk.
        assert_eq!(read(&vol.join("chapter1/only_on_pod.py")), "keep me\n");
        assert_eq!(read(&kept.join("ARENA_materials/chapter1/work.py")), "image\n");
        assert!(!home.join(".arena-restore").exists(), "nothing kept in the home");
        // The pod's own keys, name and Claude state untouched / not pushed.
        assert_eq!(read(&home.join(".bashrc")), "export OPENAI_API_KEY=current\n");
        assert_eq!(read(&home.join(".name")), "export MACHINE_NAME='a'\n");
        assert!(!home.join(".claude").exists() && !home.join(".claude.json").exists());

        // Only a subpath, onto a pod without the volume link and without the parents yet.
        let fresh = t.0.join("pod2/root");
        std::fs::create_dir_all(&fresh).unwrap();
        let args = restore_args(&target(), &bk, Some("ARENA_materials/chapter1"), &flags(&kept_dir(false, "S2")));
        fake_ssh::rsync(&fake_ssh::swap_transport(&args, &fake), &fresh);
        assert_eq!(read(&fresh.join("ARENA_materials/chapter1/work.py")), "mine, newer\n");
        assert!(!fresh.join("notes.txt").exists() && !fresh.join("ARENA_materials/.git").exists(), "only the subpath");
    }

    /// A restore of an older backup over a pod that has moved on: the pod's newer files are
    /// left alone (and reported), unless asked — then replaced, the newer copy kept.
    #[cfg(unix)]
    #[test]
    fn newer_work_on_the_pod_is_never_reverted_unless_asked() {
        use crate::volume::fake_ssh;
        if !fake_ssh::have_rsync() {
            eprintln!("rsync not installed — skipping");
            return;
        }
        let t = tmp("newer");
        let fake = fake_ssh::install(&t.0);
        let bk = t.0.join("backup/w1d2/devtest-a");
        put(&bk, "ARENA_materials/work.py", "old (backup, 3h ago)\n");
        age(&bk.join("ARENA_materials/work.py"), 3 * 3600);
        put(&bk, "ARENA_materials/deleted.py", "lost on the pod\n");
        let (home, ws) = (t.0.join("pod/root"), t.0.join("pod/workspace"));
        put(&ws, "ARENA_materials/work.py", "newer, on the pod\n");
        std::fs::create_dir_all(&home).unwrap();
        std::os::unix::fs::symlink(ws.join("ARENA_materials"), home.join("ARENA_materials")).unwrap();
        let kept = ws.join(".arena-restore/S1");
        let f = RestoreFlags { kept: kept.display().to_string(), overwrite_newer: false, git: false };
        let out = rsync_stdout(&fake_ssh::swap_transport(&restore_args(&target(), &bk, None, &f), &fake), &home);
        let read = |p: &Path| std::fs::read_to_string(p).unwrap();
        assert_eq!(read(&ws.join("ARENA_materials/work.py")), "newer, on the pod\n", "not reverted");
        assert_eq!(read(&ws.join("ARENA_materials/deleted.py")), "lost on the pod\n", "the lost file is back");
        assert_eq!(newer_on_pod(&out), ["ARENA_materials/work.py"], "{out}");
        assert!(!kept.join("ARENA_materials/work.py").exists());
        // --overwrite-newer: replaced, and the newer copy kept on the volume.
        let f = RestoreFlags { overwrite_newer: true, kept: ws.join(".arena-restore/S2").display().to_string(), ..f };
        rsync_stdout(&fake_ssh::swap_transport(&restore_args(&target(), &bk, None, &f), &fake), &home);
        assert_eq!(read(&ws.join("ARENA_materials/work.py")), "old (backup, 3h ago)\n");
        assert_eq!(read(&ws.join(".arena-restore/S2/ARENA_materials/work.py")), "newer, on the pod\n");
    }

    /// The snapshot tier drops files over its size cap — a git pack among them. Pushing its
    /// `.git` over the pod's leaves refs to objects that never arrived: `fatal: bad object HEAD`.
    /// A snapshot restore leaves `.git` out by default; the check catches a broken one.
    #[cfg(unix)]
    #[test]
    fn a_snapshot_never_breaks_the_pods_repo_by_default() {
        use crate::volume::fake_ssh;
        use std::process::Command;
        if !fake_ssh::have_rsync() || Command::new("git").arg("--version").output().is_err() {
            eprintln!("rsync/git not installed — skipping");
            return;
        }
        let t = tmp("git");
        let git = |dir: &Path, args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .env("HOME", &t.0)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        // The image's repo, then the participant's commit and a gc on the pod.
        let image = t.0.join("image");
        put(&image, "work.py", "image\n");
        git(&image, &["init", "-q", "-b", "main"]);
        git(&image, &["add", "-A"]);
        git(&image, &["commit", "-qm", "image"]);
        let pod_then = t.0.join("pod-then");
        git(&t.0, &["clone", "-q", image.to_str().unwrap(), pod_then.to_str().unwrap()]);
        put(&pod_then, "work.py", "mine\n");
        git(&pod_then, &["commit", "-qam", "work"]);
        git(&pod_then, &["gc", "-q"]);
        // The snapshot: every file except the pack (over the cap).
        let bk = t.0.join("backup/w1d2/devtest-a");
        std::fs::create_dir_all(&bk).unwrap();
        let out = Command::new("rsync")
            .args(["-a", "--exclude", "*.pack"])
            .arg(format!("{}/", pod_then.display()))
            .arg(bk.join("ARENA_materials"))
            .output()
            .unwrap();
        assert!(out.status.success());
        // The pod after a reset: the image's clone again.
        let home = t.0.join("pod/root");
        std::fs::create_dir_all(&home).unwrap();
        let repo = home.join("ARENA_materials");
        git(&t.0, &["clone", "-q", image.to_str().unwrap(), repo.to_str().unwrap()]);
        // …from the image, so older than the backup (`--update` only skips what's newer).
        let aged = Command::new("find").arg(&repo).args(["-exec", "touch", "-h", "-d", "@1700000000", "{}", "+"]).status().unwrap();
        assert!(aged.success());
        let fake = fake_ssh::install(&t.0);
        let check = |repo: &Path| {
            let out = Command::new("sh").arg("-c").arg(git_check_command(&repo.display().to_string())).env("HOME", &t.0).env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null").output().unwrap();
            parse_git_check(&String::from_utf8_lossy(&out.stdout))
        };
        assert_eq!(check(&repo), Some(true));
        // Default (no .git from a snapshot): the work is back, the repo still reads.
        let args = restore_args(&target(), &bk, None, &flags(".arena-restore/S1"));
        fake_ssh::rsync(&fake_ssh::swap_transport(&args, &fake), &home);
        assert_eq!(std::fs::read_to_string(repo.join("work.py")).unwrap(), "mine\n");
        assert_eq!(check(&repo), Some(true), "the pod's own .git untouched");
        assert!(pushes_repo_git(&bk, None, "ARENA_materials", true) && !pushes_repo_git(&bk, None, "ARENA_materials", false));
        // With the snapshot's .git (--with-git): refs to a pack that never came — the check says so.
        let f = RestoreFlags { git: true, ..flags(".arena-restore/S2") };
        fake_ssh::rsync(&fake_ssh::swap_transport(&restore_args(&target(), &bk, None, &f), &fake), &home);
        assert_eq!(check(&repo), Some(false), "a broken repo is caught");
    }
}
