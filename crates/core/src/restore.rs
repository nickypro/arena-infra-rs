//! `pods restore`: push a local backup — what `pods pull` saved — back onto a pod.
//!
//! Why: the file backups exist to get participants' work back after a pod lost it (a RunPod
//! restart/stop resets the container disk) or was replaced. This module is the pure part:
//! which backup (`<dir>/<label>/<pod>/` or `<dir>/big/<pod>/`, pull's layout), which path in
//! it, and the rsync argv. The CLI resolves the pod, confirms, and runs it with a budget.
//!
//! The argv is safe by construction:
//! - never `--delete` (nor `--force`): whatever is on the pod and not in the backup stays;
//! - a file the restore replaces is kept, under `~/.arena-restore/<UTC stamp>/` (rsync
//!   `--backup-dir`), so an older backup can't silently clobber newer work;
//! - `--keep-dirlinks`: a symlink to a directory on the pod — the ARENA repo linked onto the
//!   `/workspace` volume by setup — is written THROUGH, so the work lands in the volume copy.
//!   Without it rsync replaces the link with a fresh directory on the container disk (verified
//!   locally): the next restart would wipe what was just restored;
//! - `--no-owner --no-group --chmod=go-w`: the pod's home keeps root's ownership and never
//!   turns group/world-writable — either would make sshd's StrictModes refuse every key;
//! - `.ssh/`, the shell rc files and histories, `~/.name` and `.claude*` are not pushed: keys,
//!   the per-pod API keys in the rc files, the machine name and Claude Code state belong to the
//!   pod (setup / copy-keys write them) — the same line `pods replace`'s copy draws;
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

/// Where on the pod (relative to the home) the files a restore replaced are kept.
pub const KEPT_DIR: &str = ".arena-restore";

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
        let mut v = labels.clone();
        if non_empty_dir(&base.join(BIG).join(pod)) {
            v.push(BIG.to_string());
        }
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

/// Files and bytes under `path` (a file counts itself); symlinks are counted, never followed.
pub fn tree_size(path: &Path) -> std::io::Result<(u64, u64)> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_dir() {
        return Ok((1, meta.len()));
    }
    let (mut files, mut bytes) = (0, 0);
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let m = entry.metadata()?; // DirEntry::metadata doesn't follow symlinks
            if m.is_dir() {
                stack.push(entry.path());
            } else {
                files += 1;
                bytes += m.len();
            }
        }
    }
    Ok((files, bytes))
}

/// `YYYYMMDDTHHMMSSZ` for a UNIX time — names the run's kept-files dir.
pub fn stamp(unix_secs: u64) -> String {
    let (days, rem) = ((unix_secs / 86_400) as i64, unix_secs % 86_400);
    let (y, m, d) = crate::schedule::civil_from_days(days);
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// The rsync argv (after the program name) that pushes the backup at `src` — or only
/// `subpath` inside it, recreating its parents (`--relative` from `src/./`) — into
/// `target`'s home over its direct endpoint. Files it replaces are kept under
/// `~/.arena-restore/<stamp>/`. See the module doc for every flag's reason.
pub fn restore_args(target: &SshTarget, src: &Path, subpath: Option<&str>, stamp: &str) -> Vec<String> {
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
    a.push(format!("--backup-dir={KEPT_DIR}/{stamp}"));
    a.push(format!("--timeout={IO_TIMEOUT_SECS}"));
    a.push("--human-readable".into());
    a.push("--stats".into());
    for ex in EXCLUDES {
        a.push("--exclude".into());
        a.push(ex.to_string());
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
        std::fs::create_dir_all(t.0.join("w1d3/devtest-a")).unwrap();
        put(&t.0, "big/devtest-b/x", "x");
        let ok = |from: Option<&str>| resolve_source(&t.0, "devtest-a", from).unwrap();
        assert_eq!(ok(None), ("w1d2".to_string(), t.0.join("w1d2/devtest-a")), "the newest snapshot");
        assert_eq!(ok(Some("w1d1")).1, t.0.join("w1d1/devtest-a"));
        assert_eq!(ok(Some("big")).1, t.0.join("big/devtest-a"));
        let err = |pod: &str, from: Option<&str>| resolve_source(&t.0, pod, from).unwrap_err();
        let e = err("devtest-a", Some("w1d3"));
        assert!(e.contains("is empty") && e.contains("backups of devtest-a: w1d2, w1d1, big"), "{e}");
        let e = err("devtest-a", Some("w9d9"));
        assert!(e.contains("doesn't exist") && e.contains("w1d2, w1d1, big"), "{e}");
        // Only a big tier: no snapshot to default to — says so and what to pass.
        let e = err("devtest-b", None);
        assert!(e.contains("no wNdM snapshot of devtest-b") && e.contains("backups of devtest-b: big") && e.contains("--from"), "{e}");
        let e = err("devtest-z", None);
        assert!(e.contains("there is no backup of devtest-z"), "{e}");
        for bad in ["", "..", ".", "w1d1/../x", "/etc"] {
            assert!(err("devtest-a", Some(bad)).contains("isn't a backup label"), "{bad}");
        }
        let e = resolve_source(&t.0.join("missing"), "devtest-a", None).unwrap_err();
        assert!(e.contains("doesn't exist") && e.contains("--dir"), "{e}");
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
        assert_eq!(stamp(0), "19700101T000000Z");
        assert_eq!(stamp(1_791_400_000), "20261007T190640Z");
    }

    #[test]
    fn the_argv_never_deletes_keeps_what_it_replaces_and_writes_through_links() {
        let a = restore_args(&target(), Path::new("/b/w1d2/devtest-a/"), None, "20261008T050640Z");
        let has = |x: &str| a.iter().any(|y| y == x);
        for flag in ["-az", "--keep-dirlinks", "--no-owner", "--no-group", "--chmod=go-w", "--backup", "--timeout=300", "--stats"] {
            assert!(has(flag), "missing {flag}: {a:?}");
        }
        assert!(has("--backup-dir=.arena-restore/20261008T050640Z"), "{a:?}");
        assert!(!a.iter().any(|x| x.starts_with("--delete") || x == "--force" || x.starts_with("--remove")), "{a:?}");
        for ex in [".claude*", "/.ssh/", "/.bashrc", "/.zshrc", "/.name", "/.zsh_history"] {
            assert!(a.windows(2).any(|w| w[0] == "--exclude" && w[1] == ex), "missing exclude {ex}");
        }
        // Over the pod's direct endpoint (port + key in the transport), into its home.
        let e = a.iter().position(|x| x == "-e").unwrap();
        assert!(a[e + 1].contains("-p 22001") && a[e + 1].contains("-i /k/devtest_key"), "{a:?}");
        assert_eq!(&a[a.len() - 2..], ["/b/w1d2/devtest-a/", "root@1.2.3.4:"]);
        assert!(!has("--relative"));
        // Only a subpath: recreated under the home from the backup's root (`/./`).
        let a = restore_args(&target(), Path::new("/b/big/devtest-a"), Some("ARENA_materials/ch1"), "S");
        assert!(a.contains(&"--relative".to_string()));
        assert_eq!(&a[a.len() - 2..], ["/b/big/devtest-a/./ARENA_materials/ch1", "root@1.2.3.4:"]);
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
        let bk = t.0.join("backup/w1d2/devtest-a");
        put(&bk, "ARENA_materials/chapter1/work.py", "mine, newer\n"); // a different size: rsync sends it
        put(&bk, "ARENA_materials/.git/HEAD", "ref: refs/heads/autocommit\n");
        put(&bk, "notes.txt", "notes\n");
        put(&bk, ".bashrc", "export OPENAI_API_KEY=old\n");
        put(&bk, ".claude/projects/x.jsonl", "{}\n");
        put(&bk, ".claude.json", "{}\n");
        put(&bk, ".name", "export MACHINE_NAME='old'\n");
        // The pod after a reset + setup: home with the repo linked onto the volume, the image's
        // files on the volume copy, and a file the backup doesn't know.
        let (home, vol) = (t.0.join("pod/root"), t.0.join("pod/workspace/ARENA_materials"));
        put(&vol, "chapter1/work.py", "image\n");
        put(&vol, "chapter1/only_on_pod.py", "keep me\n");
        put(&home, ".bashrc", "export OPENAI_API_KEY=current\n");
        put(&home, ".name", "export MACHINE_NAME='a'\n");
        std::os::unix::fs::symlink(&vol, home.join("ARENA_materials")).unwrap();

        let args = restore_args(&target(), &bk, None, "S1");
        fake_ssh::rsync(&fake_ssh::swap_transport(&args, &fake), &home);
        let read = |p: &Path| std::fs::read_to_string(p).unwrap();
        // Written THROUGH the link into the volume copy; the link is still a link.
        assert!(std::fs::symlink_metadata(home.join("ARENA_materials")).unwrap().file_type().is_symlink());
        assert_eq!(read(&vol.join("chapter1/work.py")), "mine, newer\n");
        assert_eq!(read(&vol.join(".git/HEAD")), "ref: refs/heads/autocommit\n");
        assert_eq!(read(&home.join("notes.txt")), "notes\n");
        // Nothing deleted; what it replaced is kept.
        assert_eq!(read(&vol.join("chapter1/only_on_pod.py")), "keep me\n");
        assert_eq!(read(&home.join(".arena-restore/S1/ARENA_materials/chapter1/work.py")), "image\n");
        // The pod's own keys, name and Claude state untouched / not pushed.
        assert_eq!(read(&home.join(".bashrc")), "export OPENAI_API_KEY=current\n");
        assert_eq!(read(&home.join(".name")), "export MACHINE_NAME='a'\n");
        assert!(!home.join(".claude").exists() && !home.join(".claude.json").exists());

        // Only a subpath, onto a pod without the volume link and without the parents yet.
        let fresh = t.0.join("pod2/root");
        std::fs::create_dir_all(&fresh).unwrap();
        let args = restore_args(&target(), &bk, Some("ARENA_materials/chapter1"), "S2");
        fake_ssh::rsync(&fake_ssh::swap_transport(&args, &fake), &fresh);
        assert_eq!(read(&fresh.join("ARENA_materials/chapter1/work.py")), "mine, newer\n");
        assert!(!fresh.join("notes.txt").exists() && !fresh.join("ARENA_materials/.git").exists(), "only the subpath");
    }
}
