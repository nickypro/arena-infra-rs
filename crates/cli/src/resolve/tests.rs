//! `pods backup` with a BUSY pod over a scripted `FakeRemote` and a stand-in rsync: the order
//! (its backup, its look, its copy, its resolve, its backup again), its line, what keeps it from
//! being finished (a copy that failed in any part, `BUSY_AUTO_RESOLVE_HOURS=0`, a revert, the
//! resolve refusing or failing), part 2's pacing of its copies (unchanged: none; changed or an
//! hour old: copied; the record missing, corrupt, or of a copy gone since: copied), and the run's
//! failures once a pod is finished.

use super::{Busy, BusyPod, Settled};
use crate::{handle_full_backup, BackupFailed, LockMode, Selected, Unpushed, BACKUP_TIMEOUT};
use arena_core::backup::{backup_command, BACKUP_BUSY_EXIT};
use arena_core::remote::{FakeRemote, FakeReply, RemoteCall, PROBE_TIMEOUT};
use arena_core::resolve::{self as core_resolve, BusyCopy, CopyWhy};
use arena_core::{Config, Pod};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const REPO: &str = "/root/ARENA_materials";
const KEY: &str = "/root/.ssh/id_ed25519";
const HOUR: u64 = 3600;
/// MERGE_HEAD, as the look and the resolve name it.
const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

fn pod(name: &str, port: u16) -> Pod {
    Pod {
        id: format!("id-devtest-{name}"),
        name: format!("devtest-{name}"),
        provider: "runpod".into(),
        status: "RUNNING".into(),
        ssh_ip: Some("10.0.0.1".into()),
        ssh_port: Some(port),
        ..Default::default()
    }
}

fn host(port: u16) -> String {
    format!("10.0.0.1:{port}")
}

/// apple (22001, its backup pushes) and bloom (22002, mid-merge).
fn fleet() -> Selected {
    Selected::all(vec![pod("apple", 22001), pod("bloom", 22002)])
}

/// A backup dir, a state dir and a stand-in rsync that logs each destination and makes the copy's
/// `.git` there (`<dest>/ARENA_materials/.git`) — and fails for bloom when `fail_bloom`.
struct World {
    base: PathBuf,
}

impl World {
    fn new(tag: &str) -> World {
        let base = std::env::temp_dir().join(format!("arena-cli-resolve-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("backups")).unwrap();
        // Its real path: the copy record holds the copy's, every link resolved.
        let w = World { base: std::fs::canonicalize(&base).unwrap() };
        w.rsync(false);
        w
    }

    fn rsync(&self, fail_bloom: bool) {
        self.rsync_failing(if fail_bloom { "*devtest-bloom*" } else { "" }, "");
    }

    /// The stand-in rsync, failing when its destination matches `dest` (a `case` pattern) or its
    /// arguments match `args`.
    fn rsync_failing(&self, dest: &str, args: &str) {
        use std::os::unix::fs::PermissionsExt;
        let mut fail = String::new();
        if !dest.is_empty() {
            fail.push_str(&format!("case \"$last\" in {dest}) echo 'rsync: No space left on device (28)' >&2; exit 11;; esac\n"));
        }
        if !args.is_empty() {
            fail.push_str(&format!("case \"$*\" in {args}) echo 'rsync: read error' >&2; exit 23;; esac\n"));
        }
        let script = self.base.join("rsync");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\necho \"$last\" >> \"$(dirname \"$0\")/calls.log\"\n{fail}\
                 mkdir -p \"$last/ARENA_materials/.git\"\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The backup dir.
    fn dir(&self) -> String {
        self.base.join("backups").display().to_string()
    }

    fn rsync_path(&self) -> String {
        self.base.join("rsync").display().to_string()
    }

    /// The pods the rsync copied since the last call (one entry per rsync run).
    fn copied(&self) -> Vec<String> {
        let log = self.base.join("calls.log");
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let _ = std::fs::remove_file(&log);
        text.lines().map(|l| l.trim_end_matches('/').rsplit('/').next().unwrap_or_default().to_string()).collect()
    }

    fn cfg(&self, extra: &str) -> Config {
        Config::parse(&format!(
            "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom)\nBACKUP_REPO_PATH={REPO}\nGIT_SSH_KEY_REMOTE={KEY}\n\
             SHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nARENA_START_DATE=2026-01-05\nLOCAL_BACKUP_DIR={}\nARENA_STATE_DIR={}\n{extra}\n",
            self.base.join("backups").display(),
            self.base.join("state").display()
        ))
    }

    fn record(&self) -> PathBuf {
        self.base.join("state").join("devtest").join(core_resolve::BUSY_COPIES_FILE)
    }

    fn recorded(&self) -> Option<BusyCopy> {
        core_resolve::BusyCopies::load(&self.record()).0.pods.get("devtest-bloom").cloned()
    }

    /// Where a copy of this run goes: `<backups>/<wNdM>`.
    fn snapshot_root(&self, cfg: &Config) -> String {
        let (w, d) = crate::resolve_week_day(cfg, None, None).unwrap();
        format!("{}/w{w}d{d}", self.base.join("backups").display())
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// bloom's backup: a merge in progress, its conflicts unresolved (exit 6).
fn busy() -> FakeReply {
    FakeReply::exit(BACKUP_BUSY_EXIT, "").with_stdout("BRANCH autocommit-b\nBUSY autocommit-b merge unmerged\n")
}

fn pushed(b: &str) -> FakeReply {
    FakeReply::stdout(&format!("BRANCH {b}\nPUSHED {b}\n"))
}

/// bloom's look on the pod's clock `n`: a merge a day old; its newest file changed `newest_ago`
/// seconds ago; the files' checksum `print`.
fn look(n: u64, newest_ago: u64, print: &str) -> FakeReply {
    FakeReply::stdout(&look_text(n, newest_ago, print))
}

fn look_text(n: u64, newest_ago: u64, print: &str) -> String {
    format!(
        "NOW {n}\nTOP {REPO}\nREPO {REPO}\nHOME /root\nOP merge {} {SHA} 0\nNEWEST {} 40\nPRINT {print}\nINDEX {}\n",
        n - 24 * HOUR,
        n - newest_ago,
        n - 24 * HOUR
    )
}

/// The pull's question to the pod: its repo is a plain directory.
fn repo_in_place() -> FakeReply {
    FakeReply::stdout(&format!("arena-repo-home=/root\narena-repo-real={REPO}\narena-repo-mount=no\n"))
}

fn resolved() -> FakeReply {
    FakeReply::stdout(&format!(
        "SAVED {REPO}/.git/arena-resolved/20261019T140200Z\nNEWER theirs hunks 1760000000 1760500000 README.md\n\
         GONE theirs 1760000000 1760500000 old.py\nHAND notes.py\nRESOLVED merge\n"
    ))
}

/// The words [`resolved`]'s line has for its paths.
const RESOLVED_WAYS: &str = "README.md: MERGE_HEAD's side of each conflict, HEAD 2025-10-09 08:53Z / MERGE_HEAD 2025-10-15 03:46Z; \
                             old.py: removed, as on MERGE_HEAD, HEAD 2025-10-09 08:53Z / MERGE_HEAD 2025-10-15 03:46Z; notes.py: \
                             as resolved by hand";

fn execs(fake: &FakeRemote, port: u16) -> Vec<(String, Option<Duration>)> {
    fake.calls_to(&host(port))
        .into_iter()
        .filter_map(|c| match c {
            RemoteCall::Exec { cmd, timeout, .. } => Some((cmd, timeout)),
            RemoteCall::Copy { .. } => None,
        })
        .collect()
}

fn backup(name: &str) -> (String, Option<Duration>) {
    (backup_command(REPO, Some(KEY), &format!("arena backup devtest-{name}"), None), Some(BACKUP_TIMEOUT))
}

const MERGE_WHY: &str = "a merge is in progress on autocommit-b — not committed; resolve the conflicts and `git add` them, then \
                         `git commit` — or abort it (`git merge --abort`) on the pod";

/// The cron's `pods backup --no-pull`, and `pods backup` with its pull: a merge left alone for 3h+
/// (a day old, nothing changed for 4h) is backed up, looked at, copied (both tiers), finished on
/// the pod from the look the copy followed — then backed up again in the same run, under its lock.
/// Its line says what was done and where the state before is; the run succeeds (nothing failed
/// in the end); its copy record is forgotten.
#[tokio::test]
async fn a_merge_left_alone_is_backed_up_copied_finished_and_backed_up_again() {
    for no_pull in [true, false] {
        let w = World::new(if no_pull { "order-nopull" } else { "order-pull" });
        let cfg = w.cfg("");
        let fake = Arc::new(FakeRemote::new());
        let apple = if no_pull { vec![pushed("autocommit-a")] } else { vec![pushed("autocommit-a"), repo_in_place()] };
        fake.script(&host(22001), apple);
        let n = now();
        fake.script(&host(22002), [busy(), look(n, 4 * HOUR, "11-22"), repo_in_place(), resolved(), pushed("autocommit-b")]);
        let _ = crate::take_said();
        handle_full_backup(fake.clone(), &cfg, &fleet(), no_pull, None, false, true, &w.rsync_path()).await.unwrap();
        let calls = execs(&fake, 22002);
        assert_eq!(calls.len(), 5, "{calls:#?}");
        assert_eq!(calls[0], backup("bloom"));
        assert_eq!(calls[1].0, core_resolve::look_command(REPO), "then its look");
        assert_eq!(calls[2], (arena_core::volume::probe_command(REPO), Some(PROBE_TIMEOUT)), "then its copy");
        let root = w.snapshot_root(&cfg);
        let label = root.rsplit('/').next().unwrap();
        // The resolve is given what the look saw, and the copy's name.
        let op = format!("merge {} {SHA}", n - 24 * HOUR);
        let resolve = core_resolve::resolve_command(REPO, 3, &op, "11-22", &format!("{label}/devtest-bloom"));
        assert_eq!(calls[3], (resolve, Some(Duration::from_secs(300))));
        assert_eq!(calls[4], backup("bloom"), "and backed up again");
        let copied = w.copied();
        assert_eq!(copied.iter().filter(|p| *p == "devtest-bloom").count(), 3, "both tiers: {copied:?}");
        let said = crate::take_said();
        let line = format!(
            "✓ devtest-bloom -> autocommit-b — finished a merge left for 3h+ ({RESOLVED_WAYS}; before: \
             {REPO}/.git/arena-resolved/20261019T140200Z/ on the pod; today's copy: {root}/devtest-bloom)"
        );
        assert!(said.contains(&line), "{said:#?}");
        assert_eq!(w.recorded(), None, "finished: its record forgotten");
    }
}

/// A copy that fails is never followed by a resolve: bloom stays as it is, the run fails, its
/// line says both failures — and nothing is recorded (the next run copies it again).
#[tokio::test]
async fn a_failed_copy_is_never_followed_by_a_resolve() {
    let w = World::new("copyfail");
    w.rsync(true);
    let cfg = w.cfg("");
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [pushed("autocommit-a")]);
    fake.script(&host(22002), [busy(), look(now(), 4 * HOUR, "11-22"), repo_in_place(), resolved(), pushed("autocommit-b")]);
    let _ = crate::take_said();
    let err = handle_full_backup(fake.clone(), &cfg, &fleet(), true, None, false, true, &w.rsync_path()).await.unwrap_err();
    assert!(err.to_string().starts_with("1 pod(s) failed to back up; "), "{err}");
    let calls = execs(&fake, 22002);
    assert_eq!(calls.len(), 3, "its backup, look and copy only: {calls:#?}");
    assert_eq!(w.recorded(), None);
    assert!(!crate::take_said().iter().any(|l| l.starts_with("✓ devtest-bloom")));
}

/// `BUSY_AUTO_RESOLVE_HOURS=0`: never finished, however quiet — the copies are paced all the same.
#[tokio::test]
async fn off_means_never_finished_and_copies_still_paced() {
    let w = World::new("off");
    let cfg = w.cfg("BUSY_AUTO_RESOLVE_HOURS=0");
    let n = now();
    for (run, copies) in [("first", true), ("second", false)] {
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [pushed("autocommit-a")]);
        let mut bloom = vec![busy(), look(n, 4 * HOUR, "11-22")];
        if copies {
            bloom.push(repo_in_place());
        }
        bloom.extend([resolved(), pushed("autocommit-b")]);
        fake.script(&host(22002), bloom);
        let _ = crate::take_said();
        let err = handle_full_backup(fake.clone(), &cfg, &fleet(), true, None, false, true, &w.rsync_path()).await.unwrap_err();
        assert_eq!(err.to_string(), "1 pod(s) failed to back up", "{run}");
        assert_eq!(execs(&fake, 22002).len(), if copies { 3 } else { 2 }, "{run}: no resolve");
        assert_eq!(w.copied().is_empty(), !copies, "{run}");
        assert!(w.recorded().is_some(), "{run}");
    }
}

/// Part 2: bloom, BUSY every tick and its merge too recent to finish, is copied the first tick;
/// the next tick, unchanged, it isn't — its line says when it was copied, and where; its work tree
/// changed: copied; the recorded copy an hour old: copied; the record corrupt or gone: copied.
#[tokio::test]
async fn a_busy_pods_copy_is_paced_by_its_look() {
    let w = World::new("paced");
    let cfg = w.cfg("");
    let root = w.snapshot_root(&cfg);
    let n = now();
    // One tick with bloom's look saying `print`: (bloom copied, the run's lines).
    let tick = |print: &'static str| {
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [pushed("autocommit-a")]);
        // Its newest file changed 10 minutes ago: never finished.
        fake.script(&host(22002), [busy(), look(n, 600, print), repo_in_place()]);
        let cfg = cfg.clone();
        let rsync = w.rsync_path();
        async move {
            let _ = crate::take_said();
            let err = handle_full_backup(fake.clone(), &cfg, &fleet(), true, None, false, true, &rsync).await.unwrap_err();
            assert_eq!(err.to_string(), "1 pod(s) failed to back up");
            let calls = execs(&fake, 22002);
            assert!(calls.len() <= 3 && !calls.iter().any(|(c, _)| c.contains(" arena-resolve")), "{calls:#?}");
            (calls.len() == 3, crate::take_said())
        }
    };
    let (copied, said) = tick("11-22").await;
    assert!(copied, "no record: copied");
    assert_eq!(w.copied().len(), 3);
    let first = w.recorded().expect("its copy recorded");
    assert_eq!((first.print.as_str(), first.to.clone(), first.label.clone()), ("11-22", format!("{root}/devtest-bloom"), format!("{}/devtest-bloom", root.rsplit('/').next().unwrap())));
    assert_eq!(first.op, format!("merge {} {SHA}", n - 24 * HOUR));
    assert!(!said.iter().any(|l| l.contains("unchanged since")), "{said:#?}");

    let (copied, said) = tick("11-22").await;
    assert!(!copied, "unchanged since its copy, under an hour: not copied");
    assert!(w.copied().is_empty());
    let at = format!("{:02}:{:02} UTC", first.at % 86_400 / 3600, first.at % 3600 / 60);
    let line = format!("✗ devtest-bloom: not backed up to git ({MERGE_WHY}) — copied at {at} to {root}/devtest-bloom, unchanged since");
    assert!(said.contains(&line), "{line}\n{said:#?}");
    assert_eq!(w.recorded(), Some(first.clone()), "the record kept as it was");

    let (copied, _) = tick("33-44").await;
    assert!(copied, "its work tree changed: copied");
    assert_eq!(w.copied().len(), 3);
    assert_eq!(w.recorded().map(|r| r.print), Some("33-44".to_string()));

    // The recorded copy an hour old.
    let old = BusyCopy { at: now() - HOUR, ..w.recorded().unwrap() };
    core_resolve::record_busy_copies(&w.record(), &[("devtest-bloom".into(), Some(old))], now(), core_resolve::RECORD_LOCK_WAIT).unwrap();
    let (copied, _) = tick("33-44").await;
    assert!(copied, "an hour old: copied");
    assert_eq!(w.copied().len(), 3);

    // Corrupt, then gone: copied each time.
    for (case, damage) in [("corrupt", Some("{not json")), ("gone", None)] {
        match damage {
            Some(text) => std::fs::write(w.record(), text).unwrap(),
            None => std::fs::remove_file(w.record()).unwrap(),
        }
        let (copied, _) = tick("33-44").await;
        assert!(copied, "{case}: copied");
        assert_eq!(w.copied().len(), 3, "{case}");
        assert_eq!(w.recorded().map(|r| r.print), Some("33-44".to_string()), "{case}: recorded again");
    }
}

/// Finished from the recorded copy (part 2): quiet now, unchanged since a copy recorded earlier —
/// not copied again, finished, backed up; its line names that copy.
#[tokio::test]
async fn a_paced_pod_is_finished_from_its_recorded_copy() {
    let w = World::new("paced-resolve");
    let cfg = w.cfg("");
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [pushed("autocommit-a")]);
    let n = now();
    fake.script(&host(22002), [busy(), look(n, 4 * HOUR, "55-66"), repo_in_place(), FakeReply::exit(7, "").with_stdout("REFUSED changed: files changed since the copy\n")]);
    let _ = crate::take_said();
    // First tick: copied, then the resolve refuses (something changed meanwhile): said, not finished.
    let err = handle_full_backup(fake.clone(), &cfg, &fleet(), true, None, false, true, &w.rsync_path()).await.unwrap_err();
    assert_eq!(err.to_string(), "1 pod(s) failed to back up");
    assert_eq!(execs(&fake, 22002).len(), 4, "backup, look, copy, resolve — no backup after a refusal");
    let said = crate::take_said();
    assert!(said.contains(&"⊘ devtest-bloom: not finished automatically — changed: files changed since the copy".to_string()), "{said:#?}");
    let copy = w.recorded().unwrap();
    assert_eq!(w.copied().len(), 3);
    // Next tick: unchanged since that copy — not copied, finished, backed up.
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [pushed("autocommit-a")]);
    fake.script(&host(22002), [busy(), look(n, 4 * HOUR, "55-66"), resolved(), pushed("autocommit-b")]);
    handle_full_backup(fake.clone(), &cfg, &fleet(), true, None, false, true, &w.rsync_path()).await.unwrap();
    let calls = execs(&fake, 22002);
    assert_eq!(calls.len(), 4, "{calls:#?}");
    assert!(calls[2].0.contains(" arena-resolve") && calls[3] == backup("bloom"), "{calls:#?}");
    assert!(w.copied().is_empty(), "not copied again");
    let said = crate::take_said();
    assert!(said.iter().any(|l| l.starts_with("✓ devtest-bloom -> autocommit-b — finished a merge left for 3h+ (") && l.ends_with(&format!("; today's copy: {})", copy.to))), "{said:#?}");
    assert!(said.iter().any(|l| l.starts_with("✗ devtest-bloom: not backed up to git (") && l.ends_with("unchanged since")), "the paced line first: {said:#?}");
}

/// What else the resolve can end in, each said on the pod's line and still a failure: it failed
/// midway (where the files as they were are); it finished but the backup after failed; it couldn't
/// be reached.
#[tokio::test]
async fn a_resolve_that_fails_or_a_backup_after_it_that_fails_is_said() {
    let failed_midway = FakeReply::exit(8, "error: unable to unlink old 'data.bin'")
        .with_stdout(&format!("SAVED {REPO}/.git/arena-resolved/20261019T140200Z\nNEWER README.md\nFAILED git checkout --theirs data.bin\n"));
    let cases: Vec<(&str, Vec<FakeReply>, &str)> = vec![
        (
            "midway",
            vec![failed_midway],
            "✗ devtest-bloom: finishing its operation in progress failed (at git checkout --theirs data.bin); the conflicted files as \
             they were are in /root/ARENA_materials/.git/arena-resolved/20261019T140200Z on the pod — the copy ",
        ),
        (
            "backup after",
            vec![resolved(), FakeReply::exit(1, "error: failed to push some refs").with_stdout("BRANCH autocommit-b\nPUSH_FAILED\n")],
            "✗ devtest-bloom: finished a merge left for 3h+ (",
        ),
        ("unreachable", vec![FakeReply::exit(255, "ssh: connect to host 10.0.0.1 port 22002: Connection refused")], "✗ devtest-bloom: finishing its operation in progress failed (exit 255: ssh: connect to host"),
    ];
    for (case, after, want) in cases {
        let w = World::new(&format!("fail-{}", case.replace(' ', "-")));
        let cfg = w.cfg("");
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [pushed("autocommit-a")]);
        fake.script(&host(22002), [busy(), look(now(), 4 * HOUR, "11-22"), repo_in_place()].into_iter().chain(after).collect::<Vec<_>>());
        let _ = crate::take_said();
        let err = handle_full_backup(fake.clone(), &cfg, &fleet(), true, None, false, true, &w.rsync_path()).await.unwrap_err();
        assert_eq!(err.to_string(), "1 pod(s) failed to back up", "{case}");
        let said = crate::take_said();
        let line = said.iter().find(|l| l.starts_with(want)).unwrap_or_else(|| panic!("{case}: {said:#?}"));
        if case == "backup after" {
            assert!(line.contains(", but its backup then failed: "), "{line}");
        }
        assert!(w.recorded().is_some(), "{case}: not finished, its record kept");
    }
}

/// A pod finished and backed up is no longer one of the run's failures, nor given the line of one —
/// and only those pods come off the count: the git step's failure names each pod that failed,
/// whichever way (review ai-review claude #3, codex #8), never rebuilt from the BUSY pods.
#[test]
fn a_finished_pod_is_no_longer_a_failure_and_no_other_failure_is_hidden() {
    let u = |p: &str| Unpushed { pod: p.into(), why: "w".into(), branch: None, at_push: true, wrong_origin: false, busy: Some("merge".into()) };
    let unpushed = vec![u("devtest-a"), u("devtest-b")];
    let failed = |pods: &[&str]| -> anyhow::Result<()> { Err(BackupFailed { pods: pods.iter().map(|p| p.to_string()).collect() }.into()) };
    let one = Settled { lines: vec![], done: vec!["devtest-a".into()] };
    assert_eq!(one.unsettled(&unpushed), vec![u("devtest-b")]);
    assert_eq!(one.git(failed(&["devtest-a", "devtest-b"])).unwrap_err().to_string(), "1 pod(s) failed to back up");
    // One that failed before its push (ssh refused, a `set -e` exit): never taken back.
    let left = one.git(failed(&["devtest-a", "devtest-c"])).unwrap_err();
    assert_eq!((left.to_string(), left.downcast_ref::<BackupFailed>().map(|f| f.pods.clone())), ("1 pod(s) failed to back up".to_string(), Some(vec!["devtest-c".to_string()])));
    assert!(one.git(failed(&["devtest-a"])).is_ok());
    let both = Settled { lines: vec![], done: vec!["devtest-a".into(), "devtest-b".into()] };
    assert!(both.unsettled(&unpushed).is_empty());
    assert!(both.git(failed(&["devtest-a", "devtest-b"])).is_ok());
    assert_eq!(both.git(failed(&["devtest-a", "devtest-b", "devtest-c", "devtest-d"])).unwrap_err().to_string(), "2 pod(s) failed to back up");
    let none = Settled::default();
    assert_eq!(none.unsettled(&unpushed), unpushed);
    assert_eq!(none.git(failed(&["devtest-a", "devtest-b"])).unwrap_err().to_string(), "2 pod(s) failed to back up");
    assert!(none.git(Ok(())).is_ok());
    // Any other failure of the git step: as it was.
    assert_eq!(one.git(Err(anyhow::anyhow!("no SSH targets"))).unwrap_err().to_string(), "no SSH targets");
}

/// The cron's run where one pod's backup fails before its push (ssh refused) while another, BUSY,
/// is finished: the run still fails, for the one.
#[tokio::test]
async fn a_pod_that_failed_before_its_push_stays_a_failure_when_another_is_finished() {
    for (case, apple) in [
        ("ssh", FakeReply::exit(255, "ssh: connect to host 10.0.0.1 port 22001: Connection refused")),
        ("set-e", FakeReply::exit(128, "fatal: not a git repository (or any of the parent directories): .git")),
    ] {
        let w = World::new(&format!("other-failure-{case}"));
        let cfg = w.cfg("");
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [apple]);
        fake.script(&host(22002), [busy(), look(now(), 4 * HOUR, "11-22"), repo_in_place(), resolved(), pushed("autocommit-b")]);
        let _ = crate::take_said();
        let err = handle_full_backup(fake.clone(), &cfg, &fleet(), true, None, false, true, &w.rsync_path()).await.unwrap_err();
        assert_eq!(err.to_string(), "1 pod(s) failed to back up", "{case}");
        assert_eq!(err.downcast_ref::<BackupFailed>().map(|f| f.pods.clone()), Some(vec!["devtest-apple".to_string()]), "{case}");
        let said = crate::take_said();
        assert!(said.iter().any(|l| l.starts_with("✓ devtest-bloom -> autocommit-b — finished a merge")), "{case}: {said:#?}");
    }
}

/// A revert stays BUSY however quiet, and its line says arena won't finish one; it is copied and
/// paced as any BUSY pod.
#[tokio::test]
async fn a_revert_is_never_finished_and_said() {
    let w = World::new("revert");
    let cfg = w.cfg("");
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [pushed("autocommit-a")]);
    let n = now();
    let revert = look_text(n, 4 * HOUR, "11-22").replace(&format!("OP merge {} {SHA} 0", n - 24 * HOUR), &format!("OP revert {} {SHA} 0", n - 24 * HOUR));
    let busy_revert = FakeReply::exit(BACKUP_BUSY_EXIT, "").with_stdout("BRANCH autocommit-b\nBUSY autocommit-b revert unmerged\n");
    fake.script(&host(22002), [busy_revert, FakeReply::stdout(&revert), repo_in_place(), resolved()]);
    let _ = crate::take_said();
    let err = handle_full_backup(fake.clone(), &cfg, &fleet(), true, None, false, true, &w.rsync_path()).await.unwrap_err();
    assert_eq!(err.to_string(), "1 pod(s) failed to back up");
    let calls = execs(&fake, 22002);
    assert_eq!(calls.len(), 3, "its backup, look and copy — no resolve: {calls:#?}");
    let said = crate::take_said();
    let line = format!("⊘ devtest-bloom: not finished automatically — {}", core_resolve::REVERT_WORDS);
    assert!(said.contains(&line), "{said:#?}");
    assert!(w.recorded().is_some(), "copied and recorded, as any BUSY pod");
}

/// A repo linked onto the `/workspace` volume (review ai-review claude #6): the look's top is the
/// volume's, its configured path the link in the home — a pull of the home copies the tree in the
/// link's place, so the copy holds it: recorded (its `.git` there), and the merge finished.
#[tokio::test]
async fn a_repo_linked_onto_the_volume_is_finished_from_a_pull_of_the_home() {
    let w = World::new("linked");
    let cfg = w.cfg("");
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [pushed("autocommit-a")]);
    let n = now();
    let linked = look_text(n, 4 * HOUR, "11-22").replace(&format!("TOP {REPO}\n"), "TOP /workspace/ARENA_materials\n");
    fake.script(&host(22002), [busy(), FakeReply::stdout(&linked), repo_in_place(), resolved(), pushed("autocommit-b")]);
    let _ = crate::take_said();
    handle_full_backup(fake.clone(), &cfg, &fleet(), true, None, false, true, &w.rsync_path()).await.unwrap();
    let calls = execs(&fake, 22002);
    assert!(calls.len() == 5 && calls[3].0.contains(" arena-resolve"), "{calls:#?}");
    // Without the link's path in the look, the top alone isn't in the home: no resolve.
    let w = World::new("linked-unseen");
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [pushed("autocommit-a")]);
    let unseen = linked.replace(&format!("REPO {REPO}\n"), "");
    fake.script(&host(22002), [busy(), FakeReply::stdout(&unseen), repo_in_place(), resolved(), pushed("autocommit-b")]);
    let err = handle_full_backup(fake.clone(), &w.cfg(""), &fleet(), true, None, false, true, &w.rsync_path()).await.unwrap_err();
    assert_eq!(err.to_string(), "1 pod(s) failed to back up");
    assert_eq!(execs(&fake, 22002).len(), 3, "no resolve");
}

/// An earlier run's record is trusted only while its copy is still there, under this backup dir
/// (review ai-review codex #7): the copy deleted since — copied again (and a copy that then fails
/// is never followed by a resolve, as it would be on the stale record); `LOCAL_BACKUP_DIR`
/// changed since — copied into the new one.
#[tokio::test]
async fn a_record_whose_copy_is_gone_or_elsewhere_means_copy_now() {
    let w = World::new("gone");
    let cfg = w.cfg("");
    let n = now();
    // A tick whose resolve refuses: bloom copied and recorded, not finished.
    let refuse = || FakeReply::exit(7, "").with_stdout("REFUSED changed: files changed since the copy\n");
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [pushed("autocommit-a")]);
    fake.script(&host(22002), [busy(), look(n, 4 * HOUR, "55-66"), repo_in_place(), refuse()]);
    let _ = handle_full_backup(fake.clone(), &cfg, &fleet(), true, None, false, true, &w.rsync_path()).await.unwrap_err();
    let first = w.recorded().unwrap();
    assert_eq!(first.git, format!("{}/ARENA_materials/.git", first.to));
    assert_eq!(w.copied().len(), 3);
    // Its copy deleted since; this tick's copy fails: not finished — the stale record never counts.
    std::fs::remove_dir_all(&first.to).unwrap();
    w.rsync(true);
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [pushed("autocommit-a")]);
    fake.script(&host(22002), [busy(), look(n, 4 * HOUR, "55-66"), repo_in_place(), resolved(), pushed("autocommit-b")]);
    let _ = handle_full_backup(fake.clone(), &cfg, &fleet(), true, None, false, true, &w.rsync_path()).await.unwrap_err();
    let calls = execs(&fake, 22002);
    assert_eq!(calls.len(), 3, "copied again (it failed), never resolved: {calls:#?}");
    assert!(w.copied().iter().any(|p| p == "devtest-bloom"), "copied again");
    // LOCAL_BACKUP_DIR changed since (the same state dir): copied into the new one, unchanged as it is.
    w.rsync(false);
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [pushed("autocommit-a")]);
    fake.script(&host(22002), [busy(), look(n, 600, "55-66"), repo_in_place()]);
    let elsewhere = w.base.join("backups2");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let moved = w.cfg(&format!("LOCAL_BACKUP_DIR={}", elsewhere.display()));
    let _ = handle_full_backup(fake.clone(), &moved, &fleet(), true, None, false, true, &w.rsync_path()).await.unwrap_err();
    assert_eq!(execs(&fake, 22002).len(), 3, "copied");
    let now_to = w.recorded().unwrap().to;
    assert!(now_to.starts_with(&elsewhere.display().to_string()), "{now_to}");
}

/// `p` (absolute) as a path relative to this process's working directory: `../` up to the root,
/// then `p` — what a relative `LOCAL_BACKUP_DIR` is.
fn relative_to_cwd(p: &std::path::Path) -> String {
    let cwd = std::env::current_dir().unwrap();
    let up = cwd.components().filter(|c| matches!(c, std::path::Component::Normal(_))).count();
    format!("{}{}", "../".repeat(up), p.display().to_string().trim_start_matches('/'))
}

/// The copy record holds the copy's real path, and an earlier record counts however the backup dir
/// is spelled (review ai-review claude #8): a relative `LOCAL_BACKUP_DIR` (the default `./backup`
/// is one) is recorded as the absolute path it is, and the lines name that; a backup dir spelled
/// through a link and then by its real path is the same dir — the record still holds, bloom is
/// not copied again, and is finished from it.
#[tokio::test]
async fn a_backup_dir_spelled_relatively_or_through_a_link_holds_the_same_copy() {
    let n = now();
    let refuse = || FakeReply::exit(7, "").with_stdout("REFUSED changed: files changed since the copy\n");
    // One tick under `cfg`: bloom's look quiet and unchanged ("55-66"), then `after` (its copy and
    // resolve, as scripted) — (the rsync's copies of bloom, the lines).
    let tick = |w: &World, cfg: &Config, after: Vec<FakeReply>| {
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [pushed("autocommit-a")]);
        fake.script(&host(22002), [busy(), look(n, 4 * HOUR, "55-66")].into_iter().chain(after).collect::<Vec<_>>());
        let (cfg, rsync) = (cfg.clone(), w.rsync_path());
        async move {
            let _ = crate::take_said();
            let _ = handle_full_backup(fake.clone(), &cfg, &fleet(), true, None, false, true, &rsync).await;
            (fake, crate::take_said())
        }
    };
    let resolves = |fake: &FakeRemote| execs(fake, 22002).iter().filter(|(c, _)| c.contains(" arena-resolve")).count();

    // Relative to this process's working directory.
    let w = World::new("dir-relative");
    let rel = relative_to_cwd(&w.base.join("backups"));
    assert!(!std::path::Path::new(&rel).is_absolute(), "{rel}");
    let cfg = w.cfg(&format!("LOCAL_BACKUP_DIR={rel}"));
    let (fake, _) = tick(&w, &cfg, vec![repo_in_place(), refuse()]).await;
    assert_eq!((w.copied().len(), resolves(&fake)), (3, 1), "copied, then the resolve refused");
    let first = w.recorded().unwrap();
    let to = format!("{}/devtest-bloom", w.snapshot_root(&cfg));
    assert_eq!((first.to.as_str(), first.git.clone()), (to.as_str(), format!("{to}/ARENA_materials/.git")), "absolute, real");
    let (fake, said) = tick(&w, &cfg, vec![resolved(), pushed("autocommit-b")]).await;
    assert!(w.copied().is_empty(), "unchanged since: not copied again");
    assert_eq!(resolves(&fake), 1, "finished from the recorded copy");
    assert!(said.iter().any(|l| l.starts_with("✗ devtest-bloom: not backed up to git (") && l.ends_with(&format!(" to {to}, unchanged since"))), "{said:#?}");
    assert!(said.iter().any(|l| l.starts_with("✓ devtest-bloom -> autocommit-b — ") && l.ends_with(&format!("; today's copy: {to})"))), "{said:#?}");

    // Through a link, then by its real path: the same dir.
    let w = World::new("dir-link");
    std::os::unix::fs::symlink(w.base.join("backups"), w.base.join("link")).unwrap();
    let linked = w.cfg(&format!("LOCAL_BACKUP_DIR={}", w.base.join("link").display()));
    let (fake, _) = tick(&w, &linked, vec![repo_in_place(), refuse()]).await;
    assert_eq!((w.copied().len(), resolves(&fake)), (3, 1));
    let first = w.recorded().unwrap();
    let real = w.cfg("");
    assert_eq!(first.to, format!("{}/devtest-bloom", w.snapshot_root(&real)), "the real path, not the link's");
    let (fake, said) = tick(&w, &real, vec![resolved(), pushed("autocommit-b")]).await;
    assert!(w.copied().is_empty(), "the same copy, the dir spelled otherwise: not copied again");
    assert_eq!(resolves(&fake), 1, "finished from it: {said:#?}");
    assert_eq!(w.recorded(), None, "finished: forgotten");
}

/// A BUSY pod's copy counts — for its record and a resolve — only when whole (review ai-review
/// claude #8): every rsync of both tiers, the snapshot's `.git` pass among them. Each way a copy
/// can fall short leaves the pod in the pull's failures (but one never pulled at all) and out of
/// its whole copies, and then it is neither recorded nor finished: its lock held by another
/// backup, no endpoint, a timeout, the big tier failing, the `.git` pass failing, not pulled at all.
#[tokio::test]
async fn a_busy_pods_copy_counts_only_when_whole() {
    // (case, the stand-in rsync's failing destination and arguments, extra config, bloom reachable,
    // bloom copied instead of its git backup, the lock held, finished)
    type Case<'a> = (&'a str, &'a str, &'a str, &'a str, bool, bool, bool, bool);
    let cases: [Case; 7] = [
        ("whole", "", "", "", true, true, false, true),
        ("lock-busy", "", "", "", true, true, true, false),
        ("no-endpoint", "", "", "", false, true, false, false),
        ("timeout", "", "", "BACKUP_TIMEOUT_SECS=1", true, true, false, false),
        ("big-tier", "*/big/devtest-bloom*", "", "", true, true, false, false),
        ("git-pass", "", "*'--include */ --exclude *'*", "", true, true, false, false),
        ("not-pulled", "", "", "", true, false, false, false),
    ];
    for (case, dest, args, extra, reachable, copied_instead, locked, finished) in cases {
        let w = World::new(&format!("whole-{case}"));
        w.rsync_failing(dest, args);
        if case == "timeout" {
            std::fs::write(w.base.join("rsync"), "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\nsleep 5\nmkdir -p \"$last/ARENA_materials/.git\"\n").unwrap();
        }
        let cfg = w.cfg(extra);
        let n = now();
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22002), [repo_in_place(), resolved(), pushed("autocommit-b")]);
        let remote: Arc<dyn arena_core::remote::Remote> = fake.clone();
        let bloom = pod("bloom", 22002);
        let (name, target) = Selected::all(vec![bloom.clone()]).named_targets(&cfg).unwrap().remove(0);
        // apple, copied too (a failed push), always reachable: the pull isn't bloom's alone.
        let sel = Selected::all(vec![pod("apple", 22001), arena_core::Pod { ssh_ip: reachable.then(|| "10.0.0.1".to_string()), ..bloom }]);
        let u = |pod: &str, at_push| Unpushed { pod: pod.into(), why: "w".into(), branch: None, at_push, wrong_origin: false, busy: Some("merge".into()) };
        let unpushed = [u("devtest-apple", true), u(&name, copied_instead)];
        let held = locked.then(|| arena_core::pull::try_pod_lock_now(&w.dir(), &name).unwrap().unwrap());
        let pulled = crate::copy_unpushed(&remote, &cfg, &w.dir(), &sel, &unpushed, &w.rsync_path(), LockMode::Take(Duration::from_millis(300))).await;
        drop(held);
        let p = pulled.as_ref().unwrap();
        assert_eq!(p.whole.contains(&name), finished, "{case}: {:?}", p.whole);
        assert_eq!(p.whole.contains(&"devtest-apple".to_string()), !matches!(case, "timeout" | "git-pass"), "{case}: apple {:?}", p.whole);
        if !finished && copied_instead {
            assert!(p.failed.iter().any(|(q, _)| *q == name), "{case}: in the pull's failures: {:?}", p.failed);
        }
        let look = core_resolve::parse_look(&look_text(n, 4 * HOUR, "11-22")).unwrap();
        let busy = Busy {
            hours: Some(3),
            record: Some(w.record()),
            copies: Default::default(),
            pods: vec![BusyPod { name: name.clone(), target, why: "w".into(), look: Some(look), copy: Some(CopyWhy::First) }],
            dir: w.dir(),
            now: n,
        };
        let settled = busy.settle(&remote, &cfg, &pulled, false, None).await;
        let resolves = execs(&fake, 22002).iter().filter(|(c, _)| c.contains(" arena-resolve")).count();
        assert_eq!((resolves, settled.done.contains(&name)), (usize::from(finished), finished), "{case}");
        assert!(w.recorded().is_none(), "{case}: forgotten once finished, else never recorded");
    }
}

/// `config check` says when a BUSY pod is finished — by default, as set, or never — and a value it
/// can't read is a ✗ row the check fails on (`config_check_invalid_values_…` covers that).
#[test]
fn config_check_says_when_a_busy_pods_merge_is_finished() {
    let base = "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple)\nRUNPOD_API_KEY=not-a-real-key\nGPU_TYPE=\"NVIDIA RTX A4000\"\nIMAGE=img:1\n";
    let row = |words: &str| format!("  · finish a BUSY pod's merge  {words} (BUSY_AUTO_RESOLVE_HOURS)");
    for (extra, want) in [
        ("", row("after 3h with nothing changed (default)")),
        ("BUSY_AUTO_RESOLVE_HOURS=6", row("after 6h with nothing changed")),
        ("BUSY_AUTO_RESOLVE_HOURS=0", row("off — an operation left in progress stays until the participant ends it")),
        ("BUSY_AUTO_RESOLVE_HOURS=off", row("off — an operation left in progress stays until the participant ends it")),
    ] {
        let (report, missing) = crate::config_report(&Config::parse(&format!("{base}{extra}\n")), "runpod");
        assert!(report.lines().any(|l| l == want), "{extra:?}: {want}\n{report}");
        assert!(missing.is_empty(), "{extra:?}: {missing:?}");
    }
    let (report, missing) = crate::config_report(&Config::parse(&format!("{base}BUSY_AUTO_RESOLVE_HOURS=soon\n")), "runpod");
    assert!(report.lines().any(|l| l.starts_with("  ✗ finish a BUSY pod's merge  config error: BUSY_AUTO_RESOLVE_HOURS must be")), "{report}");
    assert_eq!(missing, ["BUSY_AUTO_RESOLVE_HOURS (must be a whole number of hours, 1..=720, or 0/off to never finish an operation left in progress)"]);
}
