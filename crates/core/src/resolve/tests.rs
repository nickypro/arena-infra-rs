//! The look, the resolve and the decisions around them: the pure parts by table; the scripts
//! against real git — a participant's checkout left mid-merge, mid-cherry-pick, mid-revert, with a
//! `git stash pop`'s conflicts, mid-`git am`/rebase/sequence — its files made old with `touch -d`,
//! the pod's terminals and processes stood in for by directories the tests fill.

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
        repo: Some("/root/ARENA_materials".into()),
        home: "/root".into(),
        what: "merge".into(),
        since,
        head: "abc123".into(),
        sequence: false,
        newest,
        files: 12,
        print: "111-222".into(),
        index: since,
        locked: false,
        input,
        tty: None,
        at_work: None,
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
    let out = "NOW 1760900000\nTOP /workspace/ARENA_materials\nREPO /root/ARENA_materials\nHOME /root\nOP merge 1760800000 0123abcd 0\n\
               NEWEST 1760700000 42\nPRINT 3735928559-1234\nINDEX 1760750000\nLOCKED index.lock\nINPUT 1760600000\nTTY 1760650000\n\
               PROC 4242 vim\n";
    let l = parse_look(out).unwrap();
    assert_eq!(
        l,
        Look {
            now: 1_760_900_000,
            top: "/workspace/ARENA_materials".into(),
            repo: Some("/root/ARENA_materials".into()),
            home: "/root".into(),
            what: "merge".into(),
            since: Some(1_760_800_000),
            head: "0123abcd".into(),
            sequence: false,
            newest: 1_760_700_000,
            files: 42,
            print: "3735928559-1234".into(),
            index: Some(1_760_750_000),
            locked: true,
            input: Some(1_760_600_000),
            tty: Some(1_760_650_000),
            at_work: Some("4242 vim".into()),
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
    // The lines said only when there is something to say: none of them, all off.
    let bare = ["REPO /root/ARENA_materials\n", "INDEX 1760750000\n", "LOCKED index.lock\n", "INPUT 1760600000\n", "TTY 1760650000\n", "PROC 4242 vim\n"]
        .iter()
        .fold(out.to_string(), |o, l| o.replace(l, ""));
    let l = parse_look(&bare).unwrap();
    assert_eq!((l.repo, l.index, l.locked, l.input, l.tty, l.at_work), (None, None, false, None, None, None));
    // Any line it always prints missing or unreadable: no look.
    for broken in ["NOW 1760900000\n", "NEWEST 1760700000 42\n", "PRINT 3735928559-1234\n", "OP merge 1760800000 0123abcd 0\n"] {
        assert_eq!(parse_look(&out.replace(broken, "")), None, "{broken}");
    }
    assert_eq!(parse_look(&out.replace("OP merge 1760800000 0123abcd 0", "OP merge soon x 0")), None);
    assert_eq!(parse_look(&out.replace("NEWEST 1760700000 42", "NEWEST 1760700000")), None);
    assert_eq!(parse_look(""), None);
}

/// The quiet check: the operation, every file git lists, the index (anything staged), any terminal
/// login and any terminal, all older than the threshold, git's index unlocked and nobody at work
/// in git — on the pod's clock; a time in the future never counts as quiet.
#[test]
fn quiet_means_nothing_and_nobody_at_work_for_the_threshold() {
    let n = 1_760_900_000;
    let old = n - 3 * HOUR;
    let base = look_at(n, Some(old), old, None);
    let with = |f: &dyn Fn(&mut Look)| {
        let mut l = base.clone();
        f(&mut l);
        l
    };
    let cases: Vec<(Look, Option<&str>)> = vec![
        (base.clone(), None),
        (look_at(n, Some(n - 10 * DAY), n - 4 * HOUR, Some(n - 5 * HOUR)), None),
        (look_at(n, Some(old + 1), old, None), Some("the merge started less than 3h ago")),
        (look_at(n, Some(old), n - 600, None), Some("a file changed less than 3h ago")),
        (look_at(n, Some(old), old + 1, None), Some("a file changed less than 3h ago")),
        (look_at(n, Some(old), old, Some(n - 600)), Some("a terminal login typed less than 3h ago")),
        (look_at(n, Some(n + 600), old, None), Some("the merge started less than 3h ago")),
        (look_at(n, Some(old), n + 600, None), Some("a file changed less than 3h ago")),
        (look_at(n, None, old, None), Some("no start time for the merge")),
        (look_at(n, Some(old), 0, None), None),
        // Something staged since; git's index locked; a terminal used (no login); a git command or
        // an editor it waits on.
        (with(&|l| l.index = Some(n - 600)), Some("git's index changed less than 3h ago (something was staged)")),
        (with(&|l| l.index = Some(n + 600)), Some("git's index changed less than 3h ago (something was staged)")),
        (with(&|l| l.index = None), None),
        (with(&|l| l.locked = true), Some("git's index is locked (.git/index.lock): a git command is at work, or died holding it")),
        (with(&|l| l.tty = Some(n - 600)), Some("a terminal was used less than 3h ago")),
        (with(&|l| l.tty = Some(n - 4 * HOUR)), None),
        (with(&|l| l.at_work = Some("4242 git".into())), Some("a git command, or an editor one waits on, is running (pid 4242: git)")),
        (with(&|l| l.at_work = Some("4343 vim".into())), Some("a git command, or an editor one waits on, is running (pid 4343: vim)")),
    ];
    for (look, want) in cases {
        assert_eq!(not_quiet(&look, 3).as_deref(), want, "{look:?}");
    }
    assert_eq!(not_quiet(&look_at(n, Some(n - 5 * HOUR), n - 5 * HOUR, None), 6).as_deref(), Some("the merge started less than 6h ago"));
}

fn copy_of(look: &Look, at: u64) -> BusyCopy {
    BusyCopy {
        op: look.op_key(),
        print: look.print.clone(),
        newest: look.newest,
        at,
        to: "/b/w1d1/devtest-a".into(),
        label: "w1d1/devtest-a".into(),
        git: "/b/w1d1/devtest-a/ARENA_materials/.git".into(),
    }
}

/// Part 2: a BUSY pod is copied when nothing since the operation started is recorded, when its
/// work tree changed since the last recorded copy, or when that copy is an hour old — else not.
#[test]
fn a_busy_pod_is_copied_only_when_unrecorded_changed_or_an_hour_old() {
    let n = 1_760_900_000;
    let look = look_at(n, Some(n - DAY), n - DAY, None);
    let fresh = copy_of(&look, n - 600);
    let other_op = BusyCopy { op: "merge 1 abc123".into(), ..fresh.clone() };
    let changed = BusyCopy { print: "999-1".into(), ..fresh.clone() };
    let (young, hour, ahead) = (copy_of(&look, n - COPY_EVERY_SECS + 1), copy_of(&look, n - COPY_EVERY_SECS), copy_of(&look, n + 60));
    let cases: Vec<(Option<&Look>, Option<&BusyCopy>, Option<CopyWhy>)> = vec![
        (None, Some(&fresh), Some(CopyWhy::NoLook)),
        (Some(&look), None, Some(CopyWhy::First)),
        (Some(&look), Some(&other_op), Some(CopyWhy::First)),
        (Some(&look), Some(&changed), Some(CopyWhy::Changed)),
        (Some(&look), Some(&fresh), None),
        (Some(&look), Some(&young), None),
        (Some(&look), Some(&hour), Some(CopyWhy::Stale)),
        (Some(&look), Some(&ahead), Some(CopyWhy::Stale)),
    ];
    for (l, last, want) in cases {
        assert_eq!(copy_needed(l, last, n), want, "{last:?}");
    }
}

/// An earlier run's copy is trusted (to skip a copy, or to resolve on) only while it is under the
/// backup dir as configured now and its `.git` is still on this machine (review ai-review codex
/// #7): a copy deleted or moved since, a `LOCAL_BACKUP_DIR` changed since, or a record from before
/// the `.git` was kept, is none — the pod is copied now.
#[test]
fn a_recorded_copy_counts_only_while_it_is_still_there_under_this_backup_dir() {
    let base = std::env::temp_dir().join(format!("arena-copy-there-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let dir = base.join("backups").display().to_string();
    let to = format!("{dir}/w1d1/devtest-a");
    let git = format!("{to}/ARENA_materials/.git");
    std::fs::create_dir_all(&git).unwrap();
    let copy = BusyCopy { op: "merge 1 a".into(), print: "1-2".into(), newest: 1, at: 1, to: to.clone(), label: "w1d1/devtest-a".into(), git: git.clone() };
    assert!(copy_still_there(&copy, &dir));
    assert!(copy_still_there(&copy, &format!("{dir}/")), "a trailing slash is the same dir");
    // LOCAL_BACKUP_DIR changed since (another dir; one that merely starts the same).
    assert!(!copy_still_there(&copy, &base.join("elsewhere").display().to_string()));
    assert!(!copy_still_there(&copy, &format!("{dir}-old")));
    assert!(!copy_still_there(&copy, ""));
    // A record from before the `.git` was kept, or one pointing outside its copy.
    assert!(!copy_still_there(&BusyCopy { git: String::new(), ..copy.clone() }, &dir));
    assert!(!copy_still_there(&BusyCopy { git: base.join("x/.git").display().to_string(), ..copy.clone() }, &dir));
    // The copy gone (its `.git`, or the whole of it).
    std::fs::remove_dir_all(&git).unwrap();
    assert!(!copy_still_there(&copy, &dir));
    std::fs::remove_dir_all(&base).unwrap();
    assert!(!copy_still_there(&copy, &dir));
    // An old record (no `git` field) reads back as none recorded there.
    let old: BusyCopy = serde_json::from_str(r#"{"op":"merge 1 a","print":"1-2","newest":1,"at":1,"to":"/b/x","label":"w1d1/x"}"#).unwrap();
    assert_eq!(old.git, "");
}

/// Part 1's gate: on, a look, an operation finished here and not a sequence, quiet, this run's
/// copy not failed, and a recorded copy of this operation with the work tree as it is now.
#[test]
fn the_resolve_waits_for_quiet_and_a_copy_taken_after_the_last_change() {
    let n = 1_760_900_000;
    let look = look_at(n, Some(n - DAY), n - 4 * HOUR, None);
    let copy = copy_of(&look, n - 600);
    let ok = |h, l: Option<&Look>, c, last: Option<&BusyCopy>| hold(h, l, c, last).cloned();
    assert_eq!(ok(Some(3), Some(&look), CopiedNow::Yes, Some(&copy)), Ok(copy.clone()));
    assert_eq!(ok(Some(3), Some(&look), CopiedNow::No, Some(&copy)), Ok(copy.clone()), "paced: the recorded copy holds it");
    assert_eq!(ok(None, Some(&look), CopiedNow::Yes, Some(&copy)), Err(Hold::Off));
    assert_eq!(ok(Some(3), None, CopiedNow::Yes, Some(&copy)), Err(Hold::NoLook));
    for what in ["am", "rebase", "sequence", "none"] {
        let l = Look { what: what.into(), since: None, ..look.clone() };
        assert_eq!(ok(Some(3), Some(&l), CopiedNow::Yes, Some(&copy)), Err(Hold::Never(what.into())));
    }
    let seq = Look { what: "cherry-pick".into(), sequence: true, ..look.clone() };
    assert_eq!(ok(Some(3), Some(&seq), CopiedNow::Yes, Some(&copy)), Err(Hold::Never("sequence".into())));
    let busy = look_at(n, Some(n - DAY), n - 600, None);
    assert_eq!(ok(Some(3), Some(&busy), CopiedNow::Yes, Some(&copy_of(&busy, n))), Err(Hold::Recent("a file changed less than 3h ago".into())));
    assert_eq!(ok(Some(5), Some(&look), CopiedNow::Yes, Some(&copy)), Err(Hold::Recent("a file changed less than 5h ago".into())));
    // A copy that failed: never, even with an older copy recorded.
    assert_eq!(ok(Some(3), Some(&look), CopiedNow::Failed, Some(&copy)), Err(Hold::CopyFailed));
    // No copy after the last change: none, one of another operation, one of the work tree before.
    assert_eq!(ok(Some(3), Some(&look), CopiedNow::No, None), Err(Hold::NoCopy));
    assert_eq!(ok(Some(3), Some(&look), CopiedNow::No, Some(&BusyCopy { op: "merge 7 abc123".into(), ..copy.clone() })), Err(Hold::NoCopy));
    assert_eq!(ok(Some(3), Some(&look), CopiedNow::No, Some(&BusyCopy { print: "1-1".into(), ..copy.clone() })), Err(Hold::NoCopy));
}

/// The record: missing = nothing recorded (copy); corrupt or another version = nothing recorded
/// plus a warning (copy); written atomically, owner-only, each pod's own; a pod finished is
/// forgotten, and a record a day old dropped. Its state-dir lock is waited for only so long
/// (review ai-review claude #12): held by another past the wait — an error, nothing written.
#[test]
fn the_copy_record_round_trips_and_a_missing_or_corrupt_one_means_copy() {
    let dir = std::env::temp_dir().join(format!("arena-busy-copies-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("devtest").join(BUSY_COPIES_FILE);
    let n = 1_760_900_000;
    let wait = RECORD_LOCK_WAIT;
    let look = look_at(n, Some(n - DAY), n - DAY, None);
    assert_eq!(BusyCopies::load(&path), (BusyCopies::default(), None), "missing: nothing recorded, no warning");
    assert_eq!(copy_needed(Some(&look), BusyCopies::load(&path).0.pods.get("devtest-a"), n), Some(CopyWhy::First));
    let a = copy_of(&look, n - 60);
    let b = BusyCopy { to: "/b/w1d1/devtest-b".into(), label: "w1d1/devtest-b".into(), ..a.clone() };
    assert_eq!(record_busy_copies(&path, &[("devtest-a".into(), Some(a.clone())), ("devtest-b".into(), Some(b.clone()))], n, wait).unwrap(), None);
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
    }
    let (got, warning) = BusyCopies::load(&path);
    assert_eq!((got.pods.get("devtest-a"), got.pods.get("devtest-b"), warning), (Some(&a), Some(&b), None));
    assert_eq!(copy_needed(Some(&look), got.pods.get("devtest-a"), n), None, "recorded and unchanged: not copied");
    // Finished: forgotten; the other kept. A day later, a write drops what's left.
    record_busy_copies(&path, &[("devtest-a".into(), None)], n, wait).unwrap();
    assert_eq!(BusyCopies::load(&path).0.pods.keys().collect::<Vec<_>>(), ["devtest-b"]);
    record_busy_copies(&path, &[], n + FORGET_AFTER_SECS, wait).unwrap();
    assert!(BusyCopies::load(&path).0.pods.is_empty(), "a day old: dropped");
    // Corrupt, or another version: nothing recorded (every BUSY pod copied), said; the next write replaces it.
    for bad in ["{not json", "{\"version\": 2, \"pods\": {}}", ""] {
        std::fs::write(&path, bad).unwrap();
        let (got, warning) = BusyCopies::load(&path);
        assert!(got.pods.is_empty(), "{bad}");
        assert!(warning.as_deref().is_some_and(|w| w.starts_with("warning: ignoring ")), "{bad}");
        assert_eq!(copy_needed(Some(&look), got.pods.get("devtest-a"), n), Some(CopyWhy::First), "{bad}");
        let warned = record_busy_copies(&path, &[("devtest-a".into(), Some(a.clone()))], n, wait).unwrap();
        assert!(warned.is_some(), "{bad}");
        assert_eq!(BusyCopies::load(&path).0.pods.get("devtest-a"), Some(&a));
    }
    // The state dir locked by another (the balance state, the health cache): waited for, not past
    // the wait — then nothing written. Free again: written.
    let holder = std::fs::File::open(path.parent().unwrap()).unwrap();
    holder.lock().unwrap();
    let started = std::time::Instant::now();
    let e = record_busy_copies(&path, &[("devtest-a".into(), None)], n, std::time::Duration::from_millis(300)).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::WouldBlock, "{e}");
    assert!(started.elapsed() >= std::time::Duration::from_millis(300) && started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(BusyCopies::load(&path).0.pods.get("devtest-a"), Some(&a), "nothing written");
    holder.unlock().unwrap();
    record_busy_copies(&path, &[("devtest-a".into(), None)], n, std::time::Duration::from_millis(300)).unwrap();
    assert!(BusyCopies::load(&path).0.pods.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Where a copy holds the checkout: the pull is the home or a directory the checkout's top is in
/// (or is) — the top every link resolved, or the configured repo path when it names that top
/// through a link (setup's `/root/ARENA_materials -> /workspace/ARENA_materials`, which the pull
/// splits out and copies as a tree in its place: review ai-review claude #6).
#[test]
fn a_copy_holds_the_checkout_when_it_pulls_the_home_or_a_dir_the_checkout_is_in() {
    let look = |top: &str, repo: Option<&str>, home: &str| Look { top: top.into(), repo: repo.map(String::from), home: home.into(), ..look_at(1, Some(1), 1, None) };
    let repo = look("/root/ARENA_materials", Some("/root/ARENA_materials"), "/root");
    // On the volume, linked from the home: the look's top is the volume's, its REPO the link.
    let linked = look("/workspace/ARENA_materials", Some("/root/ARENA_materials"), "/root");
    let unlinked = look("/workspace/ARENA_materials", None, "/root");
    let cases: [(&str, &Look, &str, Option<&str>); 18] = [
        ("", &repo, "root", Some("ARENA_materials")),
        ("~/", &repo, "root", Some("ARENA_materials")),
        ("/root", &repo, "root", Some("root/ARENA_materials")),
        ("/root/", &repo, "root", Some("ARENA_materials")),
        ("/root/ARENA_materials/", &repo, "root", Some("")),
        ("/root/ARENA_materials", &repo, "root", Some("ARENA_materials")),
        ("/", &repo, "root", Some("root/ARENA_materials")),
        ("/root/ARENA_materials/chapter1", &repo, "root", None),
        ("/workspace", &repo, "root", None),
        ("/root/ARENA", &repo, "root", None),
        ("$HOME", &repo, "root", None),
        // The home the look didn't say: the login's.
        ("", &look("/home/ubuntu/ARENA_materials", None, ""), "ubuntu", Some("ARENA_materials")),
        ("", &look("/root/ARENA_materials", None, ""), "ubuntu", None),
        // Linked onto the volume: the home pull holds it where the link is; a pull of the volume
        // where it really is.
        ("", &linked, "root", Some("ARENA_materials")),
        ("~/", &linked, "root", Some("ARENA_materials")),
        ("/workspace/", &linked, "root", Some("ARENA_materials")),
        ("", &unlinked, "root", None),
        ("/workspace/", &unlinked, "root", Some("ARENA_materials")),
    ];
    for (remote_path, l, user, want) in cases {
        assert_eq!(copy_place(remote_path, l, user).as_deref(), want, "{remote_path:?} {} {:?} {user}", l.top, l.repo);
        assert_eq!(copy_holds(remote_path, l, user), want.is_some(), "{remote_path:?} {} {user}", l.top);
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
/// system config (the build machine's own settings never count), a `who` that lists nobody
/// unless [`Repo::login`] says otherwise, and the pod's `/dev/pts` and `/proc` stood in for by
/// two directories of the sandbox, empty unless [`Repo::pts`] / [`Repo::process`] fill them (the
/// build machine's own terminals and git processes — other tests' — never count). The base
/// commit, 20 days old, has `README.md`, `notes.py`, `old.py`, `data.bin` (binary), `link` (→
/// README.md), `sub dir/we*ird.py`, and a `.gitignore` of `ignored/`.
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
        for d in ["home", "bin", "proc", "pts"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
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

    /// `git pull upstream main`, which must conflict.
    fn pull_conflicted(&self) {
        let (ok, said) = self.git_try(&self.work, &["pull", "-q", "--no-rebase", "--no-edit", "upstream", "main"]);
        assert!(!ok && self.work.join(".git/MERGE_HEAD").exists(), "no conflict: {said}");
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

    /// A pseudo-terminal `/dev/pts/<n>` of the stand-in, last read `read` and written `written`
    /// seconds ago; `None` removes it.
    fn pts(&self, n: u32, used: Option<(u64, u64)>) {
        let f = self.base.join("pts").join(n.to_string());
        let Some((read, written)) = used else {
            let _ = std::fs::remove_file(&f);
            return;
        };
        std::fs::write(&f, "").unwrap();
        for (flag, ago) in [("-a", read), ("-m", written)] {
            let o = std::process::Command::new("touch").args([flag, "-d", &format!("@{}", self.now - ago)]).arg(&f).output().unwrap();
            assert!(o.status.success());
        }
    }

    /// A process of the stand-in `/proc`: its name, its arguments and its session; `None` removes it.
    fn process(&self, pid: u32, it: Option<(&str, &[&str], u64)>) {
        let d = self.base.join("proc").join(pid.to_string());
        let _ = std::fs::remove_dir_all(&d);
        let Some((name, args, sid)) = it else { return };
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("comm"), format!("{name}\n")).unwrap();
        std::fs::write(d.join("cmdline"), args.iter().map(|a| format!("{a}\0")).collect::<String>()).unwrap();
        std::fs::write(d.join("stat"), format!("{pid} ({name}) S 1 {pid} {sid} 0 -1 4194560 0 0 0 0\n")).unwrap();
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
            .env("ARENA_TEST_PROC_DIR", self.base.join("proc"))
            .env("ARENA_TEST_PTS_DIR", self.base.join("pts"))
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
        self.look_of(self.work.to_str().unwrap())
    }

    /// The look of the repo at `repo` (as configured: a link to the checkout, say).
    fn look_of(&self, repo: &str) -> Look {
        let (code, out, err) = self.sh(&look_command(repo));
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
        self.pull_conflicted();
    }

    fn unmerged(&self) -> String {
        self.git(&self.work, &["ls-files", "-u"])
    }

    /// Nothing touched by a refused resolve: the merge as it was (`conflicts`: `git ls-files -u`
    /// before), nothing kept in `.git/arena-resolved/`.
    fn untouched(&self, conflicts: &str, why: &str) {
        assert!(self.work.join(".git/MERGE_HEAD").exists(), "{why}: the merge ended");
        assert_eq!(self.unmerged(), conflicts, "{why}: a conflict resolved");
        assert!(!self.work.join(".git/arena-resolved").exists(), "{why}: something done");
    }
}

#[cfg(unix)]
impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// This test process's session (the 4th field after its name in `/proc/self/stat`): what the
/// look, run from here, shares — its own processes.
#[cfg(unix)]
fn own_session() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    stat.rsplit_once(") ").unwrap().1.split_whitespace().nth(3).unwrap().parse().unwrap()
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
    assert_eq!(hold(Some(3), Some(&look), CopiedNow::Yes, Some(&copy_of(&look, r.now))), Err(Hold::Never("am".into())));
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
    assert_eq!(hold(Some(3), Some(&look), CopiedNow::Yes, Some(&copy_of(&look, r.now))), Err(Hold::Never("sequence".into())));
    let (code, rep, _) = r.resolve(3, &look);
    assert_eq!((code, rep.refused.as_deref()), (Some(RESOLVE_REFUSED_EXIT), Some("a cherry-pick or revert sequence: never finished automatically")));
    assert!(r.work.join(".git/CHERRY_PICK_HEAD").exists() && !r.unmerged().is_empty());
}

/// Activity keeps it BUSY (review ai-review codex #4, claude #5): a merge started 10 minutes ago, a
/// tracked or an untracked file changed 10 minutes ago (an operation older than the threshold, but
/// recent edits), something staged 10 minutes ago (the index), git's index locked, a terminal login
/// that typed 10 minutes ago — and with no login at all (no utmp in the images' containers, VS
/// Code's or Jupyter's terminals), a pseudo-terminal read or written 10 minutes ago, a git command
/// running, an editor open on git's merge message: the gate says so, and the script, called all
/// the same, refuses and touches nothing. Not activity: an ignored file changing, an old login or
/// terminal, the look's own processes (its session), a filesystem monitor's daemon, a process that
/// isn't git's. And a change after the look the copy followed (even one that keeps the file's old
/// mtime): refused.
#[cfg(unix)]
#[test]
fn recent_activity_or_a_change_since_the_copy_leaves_it_busy() {
    let r = Repo::new("recent");
    r.merge_conflict(r.now - 10 * DAY, r.now - 5 * DAY);
    let conflicts = r.unmerged();
    let busy = "a git command, or an editor one waits on, is running";
    // (what happened, the gate's words, the script's)
    type Recent<'a> = (Box<dyn Fn(&Repo)>, String, String);
    let recent = |gate: &str| (gate.to_string(), format!("recent: {gate}"));
    let mut cases: Vec<Recent> = Vec::new();
    let mut add = |f: Box<dyn Fn(&Repo)>, (gate, script): (String, String)| cases.push((f, gate, script));
    add(Box::new(|r: &Repo| r.touch(".git/MERGE_HEAD", 600)), recent("the merge started less than 3h ago"));
    add(Box::new(|r: &Repo| r.touch("notes.py", 600)), recent("a file changed less than 3h ago"));
    add(
        Box::new(|r: &Repo| {
            r.put("new.txt", b"new\n");
            r.touch("new.txt", 600);
        }),
        recent("a file changed less than 3h ago"),
    );
    add(Box::new(|r: &Repo| r.touch(".git/index", 600)), recent("git's index changed less than 3h ago (something was staged)"));
    add(
        Box::new(|r: &Repo| std::fs::write(r.work.join(".git/index.lock"), "").unwrap()),
        (
            "git's index is locked (.git/index.lock): a git command is at work, or died holding it".to_string(),
            "busy: git's index is locked (.git/index.lock): a git command is at work, or died holding it".to_string(),
        ),
    );
    add(Box::new(|r: &Repo| r.login(Some(600))), recent("a terminal login typed less than 3h ago"));
    add(Box::new(|r: &Repo| r.pts(3, Some((600, 5 * HOUR)))), recent("a terminal was used less than 3h ago"));
    add(Box::new(|r: &Repo| r.pts(4, Some((5 * HOUR, 600)))), recent("a terminal was used less than 3h ago"));
    add(
        Box::new(|r: &Repo| r.process(4242, Some(("git", &["git", "commit"], 4242)))),
        (format!("{busy} (pid 4242: git)"), format!("busy: {busy} (pid 4242: git)")),
    );
    add(
        Box::new(|r: &Repo| r.process(4343, Some(("vim", &["vim", "/root/ARENA_materials/.git/MERGE_MSG"], 4300)))),
        (format!("{busy} (pid 4343: vim)"), format!("busy: {busy} (pid 4343: vim)")),
    );
    add(
        Box::new(|r: &Repo| r.process(4444, Some(("nano", &["nano", ".git/COMMIT_EDITMSG"], 4300)))),
        (format!("{busy} (pid 4444: nano)"), format!("busy: {busy} (pid 4444: nano)")),
    );
    for (happened, gate, script) in cases {
        r.age(4 * HOUR);
        happened(&r);
        let look = r.look();
        assert_eq!(not_quiet(&look, 3).as_deref(), Some(gate.as_str()), "{look:?}");
        let (code, rep, said) = r.resolve(3, &look);
        assert_eq!((code, rep.refused.as_deref()), (Some(RESOLVE_REFUSED_EXIT), Some(script.as_str())), "{said}");
        r.untouched(&conflicts, &gate);
        let _ = std::fs::remove_file(r.work.join("new.txt"));
        let _ = std::fs::remove_file(r.work.join(".git/index.lock"));
        r.login(None);
        for n in [3, 4] {
            r.pts(n, None);
        }
        for pid in [4242, 4343, 4444] {
            r.process(pid, None);
        }
    }
    // Changed after the look the copy followed — same mtime, new content: refused.
    r.age(4 * HOUR);
    let look = r.look();
    r.put("README.md", b"edited again\n");
    r.touch("README.md", 4 * HOUR);
    let (code, rep, said) = r.resolve(3, &look);
    assert_eq!((code, rep.refused.as_deref()), (Some(RESOLVE_REFUSED_EXIT), Some("changed: files changed since the copy")), "{said}");
    r.untouched(&conflicts, "changed");
    // An ignored file written now, an old login and terminal, the look's own git (its session), a
    // filesystem monitor's daemon, a process that isn't git's: quiet — finished.
    r.git(&r.work, &["checkout", "--conflict=merge", "--", "README.md"]);
    r.age(4 * HOUR);
    r.put("ignored/run.log", b"step 1\n");
    r.login(Some(5 * HOUR));
    r.pts(5, Some((5 * HOUR, 5 * HOUR)));
    r.process(5001, Some(("git", &["git", "status"], own_session())));
    r.process(5002, Some(("git", &["git", "fsmonitor--daemon", "run"], 5002)));
    r.process(5003, Some(("python3", &["python3", "train.py", "--resume"], 5003)));
    let look = r.look();
    assert_eq!(not_quiet(&look, 3), None, "{look:?}");
    assert_eq!((look.input, look.tty), (Some(r.now - 5 * HOUR), Some(r.now - 5 * HOUR)));
    let (code, rep, said) = r.resolve(3, &look);
    assert_eq!((code, rep.resolved.map(|(w, s, ..)| (w, s))), (Some(0), Some(("merge".to_string(), "ours".to_string()))), "{said}");
}

/// A repo linked onto the volume (review ai-review claude #6): the look gives the checkout's top as
/// it really is and the configured path that names it, so a pull of the home — which copies the
/// tree in the link's place — counts as holding it.
#[cfg(unix)]
#[test]
fn a_repo_linked_onto_a_volume_is_held_by_a_pull_of_the_home() {
    let r = Repo::new("linked");
    let home = r.base.join("home");
    std::os::unix::fs::symlink(&r.work, home.join("ARENA_materials")).unwrap();
    let configured = home.join("ARENA_materials").display().to_string();
    let look = r.look_of(&configured);
    let top = std::fs::canonicalize(&r.work).unwrap().display().to_string();
    assert_eq!((look.top.clone(), look.repo.clone(), look.home.clone()), (top.clone(), Some(configured.clone()), home.display().to_string()));
    assert_eq!(copy_place("", &look, "root").as_deref(), Some("ARENA_materials"));
    assert_eq!(copy_place("", &Look { repo: None, ..look.clone() }, "root"), None, "the top alone isn't in the home");
    // The configured path a directory elsewhere than the top (here: inside it): no REPO.
    let inside = r.work.join("sub dir").display().to_string();
    assert_eq!(r.look_of(&inside).repo, None);
}

/// The copy's fingerprint holds each file's ctime (review ai-review codex #6): a file replaced
/// after the look by one of the same size, its old mtime kept (`cp -p`, `rsync -a`), is a change —
/// refused. Quiet is judged by mtime still: files aged with `touch -d` are old.
#[cfg(unix)]
#[test]
fn an_equal_size_replacement_that_keeps_the_mtime_is_a_change() {
    let r = Repo::new("ctime");
    r.merge_conflict(r.now - 5 * DAY, r.now - 10 * DAY);
    r.age(4 * HOUR);
    let look = r.look();
    let conflicts = r.unmerged();
    let was = r.read("README.md");
    let mut now = was.clone();
    let i = now.iter().position(|b| *b == b'c').unwrap();
    now[i] = b'C';
    let swap = r.base.join("swap");
    std::fs::write(&swap, &now).unwrap();
    let (code, _, err) = r.sh(&format!("touch -d @{} '{}' && cp -p '{}' '{}'", r.now - 4 * HOUR, swap.display(), swap.display(), r.work.join("README.md").display()));
    assert_eq!(code, Some(0), "{err}");
    let again = r.look();
    assert_eq!((again.newest, r.read("README.md").len()), (look.newest, was.len()), "same mtime, same size");
    assert_ne!(again.print, look.print);
    let (code, rep, said) = r.resolve(3, &look);
    assert_eq!((code, rep.refused.as_deref()), (Some(RESOLVE_REFUSED_EXIT), Some("changed: files changed since the copy")), "{said}");
    r.untouched(&conflicts, "ctime");
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
