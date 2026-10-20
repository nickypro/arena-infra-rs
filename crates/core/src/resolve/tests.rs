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
        changed: Some("notes.py".into()),
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
               NEWEST 1760700000 42\nCHANGED sub dir/we*ird.py\nPRINT 3735928559-1234\nINDEX 1760750000\nLOCKED index.lock\nINPUT 1760600000\n\
               TTY 1760650000\nPROC 4242 vim\n";
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
            changed: Some("sub dir/we*ird.py".into()),
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
    let bare = [
        "REPO /root/ARENA_materials\n",
        "CHANGED sub dir/we*ird.py\n",
        "INDEX 1760750000\n",
        "LOCKED index.lock\n",
        "INPUT 1760600000\n",
        "TTY 1760650000\n",
        "PROC 4242 vim\n",
    ]
    .iter()
    .fold(out.to_string(), |o, l| o.replace(l, ""));
    let l = parse_look(&bare).unwrap();
    assert_eq!((l.repo, l.changed, l.index, l.locked, l.input, l.tty, l.at_work), (None, None, None, false, None, None, None));
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

/// A BUSY pod left past the threshold but held back says why, with how long ago on the pod's clock
/// (Live 5: one sat unfinished, nothing said — its index had been written by a `git status`). An
/// operation younger than the threshold is no candidate yet: nothing said; nor with the setting
/// off, no look, or a revert (its own line).
#[test]
fn a_pod_held_back_past_the_threshold_is_said_why_with_how_long_ago() {
    assert_eq!(
        [0, 59, 60, 12 * 60 + 59, 3600, 3600 + 5 * 60, 26 * 3600].map(|d| ago_words(10_000_000, 10_000_000 - d)),
        ["0 s ago", "59 s ago", "1 min ago", "12 min ago", "1 h ago", "1 h 5 min ago", "26 h ago"].map(String::from)
    );
    assert_eq!(ago_words(100, 160), "in the future, by the pod's clock");
    let n = 1_760_900_000;
    let base = look_at(n, Some(n - DAY), n - 4 * HOUR, None);
    let with = |f: &dyn Fn(&mut Look)| {
        let mut l = base.clone();
        f(&mut l);
        l
    };
    let recent = |l: &Look| Hold::Recent(not_quiet(l, 3).unwrap());
    let waits = " — it waits for 3h of quiet";
    let file = with(&|l| l.newest = n - 40 * 60);
    let unnamed = with(&|l| {
        l.newest = n - 40 * 60;
        l.changed = None
    });
    let index = with(&|l| l.index = Some(n - 12 * 60));
    let locked = with(&|l| l.locked = true);
    let typed = with(&|l| l.input = Some(n - 90));
    let tty = with(&|l| l.tty = Some(n - 5 * 60));
    let git = with(&|l| l.at_work = Some("4242 git".into()));
    let sequence = with(&|l| {
        l.what = "cherry-pick".into();
        l.sequence = true
    });
    let cases: Vec<(Hold, &Look, Option<String>)> = vec![
        (recent(&file), &file, Some(format!("notes.py changed 40 min ago{waits}"))),
        (recent(&unnamed), &unnamed, Some(format!("a file changed 40 min ago{waits}"))),
        (recent(&index), &index, Some(format!("git's index was written 12 min ago (something staged, or a git command such as `git status` refreshing it){waits}"))),
        (recent(&locked), &locked, Some("git's index is locked (.git/index.lock): a git command is at work, or died holding it".into())),
        (recent(&typed), &typed, Some(format!("a terminal login typed 1 min ago{waits}"))),
        (recent(&tty), &tty, Some(format!("a terminal was used 5 min ago{waits}"))),
        (recent(&git), &git, Some("a git command, or an editor one waits on, is running (pid 4242: git)".into())),
        (Hold::CopyFailed, &base, Some("this run's copy of it failed (said above): it's finished only from a whole copy taken after its last change".into())),
        (Hold::NoCopy, &base, Some("no copy recorded after its last change: it's finished only from a whole copy taken after it".into())),
        (Hold::Never("sequence".into()), &sequence, Some("a cherry-pick or revert sequence is in progress: never finished automatically".into())),
        (Hold::Never("revert".into()), &base, None),
    ];
    for (hold, look, want) in cases {
        assert_eq!(held_words(&hold, Some(look), Some(3)), want, "{hold:?}");
    }
    // Nothing said: the operation younger than the threshold (or in the pod's future), the setting
    // off, no look.
    for since in [n - 3 * HOUR + 1, n - 600, n + 60] {
        let young = Look { since: Some(since), ..file.clone() };
        assert_eq!(held_words(&recent(&young), Some(&young), Some(3)), None, "{since}");
    }
    assert_eq!(held_words(&Hold::NoCopy, Some(&Look { since: None, ..base.clone() }), Some(3)), None);
    assert_eq!(held_words(&Hold::Off, Some(&base), None), None);
    assert_eq!(held_words(&Hold::NoLook, None, Some(3)), None);
    // Its own threshold: 5h, a merge a day old, a file 4h old.
    assert_eq!(held_words(&Hold::Recent(String::new()), Some(&base), Some(5)), Some("notes.py changed 4 h ago — it waits for 5h of quiet".into()));
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

/// `p` (absolute) as a path relative to this process's working directory: `../` up to the root,
/// then `p` — what a relative `LOCAL_BACKUP_DIR` is.
fn relative_to_cwd(p: &Path) -> String {
    let cwd = std::env::current_dir().unwrap();
    let up = cwd.components().filter(|c| matches!(c, std::path::Component::Normal(_))).count();
    format!("{}{}", "../".repeat(up), p.display().to_string().trim_start_matches('/'))
}

/// An earlier run's copy is trusted (to skip a copy, or to resolve on) only while it is under the
/// backup dir as configured now and its `.git` is still on this machine (review ai-review codex
/// #7): a copy deleted or moved since, a `LOCAL_BACKUP_DIR` changed since, or a record from before
/// the `.git` was kept, is none — the pod is copied now. Compared as the directories they are
/// (review ai-review claude #8): the backup dir spelled through a link or relative to the working
/// directory is the same dir, and so is a record spelled through a link; a record of a relative
/// path (some other run's working directory) is none.
#[test]
fn a_recorded_copy_counts_only_while_it_is_still_there_under_this_backup_dir() {
    let base = std::env::temp_dir().join(format!("arena-copy-there-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let base = std::fs::canonicalize(&base).unwrap();
    let dir = base.join("backups").display().to_string();
    let to = format!("{dir}/w1d1/devtest-a");
    let git = format!("{to}/ARENA_materials/.git");
    std::fs::create_dir_all(&git).unwrap();
    for other in ["elsewhere", "backups-old", "x/.git"] {
        std::fs::create_dir_all(base.join(other)).unwrap();
    }
    let copy = BusyCopy { op: "merge 1 a".into(), print: "1-2".into(), newest: 1, at: 1, to: to.clone(), label: "w1d1/devtest-a".into(), git: git.clone() };
    assert!(copy_still_there(&copy, &dir));
    assert!(copy_still_there(&copy, &format!("{dir}/")), "a trailing slash is the same dir");
    // The same dir spelled through a link, or relative to the working directory; a record spelled
    // through the link: the same copy.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(base.join("backups"), base.join("link")).unwrap();
        let link = base.join("link").display().to_string();
        assert!(copy_still_there(&copy, &link), "the backup dir through a link");
        let linked = BusyCopy { to: format!("{link}/w1d1/devtest-a"), git: format!("{link}/w1d1/devtest-a/ARENA_materials/.git"), ..copy.clone() };
        assert!(copy_still_there(&linked, &dir) && copy_still_there(&linked, &link), "a record through the link");
        // The link pointed at another dir since: another tree.
        std::fs::remove_file(base.join("link")).unwrap();
        std::os::unix::fs::symlink(base.join("elsewhere"), base.join("link")).unwrap();
        assert!(!copy_still_there(&copy, &link), "the link pointed elsewhere since");
    }
    let rel = relative_to_cwd(Path::new(&dir));
    assert!(!Path::new(&rel).is_absolute() && copy_still_there(&copy, &rel), "relative: {rel}");
    let relative = BusyCopy { to: relative_to_cwd(Path::new(&to)), git: relative_to_cwd(Path::new(&git)), ..copy.clone() };
    assert!(!copy_still_there(&relative, &dir), "a record of a relative path: whose working directory?");
    // LOCAL_BACKUP_DIR changed since (another dir; one that merely starts the same).
    assert!(!copy_still_there(&copy, &base.join("elsewhere").display().to_string()));
    assert!(!copy_still_there(&copy, &format!("{dir}-old")));
    assert!(!copy_still_there(&copy, ""));
    assert!(!copy_still_there(&copy, &to), "the copy itself is no backup dir");
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

/// Part 1's gate: on, a look, an operation finished here and not a sequence or a revert, quiet,
/// this run's copy not failed, and a recorded copy of this operation with the work tree as it is
/// now.
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
    // A revert, however quiet: never (review ai-review claude #2).
    let revert = Look { what: "revert".into(), ..look.clone() };
    assert!(!finishes("revert"));
    assert_eq!(ok(Some(3), Some(&revert), CopiedNow::Yes, Some(&copy_of(&revert, n - 600))), Err(Hold::Never("revert".into())));
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
    let out = "SAVED /root/ARENA_materials/.git/arena-resolved/20261019T140200Z\nHAND notes.py\n\
               NEWER theirs hunks 1760000000 1760500000 README.md\nGONE theirs 1760000000 1760500000 old.py\n\
               NEWER ours file 1760600000 1760500000 sub dir/we*ird.py\nREDACTED 1 notes.py\n[autocommit-x 1234] x\nRESOLVED merge\n";
    let r = parse_resolve(out);
    assert_eq!(r.resolved.as_deref(), Some("merge"));
    let c = |path: &str, side: &str, taken, ours, theirs| Choice { path: path.into(), side: side.into(), taken, ours, theirs };
    assert_eq!(
        r.newer,
        [
            c("README.md", "theirs", Taken::Hunks, 1_760_000_000, 1_760_500_000),
            c("old.py", "theirs", Taken::Gone, 1_760_000_000, 1_760_500_000),
            c("sub dir/we*ird.py", "ours", Taken::File, 1_760_600_000, 1_760_500_000),
        ]
    );
    assert_eq!((r.hand.clone(), r.redacted.clone()), (vec!["notes.py".to_string()], vec!["notes.py".to_string()]));
    assert_eq!(r.saved.as_deref(), Some("/root/ARENA_materials/.git/arena-resolved/20261019T140200Z"));
    // Times as the words give them: UTC, to the minute.
    assert_eq!((minute_words(1_760_000_000), minute_words(1_760_500_000)), ("2025-10-09 08:53Z".to_string(), "2025-10-15 03:46Z".to_string()));
    // Each path's way, with both sides' last change; the state before first, then today's copy.
    assert_eq!(
        resolved_note(&r, 3, "/b/w1d1/devtest-a"),
        "finished a merge left for 3h+ (README.md: MERGE_HEAD's side of each conflict, HEAD 2025-10-09 08:53Z / MERGE_HEAD \
         2025-10-15 03:46Z; old.py: removed, as on MERGE_HEAD, HEAD 2025-10-09 08:53Z / MERGE_HEAD 2025-10-15 03:46Z; sub \
         dir/we*ird.py: HEAD's version, HEAD 2025-10-16 07:33Z / MERGE_HEAD 2025-10-15 03:46Z; notes.py: as resolved by hand; \
         keys redacted in the commit (kept on disk): notes.py; before: \
         /root/ARENA_materials/.git/arena-resolved/20261019T140200Z/ on the pod; today's copy: /b/w1d1/devtest-a)"
    );
    let ours = parse_resolve("NEWER ours hunks 2 1 a.py\nRESOLVED cherry-pick\n");
    assert_eq!(
        resolved_note(&ours, 5, "/b/x"),
        "finished a cherry-pick left for 5h+ (a.py: HEAD's side of each conflict, HEAD 1970-01-01 00:00Z / CHERRY_PICK_HEAD \
         1970-01-01 00:00Z; today's copy: /b/x)"
    );
    let stash = parse_resolve("HAND a.py\nRESOLVED conflicts\n");
    assert_eq!(
        resolved_note(&stash, 3, "/b/x"),
        "resolved the conflicts a `git stash pop` left for 3h+ (its stash entry kept) (a.py: as resolved by hand; today's copy: /b/x)"
    );
    let clean = parse_resolve("RESOLVED merge\n");
    assert_eq!(resolved_note(&clean, 3, "/b/x"), "finished a merge left for 3h+ (nothing was left in conflict; today's copy: /b/x)");
    let refused = parse_resolve("REFUSED changed: files changed since the copy\n");
    assert_eq!((refused.refused.as_deref(), refused.resolved), (Some("changed: files changed since the copy"), None));
    assert_eq!(parse_resolve("SAVED /x\nNEWER ours file 1 2 a\nFAILED git commit\n").failed.as_deref(), Some("git commit"));
    // Lines it can't read are left out.
    // (A notebook is never taken whole any more: `notjson` and `nocheck` are no way to read.)
    for bad in [
        "NEWER theirs hunks soon 1 a.py",
        "NEWER sideways file 1 2 a.py",
        "NEWER ours lines 1 2 a.py",
        "NEWER theirs notjson 1 2 nb.ipynb",
        "NEWER ours nocheck 1 2 nb.ipynb",
        "GONE ours 1 2",
        "RESOLVED",
    ] {
        let r = parse_resolve(&format!("{bad}\n"));
        assert!(r.newer.is_empty() && r.resolved.is_none(), "{bad}: {r:?}");
    }
}

/// The scripts are POSIX sh: dash (the images' `sh`) parses them. The resolve's command is
/// `backup::bounded_sh`'s for its script — its arguments its first line — bounded on the pod a
/// little under the CLI's timeout (review ai-review claude #7): 270 s, then a KILL 10 s later,
/// with its own words for a resolve the bound stopped. Checked against `bounded_sh`'s own output,
/// never the wrapper's text, which is backup's to change.
#[cfg(unix)]
#[test]
fn the_scripts_parse_under_sh() {
    let cmd = resolve_command("/root/ARENA_materials", 3, "merge 1760000000 abc", "1-2", "w1d1/devtest-a");
    let script = format!("set -- /root/ARENA_materials 10800 'merge 1760000000 abc' 1-2 3 w1d1/devtest-a 99614720 104857600\n{}", resolve_script());
    for (name, script) in [("look", look_script()), ("resolve", resolve_script()), ("resolve with its arguments", script.clone())] {
        let o = std::process::Command::new("sh").arg("-n").arg("-c").arg(&script).output().unwrap();
        assert!(o.status.success(), "{name}: {}", String::from_utf8_lossy(&o.stderr));
        assert!(!script.contains("@ARENA_"), "{name}: a mark left unrendered");
    }
    assert!(resolve_script().contains(&format!("REVERT_WORDS={}", shell_word(REVERT_WORDS))));
    assert!(look_command("/root/my repo").ends_with(" arena-look '/root/my repo'"));
    let (stopped, killed) = resolve_stop_words(270, 10);
    assert!(cmd == crate::backup::bounded_sh(&script, "arena-resolve", 270, 10, &stopped, &killed), "bounded_sh's, for the resolve's script");
    assert!(stopped.starts_with("arena resolve: it ran past its 270 s on the pod and was stopped there (TERM)"), "{stopped}");
    assert!(killed.starts_with("arena resolve: it ran past its 270 s on the pod and was killed there (10 s after the TERM)"), "{killed}");
    assert_eq!((RESOLVE_BOUND_SECS, RESOLVE_KILL_AFTER_SECS), (270, 10));
    // A resolve's temporary files go when a signal ends it: HUP, INT, TERM and PIPE are exits.
    for (sig, code) in [("HUP", 129), ("INT", 130), ("TERM", 143), ("PIPE", 141)] {
        assert!(resolve_script().contains(&format!("trap 'exit {code}' {sig}\n")), "{sig}");
    }
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

    /// More files in the base (19 days old), on main, the course and the checkout alike.
    fn add_base(&self, files: &[(&str, &[u8])]) {
        self.git(&self.work, &["checkout", "-q", "main"]);
        for (rel, bytes) in files {
            self.put(rel, bytes);
        }
        self.commit_all(&self.work, "more base", self.now - 19 * DAY);
        self.git(&self.work, &["push", "-q", "origin", "main"]);
        self.git(&self.work, &["checkout", "-q", "autocommit-x"]);
        self.git(&self.work, &["merge", "-q", "--ff-only", "main"]);
        self.git(&self.course, &["pull", "-q", "--ff-only", "origin", "main"]);
    }

    /// A commit of the course's (on upstream's `main`) and one of the participant's, at those times.
    fn course_commit(&self, files: &[(&str, &[u8])], at: u64) {
        for (rel, bytes) in files {
            self.put_in(&self.course, rel, bytes);
        }
        self.commit_all(&self.course, "the course's change", at);
    }

    fn my_commit(&self, files: &[(&str, &[u8])], at: u64) {
        for (rel, bytes) in files {
            self.put(rel, bytes);
        }
        self.commit_all(&self.work, "mine", at);
    }

    /// `git pull upstream main`, which must conflict.
    fn pull_conflicted(&self) {
        let (ok, said) = self.git_try(&self.work, &["pull", "-q", "--no-rebase", "--no-edit", "upstream", "main"]);
        assert!(!ok && self.work.join(".git/MERGE_HEAD").exists(), "no conflict: {said}");
    }

    /// The stand-in `who`: nobody, or one login on a "tty" whose atime is `typed` seconds ago
    /// (a file under the sandbox, reached from `/dev/` by `..`).
    fn login(&self, typed: Option<u64>) {
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
        crate::remote::write_test_script(&who, script).unwrap();
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

    fn text(&self, rel: &str) -> String {
        String::from_utf8(self.read(rel)).unwrap()
    }

    fn link(&self, rel: &str) -> String {
        std::fs::read_link(self.work.join(rel)).unwrap().display().to_string()
    }

    /// A path's content in HEAD's commit.
    fn committed(&self, rel: &str) -> String {
        self.git(&self.work, &["show", &format!("HEAD:{rel}")])
    }

    /// `bash -c cmd` in the sandbox: (exit code, stdout, stderr).
    fn sh(&self, cmd: &str) -> (Option<i32>, String, String) {
        self.sh_with(cmd, &[])
    }

    fn sh_with(&self, cmd: &str, env: &[(&str, &str)]) -> (Option<i32>, String, String) {
        self.shell_with("bash", cmd, env)
    }

    /// `<shell> -c cmd` in the sandbox — the pod's login shell, which sshd hands the command to.
    fn shell_with(&self, shell: &str, cmd: &str, env: &[(&str, &str)]) -> (Option<i32>, String, String) {
        let o = self.cmd(&self.base, shell).arg("-c").arg(cmd).envs(env.iter().copied()).output().expect(shell);
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
        self.resolve_with(hours, look, GIT_MAX_FILE_BYTES_FOR_TESTS, &[])
    }

    /// [`Repo::resolve`] with a size limit and an environment (the tests' hooks).
    fn resolve_with(&self, hours: u64, look: &Look, max: u64, env: &[(&str, &str)]) -> (Option<i32>, ResolveReport, String) {
        let cmd = resolve_command_limited(self.work.to_str().unwrap(), hours, &look.op_key(), &look.print, "w1d1/devtest-x", max);
        let (code, out, err) = self.sh_with(&cmd, env);
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

    /// `refs/arena/course-main` at `rev` (of the checkout): what the backup's fetch from the
    /// configured course URL leaves — the course's own history, verified (written by hand here:
    /// the fetch is the backup's).
    fn verified_course(&self, rev: &str) {
        self.git(&self.work, &["update-ref", "refs/arena/course-main", rev]);
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

/// The backup's own limit, as the CLI passes it.
#[cfg(unix)]
const GIT_MAX_FILE_BYTES_FOR_TESTS: u64 = crate::backup::GIT_MAX_FILE_BYTES;

/// The conflicted paths a resolve took from their newer side, sorted, as `(path, side, how)`.
fn taken(r: &ResolveReport) -> Vec<(String, String, Taken)> {
    let mut v: Vec<_> = r.newer.iter().map(|c| (c.path.clone(), c.side.clone(), c.taken)).collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// This test process's session (the 4th field after its name in `/proc/self/stat`): what the
/// look, run from here, shares — its own processes.
#[cfg(unix)]
fn own_session() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    stat.rsplit_once(") ").unwrap().1.split_whitespace().nth(3).unwrap().parse().unwrap()
}

/// The human's rule, on a merge left alone for 3h+, per conflicted path: the side whose last change
/// to it is the more recent — a text file per hunk, a binary one, a symlink and an odd name whole,
/// one deleted there removed when that side is the newer — except a file the participant resolved
/// by hand (no conflict marker left), kept as it is; a binary file "resolved" is never taken as
/// such. A real merge commit in the repo's own identity whose message names, per path, the side
/// that won and both sides' last change to it, to the minute (item 15b), then where the state
/// before is; that state kept in the git dir; then the backup commits and pushes as always.
#[cfg(unix)]
#[test]
fn a_merge_left_alone_is_finished_by_the_newer_side_and_the_hand_resolved_file_kept() {
    for theirs_newer in [true, false] {
        let r = Repo::new(if theirs_newer { "merge-theirs" } else { "merge-ours" });
        // To the second, so the message's minutes are checked.
        let (course_at, mine_at) = if theirs_newer { (r.now - 5 * DAY + 77, r.now - 10 * DAY + 3) } else { (r.now - 10 * DAY + 77, r.now - 5 * DAY + 3) };
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
        assert_eq!((look.newest, look.input, look.index, look.locked, look.tty, look.at_work.clone()), (r.now - 4 * HOUR, None, Some(r.now - 4 * HOUR), false, None, None));
        assert_eq!(not_quiet(&look, 3), None);
        assert_eq!(not_quiet(&look, 5).as_deref(), Some("the merge started less than 5h ago"));

        let (code, rep, said) = r.resolve(3, &look);
        assert_eq!(code, Some(0), "{said}");
        assert_eq!(rep.resolved.as_deref(), Some("merge"), "{said}");
        assert_eq!(rep.hand, ["notes.py"], "{said}");
        let side = if theirs_newer { "theirs" } else { "ours" };
        let old_py = if theirs_newer { Taken::Gone } else { Taken::File };
        let want: Vec<(String, String, Taken)> = [("README.md", Taken::Hunks), ("data.bin", Taken::File), ("link", Taken::File), ("old.py", old_py), ("sub dir/we*ird.py", Taken::Hunks)]
            .into_iter()
            .map(|(p, t)| (p.to_string(), side.to_string(), t))
            .collect();
        assert_eq!(taken(&rep), want, "{said}");
        assert!(rep.newer.iter().all(|c| (c.ours, c.theirs) == (mine_at, course_at)), "each side's last change: {:?}", rep.newer);
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
        let saved = rep.saved.clone().unwrap();
        let stamp = saved.rsplit('/').next().unwrap();
        let body = r.git(&r.work, &["log", "-1", "--format=%B"]);
        // Item 15b: per path, the side that won and both sides' last change, UTC to the minute.
        let (m, c) = (minute_words(mine_at), minute_words(course_at));
        let dates = format!("last changed: yours (HEAD) {m}, the merged branch (MERGE_HEAD) {c}");
        let (won, its) = if theirs_newer { ("the merged branch won", "its") } else { ("yours won", "your") };
        let old_line = if theirs_newer { format!("- old.py: {won} (removed: deleted there) — {dates}") } else { format!("- old.py: {won} ({its} whole version) — {dates}") };
        assert_eq!(
            body,
            format!(
                "arena: finished a merge left in progress for 3h+\n\nNothing in this checkout changed for 3h+ while a merge was in \
                 progress, so arena finished it: each conflicted file from the side whose last change to it is the more recent (a \
                 tie keeps yours), a file you resolved by hand kept as it is.\n\
                 - README.md: {won} ({its} side of each conflict; the other changes of both sides kept) — {dates}\n\
                 - data.bin: {won} ({its} whole version) — {dates}\n\
                 - link: {won} ({its} whole version) — {dates}\n\
                 - notes.py: kept as you resolved it by hand\n\
                 {old_line}\n\
                 - sub dir/we*ird.py: {won} ({its} side of each conflict; the other changes of both sides kept) — {dates}\n\n\
                 The state before is in .git/arena-resolved/{stamp}/ on this machine (its README says how to see it or do this \
                 again yourself); today's copy: w1d1/devtest-x.\n\nMerge branch 'main' of {} into autocommit-x",
                r.course.display()
            )
        );
        assert!(m.ends_with('Z') && m.len() == "2026-10-20 13:05Z".len(), "{m}");
        // The state before, in the git dir: the markers, the hand edit, the binary — and git's own state.
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

/// "Before:" names something no later copy overwrites (review ai-review claude #8): the folder on
/// the pod holds the conflicted files as they were, git's own state files, the old HEAD, the index
/// (with its conflicts — copying it back is the state before, exactly) and a README that says how
/// to see it or do it again; the message names it first, and the day's copy as today's.
#[cfg(unix)]
#[test]
fn the_state_before_is_kept_whole_on_the_pod() {
    let r = Repo::new("before");
    r.merge_conflict(r.now - 5 * DAY, r.now - 10 * DAY);
    let head = r.git(&r.work, &["rev-parse", "HEAD"]);
    let merged = r.git(&r.work, &["rev-parse", "MERGE_HEAD"]);
    let unmerged = r.unmerged();
    let index = std::fs::read(r.work.join(".git/index")).unwrap();
    let msg = std::fs::read(r.work.join(".git/MERGE_MSG")).unwrap();
    r.age(4 * HOUR);
    let (code, rep, said) = r.resolve(3, &r.look());
    assert_eq!(code, Some(0), "{said}");
    let saved = PathBuf::from(rep.saved.unwrap());
    assert_eq!(std::fs::read_to_string(saved.join("HEAD")).unwrap(), format!("{head}\n"));
    assert_eq!(std::fs::read_to_string(saved.join("MERGE_HEAD")).unwrap().trim(), merged);
    assert_eq!(std::fs::read(saved.join("MERGE_MSG")).unwrap(), msg);
    assert_eq!(std::fs::read_to_string(saved.join("ORIG_HEAD")).unwrap().trim(), head);
    assert_eq!(std::fs::read(saved.join("index")).unwrap(), index, "the index with its conflicts");
    assert_eq!(std::fs::read_to_string(saved.join("unmerged")).unwrap().trim(), unmerged);
    assert!(saved.join("files/README.md").exists());
    let readme = std::fs::read_to_string(saved.join("README")).unwrap();
    for want in [
        "files/      the conflicted files as they were on disk (markers and all)",
        &format!("HEAD        the commit the merge was on: {head}"),
        &format!("To do it again yourself (commit or copy anything newer first: this discards it): git reset --hard {head} && git merge {merged}"),
        "Today's copy on the backup machine: w1d1/devtest-x (a later copy that day replaces it).",
        "- README.md: the merged branch won (its side of each conflict; the other changes of both sides kept) — last changed: yours (HEAD) ",
    ] {
        assert!(readme.contains(want), "{want}\n{readme}");
    }
    let body = r.git(&r.work, &["log", "-1", "--format=%B"]);
    let stamp = saved.file_name().unwrap().to_str().unwrap();
    assert!(
        body.contains(&format!(
            "The state before is in .git/arena-resolved/{stamp}/ on this machine (its README says how to see it or do this again \
             yourself); today's copy: w1d1/devtest-x."
        )),
        "{body}"
    );
}

/// Per hunk, not per file (review ai-review claude #2): only the conflicting hunks take the newer
/// side; the clean changes of both sides in the same file survive — the course's when HEAD's side
/// wins (else the merge commit would record the course's fix as merged and dropped), the
/// participant's when the course's wins.
#[cfg(unix)]
#[test]
fn the_newer_side_wins_each_conflicting_hunk_and_both_sides_clean_hunks_are_kept() {
    let base = b"l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";
    for theirs_newer in [false, true] {
        let r = Repo::new(if theirs_newer { "hunks-theirs" } else { "hunks-ours" });
        r.add_base(&[("lines.txt", base)]);
        let (course_at, mine_at) = if theirs_newer { (r.now - 5 * DAY, r.now - 10 * DAY) } else { (r.now - 10 * DAY, r.now - 5 * DAY) };
        // Both change line 1; the course alone line 10, the participant alone line 5.
        r.course_commit(&[("lines.txt", b"c1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nc10\n")], course_at);
        r.my_commit(&[("lines.txt", b"m1\nl2\nl3\nl4\nm5\nl6\nl7\nl8\nl9\nl10\n")], mine_at);
        r.pull_conflicted();
        assert!(r.text("lines.txt").contains("<<<<<<< HEAD\nm1\n=======\nc1\n>>>>>>> "), "{}", r.text("lines.txt"));
        r.age(4 * HOUR);
        let (code, rep, said) = r.resolve(3, &r.look());
        assert_eq!(code, Some(0), "{said}");
        let side = if theirs_newer { "theirs" } else { "ours" };
        assert_eq!(taken(&rep), [("lines.txt".to_string(), side.to_string(), Taken::Hunks)], "{said}");
        let first = if theirs_newer { "c1" } else { "m1" };
        let want = format!("{first}\nl2\nl3\nl4\nm5\nl6\nl7\nl8\nl9\nc10\n");
        assert_eq!(r.text("lines.txt"), want, "the participant's line 5 and the course's line 10 both kept");
        assert_eq!(format!("{}\n", r.committed("lines.txt")), want, "and committed so");
    }
}

/// Newer per path, not by the tips (review ai-review claude #2): HEAD's tip is the last autocommit,
/// newer than the course's whole commit — yet the course changed a.py after the participant last
/// did, and the participant changed b.py after the course last did. Each path its own newer side.
#[cfg(unix)]
#[test]
fn the_newer_side_is_judged_per_path_by_each_sides_last_change_to_it() {
    let r = Repo::new("per-path");
    r.add_base(&[("a.py", b"a\n"), ("b.py", b"b\n")]);
    let (course_b, course_a) = (r.now - 18 * DAY, r.now - 5 * DAY);
    r.course_commit(&[("b.py", b"b course\n")], course_b);
    r.course_commit(&[("a.py", b"a course\n")], course_a);
    let (mine_a, mine_b, mine_tip) = (r.now - 15 * DAY, r.now - 10 * DAY, r.now - DAY);
    r.my_commit(&[("a.py", b"a mine\n")], mine_a);
    r.my_commit(&[("b.py", b"b mine\n")], mine_b);
    r.my_commit(&[("c.txt", b"an autocommit\n")], mine_tip);
    r.pull_conflicted();
    r.age(4 * HOUR);
    let (code, rep, said) = r.resolve(3, &r.look());
    assert_eq!(code, Some(0), "{said}");
    let mut got: Vec<(String, String, u64, u64)> = rep.newer.iter().map(|c| (c.path.clone(), c.side.clone(), c.ours, c.theirs)).collect();
    got.sort();
    assert_eq!(got, [("a.py".to_string(), "theirs".to_string(), mine_a, course_a), ("b.py".to_string(), "ours".to_string(), mine_b, course_b)]);
    assert_eq!((r.text("a.py"), r.text("b.py")), ("a course\n".to_string(), "b mine\n".to_string()));
    let body = r.git(&r.work, &["log", "-1", "--format=%B"]);
    assert!(
        body.contains(&format!(
            "- a.py: the merged branch won (its side of each conflict; the other changes of both sides kept) — last changed: yours \
             (HEAD) {}, the merged branch (MERGE_HEAD) {}\n",
            minute_words(mine_a),
            minute_words(course_a)
        )),
        "{body}"
    );
    assert!(body.contains(&format!("- b.py: yours won (your side of each conflict; the other changes of both sides kept) — last changed: yours (HEAD) {}, ", minute_words(mine_b))), "{body}");
}

/// A single cherry-pick: finished by `--continue` (its own message and authorship, with the words
/// added, the picked commit named); one whose result is empty — the newer side HEAD's everywhere —
/// committed empty, as git advises.
#[cfg(unix)]
#[test]
fn a_cherry_pick_left_alone_is_finished_by_continuing() {
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
        let side = if theirs_newer { "theirs" } else { "ours" };
        assert_eq!(taken(&rep), [("README.md".to_string(), side.to_string(), Taken::Hunks)], "{case}");
        assert!(!r.work.join(".git/CHERRY_PICK_HEAD").exists(), "{case}");
        assert_eq!(r.git(&r.work, &["rev-parse", "HEAD~1"]), head, "{case}: one commit on top");
        assert_eq!(r.read("README.md"), if theirs_newer { b"side\n".to_vec() } else { b"mine\n".to_vec() }, "{case}");
        assert_eq!(r.read("side.py"), b"s = 1\n", "{case}: the pick's other change, applied either way");
        assert_eq!(r.git(&r.work, &["log", "-1", "--format=%an|%cn"]), "s|t", "{case}: the pick's author, the repo's committer");
        let body = r.git(&r.work, &["log", "-1", "--format=%B"]);
        let won = if theirs_newer { "the picked commit won (its side" } else { "yours won (your side" };
        let dates = format!("last changed: yours (HEAD) {}, the picked commit (CHERRY_PICK_HEAD) {}", minute_words(mine_at), minute_words(side_at));
        assert!(
            body.starts_with(&format!(
                "the side's change\n\narena: finished this cherry-pick, left in progress for 3h+: each conflicted file from the side \
                 whose last change to it is the more recent (a tie keeps yours), a file you resolved by hand kept as it is.\n\
                 - README.md: {won} of each conflict; the other changes of both sides kept) — {dates}\n\nThe state before is in \
                 .git/arena-resolved/"
            )),
            "{case}: {body}"
        );
        assert!(body.ends_with("today's copy: w1d1/devtest-x."), "{body}");
        assert!(!body.contains("Conflicts:"), "{body}");
        assert_eq!(r.backup().1.outcome, Some((BACKUP_PUSHED, "autocommit-x".to_string())), "{case}");
    }
}

/// A revert is never finished (review ai-review claude #2): its other side is the commit it undoes,
/// always older than HEAD, so "the newer side" would keep HEAD's everywhere and commit a revert
/// that reverts nothing. The gate says so; the script refuses, touching nothing; it stays BUSY.
#[cfg(unix)]
#[test]
fn a_revert_is_never_finished() {
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
    let look = r.look();
    assert_eq!(not_quiet(&look, 3), None, "quiet, all the same");
    assert_eq!(hold(Some(3), Some(&look), CopiedNow::Yes, Some(&copy_of(&look, r.now))), Err(Hold::Never("revert".into())));
    let before = r.unmerged();
    let (code, rep, said) = r.resolve(3, &look);
    assert_eq!((code, rep.refused.as_deref()), (Some(RESOLVE_REFUSED_EXIT), Some(REVERT_WORDS)), "{said}");
    assert!(r.work.join(".git/REVERT_HEAD").exists() && !r.work.join(".git/arena-resolved").exists());
    assert_eq!((r.unmerged(), r.git(&r.work, &["rev-parse", "HEAD"])), (before, head));
    assert_eq!(r.backup().1.busy.as_deref(), Some("revert"), "still BUSY");
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
        let side = if stash_newer { "theirs" } else { "ours" };
        assert_eq!((taken(&rep), rep.hand.clone()), (vec![("README.md".to_string(), side.to_string(), Taken::Hunks)], vec!["notes.py".to_string()]), "{case}");
        assert_eq!(rep.newer[0].ours, head_at, "{case}");
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
    // The look names the newest file, an odd name and all.
    r.age(4 * HOUR);
    r.touch("sub dir/we*ird.py", 600);
    let look = r.look();
    assert_eq!((look.newest, look.changed.as_deref()), (r.now - 600, Some("sub dir/we*ird.py")));
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
    assert_eq!((code, rep.resolved.as_deref()), (Some(0), Some("merge")), "{said}");
}

/// A file the participant began resolving is left alone (review ai-review claude #2): still with
/// markers, but not what the merge left — the whole merge stays BUSY, the file named. Judged by
/// making git's conflicted output again: with `git merge-file` and the file's own labels (the
/// default style, and diff3); where merge-file joins two conflicts the merge kept apart, by the
/// merge's own way (`checkout --conflict`, labels aside). Where it can't be made again — a marker
/// line deleted, or its label — nothing says the file is as git left it: BUSY too, said, however
/// soon after the merge it was written (review ai-review codex #5: a 2-minute allowance let a
/// participant's quick start, a marker deleted and the hunk edited, be overwritten).
#[cfg(unix)]
#[test]
fn a_file_begun_by_hand_is_left_alone_and_one_as_the_merge_left_it_is_merged() {
    let begun = "you started resolving f.py by hand — finish it (and git add it) or abort, on the pod";
    let unknown = "can't tell whether you started resolving f.py by hand (a marker line or its label is gone, or a side is binary: \
                   git's conflicted output can't be made again to compare) — finish it (and git add it) or abort, on the pod";
    // (case, the file's base/course/mine, diff3, what the participant did, why it stays BUSY — or
    // finished)
    type Case<'a> = (&'a str, [&'a [u8]; 3], bool, Box<dyn Fn(&Repo)>, Option<&'a str>);
    let plain: [&[u8]; 3] = [b"a\nb\nc\n", b"a\nB course\nc\n", b"a\nB mine\nc\n"];
    // Two conflicts four lines without letters or digits apart: merge-file makes them one.
    let apart: [&[u8]; 3] = [b"x = 1\n}\n\n}\n)\ny = 1\nz\n", b"x = 2\n}\n\n}\n)\ny = 2\nz\n", b"x = 3\n}\n\n}\n)\ny = 3\nz\n"];
    let mtime = |ago: u64| move |r: &Repo| r.touch("f.py", ago);
    // f.py with its `>>>>>>>` line changed by `edit` (`None`: deleted), written `ago` seconds ago.
    let closer = move |edit: Option<&'static str>, ago: u64| {
        move |r: &Repo| {
            let t = r.text("f.py");
            let kept: Vec<&str> = t.lines().filter_map(|l| if l.starts_with(">>>>>>>") { edit } else { Some(l) }).collect();
            r.put("f.py", format!("{}\n", kept.join("\n")).as_bytes());
            mtime(ago)(r);
        }
    };
    let cases: Vec<Case> = vec![
        ("as-left", plain, false, Box::new(|_: &Repo| {}), None),
        ("as-left-diff3", plain, true, Box::new(|_: &Repo| {}), None),
        ("as-left-apart", apart, false, Box::new(|_: &Repo| {}), None),
        // Edited inside its markers — written within a minute of the merge, so only the bytes tell.
        ("begun", plain, false, Box::new(move |r: &Repo| {
            r.put("f.py", r.text("f.py").replace("B mine", "B mine, and some of the course's").as_bytes());
            mtime(4 * HOUR - 60)(r);
        }), Some(begun)),
        ("begun-diff3", plain, true, Box::new(move |r: &Repo| {
            r.put("f.py", r.text("f.py").replace("B course", "B course too").as_bytes());
            mtime(4 * HOUR - 60)(r);
        }), Some(begun)),
        // Its `>>>>>>>` line deleted (labels unreadable), 5 minutes after the merge or 1: can't be
        // made again — BUSY either way.
        ("no-labels-late", plain, false, Box::new(closer(None, 4 * HOUR - 300)), Some(unknown)),
        ("no-labels-soon", plain, false, Box::new(closer(None, 4 * HOUR - 60)), Some(unknown)),
        // Its label alone deleted, the line kept, a minute after: the same.
        ("no-label-text-soon", plain, false, Box::new(closer(Some(">>>>>>>"), 4 * HOUR - 60)), Some(unknown)),
    ];
    for (case, [base, course, mine], diff3, did, busy) in cases {
        let r = Repo::new(&format!("left-{case}"));
        r.add_base(&[("f.py", base)]);
        if diff3 {
            r.git(&r.work, &["config", "merge.conflictStyle", "diff3"]);
        }
        r.course_commit(&[("f.py", course)], r.now - 5 * DAY);
        r.my_commit(&[("f.py", mine)], r.now - 10 * DAY);
        r.pull_conflicted();
        assert_eq!(r.text("f.py").contains("|||||||"), diff3, "{case}: {}", r.text("f.py"));
        r.age(4 * HOUR);
        did(&r);
        let conflicts = r.unmerged();
        let (code, rep, said) = r.resolve(3, &r.look());
        match busy {
            None => {
                assert_eq!(code, Some(0), "{case}: {said}");
                assert_eq!(taken(&rep), [("f.py".to_string(), "theirs".to_string(), Taken::Hunks)], "{case}");
                assert_eq!(r.read("f.py"), course, "{case}: the newer side's");
            }
            Some(why) => {
                assert_eq!((code, rep.refused.as_deref()), (Some(RESOLVE_REFUSED_EXIT), Some(why)), "{case}: {said}");
                r.untouched(&conflicts, case);
            }
        }
    }
}

/// Re-checked right before anything changes (review ai-review codex #2): the participant aborts the
/// merge, or aborts it and starts a rebase, while the state before is being kept — nothing is
/// touched (the plan made for the merge is never applied to the rebase). And again right before
/// the commit: the merge gone by then, git's index locked, or anything staged since the checks
/// (review ai-review codex #2) — stopped, said, nothing committed, the state before already kept.
#[cfg(unix)]
#[test]
fn the_state_is_read_again_right_before_changing_anything_and_before_the_commit() {
    let hook = "ARENA_TEST_HOOK_BEFORE_CHANGE";
    let cases = [
        ("abort", "git merge --abort", "changed: the merge isn't the one planned for any more while the state before was kept"),
        ("rebase", "git merge --abort && git rebase upstream/main", "changed: the merge isn't the one planned for any more while the state before was kept"),
        ("edit", "echo more >> notes.py", "changed: files changed while the state before was kept"),
        ("stage", "git add notes.py", "changed: git's index changed while the state before was kept"),
        ("lock", ": > .git/index.lock", "changed: git's index is locked while the state before was kept"),
    ];
    for (case, cmd, why) in cases {
        let r = Repo::new(&format!("again-{case}"));
        r.merge_conflict(r.now - 5 * DAY, r.now - 10 * DAY);
        let head = r.git(&r.work, &["rev-parse", "HEAD"]);
        r.age(4 * HOUR);
        let look = r.look();
        let (code, rep, said) = r.resolve_with(3, &look, GIT_MAX_FILE_BYTES_FOR_TESTS, &[(hook, &format!("cd '{}' && {cmd}", r.work.display()))]);
        assert_eq!((code, rep.refused.as_deref()), (Some(RESOLVE_REFUSED_EXIT), Some(why)), "{case}: {said}");
        assert!(!r.work.join(".git/arena-resolved").exists(), "{case}: nothing kept, nothing done");
        if case == "rebase" {
            assert!(r.work.join(".git/rebase-merge").exists(), "the participant's rebase, as they left it");
        } else {
            assert_eq!(r.git(&r.work, &["rev-parse", "HEAD"]), head, "{case}");
        }
    }
    // Someone back at work while the plan was made, no file touched (review ai-review codex #6):
    // a terminal used (its timestamp only), a terminal login typing, a git command started. The
    // re-read asks every condition of the first look again — nothing touched.
    type Back = (&'static str, fn(&Repo), fn(&Repo) -> String, &'static str);
    let back: [Back; 3] = [
        (
            "pts",
            |r| r.pts(3, Some((5 * HOUR, 5 * HOUR))),
            |r| format!("touch -a {}", r.base.join("pts/3").display()),
            "recent: a terminal was used less than 3h ago while the state before was kept",
        ),
        ("login", |r| r.login(Some(5 * HOUR)), |r| format!("touch -a {}", r.base.join("tty").display()), "recent: a terminal login typed less than 3h ago while the state before was kept"),
        (
            "git",
            |_| {},
            |r| {
                let d = r.base.join("proc/4242");
                format!("mkdir -p {0} && echo git > {0}/comm && echo '4242 (git) S 1 4242 4242 0 -1 0' > {0}/stat", d.display())
            },
            "busy: a git command, or an editor one waits on, is running (pid 4242: git) while the state before was kept",
        ),
    ];
    for (case, before, hook_cmd, why) in back {
        let r = Repo::new(&format!("again-back-{case}"));
        r.merge_conflict(r.now - 5 * DAY, r.now - 10 * DAY);
        let (head, conflicts) = (r.git(&r.work, &["rev-parse", "HEAD"]), r.unmerged());
        before(&r);
        r.age(4 * HOUR);
        let look = r.look();
        assert_eq!(not_quiet(&look, 3), None, "{case}: quiet at the look");
        let (code, rep, said) = r.resolve_with(3, &look, GIT_MAX_FILE_BYTES_FOR_TESTS, &[(hook, &hook_cmd(&r))]);
        assert_eq!((code, rep.refused.as_deref()), (Some(RESOLVE_REFUSED_EXIT), Some(why)), "{case}: {said}");
        r.untouched(&conflicts, case);
        assert_eq!(r.git(&r.work, &["rev-parse", "HEAD"]), head, "{case}");
    }
    // Right before the commit: the merge gone, git's index locked — or something staged after the
    // checks and the redaction (review ai-review codex #2): a key-bearing file, one over the size
    // limit (1000 bytes here), a nested repo (a gitlink) — each a `git add` finished in the
    // meantime, which the commit would take unchecked. The index is no longer the tree checked:
    // nothing committed.
    let hook = "ARENA_TEST_HOOK_BEFORE_FINISH";
    let key = format!("sk-or-v1-{}", "ab".repeat(32));
    let changed = "the index changed while arena was finishing it — nothing committed — ";
    let cases: [(&str, String, &str, u64); 5] = [
        ("finish-abort", "git merge --abort".into(), "changed before the commit: the merge isn't the one resolved any more — ", GIT_MAX_FILE_BYTES_FOR_TESTS),
        ("finish-lock", ": > .git/index.lock".into(), "changed before the commit: git's index is locked — ", GIT_MAX_FILE_BYTES_FOR_TESTS),
        ("finish-key", format!("printf 'KEY = \"{key}\"\\n' > secret.py && git add secret.py"), changed, GIT_MAX_FILE_BYTES_FOR_TESTS),
        ("finish-big", "head -c 4096 /dev/zero > big.bin && git add big.bin".into(), changed, 1000),
        (
            "finish-gitlink",
            "git init -q nested && cd nested && echo 'n = 1' > n.py && git add n.py && git -c user.name=n -c user.email=n@n commit -q -m n && cd .. \
             && git add nested"
                .into(),
            changed,
            GIT_MAX_FILE_BYTES_FOR_TESTS,
        ),
    ];
    for (case, cmd, why, max) in cases {
        let r = Repo::new(case);
        r.merge_conflict(r.now - 5 * DAY, r.now - 10 * DAY);
        let head = r.git(&r.work, &["rev-parse", "HEAD"]);
        r.age(4 * HOUR);
        let (code, rep, said) = r.resolve_with(3, &r.look(), max, &[(hook, &format!("cd '{}' && {cmd}", r.work.display()))]);
        assert_eq!(code, Some(RESOLVE_FAILED_EXIT), "{case}: {said}");
        let failed = rep.failed.clone().unwrap_or_default();
        let stamp = rep.saved.as_deref().and_then(|s| s.rsplit('/').next()).unwrap_or("?").to_string();
        assert_eq!(failed, format!("{why}the state before is in .git/arena-resolved/{stamp}/"), "{case}: {said}");
        assert_eq!(r.git(&r.work, &["rev-parse", "HEAD"]), head, "{case}: nothing committed");
        assert!(r.work.join(".git/arena-resolved").join(&stamp).join("files/README.md").exists(), "{case}");
        if why == changed {
            assert!(r.work.join(".git/MERGE_HEAD").exists(), "{case}: the merge still in progress");
        }
    }
}

/// What the resolve stages goes through the backup's redaction (review ai-review codex #5): a key
/// in a file the participant resolved by hand is committed redacted — the file on disk keeps it,
/// and the push goes through. A resolved file over the git limit is never committed: refused,
/// nothing touched.
#[cfg(unix)]
#[test]
fn keys_are_redacted_and_a_file_over_the_limit_is_left_alone() {
    let key = format!("sk-or-v1-{}", "ab".repeat(32));
    let r = Repo::new("keys");
    r.merge_conflict(r.now - 5 * DAY, r.now - 10 * DAY);
    let hand = format!("KEY = \"{key}\"\nx = 3\n");
    r.put("notes.py", hand.as_bytes());
    r.age(4 * HOUR);
    let (code, rep, said) = r.resolve(3, &r.look());
    assert_eq!(code, Some(0), "{said}");
    assert_eq!((rep.hand.clone(), rep.redacted.clone()), (vec!["notes.py".to_string()], vec!["notes.py".to_string()]), "{said}");
    assert_eq!(r.committed("notes.py"), "KEY = \"REDACTED-OPENROUTER-KEY\"\nx = 3", "the commit has it redacted");
    assert_eq!(r.text("notes.py"), hand, "the disk keeps the key");
    assert!(!r.git_try(&r.work, &["grep", "-q", "-F", &key, "HEAD"]).0, "no key in the merge commit's tree");
    let (code, report, said) = r.backup();
    assert_eq!((code, report.outcome), (Some(0), Some((BACKUP_PUSHED, "autocommit-x".to_string()))), "{said}");

    // Over the limit (a small one here): never committed — refused, nothing touched.
    let r = Repo::new("limit");
    r.merge_conflict(r.now - 5 * DAY, r.now - 10 * DAY);
    r.put("notes.py", &vec![b'x'; 4096]);
    r.age(4 * HOUR);
    let conflicts = r.unmerged();
    let (code, rep, said) = r.resolve_with(3, &r.look(), 1000, &[]);
    assert_eq!(
        (code, rep.refused.as_deref()),
        (Some(RESOLVE_REFUSED_EXIT), Some("notes.py would be 4096 bytes, over git's 1000-byte limit: never committed — resolve it on the pod")),
        "{said}"
    );
    r.untouched(&conflicts, "limit");
}

/// The commit takes the whole index, not only the paths in conflict (review ai-review codex #4,
/// claude #4): every path staged against HEAD is checked. A key in a file the participant resolved
/// and `git add`ed before the resolve ran — while another file is still in conflict, or with
/// every conflict already staged (an empty plan) — is committed redacted, the disk keeping it; so
/// is one in a file added mid-cherry-pick. A 96 MB file or a nested repo (a gitlink) staged by hand
/// is never committed: BUSY, said, nothing touched. And what only the verified course changed goes
/// in as the course published it, a key-like string in it and all.
#[cfg(unix)]
#[test]
fn every_path_the_commit_takes_is_checked_not_only_the_conflicted_ones() {
    let key = format!("sk-or-v1-{}", "ab".repeat(32));
    let with_key = format!("KEY = \"{key}\"\nx = 3\n");
    let redacted = "KEY = \"REDACTED-OPENROUTER-KEY\"\nx = 3";
    // Committed redacted, the disk keeping the key, no key anywhere in the commit's tree.
    let redacted_in_head = |r: &Repo, rel: &str, case: &str| {
        assert_eq!(r.committed(rel), redacted, "{case}: the commit has it redacted");
        assert_eq!(r.text(rel), with_key, "{case}: the disk keeps the key");
        assert!(!r.git_try(&r.work, &["grep", "-q", "-F", &key, "HEAD"]).0, "{case}: no key in the commit's tree");
    };

    // Partly staged: notes.py resolved by hand, a key in it, and added; README.md and the rest still
    // in conflict — the plan has only those.
    let r = Repo::new("staged-part");
    r.merge_conflict(r.now - 5 * DAY, r.now - 10 * DAY);
    r.put("notes.py", with_key.as_bytes());
    r.git(&r.work, &["add", "notes.py"]);
    r.age(4 * HOUR);
    let (code, rep, said) = r.resolve(3, &r.look());
    assert_eq!((code, rep.resolved.as_deref()), (Some(0), Some("merge")), "{said}");
    assert!(rep.hand.is_empty() && !rep.newer.iter().any(|c| c.path == "notes.py"), "not in the plan: {said}");
    assert_eq!(rep.redacted, ["notes.py"], "{said}");
    redacted_in_head(&r, "notes.py", "part");

    // Fully staged: every conflict resolved and added, nothing left in the plan — still checked.
    let r = Repo::new("staged-all");
    r.merge_conflict(r.now - 5 * DAY, r.now - 10 * DAY);
    let merged = r.git(&r.work, &["rev-parse", "MERGE_HEAD"]);
    r.put("README.md", b"resolved\n");
    r.put("sub dir/we*ird.py", b"w = 3\n");
    r.put("notes.py", with_key.as_bytes());
    r.git(&r.work, &["add", "-A"]);
    assert!(r.unmerged().is_empty());
    r.age(4 * HOUR);
    let (code, rep, said) = r.resolve(3, &r.look());
    assert_eq!((code, rep.resolved.as_deref()), (Some(0), Some("merge")), "{said}");
    assert!(rep.newer.is_empty() && rep.hand.is_empty(), "an empty plan: {said}");
    assert_eq!(rep.redacted, ["notes.py"], "{said}");
    redacted_in_head(&r, "notes.py", "all");
    assert_eq!(r.committed("README.md"), "resolved");
    assert!(r.git(&r.work, &["rev-list", "--parents", "-n1", "HEAD"]).ends_with(&merged), "a merge commit");

    // A file added mid-cherry-pick, a key in it: the pick's commit has it redacted.
    let r = Repo::new("staged-pick");
    r.git(&r.work, &["checkout", "-q", "-b", "side", "main"]);
    r.put("README.md", b"side\n");
    r.commit_all(&r.work, "the side's change", r.now - 5 * DAY);
    r.git(&r.work, &["checkout", "-q", "autocommit-x"]);
    r.put("README.md", b"mine\n");
    r.commit_all(&r.work, "mine", r.now - 10 * DAY);
    assert!(!r.git_try(&r.work, &["cherry-pick", "side"]).0);
    r.put("mine.py", with_key.as_bytes());
    r.git(&r.work, &["add", "mine.py"]);
    r.age(4 * HOUR);
    let (code, rep, said) = r.resolve(3, &r.look());
    assert_eq!((code, rep.resolved.as_deref()), (Some(0), Some("cherry-pick")), "{said}");
    assert_eq!(rep.redacted, ["mine.py"], "{said}");
    redacted_in_head(&r, "mine.py", "pick");

    // A 96 MB file, or a nested repo (a gitlink), staged by hand mid-merge: never committed.
    for (case, stage, why) in [
        (
            "staged-big",
            Box::new(|r: &Repo| {
                r.put("model.pt", &vec![0u8; 96 << 20]);
                r.git(&r.work, &["add", "model.pt"]);
            }) as Box<dyn Fn(&Repo)>,
            format!(
                "model.pt is staged at {} bytes, over git's {GIT_MAX_FILE_BYTES_FOR_TESTS}-byte limit: never committed — unstage it, or \
                 abort, on the pod",
                96 << 20
            ),
        ),
        (
            "staged-gitlink",
            Box::new(|r: &Repo| {
                let nested = r.work.join("nested");
                r.git(&r.work, &["init", "-q", "nested"]);
                std::fs::write(nested.join("n.py"), "n = 1\n").unwrap();
                r.git(&nested, &["add", "n.py"]);
                r.git(&nested, &["-c", "user.name=n", "-c", "user.email=n@n", "commit", "-q", "-m", "n"]);
                r.git(&r.work, &["add", "nested"]);
            }),
            "nested is staged as a nested repo (a gitlink: its commit id, none of its files): never committed — unstage it, or abort, \
             on the pod"
                .to_string(),
        ),
    ] {
        let r = Repo::new(case);
        r.merge_conflict(r.now - 5 * DAY, r.now - 10 * DAY);
        stage(&r);
        r.age(4 * HOUR);
        let conflicts = r.unmerged();
        let (code, rep, said) = r.resolve(3, &r.look());
        assert_eq!((code, rep.refused.as_deref()), (Some(RESOLVE_REFUSED_EXIT), Some(why.as_str())), "{case}: {said}");
        r.untouched(&conflicts, case);
    }

    // The course's own file, a key-like string in it (only the course changed it), the merge's
    // MERGE_HEAD in the verified course ref: committed as the course has it — the participant made
    // none of it. Either side newer: with the course's, the index is the merged branch's tree
    // everywhere; with the participant's, it isn't. (Unverified: `only_the_verified_courses_…`.)
    for course_newer in [true, false] {
        let r = Repo::new(if course_newer { "staged-theirs-course" } else { "staged-theirs-mine" });
        let example = format!("# Set your key: OPENROUTER_API_KEY = \"{key}\"\n");
        r.course_commit(&[("example.md", example.as_bytes())], r.now - 11 * DAY);
        let (course_at, mine_at) = if course_newer { (r.now - 5 * DAY, r.now - 10 * DAY) } else { (r.now - 10 * DAY, r.now - 5 * DAY) };
        r.merge_conflict(course_at, mine_at);
        r.verified_course("MERGE_HEAD");
        r.age(4 * HOUR);
        let (code, rep, said) = r.resolve(3, &r.look());
        assert_eq!((code, rep.resolved.as_deref()), (Some(0), Some("merge")), "{course_newer}: {said}");
        assert!(rep.redacted.is_empty(), "{course_newer}: {said}");
        assert_eq!(format!("{}\n", r.committed("example.md")), example, "{course_newer}: as the course published it");
    }
}

/// Content equal to the other side's is the course's own only when that is verified (review
/// ai-review codex #3): a merge whose MERGE_HEAD is in `refs/arena/course-main`, which only the
/// backup's fetch from the configured course URL writes (here `git update-ref`). Then a key-like
/// string the course published — in a conflicted file the course's side won, and in one only the
/// course changed — goes in as published. Matching the other side proves nothing by itself: a
/// cherry-pick of the participant's own commit with a key (its side chosen; its commit in the ref
/// too — a pick never counts), a merge of their own branch with the ref there, the course's merge
/// with no ref, or with a ref behind it — each committed redacted, the disk keeping the key.
#[cfg(unix)]
#[test]
fn only_the_verified_courses_content_escapes_redaction() {
    let key = format!("sk-or-v1-{}", "ab".repeat(32));
    let keyed = format!("k = \"{key}\"\n");
    let redacted = "k = \"REDACTED-OPENROUTER-KEY\"";
    let example = format!("# Set your key: OPENROUTER_API_KEY = \"{key}\"\n");
    let example_redacted = "# Set your key: OPENROUTER_API_KEY = \"REDACTED-OPENROUTER-KEY\"";
    // Committed redacted, the disk keeping the key.
    let redacted_in_head = |r: &Repo, rel: &str, want: &str, disk: &str, case: &str| {
        assert_eq!(r.committed(rel), want, "{case}: {rel} committed redacted");
        assert_eq!(r.text(rel), disk, "{case}: {rel} on disk keeps the key");
    };

    // A cherry-pick of the participant's own commit, a key in its side of the conflict, that side
    // newer: redacted — even with that very commit in the course ref.
    let r = Repo::new("verify-pick");
    r.add_base(&[("keyed.py", b"k = 0\n")]);
    r.git(&r.work, &["checkout", "-q", "-b", "side", "main"]);
    r.put("keyed.py", keyed.as_bytes());
    r.commit_all(&r.work, "the side's key", r.now - 5 * DAY);
    r.git(&r.work, &["checkout", "-q", "autocommit-x"]);
    r.put("keyed.py", b"k = 1\n");
    r.commit_all(&r.work, "mine", r.now - 10 * DAY);
    assert!(!r.git_try(&r.work, &["cherry-pick", "side"]).0);
    r.verified_course("side");
    r.age(4 * HOUR);
    let (code, rep, said) = r.resolve(3, &r.look());
    assert_eq!((code, rep.resolved.as_deref()), (Some(0), Some("cherry-pick")), "{said}");
    assert_eq!(taken(&rep), [("keyed.py".to_string(), "theirs".to_string(), Taken::Hunks)], "{said}");
    assert_eq!(rep.redacted, ["keyed.py"], "{said}");
    redacted_in_head(&r, "keyed.py", redacted, &keyed, "pick");

    // The course's merge: a key-like string in the conflicted keyed.py (the course's side newer)
    // and in example.md (only the course changed it). Verified: as published. No ref, or a ref
    // behind the merged commit (the course's older history only): redacted.
    for (case, verified) in [("verify-course", Some("MERGE_HEAD")), ("verify-none", None), ("verify-behind", Some("MERGE_HEAD~1"))] {
        let r = Repo::new(case);
        r.add_base(&[("keyed.py", b"k = 0\n")]);
        r.course_commit(&[("keyed.py", keyed.as_bytes()), ("example.md", example.as_bytes())], r.now - 5 * DAY);
        r.my_commit(&[("keyed.py", b"k = 1\n")], r.now - 10 * DAY);
        r.pull_conflicted();
        if let Some(rev) = verified {
            r.verified_course(rev);
        }
        r.age(4 * HOUR);
        let (code, rep, said) = r.resolve(3, &r.look());
        assert_eq!((code, rep.resolved.as_deref()), (Some(0), Some("merge")), "{case}: {said}");
        assert_eq!(taken(&rep), [("keyed.py".to_string(), "theirs".to_string(), Taken::Hunks)], "{case}: {said}");
        if case == "verify-course" {
            assert!(rep.redacted.is_empty(), "{case}: {said}");
            assert_eq!((format!("{}\n", r.committed("keyed.py")), format!("{}\n", r.committed("example.md"))), (keyed.clone(), example.clone()), "{case}: as published");
        } else {
            let mut got = rep.redacted.clone();
            got.sort();
            assert_eq!(got, ["example.md", "keyed.py"], "{case}: {said}");
            redacted_in_head(&r, "keyed.py", redacted, &keyed, case);
            redacted_in_head(&r, "example.md", example_redacted, &example, case);
        }
    }

    // A merge of the participant's own branch (its side newer, a key in it), the course ref there
    // (the course's main): redacted.
    let r = Repo::new("verify-own-merge");
    r.add_base(&[("keyed.py", b"k = 0\n")]);
    r.git(&r.work, &["checkout", "-q", "-b", "exp", "main"]);
    r.put("keyed.py", keyed.as_bytes());
    r.commit_all(&r.work, "my experiment", r.now - 5 * DAY);
    r.git(&r.work, &["checkout", "-q", "autocommit-x"]);
    r.put("keyed.py", b"k = 1\n");
    r.commit_all(&r.work, "mine", r.now - 10 * DAY);
    r.git(&r.work, &["fetch", "-q", "upstream"]);
    r.verified_course("refs/remotes/upstream/main");
    let (ok, said) = r.git_try(&r.work, &["merge", "--no-edit", "exp"]);
    assert!(!ok && r.work.join(".git/MERGE_HEAD").exists(), "{said}");
    r.age(4 * HOUR);
    let (code, rep, said) = r.resolve(3, &r.look());
    assert_eq!((code, rep.resolved.as_deref()), (Some(0), Some("merge")), "{said}");
    assert_eq!(rep.redacted, ["keyed.py"], "{said}");
    redacted_in_head(&r, "keyed.py", redacted, &keyed, "own merge");
}

/// The course's own content isn't refused as the participant's (review ai-review claude #3). What
/// a verified course merge stages as MERGE_HEAD has it is sized by GitHub's limit (100 MiB), as the
/// backup sizes what it pushes: a 96 MB course file goes in. Unverified (no course ref), git's
/// 95 MB limit holds and it stays BUSY — in words that never tell the participant to unstage what
/// they never staged. A course submodule (a gitlink the staged .gitmodules lists, as the backup's
/// `is_submodule` reads it) isn't refused; a course gitlink .gitmodules doesn't list is, in the
/// course's words. And a conflicted binary file taken whole from the verified course is sized the
/// same way (a 1000-byte limit here).
#[cfg(unix)]
#[test]
fn the_courses_own_content_is_sized_by_githubs_limit_and_its_submodules_kept() {
    // A 96 MB file only the course added, beside the usual conflicts.
    for verified in [true, false] {
        let r = Repo::new(if verified { "course-big" } else { "course-big-unverified" });
        r.course_commit(&[("model.pt", &vec![0u8; 96 << 20][..])], r.now - 6 * DAY);
        r.merge_conflict(r.now - 5 * DAY, r.now - 10 * DAY);
        if verified {
            r.verified_course("MERGE_HEAD");
        }
        r.age(4 * HOUR);
        let conflicts = r.unmerged();
        let (code, rep, said) = r.resolve(3, &r.look());
        if verified {
            assert_eq!((code, rep.resolved.as_deref()), (Some(0), Some("merge")), "{said}");
            assert_eq!(r.git(&r.work, &["cat-file", "-s", "HEAD:model.pt"]), (96u64 << 20).to_string());
        } else {
            let why = format!(
                "model.pt comes in from the merged branch (MERGE_HEAD) at {} bytes, over git's {GIT_MAX_FILE_BYTES_FOR_TESTS}-byte limit: \
                 arena won't commit it — finish the merge yourself, or abort it, on the pod",
                96 << 20
            );
            assert_eq!((code, rep.refused.as_deref()), (Some(RESOLVE_REFUSED_EXIT), Some(why.as_str())), "{said}");
            r.untouched(&conflicts, "unverified");
        }
    }

    // A course commit adding a gitlink (`lib` listed in .gitmodules, or `vendor` not), after the
    // course's conflicting fix, the course verified.
    for (case, gitmodules, why) in [
        ("course-submodule", true, None),
        (
            "course-gitlink",
            false,
            Some(
                "vendor comes in from the merged branch (MERGE_HEAD) as a nested repo (a gitlink: its commit id, none of its files) that \
                 .gitmodules doesn't list: arena won't commit it — finish the merge yourself, or abort it, on the pod",
            ),
        ),
    ] {
        let r = Repo::new(case);
        r.diverge(r.now - 5 * DAY, r.now - 10 * DAY);
        let path = if gitmodules { "lib" } else { "vendor" };
        let pinned = r.git(&r.course, &["rev-parse", "HEAD"]);
        if gitmodules {
            r.put_in(&r.course, ".gitmodules", b"[submodule \"lib\"]\n\tpath = lib\n\turl = https://example.invalid/lib.git\n");
            r.git(&r.course, &["add", ".gitmodules"]);
        }
        r.git(&r.course, &["update-index", "--add", "--cacheinfo", &format!("160000,{pinned},{path}")]);
        let (ok, said) = r.git_at(&r.course, r.now - 5 * DAY + 60, &["commit", "-q", "-m", "a nested repo"]);
        assert!(ok, "{said}");
        r.pull_conflicted();
        r.verified_course("MERGE_HEAD");
        r.age(4 * HOUR);
        let conflicts = r.unmerged();
        let (code, rep, said) = r.resolve(3, &r.look());
        match why {
            None => {
                assert_eq!((code, rep.resolved.as_deref()), (Some(0), Some("merge")), "{case}: {said}");
                assert_eq!(r.git(&r.work, &["ls-tree", "HEAD", "lib"]), format!("160000 commit {pinned}\tlib"), "{case}: the submodule committed");
            }
            Some(why) => {
                assert_eq!((code, rep.refused.as_deref()), (Some(RESOLVE_REFUSED_EXIT), Some(why)), "{case}: {said}");
                r.untouched(&conflicts, case);
            }
        }
    }

    // A binary file both changed, the course's side newer and taken whole: under a 1000-byte limit,
    // 4096 bytes go in from the verified course, and are refused unverified.
    let bin = |b: u8| {
        let mut v = vec![b; 4096];
        v[0] = 0;
        v
    };
    for verified in [true, false] {
        let r = Repo::new(if verified { "course-bin" } else { "course-bin-unverified" });
        r.add_base(&[("w.bin", &bin(1)[..])]);
        r.course_commit(&[("w.bin", &bin(2)[..])], r.now - 5 * DAY);
        r.my_commit(&[("w.bin", &bin(3)[..])], r.now - 10 * DAY);
        r.pull_conflicted();
        if verified {
            r.verified_course("MERGE_HEAD");
        }
        r.age(4 * HOUR);
        let conflicts = r.unmerged();
        let (code, rep, said) = r.resolve_with(3, &r.look(), 1000, &[]);
        if verified {
            assert_eq!((code, taken(&rep)), (Some(0), vec![("w.bin".to_string(), "theirs".to_string(), Taken::File)]), "{said}");
            assert_eq!(r.read("w.bin"), bin(2));
        } else {
            assert_eq!(
                (code, rep.refused.as_deref()),
                (Some(RESOLVE_REFUSED_EXIT), Some("w.bin would be 4096 bytes, over git's 1000-byte limit: never committed — resolve it on the pod")),
                "{said}"
            );
            r.untouched(&conflicts, "bin unverified");
        }
    }
}

/// A notebook stays valid JSON (review ai-review claude #3), and is never taken whole from one
/// side instead (claude #1: that drops the other side's changes to it, clean hunks and all).
/// Merged per hunk, one side's conflict beside the other side's clean hunk can break it: the
/// participant grouped cells into a list (an opening `[` three lines above the conflict, a clean
/// hunk), the course changed the cell where the list closes (the conflict) — the course's side of
/// it drops the `]`. So the result is parsed (python3, else jq): valid (the participant's side
/// newer) — per hunk as any file; not valid — BUSY, said, untouched; and so with nothing on the
/// pod to tell: no parser, or a python3 that fails whatever it's given (a conda shim whose env is
/// gone), which is no answer, never "not JSON" — jq, there, is asked next. A participant's json.py
/// at the checkout's top is never imported in place of the stdlib's, nor its `__pycache__` written
/// into the tree (claude #4: `python3 -I`). A notebook resolved by hand that isn't valid JSON, or
/// that nothing can check, leaves it BUSY; a valid one is kept. And every notebook the commit takes
/// is checked, not only the conflicted ones (codex #4): one malformed and already staged — beside a
/// conflict still left, or with every conflict staged — or one only the course changed: BUSY, said.
#[cfg(unix)]
#[test]
fn a_notebook_that_wont_merge_per_hunk_into_valid_json_leaves_it_busy() {
    let base = b"{\n \"cells\": [\n  \"a\",\n  \"b\",\n  \"c\",\n  \"d\"\n ]\n}\n";
    let course = b"{\n \"cells\": [\n  \"a\",\n  \"b\",\n  \"C\",\n  \"d\"\n ]\n}\n";
    let mine = b"{\n \"cells\": [\n  [\"a\",\n  \"b\",\n  \"c\"],\n  \"d\"\n ]\n}\n";
    // A comma lost.
    let bad = b"{\n \"cells\": [\n  \"a\"\n  \"C\"\n ]\n}\n";
    let has = |tool: &str| std::process::Command::new("sh").args(["-c", &format!("command -v {tool}")]).output().is_ok_and(|o| o.status.success());
    // A merge of nb.ipynb, the course's side newer or not (with `also` from the course too).
    let merged = |case: &str, course_newer: bool, also: &[(&str, &[u8])]| {
        let r = Repo::new(case);
        r.add_base(&[("nb.ipynb", base)]);
        let (course_at, mine_at) = if course_newer { (r.now - 5 * DAY, r.now - 10 * DAY) } else { (r.now - 10 * DAY, r.now - 5 * DAY) };
        let mut theirs: Vec<(&str, &[u8])> = vec![("nb.ipynb", course)];
        theirs.extend_from_slice(also);
        r.course_commit(&theirs, course_at);
        r.my_commit(&[("nb.ipynb", mine)], mine_at);
        r.pull_conflicted();
        r
    };
    let parses = |r: &Repo| serde_json::from_slice::<serde_json::Value>(&r.read("nb.ipynb")).is_ok();
    let busy = "nb.ipynb can't be merged per hunk into valid JSON — resolve it in Jupyter, or abort";
    let nocheck = "nothing on the pod could check it: no working python3 or jq";
    fn nothing(_: &Repo) {}
    // A python3 first on PATH that fails whatever it's given.
    fn broken_python(r: &Repo) {
        crate::remote::write_test_script(r.base.join("bin/python3"), "#!/bin/sh\nexit 1\n").unwrap();
    }
    // A participant's json.py at the checkout's top: one whose `load` takes anything, one that
    // exits 3 (python3's "not JSON") when imported.
    fn json_py_lenient(r: &Repo) {
        r.put("json.py", b"def load(*args, **kwargs):\n    return None\n");
    }
    fn json_py_exits(r: &Repo) {
        r.put("json.py", b"import sys\nsys.exit(3)\n");
    }
    // (case, the JSON tools the resolve may use, the course's side newer, what else is on the pod,
    // why it stays BUSY — None: merged per hunk)
    type Case<'a> = (&'a str, &'a str, bool, fn(&Repo), Option<String>);
    let mut cases: Vec<Case> = vec![
        ("nb-python", "python3", true, nothing, Some(busy.to_string())),
        ("nb-valid", "python3", false, nothing, None),
        ("nb-none", "", false, nothing, Some(format!("{busy} ({nocheck})"))),
        ("nb-broken-python", "python3", false, broken_python, Some(format!("{busy} ({nocheck})"))),
        ("nb-json-py", "python3", true, json_py_lenient, Some(busy.to_string())),
        ("nb-json-py-valid", "python3", false, json_py_exits, None),
    ];
    if has("jq") {
        cases.push(("nb-jq", "jq", true, nothing, Some(busy.to_string())));
        cases.push(("nb-jq-valid", "jq", false, nothing, None));
        cases.push(("nb-broken-python-jq", "python3 jq", false, broken_python, None));
    } else {
        eprintln!("jq isn't installed here: the notebook checks run with python3 alone");
    }
    assert!(has("python3"), "python3, the images' own, is what the check uses first");
    for (case, tools, course_newer, setup, why) in cases {
        let r = merged(case, course_newer, &[]);
        setup(&r);
        r.age(4 * HOUR);
        let conflicts = r.unmerged();
        let (code, rep, said) = r.resolve_with(3, &r.look(), GIT_MAX_FILE_BYTES_FOR_TESTS, &[("ARENA_TEST_JSON_TOOLS", tools)]);
        match why {
            None => {
                assert_eq!((code, rep.resolved.as_deref()), (Some(0), Some("merge")), "{case}: {said}");
                assert_eq!(taken(&rep), [("nb.ipynb".to_string(), "ours".to_string(), Taken::Hunks)], "{case}: {said}");
                assert!(parses(&r), "{case}: {}", r.text("nb.ipynb"));
                assert_eq!(r.read("nb.ipynb"), mine, "{case}: the newer side's");
            }
            Some(why) => {
                assert_eq!((code, rep.refused.as_deref()), (Some(RESOLVE_REFUSED_EXIT), Some(why.as_str())), "{case}: {said}");
                r.untouched(&conflicts, case);
                assert!(r.text("nb.ipynb").contains("<<<<<<< HEAD"), "{case}: as git left it");
            }
        }
        assert!(!r.work.join("__pycache__").exists(), "{case}: nothing written into the tree");
    }

    // Resolved by hand: not valid JSON — BUSY, said, untouched; nothing to check it — BUSY too;
    // valid — kept.
    let hand_none = format!("nb.ipynb can't be checked as JSON ({nocheck}) — finish it (and git add it) or abort, on the pod");
    for (case, by_hand, tools, why) in [
        ("nb-hand-bad", &bad[..], "python3", Some("nb.ipynb isn't valid JSON: Jupyter can't open it as it is — fix it (and git add it) or abort, on the pod")),
        ("nb-hand-none", &mine[..], "", Some(hand_none.as_str())),
        ("nb-hand", &mine[..], "python3", None),
    ] {
        let r = merged(case, true, &[]);
        r.put("nb.ipynb", by_hand);
        r.age(4 * HOUR);
        let conflicts = r.unmerged();
        let (code, rep, said) = r.resolve_with(3, &r.look(), GIT_MAX_FILE_BYTES_FOR_TESTS, &[("ARENA_TEST_JSON_TOOLS", tools)]);
        match why {
            None => {
                assert_eq!((code, rep.hand.clone()), (Some(0), vec!["nb.ipynb".to_string()]), "{case}: {said}");
                assert_eq!(r.read("nb.ipynb"), by_hand, "{case}");
            }
            Some(why) => {
                assert_eq!((code, rep.refused.as_deref()), (Some(RESOLVE_REFUSED_EXIT), Some(why)), "{case}: {said}");
                r.untouched(&conflicts, case);
            }
        }
    }

    // Already staged, malformed: a notebook of the participant's added mid-merge while conflicts
    // are left; the conflicted notebook resolved badly and added, nothing left in conflict; and a
    // notebook only the course changed, malformed as it has it (the conflicted one merging fine).
    let staged = |words: &str| format!("{words} isn't valid JSON as staged: Jupyter can't open it — fix it (and git add it) or abort, on the pod");
    let part = Repo::new("nb-staged-part");
    part.merge_conflict(part.now - 5 * DAY, part.now - 10 * DAY);
    part.put("mine.ipynb", bad);
    part.git(&part.work, &["add", "mine.ipynb"]);
    let all = merged("nb-staged-all", true, &[]);
    all.put("nb.ipynb", bad);
    all.git(&all.work, &["add", "nb.ipynb"]);
    assert!(all.unmerged().is_empty());
    let theirs = merged("nb-staged-theirs", false, &[("broken.ipynb", bad)]);
    let theirs_why = "broken.ipynb isn't valid JSON as the merged branch (MERGE_HEAD) has it: Jupyter can't open it — finish the merge yourself, or \
                      abort it, on the pod";
    for (r, why) in [(&part, staged("mine.ipynb")), (&all, staged("nb.ipynb")), (&theirs, theirs_why.to_string())] {
        r.age(4 * HOUR);
        let conflicts = r.unmerged();
        let (code, rep, said) = r.resolve(3, &r.look());
        assert_eq!((code, rep.refused.as_deref()), (Some(RESOLVE_REFUSED_EXIT), Some(why.as_str())), "{said}");
        r.untouched(&conflicts, &why);
    }
}

/// The path's `conflict-marker-size` (review ai-review codex #9): with 10, git's markers are ten
/// characters long, and the file still has them — not resolved by hand: taken per hunk from the
/// newer side, no marker committed.
#[cfg(unix)]
#[test]
fn a_conflict_marker_size_of_10_is_still_a_conflict() {
    let r = Repo::new("marker-size");
    r.add_base(&[(".gitattributes", b"*.txt conflict-marker-size=10\n"), ("t.txt", b"a\nb\nc\n")]);
    r.course_commit(&[("t.txt", b"a\nb course\nc\n")], r.now - 5 * DAY);
    r.my_commit(&[("t.txt", b"a\nb mine\nc\n")], r.now - 10 * DAY);
    r.pull_conflicted();
    assert!(r.text("t.txt").contains("<<<<<<<<<< HEAD\n"), "{}", r.text("t.txt"));
    r.age(4 * HOUR);
    let (code, rep, said) = r.resolve(3, &r.look());
    assert_eq!(code, Some(0), "{said}");
    assert!(rep.hand.is_empty(), "not resolved by hand: {said}");
    assert_eq!(taken(&rep), [("t.txt".to_string(), "theirs".to_string(), Taken::Hunks)]);
    assert_eq!(r.committed("t.txt"), "a\nb course\nc");
}

/// Conflict markers already staged (review ai-review claude #9): the participant `git add`ed a file
/// with its markers still in it — nothing unmerged is left, but the merge isn't resolved: left
/// BUSY, said. A marker-like line the course's own file has (merged in as it is) is no marker.
#[cfg(unix)]
#[test]
fn markers_already_staged_are_never_committed() {
    let r = Repo::new("staged");
    r.merge_conflict(r.now - 5 * DAY, r.now - 10 * DAY);
    r.git(&r.work, &["add", "README.md"]);
    r.age(4 * HOUR);
    let conflicts = r.unmerged();
    let (code, rep, said) = r.resolve(3, &r.look());
    assert_eq!(
        (code, rep.refused.as_deref()),
        (Some(RESOLVE_REFUSED_EXIT), Some("conflict markers were added in README.md (staged with them still in it): resolve it, or abort, on the pod")),
        "{said}"
    );
    r.untouched(&conflicts, "staged");

    // The course's doc about conflicts, merged in cleanly beside a real conflict: finished.
    let r = Repo::new("staged-doc");
    let doc = b"How a conflict looks:\n<<<<<<< HEAD\nyours\n=======\ntheirs\n>>>>>>> branch\n";
    r.course_commit(&[("conflicts.md", doc)], r.now - 6 * DAY);
    r.merge_conflict(r.now - 5 * DAY, r.now - 10 * DAY);
    r.age(4 * HOUR);
    let (code, rep, said) = r.resolve(3, &r.look());
    assert_eq!((code, rep.resolved.as_deref()), (Some(0), Some("merge")), "{said}");
    assert_eq!(format!("{}\n", r.committed("conflicts.md")).as_bytes(), doc);
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

/// The resolve bounds itself on the pod (review ai-review claude #7): arena's own timeout kills only
/// the local ssh client, and the resolve ran on — it could commit the merge after arena had said it
/// failed and released the pod's lock. Under bash and under zsh, the pods' login shell (which `-c`s
/// the command), it runs under `sh` and `timeout`: one stopped at its bound (here 2 s, while the
/// pre-change hook sleeps) exits 124, says so, and its TERM trap removes its temporary files from
/// the git dir — the merge as it was. The real command, under that shell, finishes it.
#[cfg(unix)]
#[test]
fn the_resolve_bounds_itself_on_the_pod_under_any_login_shell() {
    let has = |sh: &str| std::process::Command::new(sh).args(["-c", "true"]).output().is_ok_and(|o| o.status.success());
    let shells: Vec<&str> = ["bash", "zsh"].into_iter().filter(|sh| has(sh)).collect();
    if !shells.contains(&"zsh") {
        eprintln!("zsh isn't installed here: the resolve's login-shell checks run under bash only");
    }
    let temp_files = |r: &Repo| -> Vec<String> {
        std::fs::read_dir(r.work.join(".git"))
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .filter(|n| n.starts_with("arena-resolve."))
            .collect()
    };
    for shell in shells {
        let r = Repo::new(&format!("bound-{shell}"));
        r.merge_conflict(r.now - 5 * DAY, r.now - 10 * DAY);
        r.age(4 * HOUR);
        let conflicts = r.unmerged();
        let look = r.look();
        let work = r.work.to_str().unwrap();
        let bounded = resolve_command_bounded(work, 3, &look.op_key(), &look.print, "w1d1/devtest-x", GIT_MAX_FILE_BYTES_FOR_TESTS, (2, 1));
        let t0 = std::time::Instant::now();
        let (code, out, err) = r.shell_with(shell, &bounded, &[("ARENA_TEST_HOOK_BEFORE_CHANGE", "sleep 30")]);
        assert_eq!(code, Some(124), "{shell}: {out}{err}");
        assert!(t0.elapsed() < std::time::Duration::from_secs(20), "{shell}: bounded");
        assert!(err.contains("arena resolve: it ran past its 2 s on the pod and was stopped there (TERM)"), "{shell}: {err}");
        assert!(out.contains("SAVED "), "{shell}: stopped after the state before was kept: {out}");
        assert_eq!(temp_files(&r), Vec::<String>::new(), "{shell}: its temporary files removed");
        assert!(r.work.join(".git/MERGE_HEAD").exists() && r.unmerged() == conflicts, "{shell}: the merge as it was");
        // The real command under that shell: finished.
        let (code, out, err) = r.shell_with(shell, &resolve_command(work, 3, &look.op_key(), &look.print, "w1d1/devtest-x"), &[]);
        assert_eq!((code, parse_resolve(&out).resolved.as_deref()), (Some(0), Some("merge")), "{shell}: {out}{err}");
        assert_eq!(temp_files(&r), Vec::<String>::new(), "{shell}");
    }
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
