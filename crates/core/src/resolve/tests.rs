//! The look, the resolve and the decisions around them: the pure parts by table; the scripts
//! against real git — a participant's checkout left mid-merge, mid-cherry-pick, mid-revert, with a
//! `git stash pop`'s conflicts, mid-`git am`/rebase/sequence — its files made old with `touch -d`.

use super::*;
use crate::backup::{backup_command_limited, parse_backup_report, BackupReport, BACKUP_BUSY_EXIT, BACKUP_PUSHED};
use std::path::{Path, PathBuf};

const HOUR: u64 = 3600;
const DAY: u64 = 24 * HOUR;

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

/// A look at `now` of a merge that started at `since`, its newest file at `newest`.
fn look_at(now: u64, since: Option<u64>, newest: u64, input: Option<u64>) -> Look {
    Look {
        now,
        top: "/root/ARENA_materials".into(),
        home: "/root".into(),
        what: "merge".into(),
        since,
        head: "abc123".into(),
        sequence: false,
        newest,
        files: 12,
        print: "111-222".into(),
        input,
    }
}

#[test]
fn the_setting_is_whole_hours_on_by_default_and_0_or_off_turns_it_off() {
    let cases: [(Option<&str>, Option<u64>); 9] = [
        (None, Some(3)),
        (Some(""), Some(3)),
        (Some("  "), Some(3)),
        (Some(" 5 "), Some(5)),
        (Some("1"), Some(1)),
        (Some("720"), Some(720)),
        (Some("0"), None),
        (Some("off"), None),
        (Some("OFF"), None),
    ];
    for (value, want) in cases {
        assert_eq!(auto_resolve_hours(value).unwrap(), want, "{value:?}");
    }
    for bad in ["721", "-1", "3h", "1.5", "soon", "on", "00x"] {
        let e = auto_resolve_hours(Some(bad)).unwrap_err().to_string();
        assert_eq!(
            e,
            format!(
                "config error: BUSY_AUTO_RESOLVE_HOURS must be a whole number of hours, 1..=720, or 0/off to never finish an \
                 operation left in progress (got `{bad}`)"
            )
        );
    }
    assert_eq!(auto_resolve_words(Some(3), false), "after 3h with nothing changed (default)");
    assert_eq!(auto_resolve_words(Some(5), true), "after 5h with nothing changed");
    assert_eq!(auto_resolve_words(None, true), "off — an operation left in progress stays until the participant ends it");
}

#[test]
fn a_look_is_read_back_and_an_incomplete_one_is_none() {
    let out = "NOW 1760900000\nTOP /root/ARENA_materials\nHOME /root\nOP merge 1760800000 0123abcd 0\nNEWEST 1760700000 42\n\
               PRINT 3735928559-1234\nINPUT 1760600000\n";
    let l = parse_look(out).unwrap();
    assert_eq!(
        l,
        Look {
            now: 1_760_900_000,
            top: "/root/ARENA_materials".into(),
            home: "/root".into(),
            what: "merge".into(),
            since: Some(1_760_800_000),
            head: "0123abcd".into(),
            sequence: false,
            newest: 1_760_700_000,
            files: 42,
            print: "3735928559-1234".into(),
            input: Some(1_760_600_000),
        }
    );
    assert_eq!(l.op_key(), "merge 1760800000 0123abcd");
    // A sequence kept beside a cherry-pick; conflicts with no stash; `git am` (no time); nothing.
    let seq = parse_look(&out.replace("OP merge 1760800000 0123abcd 0", "OP cherry-pick 1760800000 0123abcd 1")).unwrap();
    assert!(seq.sequence && seq.what == "cherry-pick");
    let stash = parse_look(&out.replace("OP merge 1760800000 0123abcd 0", "OP conflicts 1760800000 - 0")).unwrap();
    assert_eq!(stash.op_key(), "conflicts 1760800000 -");
    for what in ["am", "rebase", "none"] {
        let l = parse_look(&out.replace("OP merge 1760800000 0123abcd 0", &format!("OP {what}"))).unwrap();
        assert_eq!((l.what.as_str(), l.since, l.op_key()), (what, None, format!("{what} - -")));
    }
    assert_eq!(parse_look(&out.replace("INPUT 1760600000\n", "")).unwrap().input, None, "nobody logged in");
    // Any line it always prints missing or unreadable: no look.
    for broken in ["NOW 1760900000\n", "NEWEST 1760700000 42\n", "PRINT 3735928559-1234\n", "OP merge 1760800000 0123abcd 0\n"] {
        assert_eq!(parse_look(&out.replace(broken, "")), None, "{broken}");
    }
    assert_eq!(parse_look(&out.replace("OP merge 1760800000 0123abcd 0", "OP merge soon x 0")), None);
    assert_eq!(parse_look(&out.replace("NEWEST 1760700000 42", "NEWEST 1760700000")), None);
    assert_eq!(parse_look(""), None);
}

/// The quiet check: the operation, every file git lists and any terminal login, all older than
/// the threshold — on the pod's clock; a time in the future never counts as quiet.
#[test]
fn quiet_means_the_operation_the_files_and_any_login_older_than_the_threshold() {
    let n = 1_760_900_000;
    let old = n - 3 * HOUR;
    let cases: Vec<(Look, Option<&str>)> = vec![
        (look_at(n, Some(old), old, None), None),
        (look_at(n, Some(n - 10 * DAY), n - 4 * HOUR, Some(n - 5 * HOUR)), None),
        (look_at(n, Some(old + 1), old, None), Some("the merge started less than 3h ago")),
        (look_at(n, Some(old), n - 600, None), Some("a file changed less than 3h ago")),
        (look_at(n, Some(old), old + 1, None), Some("a file changed less than 3h ago")),
        (look_at(n, Some(old), old, Some(n - 600)), Some("a terminal login typed less than 3h ago")),
        (look_at(n, Some(n + 600), old, None), Some("the merge started less than 3h ago")),
        (look_at(n, Some(old), n + 600, None), Some("a file changed less than 3h ago")),
        (look_at(n, None, old, None), Some("no start time for the merge")),
        (look_at(n, Some(old), 0, None), None),
    ];
    for (look, want) in cases {
        assert_eq!(not_quiet(&look, 3).as_deref(), want, "{look:?}");
    }
    assert_eq!(not_quiet(&look_at(n, Some(n - 5 * HOUR), n - 5 * HOUR, None), 6).as_deref(), Some("the merge started less than 6h ago"));
}

/// The gate: on, a look, an operation finished here and not a sequence, quiet, and this run's copy
/// (taken after that look) done.
#[test]
fn the_resolve_waits_for_quiet_and_this_runs_copy() {
    let n = 1_760_900_000;
    let look = look_at(n, Some(n - DAY), n - 4 * HOUR, None);
    assert_eq!(hold(Some(3), Some(&look), CopiedNow::Yes), Ok(()));
    assert_eq!(hold(None, Some(&look), CopiedNow::Yes), Err(Hold::Off));
    assert_eq!(hold(Some(3), None, CopiedNow::Yes), Err(Hold::NoLook));
    for what in ["am", "rebase", "sequence", "none"] {
        let l = Look { what: what.into(), since: None, ..look.clone() };
        assert_eq!(hold(Some(3), Some(&l), CopiedNow::Yes), Err(Hold::Never(what.into())));
    }
    let seq = Look { what: "cherry-pick".into(), sequence: true, ..look.clone() };
    assert_eq!(hold(Some(3), Some(&seq), CopiedNow::Yes), Err(Hold::Never("sequence".into())));
    let busy = look_at(n, Some(n - DAY), n - 600, None);
    assert_eq!(hold(Some(3), Some(&busy), CopiedNow::Yes), Err(Hold::Recent("a file changed less than 3h ago".into())));
    assert_eq!(hold(Some(5), Some(&look), CopiedNow::Yes), Err(Hold::Recent("a file changed less than 5h ago".into())));
    // A copy that failed: never.
    assert_eq!(hold(Some(3), Some(&look), CopiedNow::Failed), Err(Hold::CopyFailed));
}

#[test]
fn a_copy_holds_the_checkout_when_it_pulls_the_home_or_a_dir_the_checkout_is_in() {
    let look = |top: &str, home: &str| Look { top: top.into(), home: home.into(), ..look_at(1, Some(1), 1, None) };
    let repo = look("/root/ARENA_materials", "/root");
    let cases: [(&str, &Look, &str, bool); 11] = [
        ("", &repo, "root", true),
        ("~/", &repo, "root", true),
        ("/root", &repo, "root", true),
        ("/root/ARENA_materials/", &repo, "root", true),
        ("/", &repo, "root", true),
        ("/root/ARENA_materials/chapter1", &repo, "root", false),
        ("/workspace", &repo, "root", false),
        ("/root/ARENA", &repo, "root", false),
        ("$HOME", &repo, "root", false),
        // The home the look didn't say: the login's.
        ("", &look("/home/ubuntu/ARENA_materials", ""), "ubuntu", true),
        ("", &look("/root/ARENA_materials", ""), "ubuntu", false),
    ];
    for (remote_path, l, user, want) in cases {
        assert_eq!(copy_holds(remote_path, l, user), want, "{remote_path:?} {} {user}", l.top);
    }
}

#[test]
fn a_resolve_is_read_back_and_said_in_a_line() {
    let out = "SAVED /root/ARENA_materials/.git/arena-resolved/20261019T140200Z\nHAND notes.py\nNEWER README.md\nGONE old.py\n\
               NEWER sub dir/we*ird.py\n[autocommit-x 1234] x\nRESOLVED merge theirs 1760000000 1760500000\n";
    let r = parse_resolve(out);
    assert_eq!(r.resolved, Some(("merge".to_string(), "theirs".to_string(), 1_760_000_000, 1_760_500_000)));
    assert_eq!((r.newer.clone(), r.gone.clone(), r.hand.clone()), (vec!["README.md".to_string(), "old.py".into(), "sub dir/we*ird.py".into()], vec!["old.py".to_string()], vec!["notes.py".to_string()]));
    assert_eq!(r.saved.as_deref(), Some("/root/ARENA_materials/.git/arena-resolved/20261019T140200Z"));
    assert_eq!(
        resolved_note(&r, 3, "/b/w1d1/devtest-a"),
        "finished a merge left for 3h+ (kept the newer side, MERGE_HEAD's, for 3 files and the hand-resolved version of 1; \
         before: /b/w1d1/devtest-a, and /root/ARENA_materials/.git/arena-resolved/20261019T140200Z on the pod)"
    );
    let ours = parse_resolve("NEWER a.py\nRESOLVED cherry-pick ours 2 1\n");
    assert_eq!(resolved_note(&ours, 5, "/b/x"), "finished a cherry-pick left for 5h+ (kept the newer side, HEAD's, for 1 file; before: /b/x)");
    let stash = parse_resolve("HAND a.py\nRESOLVED conflicts theirs 1 2\n");
    assert_eq!(
        resolved_note(&stash, 3, "/b/x"),
        "resolved the conflicts a `git stash pop` left for 3h+ (its stash entry kept) (the hand-resolved version of 1; before: /b/x)"
    );
    let clean = parse_resolve("RESOLVED revert ours 2 1\n");
    assert_eq!(resolved_note(&clean, 3, "/b/x"), "finished a revert left for 3h+ (nothing was left in conflict; before: /b/x)");
    let refused = parse_resolve("REFUSED changed: files changed since the copy\n");
    assert_eq!((refused.refused.as_deref(), refused.resolved), (Some("changed: files changed since the copy"), None));
    assert_eq!(parse_resolve("SAVED /x\nNEWER a\nFAILED git commit\n").failed.as_deref(), Some("git commit"));
    assert_eq!(parse_resolve("RESOLVED merge theirs soon 1\n").resolved, None);
}

/// The scripts are POSIX sh: dash (the images' `sh`) parses them.
#[cfg(unix)]
#[test]
fn the_scripts_parse_under_sh() {
    for (name, script) in [("look", look_script()), ("resolve", resolve_script())] {
        let o = std::process::Command::new("sh").arg("-n").arg("-c").arg(&script).output().unwrap();
        assert!(o.status.success(), "{name}: {}", String::from_utf8_lossy(&o.stderr));
        assert!(!script.contains("# @ARENA_"), "{name}: a mark left unrendered");
    }
    assert!(look_command("/root/my repo").ends_with(" arena-look '/root/my repo'"));
    let cmd = resolve_command("/root/ARENA_materials", 3, "merge 1760000000 abc", "1-2", "w1d1/devtest-a");
    assert!(cmd.ends_with(" arena-resolve /root/ARENA_materials 10800 'merge 1760000000 abc' 1-2 3 w1d1/devtest-a"), "{cmd}");
}

// --- Real git ---------------------------------------------------------------------------------

/// A participant's checkout `work` on `autocommit-x` (pushed, with an upstream, to the bare
/// `origin`), the course repo as its `upstream`; every git run with its own empty HOME and no
/// system config (the build machine's own settings never count), and a `who` that lists nobody
/// unless [`Repo::login`] says otherwise. The base commit, 20 days old, has `README.md`,
/// `notes.py`, `old.py`, `data.bin` (binary), `link` (→ README.md), `sub dir/we*ird.py`, and a
/// `.gitignore` of `ignored/`.
#[cfg(unix)]
struct Repo {
    base: PathBuf,
    origin: PathBuf,
    course: PathBuf,
    work: PathBuf,
    now: u64,
}

#[cfg(unix)]
impl Repo {
    fn new(tag: &str) -> Repo {
        let base = std::env::temp_dir().join(format!("arena-resolve-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("home")).unwrap();
        std::fs::create_dir_all(base.join("bin")).unwrap();
        let r = Repo { origin: base.join("origin.git"), course: base.join("course"), work: base.join("work"), now: now(), base };
        r.login(None);
        r.git(&r.base, &["init", "-q", "--bare", r.origin.to_str().unwrap()]);
        r.git(&r.base, &["init", "-q", "-b", "main", r.work.to_str().unwrap()]);
        r.git(&r.work, &["config", "user.name", "t"]);
        r.git(&r.work, &["config", "user.email", "t@t"]);
        r.put("README.md", b"base\n");
        r.put("notes.py", b"x = 0\n");
        r.put("old.py", b"old = 0\n");
        r.put("data.bin", &[0, 1, 2, 3]);
        r.put("sub dir/we*ird.py", b"w = 0\n");
        r.put(".gitignore", b"ignored/\n");
        r.link_to("link", "README.md");
        r.commit_all(&r.work, "base", r.now - 20 * DAY);
        r.git(&r.work, &["remote", "add", "origin", r.origin.to_str().unwrap()]);
        r.git(&r.work, &["push", "-q", "-u", "origin", "main"]);
        r.git(&r.work, &["checkout", "-q", "-b", "autocommit-x"]);
        r.git(&r.work, &["push", "-q", "-u", "origin", "autocommit-x"]);
        r.git(&r.base, &["clone", "-q", "-b", "main", r.origin.to_str().unwrap(), r.course.to_str().unwrap()]);
        r.git(&r.course, &["config", "user.name", "u"]);
        r.git(&r.course, &["config", "user.email", "u@u"]);
        r.git(&r.work, &["remote", "add", "upstream", r.course.to_str().unwrap()]);
        r
    }

    /// The stand-in `who`: nobody, or one login on a "tty" whose atime is `typed` seconds ago
    /// (a file under the sandbox, reached from `/dev/` by `..`).
    fn login(&self, typed: Option<u64>) {
        use std::os::unix::fs::PermissionsExt;
        let who = self.base.join("bin/who");
        let script = match typed {
            None => "#!/bin/sh\n".to_string(),
            Some(ago) => {
                let tty = self.base.join("tty");
                std::fs::write(&tty, "").unwrap();
                let o = std::process::Command::new("touch").arg("-a").arg("-d").arg(format!("@{}", self.now - ago)).arg(&tty).output().unwrap();
                assert!(o.status.success());
                format!("#!/bin/sh\necho 'root     ..{}        2026-10-19 10:00 (10.0.0.9)'\n", tty.display())
            }
        };
        std::fs::write(&who, script).unwrap();
        std::fs::set_permissions(&who, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A command with the sandbox's environment.
    fn cmd(&self, dir: &Path, prog: &str) -> std::process::Command {
        let mut c = std::process::Command::new(prog);
        let path = format!("{}:{}", self.base.join("bin").display(), std::env::var("PATH").unwrap_or_default());
        c.current_dir(dir)
            .env("HOME", self.base.join("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ALLOW_PROTOCOL", "file")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_EDITOR", "true")
            .env("PATH", path);
        c
    }

    fn git_try(&self, dir: &Path, args: &[&str]) -> (bool, String) {
        let o = self.cmd(dir, "git").args(args).output().expect("git");
        (o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let o = self.cmd(dir, "git").args(args).output().expect("git");
        assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    /// git with its author and committer dated `when`.
    fn git_at(&self, dir: &Path, when: u64, args: &[&str]) -> (bool, String) {
        let date = format!("{when} +0000");
        let o = self.cmd(dir, "git").args(args).env("GIT_AUTHOR_DATE", &date).env("GIT_COMMITTER_DATE", &date).output().expect("git");
        (o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
    }

    fn commit_all(&self, dir: &Path, msg: &str, when: u64) {
        self.git(dir, &["add", "-A"]);
        let (ok, said) = self.git_at(dir, when, &["commit", "-q", "-m", msg]);
        assert!(ok, "{said}");
    }

    fn put_in(&self, dir: &Path, rel: &str, bytes: &[u8]) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&p);
        std::fs::write(p, bytes).unwrap();
    }

    fn put(&self, rel: &str, bytes: &[u8]) {
        self.put_in(&self.work, rel, bytes);
    }

    fn link_in(&self, dir: &Path, rel: &str, to: &str) {
        let p = dir.join(rel);
        let _ = std::fs::remove_file(&p);
        std::os::unix::fs::symlink(to, p).unwrap();
    }

    fn link_to(&self, rel: &str, to: &str) {
        self.link_in(&self.work, rel, to);
    }

    fn read(&self, rel: &str) -> Vec<u8> {
        std::fs::read(self.work.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    }

    fn link(&self, rel: &str) -> String {
        std::fs::read_link(self.work.join(rel)).unwrap().display().to_string()
    }

    /// `bash -c cmd` in the sandbox: (exit code, stdout, stderr).
    fn sh(&self, cmd: &str) -> (Option<i32>, String, String) {
        let o = self.cmd(&self.base, "bash").arg("-c").arg(cmd).output().expect("bash");
        (o.status.code(), String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned())
    }

    fn backup(&self) -> (Option<i32>, BackupReport, String) {
        let (code, out, err) = self.sh(&backup_command_limited(self.work.to_str().unwrap(), None, "arena backup", 1_000_000, 1_000_000, None));
        (code, parse_backup_report(&out), format!("{out}{err}"))
    }

    fn look(&self) -> Look {
        let (code, out, err) = self.sh(&look_command(self.work.to_str().unwrap()));
        assert_eq!(code, Some(0), "{out}{err}");
        parse_look(&out).unwrap_or_else(|| panic!("no look in: {out}{err}"))
    }

    /// The resolve, as `pods backup` runs it after a copy that followed `look`.
    fn resolve(&self, hours: u64, look: &Look) -> (Option<i32>, ResolveReport, String) {
        let cmd = resolve_command(self.work.to_str().unwrap(), hours, &look.op_key(), &look.print, "w1d1/devtest-x");
        let (code, out, err) = self.sh(&cmd);
        (code, parse_resolve(&out), format!("{out}{err}"))
    }

    /// Every path in the checkout, `.git` and all, last changed `ago` seconds ago.
    fn age(&self, ago: u64) {
        let o = self.cmd(&self.base, "find")
            .arg(&self.work)
            .args(["-exec", "touch", "-h", "-d", &format!("@{}", self.now - ago), "{}", "+"])
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }

    /// One path (under the checkout) last changed `ago` seconds ago.
    fn touch(&self, rel: &str, ago: u64) {
        let o = self.cmd(&self.work, "touch").args(["-h", "-d", &format!("@{}", self.now - ago), rel]).output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }

    /// The conflicting commits of [`Repo::merge_conflict`]: the course's on upstream `main` and the
    /// participant's on `autocommit-x`, dated `course_at` and `mine_at`.
    fn diverge(&self, course_at: u64, mine_at: u64) {
        let c = &self.course;
        self.put_in(c, "README.md", b"course\n");
        self.put_in(c, "notes.py", b"x = 2\n");
        std::fs::remove_file(c.join("old.py")).unwrap();
        self.put_in(c, "data.bin", &[0, 9, 9, 9]);
        self.put_in(c, "sub dir/we*ird.py", b"w = 2\n");
        self.link_in(c, "link", "notes.py");
        self.commit_all(c, "the course's fix", course_at);
        self.put("README.md", b"mine\n");
        self.put("notes.py", b"x = 1\n");
        self.put("old.py", b"old = 1\n");
        self.put("data.bin", &[0, 7, 7, 7]);
        self.put("sub dir/we*ird.py", b"w = 1\n");
        self.link_to("link", "data.bin");
        self.commit_all(&self.work, "mine", mine_at);
    }

    /// A conflicted `git pull upstream main` over [`Repo::diverge`]'s commits: README.md, notes.py,
    /// data.bin, link and `sub dir/we*ird.py` both changed, old.py changed here and deleted there.
    fn merge_conflict(&self, course_at: u64, mine_at: u64) {
        self.diverge(course_at, mine_at);
        let (ok, said) = self.git_try(&self.work, &["pull", "-q", "--no-rebase", "--no-edit", "upstream", "main"]);
        assert!(!ok, "no conflict: {said}");
        assert!(self.work.join(".git/MERGE_HEAD").exists());
    }

    fn unmerged(&self) -> String {
        self.git(&self.work, &["ls-files", "-u"])
    }
}

#[cfg(unix)]
impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// The human's rule, on a merge left alone for 3h+: each conflicted path from the side whose
/// commit is newer — a text file, a binary one, a symlink, an odd name, and one deleted there
/// (removed when that side is the newer) — except a file the participant resolved by hand (no
/// conflict marker left), kept as it is; a binary file "resolved" is never taken as such. A real
/// merge commit in the repo's own identity, saying what it did and where the state before is; the
/// files as they were kept in the git dir; then the backup commits and pushes as always.
#[cfg(unix)]
#[test]
fn a_merge_left_alone_is_finished_by_the_newer_side_and_the_hand_resolved_file_kept() {
    for theirs_newer in [true, false] {
        let r = Repo::new(if theirs_newer { "merge-theirs" } else { "merge-ours" });
        let (course_at, mine_at) = if theirs_newer { (r.now - 5 * DAY, r.now - 10 * DAY) } else { (r.now - 10 * DAY, r.now - 5 * DAY) };
        r.merge_conflict(course_at, mine_at);
        let merged = r.git(&r.work, &["rev-parse", "MERGE_HEAD"]);
        let head = r.git(&r.work, &["rev-parse", "HEAD"]);
        // The participant resolved notes.py by hand, "resolved" the binary file, left the rest, and
        // started a note of their own (untracked).
        r.put("notes.py", b"x = 3\n");
        r.put("data.bin", &[0, 5, 5, 5]);
        r.put("scratch.md", b"draft\n");
        let (code, report, said) = r.backup();
        assert_eq!((code, report.busy.as_deref()), (Some(BACKUP_BUSY_EXIT), Some("merge")), "{said}");
        r.age(4 * HOUR);
        let look = r.look();
        assert_eq!((look.what.as_str(), look.since, look.head.as_str(), look.sequence), ("merge", Some(r.now - 4 * HOUR), merged.as_str(), false));
        assert_eq!((look.newest, look.input, not_quiet(&look, 3)), (r.now - 4 * HOUR, None, None));
        assert_eq!(not_quiet(&look, 5).as_deref(), Some("the merge started less than 5h ago"));

        let (code, rep, said) = r.resolve(3, &look);
        assert_eq!(code, Some(0), "{said}");
        let side = if theirs_newer { "theirs" } else { "ours" };
        assert_eq!(rep.resolved, Some(("merge".to_string(), side.to_string(), mine_at, course_at)), "{said}");
        assert_eq!(rep.hand, ["notes.py"], "{said}");
        let mut newer = rep.newer.clone();
        newer.sort();
        assert_eq!(newer, ["README.md", "data.bin", "link", "old.py", "sub dir/we*ird.py"], "{said}");
        assert_eq!(rep.gone, if theirs_newer { vec!["old.py".to_string()] } else { vec![] }, "{said}");
        // The tree: the newer side's, notes.py as the participant left it.
        assert_eq!(r.read("notes.py"), b"x = 3\n");
        if theirs_newer {
            assert_eq!((r.read("README.md"), r.read("data.bin"), r.link("link")), (b"course\n".to_vec(), vec![0, 9, 9, 9], "notes.py".to_string()));
            assert_eq!(r.read("sub dir/we*ird.py"), b"w = 2\n");
            assert!(!r.work.join("old.py").exists(), "deleted on the newer side");
        } else {
            assert_eq!((r.read("README.md"), r.read("data.bin"), r.link("link")), (b"mine\n".to_vec(), vec![0, 7, 7, 7], "data.bin".to_string()));
            assert_eq!((r.read("sub dir/we*ird.py"), r.read("old.py")), (b"w = 1\n".to_vec(), b"old = 1\n".to_vec()));
        }
        assert_eq!(r.read("scratch.md"), b"draft\n", "untouched");
        // A merge commit of the two, in the repo's identity, saying what it did.
        assert!(!r.work.join(".git/MERGE_HEAD").exists() && r.unmerged().is_empty());
        assert_eq!(r.git(&r.work, &["rev-list", "--parents", "-n1", "HEAD"]), format!("{} {head} {merged}", r.git(&r.work, &["rev-parse", "HEAD"])));
        assert_eq!(r.git(&r.work, &["log", "-1", "--format=%an <%ae>|%cn <%ce>"]), "t <t@t>|t <t@t>");
        let day = |t: u64| crate::schedule::ymd_string((t / DAY) as i64);
        let saved = rep.saved.clone().unwrap();
        let stamp = saved.rsplit('/').next().unwrap();
        let body = r.git(&r.work, &["log", "-1", "--format=%B"]);
        let newer_list = "README.md, data.bin, link, old.py, sub dir/we*ird.py";
        assert_eq!(
            body,
            format!(
                "arena: finished a merge left in progress for 3h+\n\nNothing in this checkout changed for 3h+ while a merge was in \
                 progress, so arena finished it: kept the newer side (HEAD {} / MERGE_HEAD {}) for {newer_list}; kept your \
                 hand-resolved notes.py; the state before is in the backup copy w1d1/devtest-x and on this machine in \
                 .git/arena-resolved/{stamp}/.\n\nMerge branch 'main' of {} into autocommit-x",
                day(mine_at),
                day(course_at),
                r.course.display()
            )
        );
        // The files as they were, in the git dir: the markers, the hand edit, the binary.
        let kept = PathBuf::from(&saved).join("files");
        assert!(saved.starts_with(&format!("{}/.git/arena-resolved/", r.work.display())), "{saved}");
        assert!(String::from_utf8_lossy(&std::fs::read(kept.join("README.md")).unwrap()).contains("<<<<<<< HEAD"));
        assert_eq!(std::fs::read(kept.join("notes.py")).unwrap(), b"x = 3\n");
        assert_eq!(std::fs::read(kept.join("data.bin")).unwrap(), [0, 5, 5, 5]);
        assert_eq!(std::fs::read(kept.join("old.py")).unwrap(), b"old = 1\n");
        assert_eq!(std::fs::read_link(kept.join("link")).unwrap().display().to_string(), "data.bin");
        assert!(std::fs::read_to_string(PathBuf::from(&saved).join("README")).unwrap().starts_with("arena finished a merge left in progress for 3h+"));
        // Then backed up as always: the merge and the participant's note pushed.
        let (code, report, said) = r.backup();
        assert_eq!((code, report.outcome), (Some(0), Some((BACKUP_PUSHED, "autocommit-x".to_string()))), "{said}");
        let pushed = r.git(&r.base, &["--git-dir", r.origin.to_str().unwrap(), "rev-parse", "autocommit-x"]);
        assert_eq!(pushed, r.git(&r.work, &["rev-parse", "HEAD"]));
        let tree = r.git(&r.base, &["--git-dir", r.origin.to_str().unwrap(), "ls-tree", "-r", "--name-only", "autocommit-x"]);
        assert!(tree.lines().any(|l| l == "scratch.md") && tree.lines().any(|l| l == "notes.py"), "{tree}");
        assert_eq!(tree.lines().any(|l| l == "old.py"), !theirs_newer, "{tree}");
        assert!(!tree.contains("arena-resolved"), "{tree}");
    }
}

/// A single cherry-pick and a revert: finished by `--continue` (their own message, and authorship,
/// with the words added); one whose result is empty — the newer side HEAD's everywhere, as a
/// revert's always is — committed empty, as git advises. A revert's other side is the commit it
/// reverts: older than HEAD, so HEAD's side is kept.
#[cfg(unix)]
#[test]
fn a_cherry_pick_or_a_revert_left_alone_is_finished_by_continuing() {
    for (case, theirs_newer) in [("pick-theirs", true), ("pick-ours", false)] {
        let r = Repo::new(case);
        let (side_at, mine_at) = if theirs_newer { (r.now - 5 * DAY, r.now - 10 * DAY) } else { (r.now - 10 * DAY, r.now - 5 * DAY) };
        r.git(&r.work, &["checkout", "-q", "-b", "side", "main"]);
        r.put("README.md", b"side\n");
        r.put("side.py", b"s = 1\n");
        let (ok, said) = r.git_at(&r.work, side_at, &["-c", "user.name=s", "-c", "user.email=s@s", "commit", "-q", "-am", "the side's change"]);
        assert!(ok, "{said}");
        r.git(&r.work, &["add", "side.py"]);
        let (ok, said) = r.git_at(&r.work, side_at, &["-c", "user.name=s", "-c", "user.email=s@s", "commit", "-q", "--amend", "--no-edit"]);
        assert!(ok, "{said}");
        r.git(&r.work, &["checkout", "-q", "autocommit-x"]);
        r.put("README.md", b"mine\n");
        r.commit_all(&r.work, "mine", mine_at);
        let head = r.git(&r.work, &["rev-parse", "HEAD"]);
        let (ok, said) = r.git_try(&r.work, &["cherry-pick", "side"]);
        assert!(!ok && r.work.join(".git/CHERRY_PICK_HEAD").exists() && !r.work.join(".git/sequencer").exists(), "a single pick: {said}");
        assert_eq!(r.backup().1.busy.as_deref(), Some("cherry-pick"));
        r.age(4 * HOUR);
        let (code, rep, said) = r.resolve(3, &r.look());
        assert_eq!(code, Some(0), "{case}: {said}");
        assert_eq!(rep.newer, ["README.md"], "{case}");
        assert!(!r.work.join(".git/CHERRY_PICK_HEAD").exists(), "{case}");
        assert_eq!(r.git(&r.work, &["rev-parse", "HEAD~1"]), head, "{case}: one commit on top");
        assert_eq!(r.read("README.md"), if theirs_newer { b"side\n".to_vec() } else { b"mine\n".to_vec() }, "{case}");
        assert_eq!(r.read("side.py"), b"s = 1\n", "{case}: the pick's other change, applied either way");
        assert_eq!(r.git(&r.work, &["log", "-1", "--format=%an|%cn"]), "s|t", "{case}: the pick's author, the repo's committer");
        let body = r.git(&r.work, &["log", "-1", "--format=%B"]);
        assert!(body.starts_with("the side's change\n\narena: finished this cherry-pick, left in progress for 3h+ — kept the newer side (HEAD "), "{case}: {body}");
        assert!(body.contains(" for README.md; the state before is in the backup copy w1d1/devtest-x and on this machine in .git/arena-resolved/"), "{body}");
        assert!(!body.contains("Conflicts:"), "{body}");
        assert_eq!(r.backup().1.outcome, Some((BACKUP_PUSHED, "autocommit-x".to_string())), "{case}");
    }

    // A revert of an older commit whose line changed since: HEAD's side kept, so nothing changes —
    // committed empty with the revert's message.
    let r = Repo::new("revert");
    for (text, at) in [("a", r.now - 9 * DAY), ("b", r.now - 8 * DAY)] {
        r.put("README.md", format!("{text}\n").as_bytes());
        r.commit_all(&r.work, text, at);
    }
    let head = r.git(&r.work, &["rev-parse", "HEAD"]);
    let (ok, said) = r.git_try(&r.work, &["revert", "--no-edit", "HEAD~1"]);
    assert!(!ok && r.work.join(".git/REVERT_HEAD").exists(), "{said}");
    assert_eq!(r.backup().1.busy.as_deref(), Some("revert"));
    r.age(4 * HOUR);
    let (code, rep, said) = r.resolve(3, &r.look());
    assert_eq!(code, Some(0), "{said}");
    assert_eq!(rep.resolved.as_ref().map(|(w, s, ..)| (w.as_str(), s.as_str())), Some(("revert", "ours")));
    assert!(!r.work.join(".git/REVERT_HEAD").exists());
    assert_eq!((r.read("README.md"), r.git(&r.work, &["rev-parse", "HEAD~1"])), (b"b\n".to_vec(), head));
    assert_eq!(r.git(&r.work, &["diff", "--stat", "HEAD~1", "HEAD"]), "", "empty");
    assert!(r.git(&r.work, &["log", "-1", "--format=%B"]).starts_with("Revert \"a\""));
    assert_eq!(r.backup().1.outcome, Some((BACKUP_PUSHED, "autocommit-x".to_string())));
}

/// A `git stash pop`'s conflicts: each path from the newer side — HEAD's or the stash entry's
/// (when it was stashed) — or as the participant resolved it; nothing committed by the resolve
/// (the backup does), the stash entry kept, as git keeps it on a conflicted pop. Conflicts that
/// aren't the newest stash entry's are left alone.
#[cfg(unix)]
#[test]
fn a_stash_pops_conflicts_are_resolved_and_its_stash_entry_kept() {
    for (case, stash_newer) in [("stash-theirs", true), ("stash-ours", false)] {
        let r = Repo::new(case);
        let (stash_at, head_at) = if stash_newer { (r.now - 5 * DAY, r.now - 10 * DAY) } else { (r.now - 10 * DAY, r.now - 5 * DAY) };
        r.put("README.md", b"stashed\n");
        r.put("notes.py", b"x = stashed\n");
        let (ok, said) = r.git_at(&r.work, stash_at, &["stash", "-q"]);
        assert!(ok, "{said}");
        r.put("README.md", b"committed\n");
        r.put("notes.py", b"x = committed\n");
        r.commit_all(&r.work, "committed", head_at);
        let head = r.git(&r.work, &["rev-parse", "HEAD"]);
        let (ok, said) = r.git_try(&r.work, &["stash", "pop"]);
        assert!(!ok, "{said}");
        r.put("notes.py", b"x = both\n");
        assert_eq!(r.backup().1.busy.as_deref(), Some("conflicts"));
        r.age(4 * HOUR);
        let look = r.look();
        assert_eq!((look.what.as_str(), look.head.clone()), ("conflicts", r.git(&r.work, &["rev-parse", "refs/stash"])));
        let (code, rep, said) = r.resolve(3, &look);
        assert_eq!(code, Some(0), "{case}: {said}");
        assert_eq!((rep.newer.clone(), rep.hand.clone()), (vec!["README.md".to_string()], vec!["notes.py".to_string()]), "{case}");
        assert_eq!(r.read("README.md"), if stash_newer { b"stashed\n".to_vec() } else { b"committed\n".to_vec() }, "{case}");
        assert_eq!(r.read("notes.py"), b"x = both\n");
        assert!(r.unmerged().is_empty(), "{case}");
        assert_eq!(r.git(&r.work, &["rev-parse", "HEAD"]), head, "{case}: nothing committed by the resolve");
        assert_eq!(r.git(&r.work, &["stash", "list"]).lines().count(), 1, "{case}: the stash entry kept");
        let (code, report, said) = r.backup();
        assert_eq!((code, report.outcome), (Some(0), Some((BACKUP_PUSHED, "autocommit-x".to_string()))), "{case}: {said}");
    }

    // An older stash entry applied (not the newest): its conflicts aren't stash@{0}'s — left alone.
    let r = Repo::new("stash-older");
    r.put("README.md", b"first stash\n");
    r.git(&r.work, &["stash", "-q"]);
    r.put("notes.py", b"x = second stash\n");
    r.git(&r.work, &["stash", "-q"]);
    r.put("README.md", b"committed\n");
    r.commit_all(&r.work, "committed", r.now - DAY);
    let (ok, said) = r.git_try(&r.work, &["stash", "apply", "stash@{1}"]);
    assert!(!ok, "{said}");
    r.age(4 * HOUR);
    let before = r.unmerged();
    let (code, rep, said) = r.resolve(3, &r.look());
    assert_eq!((code, rep.refused.as_deref()), (Some(RESOLVE_REFUSED_EXIT), Some("conflicts that aren't the newest stash entry's (README.md)")), "{said}");
    assert_eq!(r.unmerged(), before, "nothing touched");
    assert!(!r.work.join(".git/arena-resolved").exists(), "nothing kept: nothing done");
}

/// `git am`, a rebase and a cherry-pick sequence are never finished — what they'd go on to apply is
/// the participant's — and neither is anything the look doesn't recognise: the gate says so, and
/// the script itself refuses (exit 7), touching nothing.
#[cfg(unix)]
#[test]
fn am_a_rebase_and_a_sequence_are_left_alone() {
    // git am with a patch that doesn't apply.
    let r = Repo::new("am");
    r.put("README.md", b"patched\n");
    let patch = r.base.join("p.patch");
    r.commit_all(&r.work, "the patch", r.now - 9 * DAY);
    let p = r.git(&r.work, &["format-patch", "-1", "--stdout"]);
    std::fs::write(&patch, format!("{p}\n")).unwrap();
    r.git(&r.work, &["reset", "-q", "--hard", "HEAD~1"]);
    r.put("README.md", b"diverged\n");
    r.commit_all(&r.work, "diverged", r.now - 8 * DAY);
    let (ok, said) = r.git_try(&r.work, &["am", patch.to_str().unwrap()]);
    assert!(!ok && r.work.join(".git/rebase-apply/applying").exists(), "{said}");
    assert_eq!(r.backup().1.busy.as_deref(), Some("am"));
    r.age(4 * HOUR);
    let look = r.look();
    assert_eq!((look.what.as_str(), look.since), ("am", None));
    assert_eq!(hold(Some(3), Some(&look), CopiedNow::Yes), Err(Hold::Never("am".into())));
    let (code, rep, _) = r.resolve(3, &look);
    assert_eq!((code, rep.refused.as_deref()), (Some(RESOLVE_REFUSED_EXIT), Some("a git am is in progress: never finished automatically")));
    assert!(r.work.join(".git/rebase-apply/applying").exists() && !r.work.join(".git/arena-resolved").exists());

    // A rebase that stopped on a conflict.
    let r = Repo::new("rebase");
    r.diverge(r.now - 10 * DAY, r.now - 5 * DAY);
    r.git(&r.work, &["fetch", "-q", "upstream"]);
    let (ok, said) = r.git_try(&r.work, &["rebase", "upstream/main"]);
    assert!(!ok && r.work.join(".git/rebase-merge").exists(), "{said}");
    r.age(4 * HOUR);
    let look = r.look();
    assert_eq!(look.what, "rebase");
    let (code, rep, _) = r.resolve(3, &look);
    assert_eq!((code, rep.refused.as_deref()), (Some(RESOLVE_REFUSED_EXIT), Some("a rebase is in progress: never finished automatically")));
    assert!(r.work.join(".git/rebase-merge").exists() && !r.unmerged().is_empty());

    // A three-commit cherry-pick that stopped on its first: CHERRY_PICK_HEAD with sequencer/.
    let r = Repo::new("sequence");
    r.git(&r.work, &["checkout", "-q", "-b", "side", "main"]);
    for (i, text) in ["s1", "s2", "s3"].iter().enumerate() {
        r.put("README.md", format!("{text}\n").as_bytes());
        r.commit_all(&r.work, text, r.now - (9 - i as u64) * DAY);
    }
    r.git(&r.work, &["checkout", "-q", "autocommit-x"]);
    r.put("README.md", b"mine\n");
    r.commit_all(&r.work, "mine", r.now - 5 * DAY);
    let (ok, said) = r.git_try(&r.work, &["cherry-pick", "main..side"]);
    assert!(!ok && r.work.join(".git/sequencer").exists() && r.work.join(".git/CHERRY_PICK_HEAD").exists(), "{said}");
    r.age(4 * HOUR);
    let look = r.look();
    assert_eq!((look.what.as_str(), look.sequence), ("cherry-pick", true));
    assert_eq!(hold(Some(3), Some(&look), CopiedNow::Yes), Err(Hold::Never("sequence".into())));
    let (code, rep, _) = r.resolve(3, &look);
    assert_eq!((code, rep.refused.as_deref()), (Some(RESOLVE_REFUSED_EXIT), Some("a cherry-pick or revert sequence: never finished automatically")));
    assert!(r.work.join(".git/CHERRY_PICK_HEAD").exists() && !r.unmerged().is_empty());
}

/// Activity keeps it BUSY: a merge started 10 minutes ago, a tracked or an untracked file changed
/// 10 minutes ago (an operation older than the threshold, but recent edits), a terminal login that
/// typed 10 minutes ago — the gate says so, and the script, called all the same, refuses and
/// touches nothing. An ignored file changing counts for nothing, nor does an old login. And a
/// change after the look the copy followed (even one that keeps the file's old mtime): refused.
#[cfg(unix)]
#[test]
fn recent_activity_or_a_change_since_the_copy_leaves_it_busy() {
    let r = Repo::new("recent");
    r.merge_conflict(r.now - 10 * DAY, r.now - 5 * DAY);
    let conflicts = r.unmerged();
    let untouched = |r: &Repo, why: &str| {
        assert!(r.work.join(".git/MERGE_HEAD").exists(), "{why}: the merge ended");
        assert_eq!(r.unmerged(), conflicts, "{why}: a conflict resolved");
        assert!(!r.work.join(".git/arena-resolved").exists(), "{why}: something done");
    };
    // (what changed 10 minutes ago, the gate's words, the script's)
    type Recent<'a> = (Box<dyn Fn(&Repo)>, &'a str, &'a str);
    let cases: Vec<Recent> = vec![
        (Box::new(|r: &Repo| r.touch(".git/MERGE_HEAD", 600)), "the merge started less than 3h ago", "recent: the merge started less than 3h ago"),
        (Box::new(|r: &Repo| r.touch("notes.py", 600)), "a file changed less than 3h ago", "recent: a file changed less than 3h ago"),
        (
            Box::new(|r: &Repo| {
                r.put("new.txt", b"new\n");
                r.touch("new.txt", 600);
            }),
            "a file changed less than 3h ago",
            "recent: a file changed less than 3h ago",
        ),
        (Box::new(|r: &Repo| r.login(Some(600))), "a terminal login typed less than 3h ago", "recent: a terminal login typed less than 3h ago"),
    ];
    for (recent, gate, script) in cases {
        r.age(4 * HOUR);
        recent(&r);
        let look = r.look();
        assert_eq!(not_quiet(&look, 3).as_deref(), Some(gate));
        let (code, rep, said) = r.resolve(3, &look);
        assert_eq!((code, rep.refused.as_deref()), (Some(RESOLVE_REFUSED_EXIT), Some(script)), "{said}");
        untouched(&r, gate);
        let _ = std::fs::remove_file(r.work.join("new.txt"));
        r.login(None);
    }
    // Changed after the look the copy followed — same mtime, new content: refused.
    r.age(4 * HOUR);
    let look = r.look();
    r.put("README.md", b"edited again\n");
    r.touch("README.md", 4 * HOUR);
    let (code, rep, said) = r.resolve(3, &look);
    assert_eq!((code, rep.refused.as_deref()), (Some(RESOLVE_REFUSED_EXIT), Some("changed: files changed since the copy")), "{said}");
    untouched(&r, "changed");
    // An ignored file written now, an old login: quiet — finished.
    r.age(4 * HOUR);
    r.put("ignored/run.log", b"step 1\n");
    r.login(Some(5 * HOUR));
    let look = r.look();
    assert_eq!(not_quiet(&look, 3), None, "{look:?}");
    assert_eq!(look.input, Some(r.now - 5 * HOUR));
    let (code, rep, said) = r.resolve(3, &look);
    assert_eq!((code, rep.resolved.map(|(w, s, ..)| (w, s))), (Some(0), Some(("merge".to_string(), "ours".to_string()))), "{said}");
}

/// A missing identity: refused, nothing touched — the resolve never makes one up (`-c user.*`).
#[cfg(unix)]
#[test]
fn no_identity_in_the_repo_is_refused() {
    let r = Repo::new("ident");
    r.merge_conflict(r.now - 10 * DAY, r.now - 5 * DAY);
    r.git(&r.work, &["config", "--unset", "user.name"]);
    r.age(4 * HOUR);
    let (code, rep, said) = r.resolve(3, &r.look());
    assert_eq!((code, rep.refused.as_deref()), (Some(RESOLVE_REFUSED_EXIT), Some("no git identity configured in the repo")), "{said}");
    assert!(r.work.join(".git/MERGE_HEAD").exists());
}
