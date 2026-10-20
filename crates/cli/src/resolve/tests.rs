//! `pods backup` with a BUSY pod over a scripted `FakeRemote` and a stand-in rsync: the order
//! (its backup, its look, its copy, its resolve, its backup again), its line, what keeps it from
//! being finished (a failed copy, `BUSY_AUTO_RESOLVE_HOURS=0`, the resolve refusing or failing),
//! and part 2's pacing of its copies (unchanged: none; changed or an hour old: copied; the record
//! missing or corrupt: copied).

use super::Settled;
use crate::{handle_full_backup, BackupFailed, Selected, Unpushed, BACKUP_TIMEOUT};
use arena_core::backup::{backup_command, BACKUP_BUSY_EXIT};
use arena_core::remote::{FakeRemote, FakeReply, RemoteCall, PROBE_TIMEOUT};
use arena_core::resolve::{self as core_resolve, BusyCopy};
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

/// A backup dir, a state dir and a stand-in rsync that logs each destination — and fails for
/// bloom when `fail_bloom`.
struct World {
    base: PathBuf,
}

impl World {
    fn new(tag: &str) -> World {
        let base = std::env::temp_dir().join(format!("arena-cli-resolve-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("backups")).unwrap();
        let w = World { base };
        w.rsync(false);
        w
    }

    fn rsync(&self, fail_bloom: bool) {
        use std::os::unix::fs::PermissionsExt;
        let fail = if fail_bloom { "case \"$last\" in *devtest-bloom*) echo 'rsync: No space left on device (28)' >&2; exit 11;; esac\n" } else { "" };
        let script = self.base.join("rsync");
        std::fs::write(&script, format!("#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\necho \"$last\" >> \"$(dirname \"$0\")/calls.log\"\n{fail}")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
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
    FakeReply::stdout(&format!(
        "NOW {n}\nTOP {REPO}\nHOME /root\nOP merge {} {SHA} 0\nNEWEST {} 40\nPRINT {print}\n",
        n - 24 * HOUR,
        n - newest_ago
    ))
}

/// The pull's question to the pod: its repo is a plain directory.
fn repo_in_place() -> FakeReply {
    FakeReply::stdout(&format!("arena-repo-home=/root\narena-repo-real={REPO}\narena-repo-mount=no\n"))
}

fn resolved() -> FakeReply {
    FakeReply::stdout(&format!(
        "SAVED {REPO}/.git/arena-resolved/20261019T140200Z\nNEWER README.md\nGONE old.py\nHAND notes.py\nRESOLVED merge theirs 1760000000 1760500000\n"
    ))
}

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
            "✓ devtest-bloom -> autocommit-b — finished a merge left for 3h+ (kept the newer side, MERGE_HEAD's, for 2 files and the \
             hand-resolved version of 1; before: {root}/devtest-bloom, and {REPO}/.git/arena-resolved/20261019T140200Z on the pod)"
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
            assert!(calls.len() <= 3 && !calls.iter().any(|(c, _)| c.contains("arena-resolve")), "{calls:#?}");
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
    core_resolve::record_busy_copies(&w.record(), &[("devtest-bloom".into(), Some(old))], now()).unwrap();
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
    assert!(calls[2].0.contains(" arena-resolve ") && calls[3] == backup("bloom"), "{calls:#?}");
    assert!(w.copied().is_empty(), "not copied again");
    let said = crate::take_said();
    assert!(said.iter().any(|l| l.starts_with("✓ devtest-bloom -> autocommit-b — finished a merge left for 3h+ (") && l.contains(&format!("before: {}, and ", copy.to))), "{said:#?}");
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
