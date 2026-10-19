//! `pods backup` with a BUSY pod over a scripted `FakeRemote` and a stand-in rsync: the order
//! (its backup, its look, its copy, its resolve, its backup again), its line, and what keeps it
//! from being finished (a failed copy, `BUSY_AUTO_RESOLVE_HOURS=0`, the resolve refusing or
//! failing).

use super::Settled;
use crate::{handle_full_backup, Selected, Unpushed, BACKUP_TIMEOUT};
use arena_core::backup::{backup_command, BACKUP_BUSY_EXIT};
use arena_core::remote::{FakeRemote, FakeReply, RemoteCall, PROBE_TIMEOUT};
use arena_core::resolve as core_resolve;
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
    }
}

/// A copy that fails is never followed by a resolve: bloom stays as it is, the run fails, its
/// line says both failures.
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
    assert!(!crate::take_said().iter().any(|l| l.starts_with("✓ devtest-bloom")));
}

/// `BUSY_AUTO_RESOLVE_HOURS=0`: never finished, however quiet — copied as before.
#[tokio::test]
async fn off_means_never_finished() {
    let w = World::new("off");
    let cfg = w.cfg("BUSY_AUTO_RESOLVE_HOURS=0");
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [pushed("autocommit-a")]);
    fake.script(&host(22002), [busy(), look(now(), 4 * HOUR, "11-22"), repo_in_place(), resolved(), pushed("autocommit-b")]);
    let _ = crate::take_said();
    let err = handle_full_backup(fake.clone(), &cfg, &fleet(), true, None, false, true, &w.rsync_path()).await.unwrap_err();
    assert_eq!(err.to_string(), "1 pod(s) failed to back up");
    assert_eq!(execs(&fake, 22002).len(), 3, "its backup, look and copy: no resolve");
    assert_eq!(w.copied().len(), 3);
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
    }
}

/// A pod finished and backed up is no longer one of the run's failures, nor given the line of one.
#[test]
fn a_finished_pod_is_no_longer_a_failure() {
    let u = |p: &str| Unpushed { pod: p.into(), why: "w".into(), branch: None, at_push: true, wrong_origin: false, busy: Some("merge".into()) };
    let unpushed = vec![u("devtest-a"), u("devtest-b")];
    let failed = || Err(anyhow::anyhow!("2 pod(s) failed to back up"));
    let one = Settled { lines: vec![], done: vec!["devtest-a".into()] };
    assert_eq!(one.unsettled(&unpushed), vec![u("devtest-b")]);
    assert_eq!(one.git(failed(), &unpushed).unwrap_err().to_string(), "1 pod(s) failed to back up");
    let both = Settled { lines: vec![], done: vec!["devtest-a".into(), "devtest-b".into()] };
    assert!(both.unsettled(&unpushed).is_empty());
    assert!(both.git(failed(), &unpushed).is_ok());
    let none = Settled::default();
    assert_eq!(none.unsettled(&unpushed), unpushed);
    assert_eq!(none.git(failed(), &unpushed).unwrap_err().to_string(), "2 pod(s) failed to back up");
    assert!(none.git(Ok(()), &unpushed).is_ok());
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
