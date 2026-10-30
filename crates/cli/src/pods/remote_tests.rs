use std::sync::Arc;
use std::time::Duration;

use arena_core::backup::{backup_command, checkout_command, checkout_command_limited, init_branch_command, BackupConfig, BACKUP_BUSY_EXIT};
use arena_core::remote::{FakeRemote, FakeReply, Remote, RemoteCall, PROBE_TIMEOUT};
use arena_core::selector::{Naming, SelectArgs};
use arena_core::ssh::{login_shell_wrap, SshTarget};
use arena_core::{Config, Pod, PodSpec, Provider, Result};
use async_trait::async_trait;
use clap::Parser;
use tokio::time::Instant;

use crate::cli::{Cli, Cmd, SelectByFlag};
use crate::pods::backup::{backup_fleet, failure_why, fallback_lines, handle_backup, handle_full_backup, telling_line, BackupTally, Unpushed};
use crate::pods::baseline::handle_baseline;
use crate::pods::branch::{discarded_note, handle_init_branches, handle_set_branch, not_pushed_note, push_first_note, unpushed_note, unredacted_note};
use crate::pods::copy::{copy_to_pod, handle_copy, CopyPlan};
use crate::pods::health::{deep_check_fleet, duplicate_names, handle_deep_test, render_deep_test};
use crate::pods::home_copy::{copy_failure_reason, copy_pod_files, mirror_refusal_text, CopyLeg};
use crate::pods::list::probe_gpus;
use crate::pods::migrate::{proxy_reaches_pod, sync_time_label};
use crate::pods::pull::{handle_pull, local_backup_dir, pull_label_and_targets, resolve_pull, take_pod_locks, Pulled};
use crate::pods::replace::{marker_present, replace_copy_failed, target_is_pod};
use crate::pods::restore::{handle_restore, restore_note, restore_notes, RestoreOpts, RestoreSource};
use crate::pods::run::{handle_run, render_run, run_failure, run_fleet, run_preview, RunResult};
use crate::pods::selection::{select, take_said, Selected, Unscoped};
use super::{cp_timeout, each_pod, handle_pods, local_size, PodCall, PodCmd, Select, BACKUP_TIMEOUT, BRANCH_TIMEOUT, CP_BASE_TIMEOUT, POD_COPY_TIMEOUT, RUN_TIMEOUT_SECS, TEST_TIMEOUT};

/// A provider that lists a fixed fleet (`name()` is `kind`: "runpod" turns on the
/// replace path's RUNPOD_POD_ID identity checks).
struct Fleet {
    kind: &'static str,
    pods: Vec<Pod>,
}

#[async_trait]
impl Provider for Fleet {
    fn name(&self) -> &'static str {
        self.kind
    }
    fn describe(&self, _spec: &PodSpec) -> String {
        String::new()
    }
    async fn list_pods(&self) -> Result<Vec<Pod>> {
        Ok(self.pods.clone())
    }
    async fn create_pod(&self, _spec: &PodSpec) -> Result<Pod> {
        unimplemented!("not exercised")
    }
    async fn stop_pod(&self, _id: &str) -> Result<()> {
        Ok(())
    }
    async fn restart_pod(&self, _id: &str) -> Result<()> {
        Ok(())
    }
    async fn terminate_pod(&self, _id: &str) -> Result<()> {
        Ok(())
    }
}

/// `devtest-<name>` at `10.0.0.1:<port>` — the FakeRemote host key is `10.0.0.1:<port>`.
fn pod(name: &str, port: u16) -> Pod {
    Pod {
        id: format!("id-devtest-{name}"),
        name: format!("devtest-{name}"),
        provider: "runpod".into(),
        status: "RUNNING".into(),
        gpu_type: Some("RTX A4000".into()),
        gpu_count: Some(1),
        ssh_ip: Some("10.0.0.1".into()),
        ssh_port: Some(port),
        ..Default::default()
    }
}

fn fleet_of(pods: &[(&str, u16)]) -> Fleet {
    Fleet { kind: "runpod", pods: pods.iter().map(|(n, p)| pod(n, *p)).collect() }
}

/// apple / bloom / cloud on ports 22001-22003.
fn fleet() -> Fleet {
    fleet_of(&[("apple", 22001), ("bloom", 22002), ("cloud", 22003)])
}

fn host(port: u16) -> String {
    format!("10.0.0.1:{port}")
}

const REPO: &str = "/root/ARENA_materials";
const KEY: &str = "/root/.ssh/id_ed25519";

fn cfg() -> Config {
    Config::parse(&cfg_text())
}

fn cfg_text() -> String {
    format!(
        "MACHINE_NAME_PREFIX=devtest\nBACKUP_REPO_PATH={REPO}\nGIT_SSH_KEY_REMOTE={KEY}\n\
         SHARED_SSH_KEY_PATH=/nonexistent/devtest_key\n"
    )
}

fn targets(f: &Fleet) -> Vec<(String, SshTarget)> {
    f.pods.iter().map(|p| (p.name.clone(), SshTarget::from_pod(p, &cfg()).unwrap())).collect()
}

/// The whole fleet, nothing named (what a bare `pods run` / `pods backup` resolves to).
fn everyone(f: &Fleet) -> Selected {
    Selected::all(f.pods.clone())
}

/// `targets` resolved against `f` by the shared selector, as the command would.
async fn picked(f: &Fleet, cfg: &Config, targets: &[&str]) -> Selected {
    select(f, cfg, &SelectArgs::exact(targets.iter().copied()), Unscoped::All).await.unwrap()
}

/// `pods <args…>` parsed as the CLI would.
fn pods(args: &[&str]) -> PodCmd {
    let argv = ["arena", "pods"].iter().chain(args).copied();
    match Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{args:?}: {e}")).cmd {
        Cmd::Pods(p) => p,
        _ => unreachable!(),
    }
}

/// The ports that got any call, sorted.
fn ports(fake: &FakeRemote) -> Vec<u16> {
    let mut p: Vec<u16> =
        fake.calls().iter().filter_map(|c| c.host().rsplit_once(':').and_then(|(_, p)| p.parse().ok())).collect();
    p.sort();
    p.dedup();
    p
}

/// The (cmd, timeout) of every exec made to one host, in order.
fn execs(fake: &FakeRemote, port: u16) -> Vec<(String, Option<Duration>)> {
    fake.calls_to(&host(port))
        .into_iter()
        .filter_map(|c| match c {
            RemoteCall::Exec { cmd, timeout, .. } => Some((cmd, timeout)),
            RemoteCall::Copy { .. } => None,
        })
        .collect()
}

/// `[n/total] rest` → `rest` (finishing order among pods that finish together isn't fixed).
fn unnumbered(lines: &[String]) -> Vec<String> {
    lines.iter().map(|l| l.split_once("] ").map_or(l.as_str(), |(_, r)| r).to_string()).collect()
}

#[tokio::test(start_paused = true)]
async fn pods_run_partial_failure_is_non_zero_with_a_line_per_pod() {
    let script = |fake: &FakeRemote| {
        fake.script(&host(22001), [FakeReply::stdout("bash: no job control in this shell\nhello\n")]);
        fake.script(&host(22002), [FakeReply::exit(127, "zsh: command not found: nvidia-smi")]);
    };
    // Through the command: one wrapped exec per pod with the given budget; non-zero exit.
    let fake = Arc::new(FakeRemote::new());
    script(&fake);
    let err = handle_run(fake.clone(), &cfg(), &everyone(&fleet()), "nvidia-smi -L", Duration::from_secs(60), false, true, true, false)
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "1 pod(s) failed");
    let wrapped = login_shell_wrap("nvidia-smi -L", Some("arena-env"));
    for port in [22001, 22002, 22003] {
        assert_eq!(execs(&fake, port), [(wrapped.clone(), Some(Duration::from_secs(60)))], "port {port}");
    }

    // The per-pod report (sorted by name), block and compact layouts.
    let fake = Arc::new(FakeRemote::new());
    script(&fake);
    let remote: Arc<dyn Remote> = fake.clone();
    let results = run_fleet(&remote, targets(&fleet()), &wrapped, Duration::from_secs(60)).await;
    let ok = |name: &str, text: &str| RunResult { name: name.into(), text: text.into(), ok: true, why: None };
    assert_eq!(
        results,
        [
            ok("devtest-apple", "hello"),
            RunResult {
                name: "devtest-bloom".into(),
                text: "exit 127: zsh: command not found: nvidia-smi".into(),
                ok: false,
                why: Some("exit 127: zsh: command not found: nvidia-smi".into()),
            },
            ok("devtest-cloud", ""),
        ]
    );
    let (lines, bad) = render_run(&results, false);
    assert_eq!(bad, 1);
    assert_eq!(
        lines,
        [
            "\n── devtest-apple ",
            "hello",
            "\n── devtest-bloom (FAILED)",
            "exit 127: zsh: command not found: nvidia-smi",
            "\n── devtest-cloud ",
            "",
            "\n2 ok, 1 failed",
        ]
    );
    let (lines, _) = render_run(&results, true);
    assert_eq!(lines[1], "devtest-bloom          ✗ exit 127: zsh: command not found: nvidia-smi");
}

/// Live BPF S9: `pods run` on a bare Hetzner VM (no zsh until setup) said only `exit 127:
/// bash: line 1: zsh: command not found`. It says the command never ran, and the fix.
#[tokio::test(start_paused = true)]
async fn pods_run_on_a_pod_without_zsh_says_to_set_it_up() {
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22002), [FakeReply::exit(127, "bash: line 1: zsh: command not found")]);
    let remote: Arc<dyn Remote> = fake.clone();
    let wrapped = login_shell_wrap("echo x > ~/unsynced.txt", Some("arena-env"));
    let results = run_fleet(&remote, targets(&fleet()), &wrapped, Duration::from_secs(60)).await;
    let bloom = &results[1];
    assert_eq!(bloom.name, "devtest-bloom");
    assert!(!bloom.ok);
    assert_eq!(
        bloom.why.as_deref(),
        Some(
            "no zsh on the pod (setup installs it) — not set up yet? `arena pods setup devtest-bloom` \
             (exit 127: bash: line 1: zsh: command not found)"
        )
    );
    assert!(bloom.text.starts_with("no zsh on the pod") && bloom.text.ends_with("exit 127: bash: line 1: zsh: command not found"), "{}", bloom.text);
    assert!(results[0].ok && results[2].ok, "{results:?}");
}

/// A failing command's stdout is part of its report (live: `pods run` showed only stderr,
/// so a script's own output before it failed was lost), and no exit reads `Some(1)`.
#[test]
fn a_failed_run_keeps_its_stdout_and_says_exit_n() {
    use arena_core::ssh::SshOutput;
    let out = |code: Option<i32>, stdout: &str, stderr: &str| SshOutput {
        success: false,
        code,
        stdout: stdout.into(),
        stderr: stderr.into(),
    };
    // (output, full text, compact reason)
    let cases: &[(SshOutput, &str, &str)] = &[
        (out(Some(1), "step 1 ok\nstep 2 ok\n", "Traceback…\nValueError: bad\n"), "step 1 ok\nstep 2 ok\nexit 1: Traceback…\nValueError: bad", "exit 1: ValueError: bad"),
        (out(Some(1), "3 matches so far\n", ""), "3 matches so far\nexit 1", "exit 1 — last output: 3 matches so far"),
        (out(Some(2), "", "usage: x\n"), "exit 2: usage: x", "exit 2: usage: x"),
        (out(None, "", ""), "killed by a signal", "killed by a signal"),
    ];
    for (o, text, why) in cases {
        assert_eq!(run_failure(o), (text.to_string(), why.to_string()), "{o:?}");
        assert!(!text.contains("Some(") && !why.contains("Some("));
    }
    let results = vec![RunResult { name: "devtest-bloom".into(), text: cases[0].1.into(), ok: false, why: Some(cases[0].2.into()) }];
    let (full, bad) = render_run(&results, false);
    assert_eq!((bad, full[1].as_str()), (1, "step 1 ok\nstep 2 ok\nexit 1: Traceback…\nValueError: bad"));
    let (compact, _) = render_run(&results, true);
    assert_eq!(compact[0], "devtest-bloom          ✗ exit 1: ValueError: bad");
}

/// A stand-in `rsync` (a shell script in a temp dir) that logs each call's destination —
/// its last argument — so a pull is observable without touching the network.
struct StubRsync(std::path::PathBuf);

impl StubRsync {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("arena-rsync-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("rsync");
        arena_core::remote::write_test_script(
            &script,
            "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\necho \"$last\" >> \"$(dirname \"$0\")/calls.log\"\n\
             echo \"$*\" >> \"$(dirname \"$0\")/args.log\"\n",
        )
        .unwrap();
        StubRsync(dir)
    }
    fn path(&self) -> String {
        self.0.join("rsync").display().to_string()
    }
    /// The destinations so far as `<tier dir>/<pod>`, sorted; the log is emptied.
    fn take(&self) -> Vec<String> {
        let log = self.0.join("calls.log");
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let _ = std::fs::remove_file(&log);
        let mut got: Vec<String> = text
            .lines()
            .map(|l| {
                let mut parts = l.trim_end_matches('/').rsplit('/');
                let pod = parts.next().unwrap_or("");
                format!("{}/{pod}", parts.next().unwrap_or(""))
            })
            .collect();
        got.sort();
        got
    }
    /// Every call's full argv (space-joined), sorted; the log is emptied.
    fn take_args(&self) -> Vec<String> {
        let log = self.0.join("args.log");
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let _ = std::fs::remove_file(&log);
        let mut got: Vec<String> = text.lines().map(String::from).collect();
        got.sort();
        got
    }
}

/// What a pod says when its repo is a plain directory at the configured path.
fn repo_in_place() -> FakeReply {
    FakeReply::stdout(&format!("arena-repo-home=/root\narena-repo-real={REPO}\narena-repo-mount=no\n"))
}

impl Drop for StubRsync {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `pods pull bravo` meant "every pod, into backup/bravo/" (live). Positionals are pods now;
/// the old label spelling still works where it can't be a pod, and is refused where it
/// could be either.
#[test]
fn pull_takes_pods_and_its_label_behind_a_flag() {
    let cfg = Config::parse("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom w2d2)\n");
    let naming = Naming::from_config(&cfg);
    let listed = vec![pod("bravo", 22001), pod("w9d9", 22002)];
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    // (positionals, --label, -t, want: Ok((label, targets, noted)) or Err(tell))
    type Want<'a> = std::result::Result<(Option<&'a str>, &'a [&'a str], bool), &'a str>;
    type Case<'a> = (&'a [&'a str], Option<&'a str>, &'a [&'a str], Want<'a>);
    let cases: &[Case] = &[
        (&["bravo"], None, &[], Ok((None, &["bravo"], false))),
        (&["bravo", "apple..bloom"], Some("w1d3"), &[], Ok((Some("w1d3"), &["bravo", "apple..bloom"], false))),
        (&[], None, &[], Ok((None, &[], false))),
        // The old spelling, where it can only be the label: a note, and it still works.
        (&["w1d3"], None, &[], Ok((Some("w1d3"), &[], true))),
        (&["w1d3"], None, &["apple..bloom"], Ok((Some("w1d3"), &["apple..bloom"], true))),
        (&["w1d3", "bravo"], None, &[], Ok((Some("w1d3"), &["bravo"], true))),
        // With --label every positional is a pod (the selector judges it).
        (&["w1d3"], Some("w2d1"), &[], Ok((Some("w2d1"), &["w1d3"], false))),
        // Not label-shaped: a target — an old custom label fails in the selector, loudly.
        (&["mylabel"], None, &[], Ok((None, &["mylabel"], false))),
        (&["apple,w1d3"], None, &[], Ok((None, &["apple,w1d3"], false))),
        // Either a label or a pod (listed, or a MACHINE_NAME_LIST entry): refused.
        (&["w9d9"], None, &[], Err("is both a snapshot label and a pod's name")),
        (&["devtest-w9d9"], None, &[], Ok((None, &["devtest-w9d9"], false))),
        (&["w2d2"], None, &[], Err("`w2d2` is both")),
        (&["w1d3", "w1d4"], None, &[], Err("two snapshot labels")),
    ];
    for (pos, label, flag, want) in cases {
        let got = pull_label_and_targets(label.map(String::from), &s(pos), &s(flag), &naming, &listed);
        match (got, want) {
            (Ok(g), Ok((l, t, noted))) => {
                assert_eq!((g.label.as_deref(), g.targets.clone(), g.note.is_some()), (*l, s(t), *noted), "{pos:?} {label:?}");
                if let Some(n) = g.note {
                    assert!(n.contains("write `--label w1d3`"), "{n}");
                }
            }
            (Err(e), Err(tell)) => assert!(e.contains(tell), "{pos:?}: {e}"),
            (got, _) => panic!("{pos:?} {label:?}: got {got:?}, want {want:?}"),
        }
    }
}

/// Through the command: what each spelling resolves to — the label, every pod it reaches,
/// the deprecation note — exactly as the `pods pull` arm hands them on (then the dry runs
/// themselves: nothing copied, no pod asked anything).
#[tokio::test]
async fn pods_pull_resolves_pods_and_says_where_a_label_goes() {
    let fake = Arc::new(FakeRemote::new());
    let cfg = Config::parse(&format!(
        "MACHINE_NAME_PREFIX=devtest\nBACKUP_REPO_PATH={REPO}\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\n\
         ARENA_START_DATE=2026-01-05\n"
    ));
    let every: Vec<String> = fleet().pods.iter().map(|p| p.name.clone()).collect();
    assert!(every.len() > 1);
    let resolve = |argv: &'static [&'static str]| {
        let cfg = cfg.clone();
        async move {
            let PodCmd::Pull { label, target_flag, sel, .. } = pods(argv) else { panic!("not pods pull") };
            resolve_pull(&fleet(), &cfg, label, sel, &target_flag)
                .await
                .map(|(label, sel, note)| (label, sel.pods.iter().map(|p| p.name.clone()).collect::<Vec<_>>(), note))
        }
    };
    // A pod, positional: that pod, the computed label, no note.
    assert_eq!(resolve(&["pull", "bloom"]).await.unwrap(), (None, vec!["devtest-bloom".to_string()], None));
    // The old spelling — a lone wNdM naming no pod: the label, EVERY pod (as it always meant),
    // and the note saying how to write it now.
    let (label, chosen, note) = resolve(&["pull", "w1d3"]).await.unwrap();
    assert_eq!((label.as_deref(), &chosen), (Some("w1d3"), &every));
    let note = note.expect("the deprecation note");
    assert!(note.contains("reading `w1d3` as the snapshot label") && note.contains("--label w1d3"), "{note}");
    // --label with the old -t: that pod, that label, no note.
    assert_eq!(resolve(&["pull", "-t", "bloom", "--label", "x"]).await.unwrap(), (Some("x".into()), vec!["devtest-bloom".into()], None));
    assert_eq!(resolve(&["pull", "bloom", "--label", "w1d3"]).await.unwrap(), (Some("w1d3".into()), vec!["devtest-bloom".into()], None));
    // A word that is no pod and no label: the error says how the command reads now.
    let msg = format!("{:#}", resolve(&["pull", "mylabel"]).await.unwrap_err());
    assert!(msg.contains("takes pods as its arguments now") && msg.contains("--label") && msg.contains("mylabel"), "{msg}");
    // The w1d3 dry run puts every pod under w1d3/ (the same resolution, through the arm).
    let base = std::env::temp_dir().join(format!("arena-pull-oldlabel-{}", std::process::id()));
    let PodCmd::Pull { label, target_flag, sel, .. } = pods(&["pull", "w1d3"]) else { panic!("not pods pull") };
    let (label, chosen, _) = resolve_pull(&fleet(), &cfg, label, sel, &target_flag).await.unwrap();
    handle_pull(&(fake.clone() as Arc<dyn Remote>), &cfg, label, &base.display().to_string(), None, None, false, false, &chosen, true, true, "rsync")
        .await
        .unwrap();
    assert!(!base.exists(), "a dry run writes nothing");
    handle_pods(pods(&["pull", "w1d3", "--dry-run"]), &fleet(), fake.clone(), &cfg, true).await.unwrap();
    handle_pods(pods(&["pull", "bloom", "--dry-run"]), &fleet(), fake.clone(), &cfg, true).await.unwrap();
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn pull_and_backup_rsync_exactly_the_selected_pods() {
    let stub = StubRsync::new("scoped");
    let base = stub.0.join("backups");
    let cfg = Config::parse(&format!(
        "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\nBACKUP_REPO_PATH={REPO}\n\
         GIT_SSH_KEY_REMOTE={KEY}\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nARENA_START_DATE=2026-01-05\n\
         LOCAL_BACKUP_DIR={}\n",
        base.display()
    ));
    let base = base.display().to_string();
    // `pods pull apple..cloud --exclude bloom --label w1d1` — and its old spelling `pods pull
    // w1d1 -t apple..cloud --exclude bloom`: exactly apple + cloud, both tiers, through the
    // same selection the command resolves.
    let listed = fleet().pods.clone();
    let mut chosen_by = Vec::new();
    for argv in [&["pull", "apple..cloud", "--exclude", "bloom", "--label", "w1d1"][..], &["pull", "w1d1", "-t", "apple..cloud", "--exclude", "bloom"]] {
        let PodCmd::Pull { label, target_flag, mut sel, .. } = pods(argv) else { panic!("not pods pull") };
        let split = pull_label_and_targets(label, &sel.targets, &target_flag, &Naming::from_config(&cfg), &listed).unwrap();
        assert_eq!(split.label.as_deref(), Some("w1d1"), "{argv:?}");
        sel.targets = split.targets;
        let chosen = select(&fleet(), &cfg, &sel.args(), Unscoped::All).await.unwrap();
        chosen_by.push(chosen.pods.iter().map(|p| p.name.clone()).collect::<Vec<_>>());
    }
    assert_eq!(chosen_by, [["devtest-apple", "devtest-cloud"], ["devtest-apple", "devtest-cloud"]]);
    let chosen = picked(&fleet(), &cfg, &["apple", "cloud"]).await;
    // Each pod is asked where its repo is (a plain directory here: one job per tier).
    let fake = Arc::new(FakeRemote::new());
    for port in [22001, 22003] {
        fake.script(&host(port), [repo_in_place()]);
    }
    let remote: Arc<dyn Remote> = fake.clone();
    handle_pull(&remote, &cfg, Some("w1d1".into()), &base, None, None, false, false, &chosen, false, true, &stub.path()).await.unwrap();
    assert_eq!(stub.take(), ["big/devtest-apple", "big/devtest-cloud", "w1d1/devtest-apple", "w1d1/devtest-apple", "w1d1/devtest-cloud", "w1d1/devtest-cloud"]);
    assert_eq!(ports(&fake), [22001, 22003], "only the selected pods were asked");
    // `pods backup bloom` (with the pull): the git push AND the rsync reach bloom only.
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22002), [FakeReply::ok(), repo_in_place()]);
    let chosen = picked(&fleet(), &cfg, &["bloom"]).await;
    handle_full_backup(fake.clone(), &cfg, &chosen, false, None, false, true, &stub.path()).await.unwrap();
    assert_eq!(ports(&fake), [22002]);
    let got = stub.take();
    assert_eq!(got.len(), 3, "the snapshot's two passes and big: {got:?}");
    assert!(got.iter().all(|d| d.ends_with("/devtest-bloom")) && got.contains(&"big/devtest-bloom".to_string()), "{got:?}");
    // --no-pull: git only. A dry run: neither.
    let fake = Arc::new(FakeRemote::new());
    handle_full_backup(fake.clone(), &cfg, &chosen, true, None, false, true, &stub.path()).await.unwrap();
    assert_eq!((ports(&fake), stub.take()), (vec![22002], vec![]));
    let fake = Arc::new(FakeRemote::new());
    handle_full_backup(fake.clone(), &cfg, &chosen, false, None, true, true, &stub.path()).await.unwrap();
    assert!(fake.calls().is_empty() && stub.take().is_empty());
}

/// Review ai-review claude #10, checked: since FIX4A a pod with no SSH endpoint (stopped,
/// booting) is in `Pulled::failed` — for the backup's per-pod fallback lines, which ask where
/// a pod's copy went. Every reader was checked: `pods pull`'s exit and its error come from the
/// failed rsync jobs (`failures`), where such a pod never is, and its summary counts jobs — not
/// a failed copy, nor counted twice; `whole` (the resolve's gate on a copy) holds only pods
/// all of whose rsyncs ran; `pods backup` (pull or `--no-pull`) copies instead only pods its
/// git step reached, never such a pod, so no line names it a failed copy and the run's verdict
/// is as without it. (The reaper's backup and its verify run their own rsync, and the archive's
/// "pull running" reads the pod's lock: neither reads a `Pulled`.)
#[tokio::test]
async fn a_pod_with_no_endpoint_is_no_failed_copy_to_any_reader_of_the_pull() {
    use crate::pods::pull::{pull_pods, LockMode, PullRun};
    let stub = StubRsync::new("no-endpoint");
    let base = stub.0.join("backups");
    let cfg = Config::parse(&format!(
        "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\nBACKUP_REPO_PATH={REPO}\n\
         GIT_SSH_KEY_REMOTE={KEY}\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nARENA_START_DATE=2026-01-05\n\
         LOCAL_BACKUP_DIR={}\n",
        base.display()
    ));
    let base = base.display().to_string();
    let stopped = Pod { status: "EXITED".into(), ssh_ip: None, ssh_port: None, ..pod("bloom", 22002) };
    let both = Selected { pods: vec![pod("apple", 22001), stopped.clone()], named: true };
    let args = || PullRun { label: Some("w1d1".into()), max_size: None, remote_path: None, no_git: false, no_big: false, dry_run: false, yes: true };
    let pull = |sel: Selected| {
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [repo_in_place()]);
        let remote: Arc<dyn Remote> = fake;
        let (cfg, base, rsync) = (cfg.clone(), base.clone(), stub.path());
        async move { pull_pods(&remote, &cfg, &base, args(), &sel, &rsync, LockMode::Take(Duration::from_millis(300))).await.unwrap() }
    };
    let no_endpoint = [("devtest-bloom".to_string(), "no SSH endpoint to pull from".to_string())];
    let p = pull(both.clone()).await;
    assert!(p.failures.is_empty(), "no failed rsync: {:?}", p.failures);
    assert_eq!((p.failed.as_slice(), p.whole.as_slice()), (&no_endpoint[..], &["devtest-apple".to_string()][..]));
    assert!(p.copied_to("devtest-apple").is_some() && p.copied_to("devtest-bloom").is_none());
    p.result().unwrap();
    assert_eq!(stub.take().len(), 3, "apple's tiers only");
    // Every pod without one, fleet-wide (named, that's refused before anything: unchanged):
    // the same, from the early return.
    let p = pull(Selected { pods: vec![stopped.clone()], named: false }).await;
    assert!(p.failures.is_empty() && p.whole.is_empty() && stub.take().is_empty());
    assert_eq!(p.failed, no_endpoint);
    p.result().unwrap();
    // `pods pull` itself: exit 0.
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [repo_in_place()]);
    let remote: Arc<dyn Remote> = fake;
    handle_pull(&remote, &cfg, Some("w1d1".into()), &base, None, None, false, false, &both, false, true, &stub.path()).await.unwrap();
    assert_eq!(stub.take().len(), 3);
    // `pods backup`, with the pull and without: apple pushed (and pulled), bloom neither, and
    // no line says a copy of bloom failed.
    for no_pull in [false, true] {
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n"), repo_in_place()]);
        let _ = take_said();
        handle_full_backup(fake.clone(), &cfg, &both, no_pull, None, false, true, &stub.path()).await.unwrap();
        assert_eq!(stub.take().len(), if no_pull { 0 } else { 3 }, "no_pull={no_pull}");
        assert!(!take_said().iter().any(|l| l.contains("devtest-bloom")), "no_pull={no_pull}");
    }
    let unpushed = [Unpushed { pod: "devtest-bloom".into(), why: "x".into(), branch: None, at_push: true, wrong_origin: false, busy: None }];
    assert_eq!(fallback_lines(&unpushed, &Ok(pull(both.clone()).await)), ["✗ devtest-bloom: git push failed (x); the fallback copy failed too: no SSH endpoint to pull from"]);
    let _ = stub.take();
}

/// disk-guard: with too little free on this box (under CONTROL_MIN_FREE_GB), a pod's copy is
/// skipped whole — before the pod is asked anything or any of its dirs is made — said on its
/// line, recorded for the daily health check, and a failure of the run: `pods pull`, and the
/// fallback copy `pods backup --no-pull` makes of a pod whose push failed. A pod with room
/// (here: onto its earlier copy, a zero floor) is copied as before.
#[tokio::test]
async fn a_copy_without_room_on_this_box_is_skipped_whole_and_fails_the_run() {
    let stub = StubRsync::new("noroom");
    let (base, state) = (stub.0.join("backups"), stub.0.join("state"));
    let at_floor = |gb: &str| {
        Config::parse(&format!(
            "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\nBACKUP_REPO_PATH={REPO}\n\
             GIT_SSH_KEY_REMOTE={KEY}\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nARENA_START_DATE=2026-01-05\n\
             LOCAL_BACKUP_DIR={}\nARENA_STATE_DIR={}\nCONTROL_MIN_FREE_GB={gb}\n",
            base.display(),
            state.display()
        ))
    };
    let full = at_floor("100000");
    let base_s = base.display().to_string();
    let chosen = picked(&fleet(), &full, &["apple", "bloom"]).await;
    // Each pod's line: why, and what to do.
    let noted = |pods: &[&str]| {
        let said = take_said();
        for pod in pods {
            assert!(
                said.iter().any(|l| l.starts_with(&format!("✗ {pod}: skipped: only "))
                    && l.contains("(floor 100000 GB, CONTROL_MIN_FREE_GB): this copy (~6 GB, a first copy, not measured) + the floor = 100006 GB needed")
                    && l.contains(" — nothing copied; free at least ")
                    && l.ends_with(" (`arena config check` names arena's biggest dirs there)")),
                "{pod}: {said:?}"
            );
        }
    };

    // `pods pull`: both skipped; nothing asked, made or rsynced; the run fails naming them.
    let fake = Arc::new(FakeRemote::new());
    let remote: Arc<dyn Remote> = fake.clone();
    let _ = take_said();
    let e = handle_pull(&remote, &full, Some("w1d1".into()), &base_s, None, None, false, false, &chosen, false, true, &stub.path())
        .await
        .unwrap_err();
    noted(&["devtest-apple", "devtest-bloom"]);
    for pod in ["devtest-apple", "devtest-bloom"] {
        assert!(format!("{e:#}").contains(&format!("{pod} (only ")), "{e:#}");
    }
    assert!(!base.exists(), "nothing written: not even the backup dir");
    assert!(stub.take().is_empty() && fake.calls().is_empty(), "no rsync, no pod asked");
    let record = state.join("devtest").join(arena_core::disk::SKIPS_FILE);
    let (skips, _) = arena_core::disk::Skips::load(&record);
    let got: Vec<(&str, &str, u64)> = skips.entries.iter().map(|e| (e.pod.as_str(), e.what.as_str(), e.count)).collect();
    assert_eq!(got, [("devtest-apple", "copy", 1), ("devtest-bloom", "copy", 1)]);
    assert!(skips.entries[0].why.starts_with("only ") && !skips.entries[0].why.contains("nothing copied"), "{:?}", skips.entries[0]);

    // `pods backup --no-pull`, bloom's push failing: its fallback copy is skipped the same
    // way, on its line, and the run fails.
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22002), [FakeReply::exit(1, "error: failed to push some refs").with_stdout("BRANCH autocommit-b\nPUSH_FAILED\n")]);
    let bloom = picked(&fleet(), &full, &["bloom"]).await;
    let _ = take_said();
    let e = handle_full_backup(fake.clone(), &full, &bloom, true, None, false, true, &stub.path()).await.unwrap_err();
    noted(&["devtest-bloom"]);
    assert!(format!("{e:#}").contains("devtest-bloom"), "{e:#}");
    assert_eq!(execs(&fake, 22002).len(), 1, "its backup only: no probe for a copy that can't happen");
    assert!(!base.exists() && stub.take().is_empty());
    assert_eq!(arena_core::disk::Skips::load(&record).0.entries.iter().find(|e| e.pod == "devtest-bloom").map(|e| e.count), Some(2));

    // Room: a later copy (bloom copied whole here before) needs ~0.1 GB per tier, the floor 0;
    // a copy that went through whole is marked so.
    for tier in ["w1d1/devtest-bloom", "big/devtest-bloom"] {
        std::fs::create_dir_all(base.join(tier)).unwrap();
        std::fs::write(base.join(tier).join("old.py"), "x").unwrap();
    }
    arena_core::disk::mark_copied(&base, "devtest-bloom").unwrap();
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22002), [repo_in_place()]);
    let remote: Arc<dyn Remote> = fake.clone();
    handle_pull(&remote, &at_floor("0"), Some("w1d1".into()), &base_s, None, None, false, false, &bloom, false, true, &stub.path())
        .await
        .unwrap();
    assert_eq!(stub.take(), ["big/devtest-bloom", "w1d1/devtest-bloom", "w1d1/devtest-bloom"]);
    assert!(!arena_core::disk::was_copied(&base, "devtest-apple"), "apple was never copied");
    // Its reservation went with the run.
    let (held, _) = arena_core::disk::Reservations::load(&state.join(arena_core::disk::RESERVATIONS_FILE)).unwrap();
    assert!(held.held.is_empty(), "{held:?}");
}

/// disk-guard: a pull that copied a pod's whole home marks it, so its next copy here counts as
/// a later one; one that didn't (a failed tier, one dir of the home) doesn't. A backup from
/// before marking gets its pods marked once, before the first judged run's copies.
#[tokio::test]
async fn a_pod_copied_whole_is_marked_and_a_failed_one_isnt() {
    let stub = StubRsync::new("marked");
    let base = stub.0.join("backups");
    let text = format!(
        "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\nBACKUP_REPO_PATH={REPO}\n\
         GIT_SSH_KEY_REMOTE={KEY}\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nARENA_START_DATE=2026-01-05\n"
    );
    let cfg = Config::parse(&text);
    let both = picked(&fleet(), &cfg, &["apple", "bloom"]).await;
    let apple = Selected { pods: vec![both.pods[0].clone()], named: true };
    // bloom has no endpoint here: never copied, never marked.
    let both = Selected { pods: vec![both.pods[0].clone(), Pod { ssh_ip: None, ssh_port: None, ..both.pods[1].clone() }], named: true };
    let pull = |cfg: Config, dir: std::path::PathBuf, sel: Selected, remote_path: Option<&'static str>| {
        let stub = stub.path();
        async move {
            let fake = Arc::new(FakeRemote::new());
            fake.script(&host(22001), [repo_in_place()]);
            let remote: Arc<dyn Remote> = fake.clone();
            let dir = dir.display().to_string();
            handle_pull(&remote, &cfg, Some("w1d1".into()), &dir, None, remote_path.map(String::from), false, false, &sel, false, true, &stub).await
        }
    };
    let _ = pull(cfg.clone(), base.clone(), both.clone(), None).await;
    assert!(arena_core::disk::was_copied(&base, "devtest-apple"));
    assert!(!arena_core::disk::was_copied(&base, "devtest-bloom"));
    // One dir of the home isn't the home; `~/` (the cron's BACKUP_REMOTE_PATH) is.
    let part = stub.0.join("part");
    pull(cfg.clone(), part.clone(), apple.clone(), Some("ARENA_materials/results")).await.unwrap();
    assert!(!arena_core::disk::was_copied(&part, "devtest-apple"), "one dir of its home");
    let home = stub.0.join("home");
    pull(Config::parse(&format!("{text}BACKUP_REMOTE_PATH=~/\n")), home.clone(), apple.clone(), None).await.unwrap();
    assert!(arena_core::disk::was_copied(&home, "devtest-apple"), "its home");

    // A backup from before marking: bloom backed up there (both tiers), no marks. The first
    // judged run marks it — its next copy a later one, as before — then copies apple whole.
    let legacy = stub.0.join("legacy");
    for tier in ["w1d1/devtest-bloom", "big/devtest-bloom"] {
        std::fs::create_dir_all(legacy.join(tier)).unwrap();
        std::fs::write(legacy.join(tier).join("old.py"), "x").unwrap();
    }
    let judged = Config::parse(&format!("{text}ARENA_STATE_DIR={}\nCONTROL_MIN_FREE_GB=0\n", stub.0.join("state").display()));
    pull(judged, legacy.clone(), apple.clone(), None).await.unwrap();
    assert!(arena_core::disk::was_copied(&legacy, "devtest-bloom"), "marked: backed up before marking");
    assert!(arena_core::disk::was_copied(&legacy, "devtest-apple"), "copied whole");
    assert!(!arena_core::disk::was_copied(&legacy, "devtest-cloud"));
}

/// S17 + AD7: every pull rsync writes its temp files to its tier root's `.rsync-tmp`
/// (`<dir>/<label>/.rsync-tmp`, `<dir>/big/.rsync-tmp`) — beside the pod trees the archive
/// pushes, never in one — created owner-only once confirmed; a dry run shows it and creates
/// nothing. A destination on another filesystem (a pod dir that is its own mount) gets no
/// `--temp-dir` — rsync would rewrite it in place — and says so. Destinations are as before.
#[tokio::test]
async fn the_pull_writes_its_temp_files_outside_every_pushed_tree() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let stub = StubRsync::new("tempdir");
    let base = stub.0.join("backups");
    let cfg = Config::parse("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\nSHARED_SSH_KEY_PATH=/nonexistent/k\n");
    let chosen = picked(&fleet(), &cfg, &["apple"]).await;
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [repo_in_place(), repo_in_place()]);
    let remote: Arc<dyn Remote> = fake.clone();
    let base_s = base.display().to_string();
    handle_pull(&remote, &cfg, Some("w1d1".into()), &base_s, None, None, false, false, &chosen, true, true, &stub.path()).await.unwrap();
    assert!(!base.exists(), "a dry run creates nothing");
    handle_pull(&remote, &cfg, Some("w1d1".into()), &base_s, None, None, false, false, &chosen, false, true, &stub.path()).await.unwrap();
    let canon = std::fs::canonicalize(&base).unwrap();
    let (snap_tmp, big_tmp) = (canon.join("w1d1/.rsync-tmp"), canon.join("big/.rsync-tmp"));
    for tmp in [&snap_tmp, &big_tmp] {
        assert!(tmp.is_dir(), "{}", tmp.display());
        assert_eq!(std::fs::metadata(tmp).unwrap().permissions().mode() & 0o777, 0o700);
    }
    let args = stub.take_args();
    assert_eq!(args.len(), 3, "{args:?}");
    for a in &args {
        let tmp = if a.contains("/big/devtest-apple") { &big_tmp } else { &snap_tmp };
        assert!(a.contains(&format!("--temp-dir={}", tmp.display())), "{a}");
    }
    assert_eq!(stub.take(), ["big/devtest-apple", "w1d1/devtest-apple", "w1d1/devtest-apple"], "destinations unchanged");
    assert!(arena_core::archive::sync::sort_labels([".rsync-tmp", "w1d1", "big"]).iter().all(|l| l != ".rsync-tmp"));
    // A pod's big dir on another filesystem (tmpfs): no --temp-dir for it, the rest unchanged.
    let shm = std::path::Path::new("/dev/shm");
    let other = shm.join(format!("arena-tempdir-{}", std::process::id()));
    if std::fs::create_dir_all(&other).is_ok() && std::fs::metadata(shm).unwrap().dev() != std::fs::metadata(&canon).unwrap().dev() {
        std::fs::remove_dir_all(canon.join("big/devtest-apple")).unwrap();
        std::os::unix::fs::symlink(&other, canon.join("big/devtest-apple")).unwrap();
        handle_pull(&remote, &cfg, Some("w1d1".into()), &base_s, None, None, false, false, &chosen, false, true, &stub.path()).await.unwrap();
        let args = stub.take_args();
        let big = args.iter().find(|a| a.contains("/big/devtest-apple")).unwrap();
        assert!(!big.contains("--temp-dir"), "across filesystems rsync would rewrite in place: {big}");
        assert!(args.iter().filter(|a| a.contains("/w1d1/devtest-apple")).all(|a| a.contains("--temp-dir=")), "{args:?}");
        let _ = std::fs::remove_dir_all(&other);
    }
}

/// One wedged pod can't hold the backup (or the cron behind it): its rsyncs are stopped
/// at `BACKUP_TIMEOUT_SECS` and named in the failure — the cron log says which pod —
/// while every other pod's tiers complete; the stopped rsyncs are really gone, not left
/// running. A malformed budget fails before anything is copied.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_wedged_pods_rsync_is_stopped_at_its_budget_and_named() {
    let stub = StubRsync::new("wedged");
    // bloom's rsync records its pid and hangs like a wedged pod; the others log and exit.
    arena_core::remote::write_test_script(
        stub.0.join("rsync"),
        "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\ncase \"$last\" in\n  \
         */devtest-bloom/) echo $$ >> \"$(dirname \"$0\")/hung.pids\"; exec sleep 30;;\nesac\n\
         echo \"$last\" >> \"$(dirname \"$0\")/calls.log\"\n",
    )
    .unwrap();
    let base = stub.0.join("backups").display().to_string();
    let with_budget = |budget: &str| {
        Config::parse(&format!(
            "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\n\
             SHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nBACKUP_TIMEOUT_SECS={budget}\n"
        ))
    };
    // 3s: room for the healthy pods' (instant) stubs even on a loaded test machine.
    let cfg = with_budget("3");
    let chosen = everyone(&fleet());
    // Each pod is first asked where its repo is (a plain directory here: one job per tier).
    let fake = Arc::new(FakeRemote::new());
    for port in [22001, 22002, 22003] {
        fake.script(&host(port), [repo_in_place()]);
    }
    let remote: Arc<dyn Remote> = fake.clone();
    let started = std::time::Instant::now();
    let err = handle_pull(&remote, &cfg, Some("w1d1".into()), &base, None, None, false, false, &chosen, false, true, &stub.path())
        .await
        .unwrap_err()
        .to_string();
    assert!(started.elapsed() < Duration::from_secs(10), "returned at the budget, not when bloom's rsync would end");
    assert!(err.starts_with("2 rsync job(s) failed: "), "{err}");
    for tier in ["snapshot", "big"] {
        assert!(err.contains(&format!("devtest-bloom [{tier}] (timed out after 3s)")), "{err}");
    }
    assert_eq!(stub.take(), ["big/devtest-apple", "big/devtest-cloud", "w1d1/devtest-apple", "w1d1/devtest-apple", "w1d1/devtest-cloud", "w1d1/devtest-cloud"]);
    // Both of bloom's rsyncs were stopped *before* the failure came back — not by a
    // background task that `main` returning (the runtime dropped) would cut short.
    let pids = std::fs::read_to_string(stub.0.join("hung.pids")).unwrap();
    assert_eq!(pids.lines().count(), 2, "{pids}");
    for pid in pids.lines() {
        let state = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| s.rsplit(')').next().and_then(|r| r.split_whitespace().next()).map(String::from));
        assert!(matches!(state.as_deref(), None | Some("Z") | Some("X")), "bloom's rsync {pid} still running ({state:?})");
    }
    // A budget that isn't whole seconds in range: refused up front, nothing copied.
    for bad in ["2h", "0", "86401"] {
        let err = handle_pull(&remote, &with_budget(bad), Some("w1d1".into()), &base, None, None, false, false, &chosen, false, true, &stub.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("BACKUP_TIMEOUT_SECS") && err.contains(&format!("`{bad}`")), "{err}");
    }
    assert!(stub.take().is_empty());
    assert_eq!(fake.calls().len(), 3, "a bad budget is refused before any pod is asked anything");
}

/// The repo jobs a linked (or unanswered) repo adds to a pull run under the same budget
/// as the home jobs: a wedged pod — which doesn't answer the repo question either, so its
/// repo is split out through the configured path — has all four rsyncs stopped and named
/// (`[snapshot repo]`, `[big repo]` too), and none is left running.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_wedged_pods_repo_rsyncs_are_bounded_too() {
    let stub = StubRsync::new("wedged-repo");
    arena_core::remote::write_test_script(
        stub.0.join("rsync"),
        "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\ncase \"$last\" in\n  \
         */devtest-bloom/|*/devtest-bloom/*) echo $$ >> \"$(dirname \"$0\")/hung.pids\"; exec sleep 30;;\nesac\n\
         echo \"$last\" >> \"$(dirname \"$0\")/calls.log\"\n",
    )
    .unwrap();
    let base = stub.0.join("backups").display().to_string();
    let cfg = Config::parse(&format!(
        "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\nBACKUP_REPO_PATH={REPO}\n\
         SHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nBACKUP_TIMEOUT_SECS=3\n"
    ));
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [repo_in_place()]);
    fake.script(&host(22002), [FakeReply::error("ssh: connect to host 10.0.0.1 port 22002: Connection timed out")]);
    fake.script(&host(22003), [repo_in_place()]);
    let remote: Arc<dyn Remote> = fake.clone();
    let started = std::time::Instant::now();
    let err = handle_pull(&remote, &cfg, Some("w1d1".into()), &base, None, None, false, false, &everyone(&fleet()), false, true, &stub.path())
        .await
        .unwrap_err()
        .to_string();
    assert!(started.elapsed() < Duration::from_secs(10), "returned at the budget, not when bloom's rsyncs would end");
    assert!(err.starts_with("4 rsync job(s) failed: "), "{err}");
    for tier in ["snapshot", "snapshot repo", "big", "big repo"] {
        assert!(err.contains(&format!("devtest-bloom [{tier}] (timed out after 3s)")), "{err}");
    }
    assert_eq!(stub.take(), ["big/devtest-apple", "big/devtest-cloud", "w1d1/devtest-apple", "w1d1/devtest-apple", "w1d1/devtest-cloud", "w1d1/devtest-cloud"]);
    let pids = std::fs::read_to_string(stub.0.join("hung.pids")).unwrap();
    assert_eq!(pids.lines().count(), 4, "{pids}");
    for pid in pids.lines() {
        let state = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| s.rsplit(')').next().and_then(|r| r.split_whitespace().next()).map(String::from));
        assert!(matches!(state.as_deref(), None | Some("Z") | Some("X")), "bloom's rsync {pid} still running ({state:?})");
    }
}

/// A repo that setup linked onto the volume: the pod says so, and the pull leaves the link
/// out of the home job and pulls the tree from its real path into the same place. A pod
/// that doesn't answer is pulled through the home path (which follows a link); one whose
/// repo is a plain directory gets exactly the old single job per tier. A dry run asks
/// nobody anything.
#[tokio::test]
async fn a_pull_follows_a_repo_linked_onto_the_volume() {
    let stub = StubRsync::new("volume");
    let base = stub.0.join("backups").display().to_string();
    let text = format!(
        "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\nBACKUP_REPO_PATH={REPO}\n\
         SHARED_SSH_KEY_PATH=/nonexistent/devtest_key\n"
    );
    let cfg = Config::parse(&text);
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [FakeReply::stdout("arena-repo-home=/root\narena-repo-real=/workspace/ARENA_materials\narena-repo-mount=yes\n")]);
    fake.script(&host(22002), [FakeReply::error("ssh: connect to host 10.0.0.1 port 22002: Connection refused")]);
    fake.script(&host(22003), [repo_in_place()]);
    let remote: Arc<dyn Remote> = fake.clone();
    let all = everyone(&fleet());
    handle_pull(&remote, &cfg, Some("w1d1".into()), &base, None, None, false, false, &all, false, true, &stub.path()).await.unwrap();
    let args = stub.take_args();
    let of = |port: u16| -> Vec<&String> { args.iter().filter(|a| a.contains(&format!("-p {port} "))).collect() };
    // apple: home job without the link + the tree from the volume, per tier — the snapshot's
    // jobs each two passes (its files under the cap, then its .git uncapped).
    let apple = of(22001);
    assert_eq!(apple.len(), 6, "{apple:#?}");
    let homes: Vec<&&String> = apple.iter().filter(|a| a.contains("root@10.0.0.1: ")).collect();
    assert_eq!(homes.len(), 3, "{apple:#?}");
    assert!(homes.iter().all(|a| a.contains("--exclude /ARENA_materials ")), "{homes:#?}");
    let trees: Vec<&&String> = apple.iter().filter(|a| a.contains("root@10.0.0.1:/workspace/ARENA_materials/ ")).collect();
    assert_eq!(trees.len(), 3, "{apple:#?}");
    assert!(trees.iter().any(|a| a.ends_with("/w1d1/devtest-apple/ARENA_materials/") && a.contains("--max-size=50M")), "{trees:#?}");
    assert!(
        trees.iter().any(|a| a.ends_with("/w1d1/devtest-apple/ARENA_materials/") && !a.contains("--max-size") && a.contains("--include */ --exclude *")),
        "the tree's .git pass: {trees:#?}"
    );
    assert!(trees.iter().any(|a| a.ends_with("/big/devtest-apple/ARENA_materials/") && !a.contains("--max-size")), "{trees:#?}");
    // bloom didn't answer: its repo comes through the home path (trailing slash follows a link).
    let bloom = of(22002);
    assert_eq!(bloom.len(), 6, "{bloom:#?}");
    assert_eq!(bloom.iter().filter(|a| a.contains("root@10.0.0.1:ARENA_materials/ ")).count(), 3, "{bloom:#?}");
    // cloud: a plain directory — one home job per tier, no exclude.
    let cloud = of(22003);
    assert_eq!(cloud.len(), 3, "{cloud:#?}");
    assert!(cloud.iter().all(|a| !a.contains("--exclude /ARENA_materials")), "{cloud:#?}");
    // A dry run: nobody asked, nothing run.
    let fake = Arc::new(FakeRemote::new());
    let remote: Arc<dyn Remote> = fake.clone();
    handle_pull(&remote, &cfg, Some("w1d1".into()), &base, None, None, false, false, &all, true, true, &stub.path()).await.unwrap();
    assert!(fake.calls().is_empty() && stub.take_args().is_empty());
    // A --remote-path inside the repo: the pod resolves the link on the way — one job per
    // tier, as given (the pods are still asked: only their answer can say).
    handle_pull(&remote, &cfg, Some("w1d2".into()), &base, None, Some("ARENA_materials/results".into()), false, false, &all, false, true, &stub.path())
        .await
        .unwrap();
    assert_eq!(fake.calls().len(), 3);
    assert_eq!(stub.take_args().len(), 9);
    // BACKUP_REMOTE_PATH=~/ (docs/BACKUPS.md's default, what the */15 cron reads) used to skip
    // the question and back up the link alone; it's the home: split like one.
    let linked = || FakeReply::stdout("arena-repo-home=/root\narena-repo-real=/workspace/ARENA_materials\narena-repo-mount=yes\n");
    let cron = Config::parse(&format!("{text}BACKUP_REMOTE_PATH=~/\n"));
    let fake = Arc::new(FakeRemote::new());
    for port in [22001, 22002, 22003] {
        fake.script(&host(port), [linked()]);
    }
    let remote: Arc<dyn Remote> = fake.clone();
    handle_pull(&remote, &cron, Some("w1d3".into()), &base, None, None, false, false, &all, false, true, &stub.path()).await.unwrap();
    let args = stub.take_args();
    assert_eq!(args.len(), 18, "home + repo job, two tiers (the snapshot's in two passes), three pods: {args:#?}");
    assert_eq!(args.iter().filter(|a| a.contains("root@10.0.0.1:~/ ") && a.contains("--exclude /ARENA_materials ")).count(), 9, "{args:#?}");
    assert_eq!(args.iter().filter(|a| a.contains("root@10.0.0.1:/workspace/ARENA_materials/ ")).count(), 9, "{args:#?}");
    // --remote-path naming the repo without a slash: the tree from its real path, into
    // <dest>/ARENA_materials/ where rsync would have put the link.
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [linked()]);
    let remote: Arc<dyn Remote> = fake.clone();
    let one = Selected::all(all.pods.iter().filter(|p| p.name == "devtest-apple").cloned().collect());
    handle_pull(&remote, &cfg, Some("w1d4".into()), &base, None, Some("ARENA_materials".into()), false, true, &one, false, true, &stub.path())
        .await
        .unwrap();
    let args = stub.take_args();
    assert_eq!(args.len(), 2, "{args:#?}");
    assert!(args.iter().all(|a| a.contains("root@10.0.0.1:/workspace/ARENA_materials/ ") && a.ends_with("/w1d4/devtest-apple/ARENA_materials/")), "{args:#?}");
}

/// A stand-in rsync that also writes into its destination: `image.txt` with an old mtime
/// (the image's), `work.txt` with a new one — so a snapshot exists afterwards, and a
/// baseline has something to keep and something to drop. Logs every argv.
fn writing_rsync(tag: &str) -> StubRsync {
    let stub = StubRsync::new(tag);
    arena_core::remote::write_test_script(
        stub.0.join("rsync"),
        "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\necho \"$*\" >> \"$(dirname \"$0\")/args.log\"\n\
         mkdir -p \"$last\" && printf img > \"$last/image.txt\" && touch -d @1000000000 \"$last/image.txt\" && \
         printf work > \"$last/work.txt\" && printf carried > \"$last/carried.txt\" && \
         touch -d @1000000000 \"$last/carried.txt\"\n",
    )
    .unwrap();
    stub
}

/// The second day's snapshot hard-links to the first (and big never does); both of the
/// snapshot's passes carry the link-dest.
#[cfg(unix)]
#[tokio::test]
async fn a_second_snapshot_links_to_the_first() {
    let stub = writing_rsync("linkdest");
    let base = stub.0.join("backups").display().to_string();
    let cfg = Config::parse("MACHINE_NAME_PREFIX=devtest\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\n");
    let one = Selected::all(fleet().pods.into_iter().filter(|p| p.name == "devtest-apple").collect());
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [repo_in_place(), repo_in_place(), repo_in_place()]);
    let remote: Arc<dyn Remote> = fake.clone();
    handle_pull(&remote, &cfg, Some("w1d1".into()), &base, None, None, false, false, &one, false, true, &stub.path()).await.unwrap();
    assert!(!stub.take_args().iter().any(|a| a.contains("--link-dest")), "nothing to link to yet");
    handle_pull(&remote, &cfg, Some("w1d2".into()), &base, None, None, false, false, &one, false, true, &stub.path()).await.unwrap();
    let args = stub.take_args();
    let first = std::fs::canonicalize(stub.0.join("backups/w1d1/devtest-apple")).unwrap();
    let linked: Vec<&String> = args.iter().filter(|a| a.contains(&format!("--link-dest={}", first.display()))).collect();
    assert_eq!(linked.len(), 2, "{args:#?}");
    assert!(linked.iter().all(|a| a.ends_with("/w1d2/devtest-apple/")), "{linked:#?}");
    assert!(args.iter().filter(|a| a.ends_with("/big/devtest-apple/")).all(|a| !a.contains("--link-dest")), "{args:#?}");
    assert!(!args.iter().any(|a| a.contains("--no-perms")), "a new snapshot is a plain -a pull: {args:#?}");
    // The same label again (the */15 cron, later that day): its files may be links into the
    // first snapshot, so their attributes are never changed in place. big isn't linked.
    handle_pull(&remote, &cfg, Some("w1d2".into()), &base, None, None, false, false, &one, false, true, &stub.path()).await.unwrap();
    let args = stub.take_args();
    let snap: Vec<&String> = args.iter().filter(|a| a.ends_with("/w1d2/devtest-apple/")).collect();
    assert_eq!(snap.len(), 2, "{args:#?}");
    assert!(snap.iter().all(|a| a.contains("--no-perms --no-owner --no-group")), "{snap:#?}");
    assert!(args.iter().filter(|a| a.ends_with("/big/devtest-apple/")).all(|a| !a.contains("--no-perms")), "{args:#?}");
    // A label that is one of the backup dir's own folders is refused.
    for bad in ["big", "baseline", "a/b"] {
        let err = handle_pull(&remote, &cfg, Some(bad.into()), &base, None, None, false, false, &one, true, true, &stub.path()).await.unwrap_err();
        assert!(err.to_string().contains("can't be a snapshot label"), "{bad}: {err}");
    }
}

/// `pods baseline` never reads a STOPPED pod: its listed endpoint may answer as another pod
/// now. Refused before anything is asked of it.
#[cfg(unix)]
#[tokio::test]
async fn a_stopped_pod_is_never_the_baseline_source() {
    let stub = writing_rsync("baseline-stopped");
    let dir = stub.0.join("backups").display().to_string();
    let mut f = fleet();
    f.pods[0].status = "EXITED".into();
    let fake = Arc::new(FakeRemote::new());
    let remote: Arc<dyn Remote> = fake.clone();
    let err = handle_baseline(&f, &remote, &cfg(), "apple", Some(0), &dir, false, false, true, &stub.path(), 1_791_000_000)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("devtest-apple is stopped (EXITED)") && err.contains("arena pods start devtest-apple"), "{err}");
    assert!(fake.calls().is_empty() && stub.take_args().is_empty(), "{:?}", fake.calls());
}

/// `pods baseline`: from a fresh pod only (its container's age known, under a day), into
/// baseline/w<N>/fleet/ holding only the files older than the container — and none the pod
/// lists as copied onto it since with an old mtime (a `pods replace` copy: `carried.txt`);
/// an existing one needs --replace, which moves it aside. A dry run writes nothing.
#[cfg(unix)]
#[tokio::test]
async fn a_baseline_comes_from_a_fresh_pod_and_keeps_only_the_image() {
    let stub = writing_rsync("baseline");
    let base = stub.0.join("backups");
    let dir = base.display().to_string();
    let cfg = Config::parse(&format!(
        "MACHINE_NAME_PREFIX=devtest\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nBACKUP_REPO_PATH={REPO}\n"
    ));
    // Real time: the stub's `work.txt` is written now, after the container's creation.
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let probe = |created: Option<u64>| {
        let c = created.map(|t| format!("arena-repo-container={t}\n")).unwrap_or_default();
        FakeReply::stdout(&format!("arena-repo-home=/root\narena-repo-real={REPO}\narena-repo-mount=no\n{c}"))
    };
    // The pod's listing of what was copied onto it since its container was created.
    let copied = |paths: &[&str]| {
        let records: String = paths.iter().map(|p| format!("./{p}\0")).collect();
        FakeReply::stdout(&format!("arena-copied-begin\0{records}arena-copied-end\0"))
    };
    let run = |fake: &Arc<FakeRemote>, week: u32, replace: bool, dry: bool| {
        let remote: Arc<dyn Remote> = fake.clone();
        let (dir, stub) = (dir.clone(), stub.path());
        let cfg = cfg.clone();
        async move { handle_baseline(&fleet(), &remote, &cfg, "apple", Some(week), &dir, replace, dry, true, &stub, now).await }
    };
    // Refused: no creation time; a container two days old.
    for created in [None, Some(now - 2 * 86_400)] {
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [probe(created)]);
        let err = run(&fake, 0, false, false).await.unwrap_err().to_string();
        assert!(err.starts_with("refusing the baseline: devtest-apple"), "{err}");
        assert!(stub.take_args().is_empty());
    }
    // Refused: what was copied onto it can't be listed (cut short, or find failed).
    for listing in [FakeReply::stdout("arena-copied-begin\0./carried.txt\0"), FakeReply::exit(1, "find: unknown predicate `-newercm'")] {
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [probe(Some(now - 600)), listing]);
        let err = run(&fake, 0, false, false).await.unwrap_err().to_string();
        assert!(err.starts_with("refusing the baseline: couldn't list what was copied onto devtest-apple"), "{err}");
        assert!(stub.take_args().is_empty() && !base.join("baseline").exists());
    }
    // A dry run: asked, nothing written.
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [probe(Some(now - 600)), copied(&["carried.txt"])]);
    run(&fake, 0, false, true).await.unwrap();
    assert!(stub.take_args().is_empty() && !base.join("baseline").exists());
    // The capture: both passes, then only the image's file stays — not what was written
    // since, nor what was copied in with its old mtime.
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [probe(Some(now - 600)), copied(&["carried.txt"])]);
    run(&fake, 0, false, false).await.unwrap();
    assert_eq!(stub.take_args().len(), 2);
    let fleet0 = base.join("baseline/w0/fleet");
    assert!(fleet0.join("image.txt").is_file() && !fleet0.join("work.txt").exists());
    assert!(!fleet0.join("carried.txt").exists(), "copied onto the pod after its container: not the image's");
    let leftovers: Vec<_> = std::fs::read_dir(base.join("baseline/w0")).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name()).collect();
    assert_eq!(leftovers, ["fleet"], "no partial dir left");
    // Again: refused without --replace (nothing run); with it, the old one moved aside.
    let fake = Arc::new(FakeRemote::new());
    let err = run(&fake, 0, false, false).await.unwrap_err().to_string();
    assert!(err.contains("already holds week 0's baseline") && err.contains("--replace"), "{err}");
    assert!(fake.calls().is_empty() && stub.take_args().is_empty());
    fake.script(&host(22001), [probe(Some(now - 600)), copied(&[])]);
    run(&fake, 0, true, false).await.unwrap();
    let mut names: Vec<String> =
        std::fs::read_dir(base.join("baseline/w0")).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    assert_eq!(names, ["fleet".to_string(), format!("fleet.replaced-{}", arena_core::restore::stamp(now))]);
    stub.take_args();
    // Week 1's links to week 0's.
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [probe(Some(now - 600)), copied(&[])]);
    run(&fake, 1, false, false).await.unwrap();
    let w0 = std::fs::canonicalize(&fleet0).unwrap();
    assert!(stub.take_args().iter().all(|a| a.contains(&format!("--link-dest={}", w0.display()))));
}

/// A backups dir with pull's layout for apple: two snapshots and the big tier.
fn backups(root: &std::path::Path) -> String {
    let put = |rel: &str, body: &str| {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    };
    put("backups/w1d1/devtest-apple/notes.txt", "old\n");
    put("backups/w1d2/devtest-apple/ARENA_materials/chapter1/work.py", "mine\n");
    put("backups/w1d2/devtest-apple/notes.txt", "newer\n");
    put("backups/big/devtest-apple/model.bin", "big\n");
    std::fs::create_dir_all(root.join("backups/w1d2/devtest-bloom")).unwrap(); // empty
    root.join("backups").display().to_string()
}

fn restore_opts(dir: &str) -> RestoreOpts {
    RestoreOpts {
        from: None,
        path: None,
        dir: dir.into(),
        timeout: Duration::from_secs(60),
        overwrite_newer: false,
        with_git: false,
        dry_run: false,
        yes: true,
    }
}

/// 2026-10-08T05:06:40Z — names the kept-files dir.
const NOW: u64 = 1_791_436_000;

/// Everything refusable is refused before the pod is touched (no probe, no rsync).
#[tokio::test]
async fn restore_refuses_a_missing_or_empty_backup_before_touching_the_pod() {
    let stub = StubRsync::new("restore-refuse");
    let dir = backups(&stub.0);
    let mut f = fleet();
    f.pods.push(Pod { id: "id-devtest-dune".into(), name: "devtest-dune".into(), status: "RUNNING".into(), ..Default::default() });
    std::fs::create_dir_all(stub.0.join("backups/w1d2/devtest-dune")).unwrap();
    std::fs::write(stub.0.join("backups/w1d2/devtest-dune/x"), "x").unwrap();
    let cases: &[(&str, RestoreOpts, &str)] = &[
        ("apple", restore_opts(&stub.0.join("nowhere").display().to_string()), "doesn't exist"),
        ("bloom", restore_opts(&dir), "no wNdM snapshot of devtest-bloom"),
        ("apple", RestoreOpts { from: Some("w9d9".into()), ..restore_opts(&dir) }, "backups of devtest-apple: w1d2, w1d1, big"),
        ("apple", RestoreOpts { from: Some("../w1d1".into()), ..restore_opts(&dir) }, "isn't a backup label"),
        ("apple", RestoreOpts { path: Some("../etc".into()), ..restore_opts(&dir) }, "must be a relative path"),
        ("apple", RestoreOpts { path: Some("ARENA_materials/chapter9".into()), ..restore_opts(&dir) }, "isn't in that backup"),
        ("dune", restore_opts(&dir), "no SSH endpoint"),
        ("zebra", restore_opts(&dir), "zebra"),
    ];
    for (pod, opts, says) in cases {
        let fake = Arc::new(FakeRemote::new());
        let remote: Arc<dyn Remote> = fake.clone();
        let e = handle_restore(&f, &remote, &cfg(), pod, opts.clone(), &stub.path(), NOW).await.unwrap_err();
        assert!(format!("{e:#}").contains(says), "{pod} {opts:?}: {e:#}");
        assert!(fake.calls().is_empty() && stub.take_args().is_empty(), "{pod} {opts:?}");
    }
}

/// A STOPPED pod is never restored onto, dry run included: the listing may still show its
/// last endpoint, which belongs to nothing now — whatever answers there could be another
/// pod, and the push would put this pod's backup into its home. Refused before any probe.
#[tokio::test]
async fn a_stopped_pod_is_never_restored_onto() {
    let stub = StubRsync::new("restore-stopped");
    let dir = backups(&stub.0);
    let mut f = fleet();
    f.pods[0].status = "EXITED".into(); // its last endpoint still listed
    for dry_run in [false, true] {
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::stdout("arena-repo-home=/root\narena-repo-real=/root/ARENA_materials\n")]);
        let remote: Arc<dyn Remote> = fake.clone();
        let opts = RestoreOpts { dry_run, ..restore_opts(&dir) };
        let e = handle_restore(&f, &remote, &cfg(), "apple", opts, &stub.path(), NOW).await.unwrap_err().to_string();
        assert!(e.contains("devtest-apple: it is stopped (EXITED)") && e.contains("`arena pods start devtest-apple`"), "{e}");
        assert!(fake.calls().is_empty() && stub.take_args().is_empty(), "dry run {dry_run}: {:?}", fake.calls());
    }
}

#[tokio::test]
async fn restore_pushes_the_newest_snapshot_through_the_safe_argv() {
    let stub = StubRsync::new("restore-run");
    let dir = backups(&stub.0);
    let f = fleet();
    let linked = "arena-repo-home=/root\narena-repo-real=/workspace/ARENA_materials\narena-repo-mount=yes\n";
    // Default: the newest snapshot (w1d2), the whole of it, into apple's home.
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [FakeReply::stdout(linked)]);
    let remote: Arc<dyn Remote> = fake.clone();
    handle_restore(&f, &remote, &cfg(), "apple", restore_opts(&dir), &stub.path(), NOW).await.unwrap();
    let args = stub.take_args();
    assert_eq!(args.len(), 1, "{args:?}");
    let a = &args[0];
    assert!(a.ends_with(&format!("{dir}/w1d2/devtest-apple/ root@10.0.0.1:")), "{a}");
    // The volume is mounted there: what it replaces is kept ON it; newer files and .git
    // (a snapshot) stay as they are.
    for flag in [
        "--keep-dirlinks",
        "--no-owner",
        "--no-group",
        "--chmod=go-w",
        "--backup-dir=/workspace/.arena-restore/20261008T050640Z",
        "--update",
        "--exclude .git/",
        "--timeout=300",
        "-p 22001",
    ] {
        assert!(a.contains(flag), "missing {flag}: {a}");
    }
    assert!(!a.contains("--delete") && !a.contains("--force"), "{a}");
    // The pod was asked (read-only) where its repo is — once, before the rsync.
    assert!(matches!(&fake.calls()[..], [RemoteCall::Exec { cmd, timeout, .. }] if cmd.contains("arena-repo-real") && *timeout == Some(PROBE_TIMEOUT)));
    // No volume (or no answer): kept in the home; --overwrite-newer / --with-git as asked.
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [FakeReply::stdout("arena-repo-home=/root\narena-repo-real=/root/ARENA_materials\narena-repo-mount=no\n")]);
    let remote2: Arc<dyn Remote> = fake.clone();
    let opts = RestoreOpts { overwrite_newer: true, with_git: true, ..restore_opts(&dir) };
    handle_restore(&f, &remote2, &cfg(), "apple", opts, &stub.path(), NOW).await.unwrap();
    let a = stub.take_args().remove(0);
    assert!(a.contains("--backup-dir=.arena-restore/20261008T050640Z") && !a.contains("--update") && !a.contains(".git/"), "{a}");
    // --from big, only a path: the all-files tier, `--relative` from its root.
    let opts = RestoreOpts { from: Some("big".into()), path: Some("model.bin".into()), ..restore_opts(&dir) };
    handle_restore(&f, &remote, &cfg(), "apple", opts, &stub.path(), NOW).await.unwrap();
    let a = stub.take_args().remove(0);
    assert!(a.contains("--relative") && a.ends_with(&format!("{dir}/big/devtest-apple/./model.bin root@10.0.0.1:")), "{a}");
    // A dry run copies nothing (the read-only question is still asked, for the plan).
    let fake = Arc::new(FakeRemote::new());
    let remote: Arc<dyn Remote> = fake.clone();
    let opts = RestoreOpts { dry_run: true, yes: false, ..restore_opts(&dir) };
    handle_restore(&f, &remote, &cfg(), "apple", opts, &stub.path(), NOW).await.unwrap();
    assert!(stub.take_args().is_empty());
    assert_eq!(fake.calls().len(), 1);
    // …and with a pod whose disk was reset after the snapshot was written (the plan warns;
    // nothing else changes).
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [FakeReply::stdout(&format!("{linked}arena-repo-container=1\n"))]);
    let remote: Arc<dyn Remote> = fake.clone();
    let opts = RestoreOpts { dry_run: true, yes: false, ..restore_opts(&dir) };
    handle_restore(&f, &remote, &cfg(), "apple", opts, &stub.path(), NOW).await.unwrap();
    assert!(stub.take_args().is_empty());
}

/// A restore that pushed the repo's `.git` (the big tier) checks the repo afterwards: a
/// repo git can't read any more fails the command, saying where the replaced files are.
#[tokio::test]
async fn a_restore_that_pushed_git_checks_the_repo() {
    let stub = StubRsync::new("restore-git");
    let dir = backups(&stub.0);
    std::fs::create_dir_all(stub.0.join("backups/big/devtest-apple/ARENA_materials/.git")).unwrap();
    std::fs::write(stub.0.join("backups/big/devtest-apple/ARENA_materials/.git/HEAD"), "ref\n").unwrap();
    let f = fleet();
    let linked = "arena-repo-home=/root\narena-repo-real=/workspace/ARENA_materials\narena-repo-mount=yes\n";
    let big = || RestoreOpts { from: Some("big".into()), ..restore_opts(&dir) };
    // Reads fine: done.
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [FakeReply::stdout(linked), FakeReply::stdout("arena-git=ok\n")]);
    let remote: Arc<dyn Remote> = fake.clone();
    handle_restore(&f, &remote, &cfg(), "apple", big(), &stub.path(), NOW).await.unwrap();
    let calls = fake.calls();
    assert!(
        matches!(&calls[..], [_, RemoteCall::Exec { cmd, timeout, .. }] if cmd.contains("fsck --connectivity-only") && *timeout == Some(arena_core::restore::GIT_CHECK_TIMEOUT)),
        "{calls:?}"
    );
    assert!(!stub.take_args()[0].contains(".git/"), "the big tier carries .git");
    // Broken: the command fails and says where the replaced .git files are.
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [FakeReply::stdout(linked), FakeReply::stdout("arena-git=broken\n")]);
    let remote: Arc<dyn Remote> = fake.clone();
    let e = handle_restore(&f, &remote, &cfg(), "apple", big(), &stub.path(), NOW).await.unwrap_err().to_string();
    assert!(e.contains("git can't read") && e.contains("/workspace/.arena-restore/20261008T050640Z/"), "{e}");
    // A snapshot without --with-git pushes no .git: nothing to check.
    std::fs::create_dir_all(stub.0.join("backups/w1d2/devtest-apple/ARENA_materials/.git")).unwrap();
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [FakeReply::stdout(linked)]);
    let remote: Arc<dyn Remote> = fake.clone();
    handle_restore(&f, &remote, &cfg(), "apple", restore_opts(&dir), &stub.path(), NOW).await.unwrap();
    assert_eq!(fake.calls().len(), 1, "only the probe");
    stub.take_args();
}

#[tokio::test]
async fn a_failed_or_stuck_restore_says_nothing_was_deleted() {
    let stub = StubRsync::new("restore-fail");
    let dir = backups(&stub.0);
    let f = fleet();
    let remote: Arc<dyn Remote> = Arc::new(FakeRemote::new());
    let script = |name: &str, body: &str| {
        let p = stub.0.join(name);
        arena_core::remote::write_test_script(&p, body).unwrap();
        p.display().to_string()
    };
    let failing = script("rsync-fails", "#!/bin/sh\necho 'rsync: connection unexpectedly closed' >&2\nexit 12\n");
    let e = handle_restore(&f, &remote, &cfg(), "apple", restore_opts(&dir), &failing, NOW).await.unwrap_err().to_string();
    assert!(e.contains("exit 12") && e.contains("connection unexpectedly closed") && e.contains("nothing on the pod was deleted"), "{e}");
    // Stuck: stopped at its budget, reported as such.
    let stuck = script("rsync-stuck", "#!/bin/sh\nexec sleep 30\n");
    let opts = RestoreOpts { timeout: Duration::from_millis(500), ..restore_opts(&dir) };
    let started = std::time::Instant::now();
    let e = handle_restore(&f, &remote, &cfg(), "apple", opts, &stuck, NOW).await.unwrap_err().to_string();
    assert!(e.contains("timed out after 500ms") && e.contains("nothing on the pod was deleted"), "{e}");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn restore_says_where_the_repo_lands() {
    let root = std::env::temp_dir().join(format!("arena-restore-note-{}", std::process::id()));
    let src = root.join("w1d2/devtest-apple");
    std::fs::create_dir_all(src.join("ARENA_materials/chapter1")).unwrap();
    let probe = |real: &str, mounted: bool| arena_core::volume::RepoProbe {
        home: Some("/root".into()),
        real: Some(real.into()),
        volume_mounted: mounted,
        ..Default::default()
    };
    let note = |p: Option<&arena_core::volume::RepoProbe>, sub: Option<&str>| restore_note(REPO, "root", p, &src, sub);
    let linked = probe("/workspace/ARENA_materials", true);
    assert_eq!(note(Some(&linked), None).unwrap(), "(ARENA_materials/ lands in /workspace/ARENA_materials — the volume copy, through the pod's link)");
    assert!(note(Some(&linked), Some("ARENA_materials/chapter1")).is_some());
    assert_eq!(note(Some(&linked), Some("notes.txt")), None, "the repo isn't part of it");
    assert!(note(Some(&probe(REPO, true)), None).unwrap().contains("`arena pods setup` first puts it there"));
    assert_eq!(note(Some(&probe(REPO, false)), None), None, "no volume: nothing to say");
    assert!(note(None, None).unwrap().contains("couldn't ask the pod"));
    // A backup without the repo: nothing to say either.
    assert_eq!(restore_note(REPO, "root", Some(&linked), &root, None), None);
    let _ = std::fs::remove_dir_all(&root);
}

/// The plan's notes: a snapshot written after the pod's disk was reset is flagged, with the
/// newest snapshot from before that; a snapshot's .git staying behind is said.
#[test]
fn restore_warns_about_a_snapshot_written_after_the_reset() {
    let root = std::env::temp_dir().join(format!("arena-restore-notes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let put = |rel: &str| {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, rel).unwrap();
    };
    put("w1d1/devtest-apple/ARENA_materials/.git/HEAD");
    put("w1d1/devtest-apple/ARENA_materials/work.py");
    let secs = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    // The pod's disk is reset after w1d1 was written; the */15 pull then writes w1d2.
    std::thread::sleep(Duration::from_millis(1100));
    let reset = secs();
    std::thread::sleep(Duration::from_millis(1100));
    put("w1d2/devtest-apple/ARENA_materials/.git/HEAD");
    put("w1d2/devtest-apple/ARENA_materials/work.py");
    let src = root.join("w1d2/devtest-apple");
    let written = arena_core::restore::tree_stats(&src).unwrap().last_written;
    let probe = arena_core::volume::RepoProbe {
        home: Some("/root".into()),
        real: Some("/workspace/ARENA_materials".into()),
        volume_mounted: true,
        container_created: Some(reset),
    };
    let b = RestoreSource { base: &root, pod: "devtest-apple", label: "w1d2", src: &src, sub: None, last_written: written };
    let notes = restore_notes(REPO, "root", Some(&probe), &b, false);
    assert!(notes.iter().any(|n| n.starts_with("⚠ w1d2 was last written") && n.contains("--from w1d1")), "{notes:#?}");
    assert!(notes.iter().any(|n| n.contains(".git isn't restored")), "{notes:#?}");
    // The pod's container is older than every backup: nothing to flag; .git pushed: not said.
    let old = arena_core::volume::RepoProbe { container_created: Some(1), ..probe.clone() };
    let notes = restore_notes(REPO, "root", Some(&old), &b, true);
    assert!(!notes.iter().any(|n| n.starts_with('⚠') || n.contains(".git")), "{notes:#?}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn restore_parses() {
    match pods(&["restore", "apple", "--from", "big", "--path", "ARENA_materials", "--dir", "/b", "--timeout", "60", "--dry-run"]) {
        PodCmd::Restore { target, from, path, dir, timeout, overwrite_newer, with_git, dry_run } => {
            assert_eq!((target.as_str(), from.as_deref(), path.as_deref(), dir.as_deref(), timeout, dry_run), ("apple", Some("big"), Some("ARENA_materials"), Some("/b"), Some(60), true));
            assert!(!overwrite_newer && !with_git, "both off by default");
        }
        _ => panic!("not restore"),
    }
    match pods(&["restore", "apple", "--overwrite-newer", "--with-git"]) {
        PodCmd::Restore { overwrite_newer, with_git, .. } => assert!(overwrite_newer && with_git),
        _ => panic!("not restore"),
    }
    assert!(Cli::try_parse_from(["arena", "pods", "restore", "apple", "--timeout", "0"]).is_err());
    assert!(Cli::try_parse_from(["arena", "pods", "restore"]).is_err(), "one pod, named");
}

#[test]
fn run_previews_name_every_resolved_pod() {
    let names = ["devtest-apple", "devtest-cloud"];
    assert_eq!(
        run_preview(true, "hostname", "<wrapped>", &names, Duration::from_secs(60)),
        "[dry-run] would run on 2 pod(s) (60s budget each): devtest-apple, devtest-cloud\n  <wrapped>"
    );
    assert_eq!(
        run_preview(false, "hostname", "<wrapped>", &names, Duration::from_secs(60)),
        "Run `hostname` on 2 pod(s) over SSH: devtest-apple, devtest-cloud"
    );
}

#[tokio::test(start_paused = true)]
async fn ssh_fleet_commands_reach_exactly_the_selected_pods() {
    // PLAN 1.C through each SSH command family: the shared selector decides which pods
    // are reached — names, ranges, --exclude, `-t` for `run` — and nothing else is.
    let cfg = Config::parse(&format!(
        "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\nBACKUP_REPO_PATH={REPO}\n\
         GIT_SSH_KEY_REMOTE={KEY}\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\n"
    ));
    let cases: &[(&[&str], &[u16])] = &[
        (&["run", "-t", "apple,cloud", "true"], &[22001, 22003]),
        (&["run", "-t", "bloom..cloud", "--", "true"], &[22002, 22003]),
        (&["run", "--exclude", "bloom", "true"], &[22001, 22003]),
        (&["test", "bloom"], &[22002]),
        (&["test", "--deep", "apple..bloom"], &[22001, 22002]),
        (&["backup", "apple", "--no-pull"], &[22001]),
        (&["backup", "--exclude", "apple", "--no-pull"], &[22002, 22003]),
        (&["init-branches", "cloud", "--week", "1", "--day", "2"], &[22003]),
        (&["set-branch", "main", "apple", "cloud"], &[22001, 22003]),
        (&["set-branch", "main", "--all", "--exclude", "bloom..cloud"], &[22001]),
    ];
    for (argv, want) in cases {
        let fake = Arc::new(FakeRemote::new());
        // `test --deep` on an empty reply FAILs its pods; only who was reached matters here.
        let _ = handle_pods(pods(argv), &fleet(), fake.clone(), &cfg, true).await;
        assert_eq!(ports(&fake), *want, "{argv:?}");
    }
    // Typos, a reversed range, a bad --on and a missing scope reach nobody.
    for argv in [
        &["run", "-t", "aple", "true"][..],
        &["backup", "--exclude", "clod", "--no-pull"],
        &["test", "--deep", "cloud..apple"],
        &["init-branches", "--on", "aws"],
        &["set-branch", "main"],
    ] {
        let fake = Arc::new(FakeRemote::new());
        assert!(handle_pods(pods(argv), &fleet(), fake.clone(), &cfg, true).await.is_err(), "{argv:?}");
        assert!(fake.calls().is_empty(), "{argv:?}");
    }
    // An empty argument (a script's unset `$POD`) is an error — never "every pod".
    for argv in [
        &["run", "-t", "", "true"][..],
        &["run", "-t", ",", "true"],
        &["run", "--include", " ", "true"],
        &["setup", ""],
        &["setup", "--force", "--include", ""],
        &["copy-keys", ""],
        &["copy-keys", "--include", ""],
        &["cp", "/nonexistent/f", "-t", ""],
        &["test", ""],
        &["test", "--deep", " , "],
        &["backup", "", "--no-pull"],
        &["init-branches", ""],
        &["set-branch", "main", "--all", "--exclude", ""],
    ] {
        let fake = Arc::new(FakeRemote::new());
        let e = handle_pods(pods(argv), &fleet(), fake.clone(), &cfg, true).await.unwrap_err().to_string();
        assert!(e.contains("an empty target ("), "{argv:?}: {e}");
        assert!(fake.calls().is_empty(), "{argv:?}");
    }
    // A named pod without an endpoint fails the command; swept in by default, it's skipped.
    let mut f = fleet();
    f.pods[0].ssh_port = None;
    let fake = Arc::new(FakeRemote::new());
    let err = handle_pods(pods(&["run", "-t", "apple", "true"]), &f, fake.clone(), &cfg, true).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "none of the selected pods (devtest-apple) has an SSH endpoint yet — check `arena pods list`"
    );
    handle_pods(pods(&["run", "true"]), &f, fake.clone(), &cfg, true).await.unwrap();
    assert_eq!(ports(&fake), [22002, 22003]);
}

#[tokio::test(start_paused = true)]
async fn a_hanging_pod_times_out_while_the_others_finish() {
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22002), [FakeReply::hang()]);
    let remote: Arc<dyn Remote> = fake.clone();
    let start = Instant::now();
    let results = run_fleet(&remote, targets(&fleet()), "true", Duration::from_secs(90)).await;
    assert_eq!(start.elapsed(), Duration::from_secs(90), "ends at the budget, not the hang");
    let failed: Vec<(&str, &str)> = results.iter().filter(|r| !r.ok).map(|r| (r.name.as_str(), r.text.as_str())).collect();
    assert_eq!(failed, [("devtest-bloom", "timed out after 90s")]);
    let (lines, bad) = render_run(&results, true);
    assert_eq!((bad, lines[1].as_str()), (1, "devtest-bloom          ✗ timed out after 90s"));
}

#[tokio::test(start_paused = true)]
async fn pods_test_and_pods_run_carry_their_budgets_through_the_command() {
    // `pods test`: 90s per pod; a wedged pod fails the command at that budget.
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22003), [FakeReply::hang()]);
    let start = Instant::now();
    let plain = PodCmd::Test { deep: false, sel: Select::default(), json: false, verbose: false, cohort: crate::cli::CohortScope::default() };
    let err = handle_pods(plain, &fleet(), fake.clone(), &cfg(), true).await.unwrap_err();
    assert_eq!(err.to_string(), "1 pod(s) failed");
    assert_eq!(start.elapsed(), TEST_TIMEOUT);
    assert!(fake.calls().iter().all(|c| matches!(c, RemoteCall::Exec { cmd, timeout, .. }
        if cmd.contains("import torch") && *timeout == Some(TEST_TIMEOUT))));

    // `pods run --timeout 45`.
    let fake = Arc::new(FakeRemote::new());
    let run = PodCmd::Run { sel: SelectByFlag::default(), command: vec!["nvidia-smi".into()], timeout: 45, dry_run: false, background: false };
    handle_pods(run, &fleet(), fake.clone(), &cfg(), true).await.unwrap();
    assert_eq!(fake.calls().len(), 3);
    assert!(fake.calls().iter().all(|c| matches!(c, RemoteCall::Exec { timeout, .. } if *timeout == Some(Duration::from_secs(45)))));

    // A dry run reaches no pod.
    let fake = Arc::new(FakeRemote::new());
    let run = PodCmd::Run { sel: SelectByFlag::default(), command: vec!["reboot".into()], timeout: 45, dry_run: true, background: false };
    handle_pods(run, &fleet(), fake.clone(), &cfg(), true).await.unwrap();
    assert!(fake.calls().is_empty());
}

#[test]
fn run_timeout_flag_goes_before_the_command_and_defaults_to_30_min() {
    let run = |args: &[&str]| match Cli::try_parse_from(args).map(|c| c.cmd) {
        Ok(Cmd::Pods(PodCmd::Run { command, timeout, .. })) => Ok((command, timeout)),
        Ok(_) => panic!("not a run"),
        Err(e) => Err(e),
    };
    assert_eq!(RUN_TIMEOUT_SECS, 1800);
    assert_eq!(run(&["arena", "pods", "run", "nvidia-smi", "-L"]).unwrap(), (vec!["nvidia-smi".into(), "-L".into()], 1800));
    assert_eq!(
        run(&["arena", "pods", "run", "--timeout", "60", "nvidia-smi", "-L"]).unwrap(),
        (vec!["nvidia-smi".into(), "-L".into()], 60)
    );
    // After the command, it's part of the command (like every other flag).
    assert_eq!(run(&["arena", "pods", "run", "echo", "--timeout", "5"]).unwrap().0, ["echo", "--timeout", "5"]);
    // Bounded: no zero, no "forever" spelled as a huge number.
    for bad in ["0", "86401"] {
        assert!(run(&["arena", "pods", "run", "--timeout", bad, "true"]).is_err(), "{bad}");
    }
}

#[tokio::test(start_paused = true)]
async fn backup_classifies_each_pods_reply_and_a_wedged_pod_fails_at_the_budget() {
    let f = fleet_of(&[("apple", 22001), ("bloom", 22002), ("cloud", 22003), ("delta", 22004), ("echo", 22005)]);
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [FakeReply::stdout("[autocommit-x 1a2b3c] arena backup\nPUSHED autocommit-x\n")]);
    fake.script(&host(22002), [FakeReply::hang()]);
    fake.script(&host(22003), [FakeReply::stdout("NO_CHANGES feature-y\n")]);
    fake.script(&host(22004), [FakeReply::stdout("SKIP main\n")]);
    fake.script(&host(22005), [FakeReply::exit(128, "fatal: Could not read from remote repository.")]);
    let remote: Arc<dyn Remote> = fake.clone();
    let jobs = targets(&f).into_iter().map(|(n, t)| (n, t, "backup".to_string())).collect();
    let mut lines = Vec::new();
    let start = Instant::now();
    let tally = backup_fleet(&remote, jobs, REPO, |l| lines.push(l.to_string()), |_| {}).await;
    let on_main = vec![("devtest-delta".to_string(), "main".to_string())];
    let failed = |pod: &str, why: &str| Unpushed { pod: pod.into(), why: why.into(), branch: None, at_push: false, wrong_origin: false, busy: None };
    assert_eq!(
        tally,
        BackupTally {
            pushed: 1,
            unchanged: 1,
            skipped: 1,
            failed: vec!["devtest-echo".to_string(), "devtest-bloom".to_string()],
            unpushed: vec![
                failed("devtest-echo", "exit 128: fatal: Could not read from remote repository."),
                failed("devtest-bloom", "timed out after 300s"),
            ],
            on_protected: on_main,
            uncommitted: vec![],
        }
    );
    assert_eq!(start.elapsed(), BACKUP_TIMEOUT);
    // The wedged pod reports last, after its budget; the rest as soon as they answered. The
    // pod on main has no line of its own: it's said once, after the run.
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert_eq!(lines[3], "[5/5] ✗ devtest-bloom: timed out after 300s");
    let mut first: Vec<String> = unnumbered(&lines[..3]);
    first.sort();
    assert_eq!(
        first,
        [
            "= devtest-cloud (no changes, on feature-y)",
            "✓ devtest-apple -> autocommit-x",
            "✗ devtest-echo (exit 128): fatal: Could not read from remote repository.",
        ]
    );

    // Through the command: each pod's own commit message, the budget, a non-zero result.
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22002), [FakeReply::exit(1, "boom")]);
    let err = handle_backup(fake.clone(), &cfg(), true, None, &everyone(&fleet()), &mut crate::PodLocks::default(), &mut Vec::new())
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "1 pod(s) failed to back up");
    for (name, port) in [("apple", 22001), ("bloom", 22002), ("cloud", 22003)] {
        let want = backup_command(REPO, Some(KEY), &format!("arena backup devtest-{name}"), None);
        assert_eq!(execs(&fake, port), [(want, Some(BACKUP_TIMEOUT))]);
    }
    // Dry run: nothing reaches a pod.
    let fake = Arc::new(FakeRemote::new());
    handle_backup(fake.clone(), &cfg(), false, None, &everyone(&fleet()), &mut crate::PodLocks::default(), &mut Vec::new())
        .await
        .unwrap();
    assert!(fake.calls().is_empty());
}

/// RR3: `pods backup`'s git push releases each pod's backup lock the moment that pod's push
/// ends — a wedged pod (here: one that hangs to the budget) keeps only its own.
#[tokio::test(start_paused = true)]
async fn rr3_backup_releases_each_pods_lock_as_its_push_ends() {
    let dir = std::env::temp_dir().join(format!("arena-rr3-git-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let d = dir.display().to_string();
    let f = fleet_of(&[("apple", 22001), ("bloom", 22002)]);
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [FakeReply::stdout("NO_CHANGES feature-y\n")]);
    fake.script(&host(22002), [FakeReply::hang()]);
    let names: Vec<String> = f.pods.iter().map(|p| p.name.clone()).collect();
    let mut locks = crate::pods::pull::take_pod_locks(&d, &names, Duration::from_millis(10)).await;
    assert!(locks.holds("devtest-apple") && locks.holds("devtest-bloom"));
    let held = |n: &str| arena_core::pull::try_lock(arena_core::pull::open_pod_lock(&d, n).unwrap()).unwrap().is_err();
    let free = |n: &str| crate::pods::lock::lock_settles_free(arena_core::pull::open_pod_lock(&d, n).unwrap());
    let (sel, c) = (everyone(&f), cfg());
    let mut unpushed = Vec::new();
    let backup = handle_backup(fake.clone(), &c, true, None, &sel, &mut locks, &mut unpushed);
    let check = async {
        tokio::time::sleep(Duration::from_secs(10)).await;
        (free("devtest-apple"), !held("devtest-bloom"))
    };
    let (_, (apple, bloom)) = tokio::join!(backup, check);
    assert!(apple, "apple's push ended, yet its lock was still held");
    assert!(!bloom, "bloom's push was still running");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A config whose backup dir is `base` — created, so the per-pod locks are really taken
/// (`take_pod_locks` takes none in a dir that doesn't exist) — with a label to use.
fn backup_cfg(base: &std::path::Path) -> Config {
    std::fs::create_dir_all(base).unwrap();
    Config::parse(&format!(
        "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\nBACKUP_REPO_PATH={REPO}\n\
         GIT_SSH_KEY_REMOTE={KEY}\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nARENA_START_DATE=2026-01-05\n\
         LOCAL_BACKUP_DIR={}\n",
        base.display()
    ))
}

/// A backup GitHub refuses over a secret the redaction doesn't know (exit 4).
fn push_blocked(branch: &str) -> FakeReply {
    FakeReply::exit(4, "remote: error: GH013: Repository rule violations found")
        .with_stdout(&format!("BRANCH {branch}\nPUSH_BLOCKED OpenRouter API Key at ex.py:3\n"))
}

/// Live: GitHub refused three pods' pushes over a pasted key for ~20 hours, and the prod
/// cron is `pods backup --no-pull` — so those pods had no copy anywhere but on themselves.
/// Now every pod whose push fails, whatever the reason (a key, the network), is rsynced in
/// the same run — both tiers, under its own lock — even under --no-pull; a pod whose push
/// worked isn't; and the run still fails (a copy is no git backup). A copy that fails too
/// is in the error beside the push's failure. Without --no-pull the pull covers every pod
/// once — the failed ones aren't copied twice.
#[tokio::test]
async fn a_failed_push_is_copied_in_the_same_run_even_under_no_pull_and_the_run_still_fails() {
    let stub = StubRsync::new("fallback");
    let cfg = backup_cfg(&stub.0.join("backups"));
    let four = fleet_of(&[("apple", 22001), ("bloom", 22002), ("cloud", 22003), ("delta", 22004)]);
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n")]);
    fake.script(&host(22002), [push_blocked("autocommit-b"), repo_in_place()]);
    fake.script(
        &host(22003),
        [FakeReply::exit(128, "fatal: Could not read from remote repository.").with_stdout("BRANCH autocommit-c\nPUSH_FAILED\n"), repo_in_place()],
    );
    // Review: a pod that fails before its push (here: no repo) fails the same way every run —
    // never rsynced every 15 minutes.
    fake.script(&host(22004), [FakeReply::exit(2, "sh: 1: cd: can't cd to /root/ARENA_materials")]);
    let err = handle_full_backup(fake.clone(), &cfg, &everyone(&four), true, None, false, true, &stub.path()).await.unwrap_err();
    assert_eq!(err.to_string(), "3 pod(s) failed to back up");
    assert_eq!(fake.calls_to(&host(22004)).len(), 1, "no repo: not even asked where its repo is");
    let got = stub.take();
    for pod in ["devtest-bloom", "devtest-cloud"] {
        let theirs: Vec<&String> = got.iter().filter(|d| d.ends_with(&format!("/{pod}"))).collect();
        assert_eq!(theirs.len(), 3, "{pod}: the snapshot's two passes and big: {got:?}");
        assert!(got.contains(&format!("big/{pod}")), "{got:?}");
    }
    assert_eq!(got.len(), 6, "never the pod whose push worked, nor the one with no repo: {got:?}");
    // Each failed pod: its backup, then the copy's question where its repo is.
    assert_eq!(fake.calls_to(&host(22001)).len(), 1);
    let bloom = execs(&fake, 22002);
    assert_eq!(bloom[1], (arena_core::volume::probe_command(REPO), Some(PROBE_TIMEOUT)));

    // The copy fails too: both in the error (and on the pod's line, below).
    let failing = StubRsync::new("fallback-fail");
    arena_core::remote::write_test_script(
        failing.0.join("rsync"),
        "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\necho \"$last\" >> \"$(dirname \"$0\")/calls.log\"\n\
         case \"$last\" in *devtest-bloom*) echo 'rsync: connection unexpectedly closed' >&2; exit 12;; esac\n",
    )
    .unwrap();
    let cfg = backup_cfg(&failing.0.join("backups"));
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22002), [push_blocked("autocommit-b"), repo_in_place()]);
    let err = handle_full_backup(fake.clone(), &cfg, &everyone(&fleet()), true, None, false, true, &failing.path()).await.unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.starts_with("1 pod(s) failed to back up; 2 rsync job(s) failed: devtest-bloom ["), "{msg}");
    assert!(msg.contains("devtest-bloom [snapshot]") && msg.contains("devtest-bloom [big]"), "{msg}");

    // Without --no-pull: every pod pulled once.
    let fake = Arc::new(FakeRemote::new());
    for port in [22001, 22003] {
        fake.script(&host(port), [FakeReply::stdout("NO_CHANGES autocommit-x\n"), repo_in_place()]);
    }
    fake.script(&host(22002), [push_blocked("autocommit-b"), repo_in_place()]);
    let err = handle_full_backup(fake.clone(), &cfg_at(&stub), &everyone(&fleet()), false, None, false, true, &stub.path()).await.unwrap_err();
    assert_eq!(err.to_string(), "1 pod(s) failed to back up");
    let got = stub.take();
    assert_eq!(got.len(), 9, "{got:?}");
    assert_eq!(got.iter().filter(|d| d.ends_with("/devtest-bloom")).count(), 3, "{got:?}");
}

fn cfg_at(stub: &StubRsync) -> Config {
    backup_cfg(&stub.0.join("backups"))
}

/// Live (bsec B2): a push to a missing repo said `git push failed (exit 1: and the repository
/// exists.)` — the last line of git's advice. The "why" (the per-pod line, set-branch's
/// `NOT PUSHED FIRST (…)`) is now the first verdict line git or GitHub wrote, else the last
/// non-empty one. Pure.
#[test]
fn a_failed_push_says_why_by_its_verdict_line_not_its_last() {
    let missing = "ERROR: Repository not found.\nfatal: Could not read from remote repository.\n\n\
                   Please make sure you have the correct access rights\nand the repository exists.\n";
    let cases: &[(&str, Option<&str>)] = &[
        (missing, Some("ERROR: Repository not found.")),
        ("ssh: connect to host github.com port 22: Connection timed out\r\nfatal: Could not read from remote repository.\r\n\r\nand the repository exists.\r\n", Some("ssh: connect to host github.com port 22: Connection timed out")),
        // Live (hz2): the deploy key moved away — ssh's line names the cause git's `fatal:` hides.
        ("Warning: Identity file /root/.ssh/id_ed25519 not accessible: No such file or directory.\nno such identity: /root/.ssh/id_ed25519: No such file or directory\ngit@github.com: Permission denied (publickey).\nfatal: Could not read from remote repository.\n\nPlease make sure you have the correct access rights\nand the repository exists.\n", Some("git@github.com: Permission denied (publickey).")),
        ("To github.com:o/r.git\n ! [rejected]        autocommit-x -> autocommit-x (non-fast-forward)\nerror: failed to push some refs to 'github.com:o/r.git'\nhint: Updates were rejected\n", Some("! [rejected]        autocommit-x -> autocommit-x (non-fast-forward)")),
        ("remote: Resolving deltas: 100% (3/3)\nremote: error: File big.bin is 120.00 MB; this exceeds GitHub's file size limit\nTo github.com:o/r.git\n ! [remote rejected] x -> x (pre-receive hook declined)\nerror: failed to push some refs\n", Some("remote: error: File big.bin is 120.00 MB; this exceeds GitHub's file size limit")),
        ("To /root/gh013.git\n ! [remote rejected] x -> x (pre-receive hook declined)\n", Some("! [remote rejected] x -> x (pre-receive hook declined)")),
        ("error: src refspec x does not match any\n", Some("error: src refspec x does not match any")),
        // A harmless `Permission denied` earlier on (git add, a mount it can't read) isn't the push's.
        ("warning: could not open directory 'data/mnt/': Permission denied\nTo github.com:o/r.git\n ! [rejected]        x -> x (non-fast-forward)\n", Some("! [rejected]        x -> x (non-fast-forward)")),
        // No verdict: the last non-empty line, as before.
        ("Warning: Permanently added 'github.com' to the list of known hosts.\nsh: 1: cd: can't cd to /root/x\n\n", Some("sh: 1: cd: can't cd to /root/x")),
        // A verdict word mid-line is not a verdict line.
        ("hook says: error: no\nthe end\n", Some("the end")),
        ("", None),
        ("  \n\n", None),
    ];
    for (stderr, want) in cases {
        assert_eq!(telling_line(stderr), *want, "{stderr:?}");
    }
    let out = |code: i32, stderr: &str| -> PodCall {
        Ok(arena_core::ssh::SshOutput { success: false, code: Some(code), stdout: "PUSH_FAILED\n".into(), stderr: stderr.into() })
    };
    assert_eq!(failure_why(&out(1, missing)), "exit 1: ERROR: Repository not found.");
    assert_eq!(failure_why(&out(2, "")), "exit 2");
    assert_eq!(failure_why(&Err("timed out after 300s".into())), "timed out after 300s");
}

/// The per-pod lines of the fallback copy: why the push failed, and where the files went —
/// or why the copy failed too.
#[test]
fn each_failed_push_says_where_its_files_went_or_why_they_didnt() {
    let u = |pod: &str, why: &str| Unpushed { pod: pod.into(), why: why.into(), branch: Some("autocommit-x".into()), at_push: true, wrong_origin: false, busy: None };
    let mut unpushed = vec![
        u("devtest-bloom", "GitHub push protection: OpenRouter API Key at ex.py:3"),
        u("devtest-cloud", "exit 128: fatal: Could not read from remote repository."),
    ];
    // One that failed before its push: its own failure line is all it gets.
    unpushed.push(Unpushed { pod: "devtest-delta".into(), why: "exit 2: no repo".into(), branch: None, at_push: false, wrong_origin: false, busy: None });
    let pulled: anyhow::Result<Pulled> = Ok(Pulled {
        failures: vec!["devtest-cloud [big]".into()],
        failed: vec![("devtest-cloud".into(), "[big] rsync exit 12".into())],
        snapshot_root: "/b/w41d5".into(),
        big_root: Some("/b/big".into()),
        whole: vec![],
    });
    assert_eq!(
        fallback_lines(&unpushed, &pulled),
        [
            "✗ devtest-bloom: git push failed (GitHub push protection: OpenRouter API Key at ex.py:3) → files copied to \
             /b/w41d5/devtest-bloom (and /b/big/devtest-bloom)",
            "✗ devtest-cloud: git push failed (exit 128: fatal: Could not read from remote repository.); the fallback copy \
             failed too: [big] rsync exit 12",
        ]
    );
    let refused: anyhow::Result<Pulled> = Err(anyhow::anyhow!("refusing to pull (LOCAL_BACKUP_BIG_DIR is set, …)"));
    assert_eq!(
        fallback_lines(&unpushed[..1], &refused),
        ["✗ devtest-bloom: git push failed (GitHub push protection: OpenRouter API Key at ex.py:3); the fallback copy failed \
          too: refusing to pull (LOCAL_BACKUP_BIG_DIR is set, …)"]
    );
    assert!(fallback_lines(&[], &pulled).is_empty());
    assert!(fallback_lines(&unpushed[2..], &pulled).is_empty(), "never copied: never said to be");
}

/// FIX10: the backup's own last scan finding a key left after the redaction (exit 4,
/// `PUSH_BLOCKED key left after redaction`) fails at the push like GitHub's refusal: the pod
/// goes to the fallback copy, and its line says what was left and what to do — never that
/// GitHub refused it, or that a pattern is missing.
#[tokio::test]
async fn a_key_left_after_the_redaction_fails_at_the_push_and_is_copied() {
    let f = fleet_of(&[("bloom", 22002)]);
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22002), [FakeReply::exit(4, "").with_stdout("BRANCH autocommit-b\nPUSH_BLOCKED key left after redaction\n")]);
    let remote: Arc<dyn Remote> = fake.clone();
    let jobs = targets(&f).into_iter().map(|(n, t)| (n, t, "backup".to_string())).collect();
    let mut lines = Vec::new();
    let tally = backup_fleet(&remote, jobs, REPO, |l| lines.push(l.to_string()), |_| {}).await;
    assert_eq!(
        lines,
        ["[1/1] ✗ devtest-bloom: a key is still in the unpushed commits of autocommit-b after the redaction — in a commit's \
          author, committer or merged tag, a binary file or a symlink, or a redaction that didn't take — so nothing was \
          pushed. Take it out of those commits on devtest-bloom by hand"]
    );
    let why = "a key left in the unpushed commits after the redaction";
    let left = Unpushed { pod: "devtest-bloom".into(), why: why.into(), branch: Some("autocommit-b".into()), at_push: true, wrong_origin: false, busy: None };
    assert_eq!(tally.failed, ["devtest-bloom"]);
    assert_eq!(tally.unpushed, [left]);
    let pulled: anyhow::Result<Pulled> =
        Ok(Pulled { failures: vec![], failed: vec![], snapshot_root: "/b/w41d5".into(), big_root: Some("/b/big".into()), whole: vec![] });
    assert_eq!(
        fallback_lines(&tally.unpushed, &pulled),
        [format!("✗ devtest-bloom: git push failed ({why}) → files copied to /b/w41d5/devtest-bloom (and /b/big/devtest-bloom)")]
    );
}

/// [`backup_cfg`] with a configured repo, `o/r`: every backup checks each pod's `origin`.
fn origin_cfg(base: &std::path::Path) -> Config {
    std::fs::create_dir_all(base).unwrap();
    Config::parse(&format!(
        "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\nBACKUP_REPO_PATH={REPO}\n\
         GIT_SSH_KEY_REMOTE={KEY}\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nARENA_START_DATE=2026-01-05\n\
         LOCAL_BACKUP_DIR={}\nARENA_REPO_OWNER=o\nARENA_REPO_NAME=r\n",
        base.display()
    ))
}

/// A backup the pod refused (exit 5): its `origin` is the upstream course repo, not `o/r`.
fn wrong_origin() -> FakeReply {
    FakeReply::exit(5, "").with_stdout("WRONG_ORIGIN o/r https://github.com/ARENA-education/ARENA_materials\n")
}

const WRONG_WHY: &str = "origin is https://github.com/ARENA-education/ARENA_materials, not the configured o/r";

/// Live (2026-10-17): a pod nobody set up had the upstream course repo as `origin`, and was
/// never backed up to git, silently. Now every backup carries the configured repo, a pod whose
/// origin isn't it fails on a line that says whose repo it has and what fixes it, and the run
/// fails. Under `--no-pull` (the */15 cron) it is copied by rsync in the same run: its work
/// reaches no git remote at all.
#[tokio::test]
async fn a_pod_whose_origin_isnt_the_configured_repo_fails_says_how_to_fix_it_and_is_copied() {
    let f = fleet_of(&[("apple", 22001), ("bloom", 22002)]);
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n")]);
    fake.script(&host(22002), [wrong_origin()]);
    let remote: Arc<dyn Remote> = fake.clone();
    let jobs = targets(&f).into_iter().map(|(n, t)| (n, t, "backup".to_string())).collect();
    let mut lines = Vec::new();
    let tally = backup_fleet(&remote, jobs, REPO, |l| lines.push(l.to_string()), |_| {}).await;
    assert_eq!((tally.pushed, tally.failed.len()), (1, 1));
    let bloom = Unpushed { pod: "devtest-bloom".into(), why: WRONG_WHY.into(), branch: None, at_push: false, wrong_origin: true, busy: None };
    assert_eq!(tally.unpushed, std::slice::from_ref(&bloom));
    let mut got = unnumbered(&lines);
    got.sort();
    assert_eq!(
        got,
        [
            "✓ devtest-apple -> autocommit-a".to_string(),
            format!("✗ devtest-bloom: {WRONG_WHY} — not backed up to git; run `arena pods setup devtest-bloom`"),
        ]
    );

    // `pods backup --no-pull`: every pod's backup checks `o/r`; bloom fails the run and is copied.
    let stub = StubRsync::new("wrong-origin");
    let cfg = origin_cfg(&stub.0.join("backups"));
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n")]);
    fake.script(&host(22002), [wrong_origin(), repo_in_place()]);
    fake.script(&host(22003), [FakeReply::stdout("SKIP main\n")]);
    let err = handle_full_backup(fake.clone(), &cfg, &everyone(&fleet()), true, None, false, true, &stub.path()).await.unwrap_err();
    assert_eq!(err.to_string(), "1 pod(s) failed to back up");
    for (name, port) in [("apple", 22001), ("bloom", 22002), ("cloud", 22003)] {
        let want = backup_command(REPO, Some(KEY), &format!("arena backup devtest-{name}"), Some("o/r"));
        assert_eq!(execs(&fake, port)[0], (want, Some(BACKUP_TIMEOUT)), "{name}");
    }
    assert_eq!(execs(&fake, 22002)[1], (arena_core::volume::probe_command(REPO), Some(PROBE_TIMEOUT)), "bloom's copy");
    let copied = stub.take();
    assert!(copied.len() == 3 && copied.iter().all(|d| d.ends_with("/devtest-bloom")), "only bloom, both tiers: {copied:?}");
    // Its line once copied.
    let pulled: anyhow::Result<Pulled> =
        Ok(Pulled { failures: vec![], failed: vec![], snapshot_root: "/b/w41d5".into(), big_root: Some("/b/big".into()), whole: vec![] });
    assert_eq!(
        fallback_lines(&[bloom], &pulled),
        [format!("✗ devtest-bloom: not backed up to git ({WRONG_WHY}) → files copied to /b/w41d5/devtest-bloom (and /b/big/devtest-bloom)")]
    );
    // No configured repo: no check asked for (an empty last argument).
    assert!(backup_command(REPO, Some(KEY), "m", None).ends_with(&format!("'{KEY}' ''")));
}

/// `init-branches` would push a participant's new branch to whatever `origin` is, and
/// `set-branch` would fetch (and with `--hard` reset to) that repo's branch. A pod whose
/// origin isn't the configured repo is refused by its push-first backup, so neither switches
/// it: no copy, no switch, no init, said on its line with the fix. The others go ahead.
#[tokio::test]
async fn set_branch_and_init_branches_never_switch_a_pod_whose_origin_isnt_the_configured_repo() {
    let stub = StubRsync::new("wrong-origin-switch");
    let cfg = origin_cfg(&stub.0.join("backups"));
    let backup = |name: &str| (backup_command(REPO, Some(KEY), &format!("arena backup devtest-{name}"), Some("o/r")), Some(BACKUP_TIMEOUT));
    let refused = format!("✗ devtest-bloom not switched: {WRONG_WHY} — not backed up to git; run `arena pods setup devtest-bloom`");
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n"), FakeReply::ok()]);
    fake.script(&host(22002), [wrong_origin()]);
    fake.script(&host(22003), [FakeReply::stdout("SKIP main\n"), FakeReply::ok()]);
    let _ = crate::pods::selection::take_said();
    let err = handle_set_branch(fake.clone(), &cfg, "main", &everyone(&fleet()), true, false, true, &stub.path()).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "1 pod(s) NOT switched — each one's line says why: devtest-bloom"
    );
    assert!(crate::pods::selection::take_said().contains(&refused), "{refused}");
    assert_eq!(execs(&fake, 22002), [backup("bloom")], "bloom: refused, never copied or switched");
    let hard = checkout_command(REPO, "main", Some(KEY), true);
    for (name, port) in [("apple", 22001), ("cloud", 22003)] {
        assert_eq!(execs(&fake, port), [backup(name), (hard.clone(), Some(BRANCH_TIMEOUT))], "{name}");
    }
    assert!(stub.take().is_empty(), "nothing copied");

    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [FakeReply::stdout("SKIP main\n"), FakeReply::ok()]);
    fake.script(&host(22002), [wrong_origin()]);
    fake.script(&host(22003), [FakeReply::stdout("SKIP main\n"), FakeReply::ok()]);
    let err = handle_init_branches(fake.clone(), &cfg, Some(1), Some(2), &everyone(&fleet()), false, true, &stub.path()).await.unwrap_err();
    assert!(err.to_string().contains("NOT switched") && err.to_string().ends_with(": devtest-bloom"), "{err}");
    assert!(crate::pods::selection::take_said().contains(&refused));
    assert_eq!(execs(&fake, 22002), [backup("bloom")], "bloom never gets the init command");
    let bcfg = BackupConfig::from_config(&cfg, 1, 2);
    for (name, port) in [("apple", 22001), ("cloud", 22003)] {
        let init = init_branch_command(&bcfg, &format!("devtest-{name}"));
        // (The check is in the script the command runs under `sh -c`: its quotes escaped.)
        assert!(init.contains(r"origin_check '\''o/r'\''"), "the init command checks too");
        assert_eq!(execs(&fake, port), [backup(name), (init, Some(BRANCH_TIMEOUT))], "{name}");
    }
}

/// A stand-in `rsync` that runs the real one with the pull's own argv, the pod's side
/// (`root@10.0.0.1:<path>`) read from the local tree `pod` instead. `None` without an rsync.
fn local_pod_rsync(tag: &str, pod: &std::path::Path) -> Option<StubRsync> {
    let real = std::process::Command::new("sh").args(["-c", "command -v rsync"]).output().ok()?;
    let real = String::from_utf8_lossy(&real.stdout).trim().to_string();
    if real.is_empty() {
        return None;
    }
    let stub = StubRsync::new(tag);
    arena_core::remote::write_test_script(
        stub.0.join("rsync"),
        format!(
            "#!/bin/sh\nfor a do\n  shift\n  case $a in root@10.0.0.1:*) a=\"{}/${{a#root@10.0.0.1:}}\";; esac\n  set -- \"$@\" \"$a\"\ndone\n\
             exec {real} \"$@\"\n",
            pod.display()
        ),
    )
    .unwrap();
    Some(stub)
}

/// Live (2026-10-17): `pods backup` failed `[big]` on 9 pods every run, rsync exit 23 "could
/// not make way for new symlink: ARENA_3.0" — a directory in the backup where the pods now
/// have a symlink. Through the real pull and the real rsync: in both tiers the directory is
/// moved aside — into `.arena-olddir-<day>/`, its name kept (review b5 #1) — nothing deleted,
/// the rsync runs again, the pull succeeds, and the pod's line says so. The next pull, with
/// each tier's own filters, leaves the holding directories as they are and says nothing.
#[cfg(unix)]
#[tokio::test]
async fn a_directory_that_became_a_symlink_on_the_pod_is_moved_aside_and_the_pull_succeeds() {
    let pod = std::env::temp_dir().join(format!("arena-pull-pod-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&pod);
    let Some(stub) = local_pod_rsync("makeway", &pod) else {
        eprintln!("no rsync here: skipped");
        return;
    };
    let put = |p: &std::path::Path, text: &str| {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    };
    put(&pod.join("volume/ARENA_3.0/chapter1/new.py"), "today");
    put(&pod.join("notes.md"), "notes");
    std::os::unix::fs::symlink("volume/ARENA_3.0", pod.join("ARENA_3.0")).unwrap();
    let base = stub.0.join("backups");
    // What earlier pulls left: the directory, in the big tier and today's snapshot.
    put(&base.join("big/devtest-apple/ARENA_3.0/chapter1/old_work.py"), "yesterday's work");
    put(&base.join("w1d1/devtest-apple/ARENA_3.0/chapter1/old_work.py"), "this morning's work");
    let cfg = Config::parse(&format!("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\nBACKUP_REPO_PATH={REPO}\nSHARED_SSH_KEY_PATH=/nonexistent/k\n"));
    let chosen = picked(&fleet(), &cfg, &["apple"]).await;
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [repo_in_place()]);
    let remote: Arc<dyn Remote> = fake.clone();
    let _ = crate::pods::selection::take_said();
    handle_pull(&remote, &cfg, Some("w1d1".into()), &base.display().to_string(), None, None, false, false, &chosen, false, true, &stub.path())
        .await
        .unwrap();
    let said = crate::pods::selection::take_said();
    for (tier, dir, old) in [("snapshot", "w1d1", "this morning's work"), ("big", "big", "yesterday's work")] {
        let dest = base.join(dir).join("devtest-apple");
        assert_eq!(std::fs::read_link(dest.join("ARENA_3.0")).unwrap(), std::path::Path::new("volume/ARENA_3.0"), "{tier}");
        let aside: Vec<String> = std::fs::read_dir(&dest)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".arena-olddir-") || n.contains(".olddir-"))
            .map(|n| format!("{n}/ARENA_3.0"))
            .collect();
        assert_eq!(aside, [format!(".arena-olddir-{}/ARENA_3.0", arena_core::pull::aside_day(arena_core::snapshot::unix_now()))], "{tier}");
        assert_eq!(std::fs::read_to_string(dest.join(&aside[0]).join("chapter1/old_work.py")).unwrap(), old, "{tier}");
        assert_eq!(std::fs::read_to_string(dest.join("notes.md")).unwrap(), "notes", "{tier}");
        let line = said.iter().find(|l| l.contains(&format!("✓ devtest-apple [{tier}] ("))).unwrap_or_else(|| panic!("{tier}: {said:#?}"));
        assert!(
            line.ends_with(&format!(
                " — moved ARENA_3.0 aside to {} (a directory here, a symlink on the pod now; nothing deleted)",
                aside[0]
            )),
            "{line}"
        );
    }
    // Again: nothing in the way, the holding directories as they were, in both tiers.
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [repo_in_place()]);
    let remote: Arc<dyn Remote> = fake.clone();
    handle_pull(&remote, &cfg, Some("w1d1".into()), &base.display().to_string(), None, None, false, false, &chosen, false, true, &stub.path())
        .await
        .unwrap();
    let said = crate::pods::selection::take_said();
    assert!(!said.iter().any(|l| l.contains("moved")), "{said:#?}");
    for (dir, old) in [("w1d1", "this morning's work"), ("big", "yesterday's work")] {
        let held = base.join(dir).join("devtest-apple").join(format!(".arena-olddir-{}/ARENA_3.0/chapter1/old_work.py", arena_core::pull::aside_day(arena_core::snapshot::unix_now())));
        assert_eq!(std::fs::read_to_string(held).unwrap(), old, "{dir}");
    }
    let _ = std::fs::remove_dir_all(&pod);
}

/// rsync's stderr is the only source of the path it couldn't replace, and a pod can put words
/// there. A path outside the destination (`..`, absolute) or through a symlink in it is never
/// acted on: everything stays where it was, the rsync isn't run again, the job fails as it
/// would have, and its line says what was left alone and why.
#[cfg(unix)]
#[tokio::test]
async fn a_pull_never_moves_anything_rsync_names_outside_its_destination() {
    let stub = StubRsync::new("makeway-spoof");
    arena_core::remote::write_test_script(
        stub.0.join("rsync"),
        "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\necho \"$last\" >> \"$(dirname \"$0\")/calls.log\"\n\
         echo 'could not make way for new symlink: ../../outside/victim' >&2\n\
         echo 'could not make way for new symlink: linked/victim' >&2\n\
         echo 'could not make way for new regular file: /etc' >&2\nexit 23\n",
    )
    .unwrap();
    let base = stub.0.join("backups");
    let outside = stub.0.join("outside");
    std::fs::create_dir_all(outside.join("victim")).unwrap();
    std::fs::write(outside.join("victim/keep.txt"), "not ours").unwrap();
    std::fs::create_dir_all(base.join("big/devtest-apple")).unwrap();
    std::os::unix::fs::symlink(&outside, base.join("big/devtest-apple/linked")).unwrap();
    let cfg = Config::parse(&format!("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\nBACKUP_REPO_PATH={REPO}\nSHARED_SSH_KEY_PATH=/nonexistent/k\n"));
    let chosen = picked(&fleet(), &cfg, &["apple"]).await;
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [repo_in_place()]);
    let remote: Arc<dyn Remote> = fake.clone();
    let _ = crate::pods::selection::take_said();
    let err = handle_pull(&remote, &cfg, Some("w1d1".into()), &base.display().to_string(), None, None, false, false, &chosen, false, true, &stub.path())
        .await
        .unwrap_err();
    assert!(err.to_string().starts_with("2 rsync job(s) failed"), "{err}");
    assert_eq!(stub.take(), ["big/devtest-apple", "w1d1/devtest-apple"], "each job's first pass once: no retry");
    let names = |d: &std::path::Path| std::fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect::<Vec<_>>();
    assert_eq!(names(&outside), ["victim"]);
    assert_eq!(names(&outside.join("victim")), ["keep.txt"]);
    assert!(std::fs::symlink_metadata(base.join("big/devtest-apple/linked")).unwrap().file_type().is_symlink());
    let said = crate::pods::selection::take_said();
    let big = said.iter().find(|l| l.contains("devtest-apple [big]")).unwrap();
    assert!(
        big.ends_with(
            " — rsync couldn't replace, and left as it is: ../../outside/victim (not a path inside the destination), \
             linked/victim (linked on the way to it is a symlink here), /etc (not a path inside the destination)"
        ),
        "{big}"
    );
}

/// `set-branch` pushes each pod's branch first. One GitHub refuses (a secret the redaction
/// doesn't know) can't be: its files are copied into LOCAL_BACKUP_DIR, then it's switched
/// all the same — its commits stay in the local branch — and its line and the run's error
/// say so loudly. The pods whose push worked are switched with nothing copied.
#[tokio::test]
async fn set_branch_pushes_each_branch_first_and_copies_a_pod_whose_branch_cant_be_before_switching() {
    let stub = StubRsync::new("setbranch-fallback");
    let cfg = cfg_at(&stub);
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n"), FakeReply::ok()]);
    fake.script(&host(22002), [push_blocked("autocommit-devtest-w1d1-bloom"), repo_in_place(), FakeReply::ok()]);
    let err = handle_set_branch(fake.clone(), &cfg, "main", &everyone(&fleet()), true, false, true, &stub.path()).await.unwrap_err();
    assert_eq!(err.to_string(), "1 pod(s) switched with their branch copied, not pushed (see each one's line): devtest-bloom");
    let hard = checkout_command(REPO, "main", Some(KEY), true);
    let backup = |name: &str| (backup_command(REPO, Some(KEY), &format!("arena backup devtest-{name}"), None), Some(BACKUP_TIMEOUT));
    // bloom: its backup, the copy (asked where its repo is), then the switch — in that order.
    assert_eq!(
        execs(&fake, 22002),
        [backup("bloom"), (arena_core::volume::probe_command(REPO), Some(PROBE_TIMEOUT)), (hard.clone(), Some(BRANCH_TIMEOUT))]
    );
    for (name, port) in [("apple", 22001), ("cloud", 22003)] {
        assert_eq!(execs(&fake, port), [backup(name), (hard.clone(), Some(BRANCH_TIMEOUT))], "{name}");
    }
    let got = stub.take();
    assert!(got.len() == 3 && got.iter().all(|d| d.ends_with("/devtest-bloom")), "only bloom copied: {got:?}");

    // Its line: which branch, why, where its files went, and where its commits are now — in
    // the branch, or after `--hard` onto that same branch, pinned by a ref (codex #3).
    let u = Unpushed {
        pod: "devtest-bloom".into(),
        why: "GitHub push protection: OpenRouter API Key at ex.py:3".into(),
        branch: Some("autocommit-devtest-w1d1-bloom".into()),
        at_push: true,
        wrong_origin: false,
        busy: None,
    };
    let to = "/b/w1d1/devtest-bloom (and /b/big/devtest-bloom)";
    assert_eq!(
        not_pushed_note(&u, to, Some("main")),
        " — ⚠ NOT PUSHED FIRST: branch autocommit-devtest-w1d1-bloom (GitHub push protection: OpenRouter API Key at ex.py:3); \
         its files were copied to /b/w1d1/devtest-bloom (and /b/big/devtest-bloom) first, and its commits stay on the pod in \
         that branch (switching deletes no branch)"
    );
    assert!(
        not_pushed_note(&u, to, Some("autocommit-devtest-w1d1-bloom")).ends_with(
            "and the --hard reset moves autocommit-devtest-w1d1-bloom to origin's, and its unpushed commits stay on the pod \
             under refs/arena-discarded/<time>-autocommit-devtest-w1d1-bloom"
        ),
        "{}",
        not_pushed_note(&u, to, Some("autocommit-devtest-w1d1-bloom"))
    );
    assert_eq!(not_pushed_note(&u, to, None), not_pushed_note(&u, to, Some("main")));
    // What the switch did with the real keys of files git has redacted.
    assert_eq!(
        unredacted_note("RESTORED Grokking\nUNREDACTED_KEPT part4/exercises.py\nUNREDACTED_KEPT joep.py\n"),
        " — the real key(s) in 2 file(s) git has redacted are set aside in .git/arena-unredacted/ (a switch back puts them \
         back): part4/exercises.py, joep.py"
    );
    assert_eq!(unredacted_note("UNREDACTED_RESTORED joep.py\n"), " — real key(s) back in 1 file(s) git has redacted: joep.py");
    assert_eq!(unredacted_note("RESTORED Grokking\n"), "");
    // What init-branches left for the next backup to scan and push (FIX9B item 2).
    assert_eq!(
        unpushed_note("Switched to a new branch 'x'\nUNPUSHED 2 0123456789abcdef0123456789abcdef01234567\n"),
        " — 2 commit(s) origin doesn't have stay on it on the pod, unpushed (the branch went to origin at 0123456789, the \
         newest commit origin has): the next backup scans them for keys and pushes them"
    );
    assert_eq!(
        unpushed_note("UNPUSHED 3 none\n"),
        " — not pushed: origin has none of its 3 commit(s); the next backup scans them for keys and pushes the branch"
    );
    assert_eq!(unpushed_note("Switched to a new branch 'x'\nUNREDACTED_RESTORED a.py\n"), "");
}

/// A backup that found a merge in progress on the pod, its conflicts unresolved (a conflicted
/// `git pull upstream main`; exit 6): nothing committed.
fn busy_merge(branch: &str) -> FakeReply {
    FakeReply::exit(BACKUP_BUSY_EXIT, "").with_stdout(&format!("BRANCH {branch}\nBUSY {branch} merge unmerged\n"))
}

const MERGE_WHY: &str = "a merge is in progress on autocommit-b — not committed; resolve the conflicts and `git add` them, then \
                         `git commit` — or abort it (`git merge --abort`) on the pod";

/// Review R3: the 15-min backup committed a participant's conflicted `git pull upstream main`
/// — conflict markers and all, as a merge commit, pushed — and `git merge --abort` no longer
/// worked. Now that pod's backup stops before anything is staged (`BUSY`, exit 6): its line
/// says what is in progress and how to end it either way, it is copied by rsync in the same
/// run like a failed push (both tiers, even under --no-pull: its uncommitted work is nowhere
/// else), and the run fails. The other pods back up as always.
#[tokio::test]
async fn a_pod_mid_merge_is_left_as_it_is_said_how_to_end_it_and_copied() {
    // bloom's look at its merge (`resolve`) fails here: it is copied as before, never finished.
    let script = |fake: &FakeRemote| {
        fake.script(&host(22001), [FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n")]);
        fake.script(&host(22002), [busy_merge("autocommit-b"), FakeReply::exit(2, "sh: cd: no such dir"), repo_in_place()]);
        fake.script(&host(22003), [FakeReply::stdout("BRANCH autocommit-c\nNO_CHANGES autocommit-c\n")]);
    };
    // Each pod's line: bloom's says what is in progress and both ways to end it.
    let fake = Arc::new(FakeRemote::new());
    script(&fake);
    let remote: Arc<dyn Remote> = fake.clone();
    let jobs = targets(&fleet()).into_iter().map(|(n, t)| (n, t, "backup".to_string())).collect();
    let mut lines = Vec::new();
    let tally = backup_fleet(&remote, jobs, REPO, |l| lines.push(l.to_string()), |_| {}).await;
    assert_eq!((tally.pushed, tally.unchanged, tally.failed.len()), (1, 1, 1));
    let mut got = unnumbered(&lines);
    got.sort();
    assert_eq!(
        got,
        [
            "= devtest-cloud (no changes, on autocommit-c)".to_string(),
            "✓ devtest-apple -> autocommit-a".to_string(),
            "✗ devtest-bloom: a merge is in progress on autocommit-b — not committed (a backup now would commit the merge's \
             state, unresolved conflicts and all, and end the merge); resolve the conflicts and `git add` them, then `git \
             commit` — or abort it (`git merge --abort`) on the pod"
                .to_string(),
        ]
    );
    // Its record: what is in progress, failed at the push (so it's copied), said as what it is.
    let bloom = Unpushed {
        pod: "devtest-bloom".into(),
        why: MERGE_WHY.into(),
        branch: Some("autocommit-b".into()),
        at_push: true,
        wrong_origin: false,
        busy: Some("merge".into()),
    };
    assert_eq!(tally.unpushed, std::slice::from_ref(&bloom));

    // `pods backup --no-pull` (the cron's): bloom alone copied, both tiers; the run fails.
    let stub = StubRsync::new("busy");
    let fake = Arc::new(FakeRemote::new());
    script(&fake);
    let err = handle_full_backup(fake.clone(), &cfg_at(&stub), &everyone(&fleet()), true, None, false, true, &stub.path()).await.unwrap_err();
    assert_eq!(err.to_string(), "1 pod(s) failed to back up");
    let copied = stub.take();
    assert!(copied.len() == 3 && copied.iter().all(|d| d.ends_with("/devtest-bloom")), "only bloom, both tiers: {copied:?}");
    let bloom_calls = execs(&fake, 22002);
    assert_eq!(bloom_calls.len(), 3, "its backup, its look, its copy");
    assert_eq!(bloom_calls[1].0, arena_core::resolve::look_command(REPO), "bloom's look");
    assert_eq!(bloom_calls[2], (arena_core::volume::probe_command(REPO), Some(PROBE_TIMEOUT)), "bloom's copy");
    for port in [22001, 22003] {
        assert_eq!(execs(&fake, port).len(), 1, "{port}: its backup only");
    }
    // Its line after the copy: not "git push failed" — nothing was committed.
    let pulled: anyhow::Result<Pulled> =
        Ok(Pulled { failures: vec![], failed: vec![], snapshot_root: "/b/w41d5".into(), big_root: Some("/b/big".into()), whole: vec![] });
    assert_eq!(
        fallback_lines(std::slice::from_ref(&bloom), &pulled),
        [format!("✗ devtest-bloom: not backed up to git ({MERGE_WHY}) → files copied to /b/w41d5/devtest-bloom (and /b/big/devtest-bloom)")]
    );
    let refused: anyhow::Result<Pulled> = Err(anyhow::anyhow!("no space"));
    assert_eq!(
        fallback_lines(&[bloom], &refused),
        [format!("✗ devtest-bloom: not backed up to git ({MERGE_WHY}); the fallback copy failed too: no space")]
    );
}

/// `set-branch` backs up each pod's branch first; a pod mid-merge can't be (`BUSY`). Its files
/// are copied first, as for a failed push, and then the switch: `--hard` ends the merge with
/// the uncommitted changes it discards — said, with where the copy is and where on the pod the
/// switch saved them first (`DISCARDED_SAVED`: review ai-review claude #2) — while the
/// gentle switch and init-branches refuse to end it (their command's own refusal, on the pod's
/// line with the copy's note), and the run never calls that pod switched. A pod whose copy
/// failed too is not switched at all.
#[tokio::test]
async fn set_branch_copies_a_pod_mid_merge_first_and_its_line_names_the_merge() {
    let stub = StubRsync::new("busy-switch");
    let cfg = cfg_at(&stub);
    let backup = |name: &str| (backup_command(REPO, Some(KEY), &format!("arena backup devtest-{name}"), None), Some(BACKUP_TIMEOUT));
    let probe = (arena_core::volume::probe_command(REPO), Some(PROBE_TIMEOUT));
    let copied_first = " — ⚠ NOT COMMITTED FIRST: a merge is in progress on autocommit-b, so nothing was committed; its files were \
                        copied to ";
    let stays = "its commits stay on the pod in that branch (switching deletes no branch)";

    // --hard: copied, then switched; the line says the switch ended the merge. Switched like
    // the rest — not a failure (review fb2 F2) — and one line after the run names it.
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n"), FakeReply::ok()]);
    fake.script(&host(22002), [busy_merge("autocommit-b"), repo_in_place(), FakeReply::stdout(SAVED_BLOOM)]);
    let _ = crate::pods::selection::take_said();
    handle_set_branch(fake.clone(), &cfg, "main", &everyone(&fleet()), true, false, true, &stub.path()).await.unwrap();
    let hard = (checkout_command(REPO, "main", Some(KEY), true), Some(BRANCH_TIMEOUT));
    assert_eq!(execs(&fake, 22002), [backup("bloom"), probe.clone(), hard]);
    assert!(stub.take().iter().all(|d| d.ends_with("/devtest-bloom")));
    let said = unnumbered(&crate::pods::selection::take_said());
    assert!(said.contains(&ENDED_BLOOM.to_string()), "{said:#?}");
    let line = said.iter().find(|l| l.starts_with("✓ devtest-bloom (on main)")).unwrap_or_else(|| panic!("{said:#?}"));
    assert!(line.starts_with(&format!("✓ devtest-bloom (on main){copied_first}")), "{line}");
    assert!(
        line.ends_with(&format!(
            " first, and the --hard switch ends the merge and discards its uncommitted changes to tracked files; {stays} — what \
             it discarded was saved to .git/arena-discarded/20261019T090102Z/ on the pod first (its README says how to get it \
             back)"
        )),
        "{line}"
    );

    // Gentle, and init-branches: copied, then the switch refuses to end the merge — said on
    // the pod's line with the copy's note, and counted as a failed switch, never a switched one.
    let refusal = |who: &str| {
        format!(
            "{who}: a merge is in progress on autocommit-b — not switched (a switch now would end the merge); resolve the \
             conflicts and `git add` them, then `git commit` — or abort it (`git merge --abort`) on the pod, then switch again"
        )
    };
    for (who, init) in [("arena set-branch", false), ("arena init-branches", true)] {
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n"), FakeReply::ok()]);
        fake.script(&host(22002), [busy_merge("autocommit-b"), repo_in_place(), FakeReply::exit(BACKUP_BUSY_EXIT, &refusal(who))]);
        let err = if init {
            handle_init_branches(fake.clone(), &cfg, Some(1), Some(2), &everyone(&fleet()), false, true, &stub.path()).await
        } else {
            handle_set_branch(fake.clone(), &cfg, "main", &everyone(&fleet()), false, false, true, &stub.path()).await
        }
        .unwrap_err();
        let verb = if init { "init branch" } else { "switch branch" };
        assert_eq!(err.to_string(), format!("1 pod(s) failed to {verb}"), "{who}: never said to be switched");
        assert_eq!(execs(&fake, 22002).len(), 3, "{who}: backup, copy, switch");
        assert!(stub.take().iter().all(|d| d.ends_with("/devtest-bloom")), "{who}");
        let said = unnumbered(&crate::pods::selection::take_said());
        let line = said.iter().find(|l| l.starts_with("✗ devtest-bloom (exit 6): ")).unwrap_or_else(|| panic!("{who}: {said:#?}"));
        assert!(line.starts_with(&format!("✗ devtest-bloom (exit 6): {}{copied_first}", refusal(who))), "{line}");
        assert!(line.ends_with(&format!(" first, and {stays}")), "{line}");
    }

    // The copy fails too: not switched, its line says why both failed.
    let failing = StubRsync::new("busy-copyfail");
    arena_core::remote::write_test_script(
        failing.0.join("rsync"),
        "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\necho \"$last\" >> \"$(dirname \"$0\")/calls.log\"\n\
         echo 'rsync: No space left on device (28)' >&2; exit 11\n",
    )
    .unwrap();
    let cfg = cfg_at(&failing);
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22002), [busy_merge("autocommit-b"), repo_in_place()]);
    let err = handle_set_branch(fake.clone(), &cfg, "main", &everyone(&fleet_of(&[("bloom", 22002)])), true, false, true, &failing.path())
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "1 pod(s) NOT switched — each one's line says why: devtest-bloom");
    assert_eq!(execs(&fake, 22002), [backup("bloom"), probe], "never switched");
    let said = crate::pods::selection::take_said();
    let line = said.iter().find(|l| l.starts_with("✗ devtest-bloom not switched: ")).unwrap_or_else(|| panic!("{said:#?}"));
    assert!(
        line.starts_with(&format!(
            "✗ devtest-bloom not switched: its work couldn't be committed ({MERGE_WHY}) and copying its files into "
        )),
        "{line}"
    );
}

/// The line after a `--hard` run that ended bloom's merge after copying it.
const ENDED_BLOOM: &str = "⚠ 1 pod(s) had work no backup had saved (an operation in progress, or changes, commits or untracked \
                           files on a branch no backup commits), copied before the --hard switch discarded or overwrote it — what \
                           it discarded is also saved on the pod, in .git/arena-discarded/<time>/, which no later copy overwrites \
                           (see each one's line): devtest-bloom";

/// Review ai-review claude #2: the most a `--hard` save copies (`DISCARD_MAX_GB`, GB) reaches
/// the switch's command from the config; a value it can't read stops the run before anything
/// is backed up or switched. The gentle switch saves nothing, so never reads it.
#[tokio::test]
async fn hard_takes_its_save_limit_from_the_config_and_a_bad_one_stops_it_first() {
    let stub = StubRsync::new("discard-limit");
    let dir = stub.0.join("backups");
    std::fs::create_dir_all(&dir).unwrap();
    let text = |extra: &str| {
        format!(
            "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud)\nBACKUP_REPO_PATH={REPO}\n\
             GIT_SSH_KEY_REMOTE={KEY}\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nARENA_START_DATE=2026-01-05\n\
             LOCAL_BACKUP_DIR={}\n{extra}",
            dir.display()
        )
    };
    let cfg = |extra: &str| Config::parse(&text(extra));
    let apple = everyone(&fleet_of(&[("apple", 22001)]));
    let backup = (backup_command(REPO, Some(KEY), "arena backup devtest-apple", None), Some(BACKUP_TIMEOUT));
    let pushed = || [FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n"), FakeReply::ok()];
    for (extra, kb) in [("", 1024 * 1024), ("DISCARD_MAX_GB=0.5\n", 512 * 1024)] {
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), pushed());
        handle_set_branch(fake.clone(), &cfg(extra), "main", &apple, true, false, true, &stub.path()).await.unwrap();
        let hard = (checkout_command_limited(REPO, "main", Some(KEY), true, kb), Some(BRANCH_TIMEOUT));
        assert_eq!(execs(&fake, 22001), [backup.clone(), hard], "{extra:?}");
    }
    // From the environment alone, the file without it (Live 6, FIX6B item 5): the same.
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), pushed());
    let from_env = Config::parse_with_env(&text(""), |k| (k == "DISCARD_MAX_GB").then(|| "0.5".to_string()));
    handle_set_branch(fake.clone(), &from_env, "main", &apple, true, false, true, &stub.path()).await.unwrap();
    let hard = (checkout_command_limited(REPO, "main", Some(KEY), true, 512 * 1024), Some(BRANCH_TIMEOUT));
    assert_eq!(execs(&fake, 22001), [backup.clone(), hard]);
    let fake = Arc::new(FakeRemote::new());
    let err = handle_set_branch(fake.clone(), &cfg("DISCARD_MAX_GB=lots\n"), "main", &apple, true, false, true, &stub.path()).await.unwrap_err();
    assert_eq!(err.to_string(), "DISCARD_MAX_GB must be a number of GiB above 0, a fraction too, like 1 or 0.5 (got `lots`)");
    assert!(execs(&fake, 22001).is_empty(), "nothing backed up or switched");
    fake.script(&host(22001), pushed());
    handle_set_branch(fake.clone(), &cfg("DISCARD_MAX_GB=lots\n"), "main", &apple, false, false, true, &stub.path()).await.unwrap();
    assert_eq!(execs(&fake, 22001), [backup, (checkout_command(REPO, "main", Some(KEY), false), Some(BRANCH_TIMEOUT))]);
}

/// What a `--hard` switch prints when it saved what it discarded ([`checkout_command`]).
const SAVED_BLOOM: &str = "Switched to and reset branch 'main'\nDISCARDED_SAVED .git/arena-discarded/20261019T090102Z\n";

/// Review ai-review codex #3: the refs a `--hard` switch pinned the commits it left on no branch
/// with (`DISCARDED_PINNED`) are named on the pod's line, after where it saved the rest; with
/// none, no words for them.
#[test]
fn a_hard_switchs_line_names_the_refs_it_pinned() {
    assert_eq!(discarded_note("Switched to and reset branch 'main'\n"), "");
    let saved = " — what it discarded was saved to .git/arena-discarded/20261019T090102Z/ on the pod first (its README says how to get it back";
    assert_eq!(discarded_note(SAVED_BLOOM), format!("{saved})"));
    let pinned = "DISCARDED_SAVED .git/arena-discarded/20261019T090102Z\r\nDISCARDED_PINNED refs/arena-discarded/20261019T090102Z\r\n\
                  DISCARDED_PINNED refs/arena-discarded/20261019T090102Z-main\n";
    assert_eq!(
        discarded_note(pinned),
        format!("{saved}; its commits kept by refs/arena-discarded/20261019T090102Z, refs/arena-discarded/20261019T090102Z-main)")
    );
}

/// Review fb2 F2: the morning's `set-branch main --all --hard --init-branches` skipped
/// init-branches for the whole fleet when one pod was mid-merge — that pod was copied and
/// switched, yet counted as an error, so every pod stayed on main, where no backup commits.
/// Now it is switched like the rest (its line's ⚠ says what the copy holds) and the init goes
/// on, on every pod.
#[tokio::test]
async fn hard_init_branches_goes_on_past_a_pod_mid_merge_it_switched() {
    use super::resolve_week_day;
    use crate::pods::branch::SetBranchRun;
    use crate::pods::branch::set_branch_and_init;
    let stub = StubRsync::new("chain-busy");
    let cfg = cfg_at(&stub);
    let (week, day) = resolve_week_day(&cfg, None, None).unwrap();
    let bcfg = BackupConfig::from_config(&cfg, week, day);
    let fake = Arc::new(FakeRemote::new());
    let pushed = |b: &str| FakeReply::stdout(&format!("BRANCH {b}\nPUSHED {b}\n"));
    let on_main = || FakeReply::stdout("SKIP main\n");
    fake.script(&host(22001), [pushed("autocommit-a"), FakeReply::ok(), on_main(), FakeReply::ok()]);
    fake.script(&host(22002), [busy_merge("autocommit-b"), repo_in_place(), FakeReply::ok(), on_main(), FakeReply::ok()]);
    fake.script(&host(22003), [pushed("autocommit-c"), FakeReply::ok(), on_main(), FakeReply::ok()]);
    let _ = crate::pods::selection::take_said();
    let how = SetBranchRun { branch: "main", hard: true, dry_run: false, yes: true, init_branches: true };
    set_branch_and_init(fake.clone(), &cfg, &everyone(&fleet()), &how, &stub.path()).await.unwrap();
    let backup = |name: &str| (backup_command(REPO, Some(KEY), &format!("arena backup devtest-{name}"), None), Some(BACKUP_TIMEOUT));
    let hard = (checkout_command(REPO, "main", Some(KEY), true), Some(BRANCH_TIMEOUT));
    let init = |name: &str| (init_branch_command(&bcfg, &format!("devtest-{name}")), Some(BRANCH_TIMEOUT));
    let probe = (arena_core::volume::probe_command(REPO), Some(PROBE_TIMEOUT));
    assert_eq!(execs(&fake, 22001), [backup("apple"), hard.clone(), backup("apple"), init("apple")]);
    assert_eq!(execs(&fake, 22002), [backup("bloom"), probe, hard.clone(), backup("bloom"), init("bloom")], "bloom: copied, switched, init'd");
    assert_eq!(execs(&fake, 22003), [backup("cloud"), hard, backup("cloud"), init("cloud")]);
    assert!(stub.take().iter().all(|d| d.ends_with("/devtest-bloom")), "only bloom copied");
    let said = unnumbered(&crate::pods::selection::take_said());
    assert!(said.contains(&ENDED_BLOOM.to_string()), "{said:#?}");
    assert!(said.iter().any(|l| l.starts_with("✓ devtest-bloom (on main) — ⚠ NOT COMMITTED FIRST: a merge is in progress on autocommit-b")), "{said:#?}");
    for name in ["apple", "bloom", "cloud"] {
        let line = format!("✓ devtest-{name} -> {}", bcfg.branch_for(&format!("devtest-{name}")));
        assert!(said.contains(&line), "{line}: {said:#?}");
    }
}

/// The reverse of [`hard_init_branches_goes_on_past_a_pod_mid_merge_it_switched`]: a pod that
/// really wasn't switched — its copy failed, its `--hard` switch failed, or the gentle switch
/// refused its merge (a BUSY pod not switched is a failure, as before) — is left out of the
/// init, said in one line, and the run fails naming it. The pods that did switch are init'd
/// all the same (review pr4: one pod used to stop the init for the whole fleet). With none
/// switched, no init at all.
#[tokio::test]
async fn init_branches_leaves_out_a_pod_that_wasnt_switched_and_inits_the_rest() {
    use super::resolve_week_day;
    use crate::pods::branch::SetBranchRun;
    use crate::pods::branch::set_branch_and_init;
    let stub = StubRsync::new("chain-busy-fail");
    let failing = StubRsync::new("chain-busy-copyfail");
    arena_core::remote::write_test_script(
        failing.0.join("rsync"),
        "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\necho \"$last\" >> \"$(dirname \"$0\")/calls.log\"\n\
         echo 'rsync: No space left on device (28)' >&2; exit 11\n",
    )
    .unwrap();
    let pushed = || FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n");
    let refused = FakeReply::exit(BACKUP_BUSY_EXIT, "arena set-branch: a merge is in progress on autocommit-b — not switched");
    let left_out = "init-branches on the 1 pod(s) that switched — not on devtest-bloom, which didn't (see each one's line above)";
    // (case, hard, the rsync, bloom's replies after its backup, the run's error)
    let cases: Vec<(&str, bool, &StubRsync, Vec<FakeReply>, &str)> = vec![
        ("copy failed", true, &failing, vec![repo_in_place()], "1 pod(s) NOT switched — each one's line says why: devtest-bloom"),
        ("--hard failed", true, &stub, vec![repo_in_place(), FakeReply::exit(1, "fatal: couldn't find remote ref main")], "1 pod(s) failed to switch branch"),
        ("gentle refused", false, &stub, vec![repo_in_place(), refused], "1 pod(s) failed to switch branch"),
    ];
    for (case, hard, rsync, after, want) in cases {
        let cfg = cfg_at(rsync);
        let (week, day) = resolve_week_day(&cfg, None, None).unwrap();
        let apple_init = (init_branch_command(&BackupConfig::from_config(&cfg, week, day), "devtest-apple"), Some(BRANCH_TIMEOUT));
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [pushed(), FakeReply::ok(), FakeReply::stdout("SKIP main\n"), FakeReply::ok()]);
        fake.script(&host(22002), std::iter::once(busy_merge("autocommit-b")).chain(after).collect::<Vec<_>>());
        let _ = crate::pods::selection::take_said();
        let how = SetBranchRun { branch: "main", hard, dry_run: false, yes: true, init_branches: true };
        let err = set_branch_and_init(fake.clone(), &cfg, &everyone(&fleet_of(&[("apple", 22001), ("bloom", 22002)])), &how, &rsync.path())
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), want, "{case}");
        assert!(!execs(&fake, 22002).iter().any(|(c, _)| c.contains("git checkout -b")), "{case}: bloom init'd");
        let apple = execs(&fake, 22001);
        assert_eq!((apple.len(), apple.last()), (4, Some(&apple_init)), "{case}: apple switched, then init'd");
        let said = crate::pods::selection::take_said();
        assert!(said.iter().any(|l| l == left_out), "{case}: {said:#?}");
        assert!(!said.iter().any(|l| l.starts_with("⚠ ") && l.contains("had work no backup had saved")), "{case}");
    }
    // Nobody switched: no init at all, said.
    let cfg = cfg_at(&failing);
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22002), [busy_merge("autocommit-b"), repo_in_place()]);
    let how = SetBranchRun { branch: "main", hard: true, dry_run: false, yes: true, init_branches: true };
    let err = set_branch_and_init(fake.clone(), &cfg, &everyone(&fleet_of(&[("bloom", 22002)])), &how, &failing.path()).await.unwrap_err();
    assert_eq!(err.to_string(), "1 pod(s) NOT switched — each one's line says why: devtest-bloom");
    assert_eq!(execs(&fake, 22002).len(), 2, "its backup and the copy's probe, nothing more");
    assert!(crate::pods::selection::take_said().iter().any(|l| l == "init-branches NOT run: no pod switched"));
}

/// Review pr4: a pod whose push failed for an ordinary reason — the network, a non-fast-forward,
/// GitHub's push protection — was copied and switched like the rest, yet `set-branch main
/// --hard --init-branches` then skipped the init for the whole fleet, leaving every pod on main
/// where no backup commits (the */15 cron SKIPs them all). Now every pod that switched is
/// init'd, the copied one too; the run still fails after the init, naming that pod.
#[tokio::test]
async fn init_branches_runs_on_every_switched_pod_even_one_copied_after_a_failed_push() {
    use super::resolve_week_day;
    use crate::pods::branch::SetBranchRun;
    use crate::pods::branch::set_branch_and_init;
    let stub = StubRsync::new("chain-pushfail");
    let cfg = cfg_at(&stub);
    let (week, day) = resolve_week_day(&cfg, None, None).unwrap();
    let bcfg = BackupConfig::from_config(&cfg, week, day);
    let pushed = |b: &str| FakeReply::stdout(&format!("BRANCH {b}\nPUSHED {b}\n"));
    let on_main = || FakeReply::stdout("SKIP main\n");
    let push_failed = |stderr: &str| FakeReply::exit(1, stderr).with_stdout("BRANCH autocommit-b\nPUSH_FAILED\n");
    let backup = |name: &str| (backup_command(REPO, Some(KEY), &format!("arena backup devtest-{name}"), None), Some(BACKUP_TIMEOUT));
    let hard = (checkout_command(REPO, "main", Some(KEY), true), Some(BRANCH_TIMEOUT));
    let init = |name: &str| (init_branch_command(&bcfg, &format!("devtest-{name}")), Some(BRANCH_TIMEOUT));
    let probe = (arena_core::volume::probe_command(REPO), Some(PROBE_TIMEOUT));
    let failures = [
        ("network", push_failed("ssh: connect to host github.com port 22: Connection timed out\nfatal: Could not read from remote repository.")),
        ("non-fast-forward", push_failed(" ! [rejected]        autocommit-b -> autocommit-b (fetch first)\nerror: failed to push some refs")),
        ("push protection", push_blocked("autocommit-b")),
    ];
    for (case, failed) in failures {
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [pushed("autocommit-a"), FakeReply::ok(), on_main(), FakeReply::ok()]);
        fake.script(&host(22002), [failed, repo_in_place(), FakeReply::ok(), on_main(), FakeReply::ok()]);
        fake.script(&host(22003), [pushed("autocommit-c"), FakeReply::ok(), on_main(), FakeReply::ok()]);
        let _ = crate::pods::selection::take_said();
        let how = SetBranchRun { branch: "main", hard: true, dry_run: false, yes: true, init_branches: true };
        let err = set_branch_and_init(fake.clone(), &cfg, &everyone(&fleet()), &how, &stub.path()).await.unwrap_err();
        assert_eq!(err.to_string(), "1 pod(s) switched with their branch copied, not pushed (see each one's line): devtest-bloom", "{case}");
        assert_eq!(execs(&fake, 22002), [backup("bloom"), probe.clone(), hard.clone(), backup("bloom"), init("bloom")], "{case}");
        for (name, port) in [("apple", 22001), ("cloud", 22003)] {
            assert_eq!(execs(&fake, port), [backup(name), hard.clone(), backup(name), init(name)], "{case}: {name}");
        }
        assert!(stub.take().iter().all(|d| d.ends_with("/devtest-bloom")), "{case}: only bloom copied");
        let said = unnumbered(&crate::pods::selection::take_said());
        assert!(said.iter().any(|l| l.starts_with("✓ devtest-bloom (on main) — ⚠ NOT PUSHED FIRST: branch autocommit-b (")), "{case}: {said:#?}");
        for name in ["apple", "bloom", "cloud"] {
            let line = format!("✓ devtest-{name} -> {}", bcfg.branch_for(&format!("devtest-{name}")));
            assert!(said.contains(&line), "{case}: {line}: {said:#?}");
        }
        assert!(!said.iter().any(|l| l.starts_with("init-branches")), "{case}: nobody left out");
    }
    // An init that fails too: both said, set-branch's first.
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22002), [push_blocked("autocommit-b"), repo_in_place(), FakeReply::ok(), on_main(), FakeReply::exit(1, "fatal: the remote end hung up")]);
    let how = SetBranchRun { branch: "main", hard: true, dry_run: false, yes: true, init_branches: true };
    let err = set_branch_and_init(fake.clone(), &cfg, &everyone(&fleet_of(&[("bloom", 22002)])), &how, &stub.path()).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "1 pod(s) switched with their branch copied, not pushed (see each one's line): devtest-bloom; then init-branches: 1 \
         pod(s) failed to init branch"
    );
}

/// Review fb2 F3: a pod on main — where no backup commits — with a merge in progress or
/// changes to tracked files was reset by `set-branch --hard` with no copy (its backup said
/// only SKIP). Now its backup says so (`UNCOMMITTED`) and `--hard` copies it first, then
/// switches it — its line saying what the copy holds, not a failure — and doesn't reset it if
/// the copy fails. A clean pod on main isn't copied; the gentle switch and `pods backup`
/// copy none of them, as before.
#[tokio::test]
async fn hard_copies_a_pod_on_main_with_uncommitted_work_before_resetting_it() {
    let stub = StubRsync::new("main-dirty");
    let cfg = cfg_at(&stub);
    let backup = |name: &str| (backup_command(REPO, Some(KEY), &format!("arena backup devtest-{name}"), None), Some(BACKUP_TIMEOUT));
    let probe = (arena_core::volume::probe_command(REPO), Some(PROBE_TIMEOUT));
    let dirty = || FakeReply::stdout("UNCOMMITTED main changes\nSKIP main\n");
    let merging = || FakeReply::stdout("UNCOMMITTED main merge unmerged\nSKIP main\n");
    let clean = || FakeReply::stdout("SKIP main\n");
    let script = |fake: &FakeRemote| {
        fake.script(&host(22001), [dirty(), repo_in_place(), FakeReply::stdout(SAVED_BLOOM)]);
        fake.script(&host(22002), [clean(), FakeReply::ok()]);
        fake.script(&host(22003), [merging(), repo_in_place(), FakeReply::ok()]);
    };
    let fake = Arc::new(FakeRemote::new());
    script(&fake);
    let _ = crate::pods::selection::take_said();
    handle_set_branch(fake.clone(), &cfg, "main", &everyone(&fleet()), true, false, true, &stub.path()).await.unwrap();
    let hard = (checkout_command(REPO, "main", Some(KEY), true), Some(BRANCH_TIMEOUT));
    assert_eq!(execs(&fake, 22001), [backup("apple"), probe.clone(), hard.clone()]);
    assert_eq!(execs(&fake, 22002), [backup("bloom"), hard.clone()], "clean: not copied");
    assert_eq!(execs(&fake, 22003), [backup("cloud"), probe.clone(), hard.clone()]);
    let copied = stub.take();
    assert!(!copied.is_empty() && copied.iter().all(|d| d.ends_with("/devtest-apple") || d.ends_with("/devtest-cloud")), "{copied:?}");
    let said = unnumbered(&crate::pods::selection::take_said());
    let line = |pod: &str| said.iter().find(|l| l.starts_with(&format!("✓ {pod} (on main)"))).cloned().unwrap_or_else(|| panic!("{said:#?}"));
    let reflog = "the --hard reset moves main to origin's, and its unpushed commits stay on the pod under refs/arena-discarded/<time>-main";
    let apple = line("devtest-apple");
    assert!(
        apple.starts_with(
            "✓ devtest-apple (on main) — ⚠ NOT COMMITTED FIRST: main has uncommitted changes to tracked files, and no backup \
             commits on main; its files were copied to "
        ) && apple.ends_with(&format!(
            " first, and the --hard switch discards them; {reflog} — what it discarded was saved to \
             .git/arena-discarded/20261019T090102Z/ on the pod first (its README says how to get it back)"
        )),
        "{apple}"
    );
    let cloud = line("devtest-cloud");
    assert!(
        cloud.starts_with("✓ devtest-cloud (on main) — ⚠ NOT COMMITTED FIRST: a merge is in progress on main, and no backup commits on main; ")
            && cloud.contains(" first, and the --hard switch ends the merge and discards its uncommitted changes to tracked files"),
        "{cloud}"
    );
    assert_eq!(line("devtest-bloom"), "✓ devtest-bloom (on main)");
    assert!(said.iter().any(|l| l.starts_with("⚠ 2 pod(s) had work no backup had saved") && l.ends_with(": devtest-apple, devtest-cloud")), "{said:#?}");

    // The copy fails: not reset.
    let failing = StubRsync::new("main-dirty-copyfail");
    arena_core::remote::write_test_script(
        failing.0.join("rsync"),
        "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\necho \"$last\" >> \"$(dirname \"$0\")/calls.log\"\n\
         echo 'rsync: No space left on device (28)' >&2; exit 11\n",
    )
    .unwrap();
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [dirty(), repo_in_place(), FakeReply::ok()]);
    let err = handle_set_branch(fake.clone(), &cfg_at(&failing), "main", &everyone(&fleet_of(&[("apple", 22001)])), true, false, true, &failing.path())
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "1 pod(s) NOT switched — each one's line says why: devtest-apple");
    assert_eq!(execs(&fake, 22001), [backup("apple"), probe], "never reset");
    let said = crate::pods::selection::take_said();
    assert!(
        said.iter().any(|l| l.starts_with(
            "✗ devtest-apple not switched: its work couldn't be committed (main has uncommitted changes to tracked files — no \
             backup commits on main) and copying its files into "
        )),
        "{said:#?}"
    );

    // Gentle: nothing discarded, nothing copied.
    let fake = Arc::new(FakeRemote::new());
    script(&fake);
    handle_set_branch(fake.clone(), &cfg, "main", &everyone(&fleet()), false, false, true, &stub.path()).await.unwrap();
    let gentle = (checkout_command(REPO, "main", Some(KEY), false), Some(BRANCH_TIMEOUT));
    for (name, port) in [("apple", 22001), ("bloom", 22002), ("cloud", 22003)] {
        assert_eq!(execs(&fake, port), [backup(name), gentle.clone()], "{name}");
    }
    assert!(stub.take().is_empty());

    // `pods backup --no-pull` (the cron's): skipped as always — no copy, no failure.
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [dirty()]);
    fake.script(&host(22003), [merging()]);
    fake.script(&host(22002), [clean()]);
    handle_full_backup(fake.clone(), &cfg, &everyone(&fleet()), true, None, false, true, &stub.path()).await.unwrap();
    assert!(stub.take().is_empty());
    for port in [22001, 22002, 22003] {
        assert_eq!(execs(&fake, port).len(), 1, "{port}: its backup only");
    }
}

/// Review pr4: on a branch no backup commits, commits no remote has and untracked files —
/// `UNCOMMITTED main commits`, `UNCOMMITTED HEAD untracked` — are copied before `--hard` as
/// changes are, `.git` and all, each pod's line saying what the copy holds (the commits are
/// pushed nowhere, so NOT PUSHED; an untracked file is overwritten where origin's branch tracks
/// it); switched, not failures. A clean pod isn't copied, and the gentle switch copies none.
#[tokio::test]
async fn hard_copies_a_pod_on_main_with_commits_or_untracked_files_first() {
    let stub = StubRsync::new("main-commits");
    let cfg = cfg_at(&stub);
    let backup = |name: &str| (backup_command(REPO, Some(KEY), &format!("arena backup devtest-{name}"), None), Some(BACKUP_TIMEOUT));
    let probe = (arena_core::volume::probe_command(REPO), Some(PROBE_TIMEOUT));
    let script = |fake: &FakeRemote| {
        fake.script(&host(22001), [FakeReply::stdout("UNCOMMITTED main commits\nSKIP main\n"), repo_in_place(), FakeReply::ok()]);
        fake.script(&host(22002), [FakeReply::stdout("SKIP main\n"), FakeReply::ok()]);
        fake.script(&host(22003), [FakeReply::stdout("UNCOMMITTED HEAD untracked\nSKIP HEAD\n"), repo_in_place(), FakeReply::ok()]);
    };
    let fake = Arc::new(FakeRemote::new());
    script(&fake);
    let _ = crate::pods::selection::take_said();
    handle_set_branch(fake.clone(), &cfg, "main", &everyone(&fleet()), true, false, true, &stub.path()).await.unwrap();
    let hard = (checkout_command(REPO, "main", Some(KEY), true), Some(BRANCH_TIMEOUT));
    assert_eq!(execs(&fake, 22001), [backup("apple"), probe.clone(), hard.clone()]);
    assert_eq!(execs(&fake, 22002), [backup("bloom"), hard.clone()], "clean: not copied");
    assert_eq!(execs(&fake, 22003), [backup("cloud"), probe, hard]);
    let copied = stub.take();
    assert!(!copied.is_empty() && copied.iter().all(|d| d.ends_with("/devtest-apple") || d.ends_with("/devtest-cloud")), "{copied:?}");
    let said = unnumbered(&crate::pods::selection::take_said());
    let line = |pod: &str| said.iter().find(|l| l.starts_with(&format!("✓ {pod} (on main)"))).cloned().unwrap_or_else(|| panic!("{said:#?}"));
    let apple = line("devtest-apple");
    assert!(
        apple.starts_with(
            "✓ devtest-apple (on main) — ⚠ NOT PUSHED FIRST: main has commits no remote has, and no backup pushes main; its \
             files were copied to "
        ) && apple.ends_with(
            " first, and that copy has its .git, those commits with it; the --hard reset moves main to origin's, and its \
             unpushed commits stay on the pod under refs/arena-discarded/<time>-main"
        ),
        "{apple}"
    );
    let cloud = line("devtest-cloud");
    assert!(
        cloud.starts_with(
            "✓ devtest-cloud (on main) — ⚠ NOT COMMITTED FIRST: a detached HEAD has untracked files, and no backup commits on \
             a detached HEAD; its files were copied to "
        ) && cloud.ends_with(
            " first, and the --hard switch overwrites any of them origin/main tracks; the commits of its detached HEAD stay \
             on the pod under refs/arena-discarded/<time>"
        ),
        "{cloud}"
    );
    assert_eq!(line("devtest-bloom"), "✓ devtest-bloom (on main)");
    assert!(said.iter().any(|l| l.starts_with("⚠ 2 pod(s) had work no backup had saved") && l.ends_with(": devtest-apple, devtest-cloud")), "{said:#?}");
    // Gentle: nothing discarded or overwritten (git refuses to), nothing copied.
    let fake = Arc::new(FakeRemote::new());
    script(&fake);
    handle_set_branch(fake.clone(), &cfg, "main", &everyone(&fleet()), false, false, true, &stub.path()).await.unwrap();
    let gentle = (checkout_command(REPO, "main", Some(KEY), false), Some(BRANCH_TIMEOUT));
    for (name, port) in [("apple", 22001), ("bloom", 22002), ("cloud", 22003)] {
        assert_eq!(execs(&fake, port), [backup(name), gentle.clone()], "{name}");
    }
    assert!(stub.take().is_empty());
}

/// A [`Remote`] that, at every call, notes whether the pod's backup lock was held right then
/// (a second open of its lock file can't take it), then answers as `inner` scripts it.
struct LockProbe {
    inner: Arc<FakeRemote>,
    dir: String,
    /// `(port, the lock was held)` per exec, in order.
    seen: std::sync::Mutex<Vec<(u16, bool)>>,
}

impl LockProbe {
    fn held(&self, port: u16) -> Vec<bool> {
        self.seen.lock().unwrap().iter().filter(|(p, _)| *p == port).map(|(_, h)| *h).collect()
    }
}

#[async_trait]
impl Remote for LockProbe {
    async fn exec(&self, t: &SshTarget, cmd: &str, timeout: Option<Duration>) -> Result<arena_core::ssh::SshOutput> {
        let pod = fleet().pods.into_iter().find(|p| p.ssh_port == Some(t.port)).map(|p| p.name).unwrap();
        let f = arena_core::pull::open_pod_lock(&self.dir, &pod).unwrap();
        let free = arena_core::pull::try_lock(f).unwrap().is_ok();
        self.seen.lock().unwrap().push((t.port, !free));
        self.inner.exec(t, cmd, timeout).await
    }
    async fn copy(&self, t: &SshTarget, l: &str, r: &str, timeout: Option<Duration>) -> Result<arena_core::ssh::SshOutput> {
        self.inner.copy(t, l, r, timeout).await
    }
    async fn copy_recursive(&self, t: &SshTarget, l: &str, r: &str, timeout: Option<Duration>) -> Result<arena_core::ssh::SshOutput> {
        self.inner.copy_recursive(t, l, r, timeout).await
    }
}

/// Review (high): a pod was switched — even with `--hard` — when its push failed and its copy
/// failed too, and the switch ran with no lock held. Now each pod's lock is held from its
/// backup through its switch (seen at every SSH call: the backup, the copy's question, the
/// switch), and a pod with neither a push nor a copy is not switched at all, said, and the
/// run fails. One whose backup failed before the push (no repo) isn't copied, so isn't
/// switched either.
#[tokio::test]
async fn set_branch_never_switches_a_pod_with_neither_a_push_nor_a_copy_and_holds_each_lock_through_its_switch() {
    let failing = StubRsync::new("setbranch-copyfail");
    arena_core::remote::write_test_script(
        failing.0.join("rsync"),
        "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\necho \"$last\" >> \"$(dirname \"$0\")/calls.log\"\n\
         case \"$last\" in *devtest-bloom*) echo 'rsync: No space left on device (28)' >&2; exit 11;; esac\n",
    )
    .unwrap();
    let base = failing.0.join("backups");
    let cfg = backup_cfg(&base);
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n"), FakeReply::ok()]);
    fake.script(
        &host(22002),
        [FakeReply::exit(1, "error: failed to push some refs").with_stdout("BRANCH autocommit-b\nPUSH_FAILED\n"), repo_in_place(), FakeReply::ok()],
    );
    fake.script(&host(22003), [FakeReply::exit(2, "sh: 1: cd: can't cd to /root/ARENA_materials"), FakeReply::ok()]);
    let probe = Arc::new(LockProbe { inner: fake.clone(), dir: base.display().to_string(), seen: Default::default() });
    let err = handle_set_branch(probe.clone(), &cfg, "main", &everyone(&fleet()), true, false, true, &failing.path()).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.starts_with("2 pod(s) NOT switched — each one's line says why: "), "{msg}");
    assert!(msg.contains("devtest-bloom") && msg.contains("devtest-cloud") && !msg.contains("devtest-apple"), "{msg}");
    let hard = checkout_command(REPO, "main", Some(KEY), true);
    let backup = |name: &str| (backup_command(REPO, Some(KEY), &format!("arena backup devtest-{name}"), None), Some(BACKUP_TIMEOUT));
    assert_eq!(execs(&fake, 22001), [backup("apple"), (hard.clone(), Some(BRANCH_TIMEOUT))]);
    // bloom: backed up, its copy tried (and failed) — never switched.
    assert_eq!(execs(&fake, 22002), [backup("bloom"), (arena_core::volume::probe_command(REPO), Some(PROBE_TIMEOUT))]);
    // cloud: no repo — nothing to copy, never switched.
    assert_eq!(execs(&fake, 22003), [backup("cloud")]);
    // The lock, held at every call — the backup, the copy's question, the switch — and free after.
    assert_eq!(probe.held(22001), [true, true]);
    assert_eq!(probe.held(22002), [true, true]);
    assert_eq!(probe.held(22003), [true]);
    for pod in ["devtest-apple", "devtest-bloom", "devtest-cloud"] {
        let f = arena_core::pull::open_pod_lock(&base.display().to_string(), pod).unwrap();
        assert!(crate::pods::lock::lock_settles_free(f), "{pod}'s lock still held after the run");
    }
}

/// Review (high): a pod whose lock another backup held for the whole wait was switched anyway,
/// unlocked, alongside that backup. Now it isn't touched at all: no backup, no switch, said,
/// and the run fails. The others go ahead.
#[tokio::test(start_paused = true)]
async fn set_branch_never_switches_a_pod_another_backup_holds() {
    let dir = std::env::temp_dir().join(format!("arena-setbranch-busy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let cfg = backup_cfg(&dir);
    let d = dir.display().to_string();
    let held = arena_core::pull::try_lock(arena_core::pull::open_pod_lock(&d, "devtest-bloom").unwrap()).unwrap().expect("free");
    let fake = Arc::new(FakeRemote::new());
    let err = handle_set_branch(fake.clone(), &cfg, "main", &everyone(&fleet()), true, false, true, "rsync").await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "1 pod(s) NOT switched — each one's line says why: devtest-bloom"
    );
    assert!(fake.calls_to(&host(22002)).is_empty(), "bloom touched: {:?}", fake.calls_to(&host(22002)));
    let hard = checkout_command(REPO, "main", Some(KEY), true);
    for (name, port) in [("apple", 22001), ("cloud", 22003)] {
        let backup = backup_command(REPO, Some(KEY), &format!("arena backup devtest-{name}"), None);
        assert_eq!(execs(&fake, port), [(backup, Some(BACKUP_TIMEOUT)), (hard.clone(), Some(BRANCH_TIMEOUT))], "{name}");
    }
    drop(held);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Live (bsec B1): with the legacy relative `./backup`, set-branch took no per-pod lock (by
/// design: RR5) — yet its dry run said each pod is switched "under the pod's backup lock". Now
/// the dry run, and the run itself before its question, say in one line that no locks are
/// taken and how to get them, whenever [`take_pod_locks`] takes none; with an absolute dir
/// that exists, the lock claim stands and that line is never said. The locking is unchanged.
#[tokio::test(start_paused = true)]
async fn a_switch_without_per_pod_locks_says_so_and_never_claims_them() {
    let dir = std::env::temp_dir().join(format!("arena-switch-locks-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let missing = dir.join("not-made-yet").display().to_string();
    let relative = "(no per-pod backup locks: LOCAL_BACKUP_DIR (./backup) is relative — set an absolute path to keep other \
                    backups out during the switch)"
        .to_string();
    let cases = [
        (cfg(), Some(relative.clone())),
        (
            Config::parse(&format!("MACHINE_NAME_PREFIX=devtest\nBACKUP_REPO_PATH={REPO}\nLOCAL_BACKUP_DIR={missing}\n")),
            Some(format!(
                "(no per-pod backup locks: LOCAL_BACKUP_DIR ({missing}) doesn't exist — create it to keep other backups out \
                 during the switch)"
            )),
        ),
        (backup_cfg(&dir), None),
    ];
    let names = vec!["devtest-apple".to_string()];
    for (cfg, line) in &cases {
        let note = push_first_note(cfg, false);
        let dir = local_backup_dir(cfg);
        match line {
            Some(line) => {
                assert!(note.ends_with(&format!(")\n{line}")), "{note}");
                assert!(!note.contains("backup lock, held") && !note.contains("whose lock"), "{note}");
            }
            None => {
                assert!(note.contains("keys redacted — under the pod's backup lock, held until its switch ends; a pod whose push"), "{note}");
                assert!(note.contains("neither pushed nor copied, or whose lock another backup holds, is NOT switched"), "{note}");
                assert!(!note.contains("no per-pod backup locks"), "{note}");
            }
        }
        // What's said is what's done.
        let locks = take_pod_locks(&dir, &names, Duration::ZERO).await;
        assert_eq!(locks.holds("devtest-apple"), line.is_none(), "{dir}");
        drop(locks);
    }
    assert!(!std::path::Path::new(&missing).exists(), "the locks never make the dir");

    // The run: the line once, before the question — set-branch and init-branches alike.
    take_said();
    handle_set_branch(Arc::new(FakeRemote::new()), &cfg(), "main", &everyone(&fleet()), true, false, true, "rsync").await.unwrap();
    handle_init_branches(Arc::new(FakeRemote::new()), &cfg(), Some(1), Some(2), &everyone(&fleet()), false, true, "rsync").await.unwrap();
    let said = take_said();
    assert_eq!(said.iter().filter(|l| **l == relative).count(), 2, "{said:#?}");
    let locked = backup_cfg(&dir);
    handle_set_branch(Arc::new(FakeRemote::new()), &locked, "main", &everyone(&fleet()), true, false, true, "rsync").await.unwrap();
    handle_init_branches(Arc::new(FakeRemote::new()), &locked, Some(1), Some(2), &everyone(&fleet()), false, true, "rsync").await.unwrap();
    let said = take_said();
    assert!(!said.iter().any(|l| l.contains("no per-pod backup locks")), "{said:#?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Review F5: the dry run said every pod whose push fails "is copied … and then switched" — a
/// pod mid-merge is copied, then the gentle switch (and init-branches) refuses to end the
/// merge, while `--hard` ends it, discarding its uncommitted changes to tracked files only.
/// Each note says which.
#[test]
fn the_dry_runs_note_says_what_becomes_of_a_pod_mid_merge() {
    let gentle = push_first_note(&cfg(), false);
    assert!(
        gentle.contains(
            "; a pod with a merge (cherry-pick, revert, `git am`, rebase, conflicts) in progress can't be committed: it is \
             copied into ./backup too, and then NOT switched: the switch refuses to end it; a pod that can be neither pushed \
             nor copied"
        ),
        "{gentle}"
    );
    let hard = push_first_note(&cfg(), true);
    assert!(
        hard.contains(
            "it is copied into ./backup too, and the --hard switch then ends it, discarding its uncommitted changes to tracked \
             files; so is a pod on main/master or a detached HEAD, which no backup commits, with an operation in progress, \
             changes to tracked files, commits no remote has or untracked files (the --hard switch overwrites any the new \
             branch tracks); on every pod, what the --hard reset discards is saved there first, in \
             .git/arena-discarded/<time>/; "
        ),
        "{hard}"
    );
    assert!(!hard.contains("refuses") && !hard.contains("untracked files stay"), "{hard}");
}

#[tokio::test(start_paused = true)]
async fn set_branch_dry_run_runs_nothing_and_apply_runs_the_checkout_on_each_pod() {
    let fake = Arc::new(FakeRemote::new());
    handle_set_branch(fake.clone(), &cfg(), "main", &everyone(&fleet()), false, true, true, "rsync").await.unwrap();
    assert!(fake.calls().is_empty(), "dry run: {:?}", fake.calls());

    // Gentle, whole fleet: each pod's branch backed up (pushed) first, then the ff-only
    // checkout, each within its budget.
    handle_set_branch(fake.clone(), &cfg(), "main", &everyone(&fleet()), false, false, true, "rsync").await.unwrap();
    let gentle = checkout_command(REPO, "main", Some(KEY), false);
    for (name, port) in [("apple", 22001), ("bloom", 22002), ("cloud", 22003)] {
        let backup = backup_command(REPO, Some(KEY), &format!("arena backup devtest-{name}"), None);
        assert_eq!(execs(&fake, port), [(backup, Some(BACKUP_TIMEOUT)), (gentle.clone(), Some(BRANCH_TIMEOUT))], "port {port}");
    }

    // --hard on one pod: only that pod, the destructive command (after its backup).
    let fake = Arc::new(FakeRemote::new());
    handle_pods(pods(&["set-branch", "main", "bloom", "--hard"]), &fleet(), fake.clone(), &cfg(), true).await.unwrap();
    let hard = checkout_command(REPO, "main", Some(KEY), true);
    assert!(hard.contains("reset --hard"));
    assert_eq!(fake.calls().len(), 2);
    assert_eq!(execs(&fake, 22002)[1], (hard, Some(BRANCH_TIMEOUT)));

    // Concurrent: a wedged pod costs the run one budget (not one per pod after it), fails it.
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [FakeReply::ok(), FakeReply::hang()]);
    let start = Instant::now();
    let err =
        handle_set_branch(fake.clone(), &cfg(), "main", &everyone(&fleet()), false, false, true, "rsync").await.unwrap_err();
    assert_eq!(err.to_string(), "1 pod(s) failed to switch branch");
    assert_eq!(start.elapsed(), BRANCH_TIMEOUT);
    assert_eq!(fake.calls().len(), 6);
}

#[tokio::test(start_paused = true)]
async fn init_branches_runs_each_pods_branch_command_within_the_budget() {
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22003), [FakeReply::stdout("SKIP main\n"), FakeReply::exit(1, "error: failed to push some refs")]);
    let err =
        handle_init_branches(fake.clone(), &cfg(), Some(1), Some(2), &everyone(&fleet()), false, true, "rsync").await.unwrap_err();
    assert_eq!(err.to_string(), "1 pod(s) failed to init branch");
    let bcfg = BackupConfig::from_config(&cfg(), 1, 2);
    for (name, port) in [("apple", 22001), ("bloom", 22002), ("cloud", 22003)] {
        let want = init_branch_command(&bcfg, &format!("devtest-{name}"));
        assert!(want.contains(&format!("autocommit-devtest-w1d2-{name}")));
        // The branch it's on pushed first (a no-op on main), then the new day's branch.
        let backup = backup_command(REPO, Some(KEY), &format!("arena backup devtest-{name}"), None);
        assert_eq!(execs(&fake, port), [(backup, Some(BACKUP_TIMEOUT)), (want, Some(BRANCH_TIMEOUT))]);
    }
    let fake = Arc::new(FakeRemote::new());
    handle_init_branches(fake.clone(), &cfg(), Some(1), Some(2), &everyone(&fleet()), true, true, "rsync").await.unwrap();
    assert!(fake.calls().is_empty(), "dry run");
}

/// Live (bsec O1): init-branches onto a day branch that already exists said nothing of the
/// keys set aside on it. Its command now puts them back ([`init_branch_command`]); its line
/// says so, as set-branch's does.
#[tokio::test(start_paused = true)]
async fn init_branches_says_which_set_aside_keys_came_back() {
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [FakeReply::stdout("SKIP main\n"), FakeReply::stdout("Switched to branch 'x'\nUNREDACTED_RESTORED ex/solutions.py\n")]);
    take_said();
    handle_init_branches(fake.clone(), &cfg(), Some(1), Some(2), &everyone(&fleet_of(&[("apple", 22001)])), false, true, "rsync").await.unwrap();
    let said = take_said();
    assert!(
        said.iter().any(|l| l
            == "[1/1] ✓ devtest-apple -> autocommit-devtest-w1d2-apple — real key(s) back in 1 file(s) git has redacted: \
                ex/solutions.py"),
        "{said:#?}"
    );
    assert!(init_branch_command(&BackupConfig::from_config(&cfg(), 1, 2), "devtest-apple").contains("unredacted_restore "));
}

/// Review claude #2 (FIX9B item 2): init-branches no longer pushes commits origin doesn't have
/// (no backup has scanned them). Its line says so, and that the next backup scans and pushes
/// them — the run still succeeds: the day's branch is made and on origin.
#[tokio::test(start_paused = true)]
async fn init_branches_says_what_it_left_for_the_next_backup() {
    let fake = Arc::new(FakeRemote::new());
    let at = "89abcdef0123456789abcdef0123456789abcdef";
    fake.script(&host(22001), [FakeReply::stdout("SKIP main\n"), FakeReply::stdout(&format!("Switched to a new branch 'x'\nUNPUSHED 2 {at}\n"))]);
    take_said();
    handle_init_branches(fake.clone(), &cfg(), Some(1), Some(2), &everyone(&fleet_of(&[("apple", 22001)])), false, true, "rsync").await.unwrap();
    let said = take_said();
    assert!(
        said.iter().any(|l| l
            == "[1/1] ✓ devtest-apple -> autocommit-devtest-w1d2-apple — 2 commit(s) origin doesn't have stay on it on the pod, \
                unpushed (the branch went to origin at 89abcdef01, the newest commit origin has): the next backup scans them for \
                keys and pushes them"),
        "{said:#?}"
    );
}

/// A local file to copy, removed on drop.
struct TmpFile(std::path::PathBuf);
impl Drop for TmpFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
        let _ = std::fs::remove_file(&self.0);
    }
}

#[tokio::test(start_paused = true)]
async fn cp_runs_mkdir_then_copy_then_check_and_stops_a_pod_at_its_first_failure() {
    let file = TmpFile(std::env::temp_dir().join(format!("arena-cp-test-{}.txt", std::process::id())));
    std::fs::write(&file.0, "hello").unwrap();
    let local = file.0.to_string_lossy().into_owned();
    let f = fleet_of(&[("apple", 22001), ("bloom", 22002), ("cloud", 22003), ("delta", 22004)]);
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [FakeReply::ok(), FakeReply::ok(), FakeReply::stdout("OK 5\n")]);
    fake.script(&host(22002), [FakeReply::exit(1, "mkdir: cannot create directory '/root/x': Permission denied")]);
    fake.script(&host(22003), [FakeReply::ok(), FakeReply::exit(1, "scp: /root/x/hello.txt: No space left on device")]);
    fake.script(&host(22004), [FakeReply::ok(), FakeReply::hang()]);
    let start = Instant::now();
    let err = handle_copy(fake.clone(), &cfg(), &file.0, Some("/root/x/hello.txt"), false, &everyone(&f), None, false, true)
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "3 pod(s) failed to receive the file");
    // 5 bytes to 4 pods: the base budget.
    assert_eq!(start.elapsed(), CP_BASE_TIMEOUT, "the stuck scp costs its budget, nothing more");

    let mkdir = RemoteCall::Exec { host: host(22001), cmd: "mkdir -p '/root/x'".into(), timeout: Some(PROBE_TIMEOUT) };
    let copy = |port| RemoteCall::Copy {
        host: host(port),
        local: local.clone(),
        remote: "/root/x/hello.txt".into(),
        recursive: false,
        timeout: Some(CP_BASE_TIMEOUT),
    };
    let apple = fake.calls_to(&host(22001));
    assert_eq!(apple[..2], [mkdir, copy(22001)]);
    assert!(
        matches!(&apple[2], RemoteCall::Exec { cmd, timeout, .. } if cmd.contains("wc -c") && *timeout == Some(PROBE_TIMEOUT)),
        "{apple:?}"
    );
    assert_eq!(apple.len(), 3);
    assert_eq!(fake.calls_to(&host(22002)).len(), 1, "a failed mkdir stops the pod before the copy");
    assert_eq!(fake.calls_to(&host(22003)).len(), 2, "a failed copy is not size-checked");
    assert_eq!(fake.calls_to(&host(22004))[1], copy(22004));
    assert_eq!(fake.calls_to(&host(22004)).len(), 2);

    // `-r`: the tree goes through copy_recursive, with no single-file size check (and
    // `-t` scopes it to one pod).
    let fake = Arc::new(FakeRemote::new());
    let local = file.0.to_str().unwrap();
    handle_pods(pods(&["cp", "-r", "-t", "apple", local, "/root/x/"]), &f, fake.clone(), &cfg(), true).await.unwrap();
    assert!(matches!(
        &fake.calls()[..],
        [RemoteCall::Exec { cmd, .. }, RemoteCall::Copy { recursive: true, remote, .. }] if cmd == "mkdir -p '/root/x'" && remote == "/root/x/"
    ), "{:?}", fake.calls());
}

#[tokio::test(start_paused = true)]
async fn cp_says_why_a_pod_failed() {
    let plan = CopyPlan {
        local: "/l/hello.txt".into(),
        remote: "/root/x/hello.txt".into(),
        remote_parent: "/root/x".into(),
        recursive: false,
        expect_size: Some(5),
        basename: "hello.txt".into(),
        timeout: CP_BASE_TIMEOUT,
    };
    let t = SshTarget::from_pod(&pod("apple", 22001), &cfg()).unwrap();
    let fake = FakeRemote::new();
    fake.script(&host(22001), [FakeReply::ok(), FakeReply::hang()]);
    assert_eq!(copy_to_pod(&fake, &t, &plan).await, Err("timed out after 600s".to_string()));
    fake.script(&host(22001), [FakeReply::exit(1, "Permission denied")]);
    assert_eq!(copy_to_pod(&fake, &t, &plan).await, Err("mkdir failed: Permission denied".to_string()));
    fake.script(&host(22001), [FakeReply::ok(), FakeReply::ok(), FakeReply::stdout("OK 3")]);
    assert!(copy_to_pod(&fake, &t, &plan).await.unwrap_err().starts_with("size mismatch after copy: 3B on pod vs 5B"));
    fake.script(&host(22001), [FakeReply::ok(), FakeReply::ok(), FakeReply::stdout("MISPLACED")]);
    assert!(copy_to_pod(&fake, &t, &plan).await.unwrap_err().contains("is a directory on the pod"));
    // A check that can't run (here: wedged) doesn't override a successful scp.
    fake.script(&host(22001), [FakeReply::ok(), FakeReply::ok(), FakeReply::hang()]);
    assert_eq!(copy_to_pod(&fake, &t, &plan).await, Ok(()));
}

#[test]
fn cp_budget_scales_with_bytes_times_pods() {
    let base = CP_BASE_TIMEOUT.as_secs();
    let cases: &[(u64, usize, u64)] = &[
        (0, 1, base),
        (5, 4, base),
        (999_999, 1, base),
        (1_000_000, 1, base + 1),
        // The regression: 500 MB to 30 pods (~750s at 20 MB/s) no longer dies at 600s.
        (500_000_000, 30, base + 15_000),
        // Bounded at a day, never overflowing.
        (u64::MAX, 1000, 86_400),
    ];
    for &(bytes, pods, want) in cases {
        assert_eq!(cp_timeout(bytes, pods), Duration::from_secs(want), "{bytes}B × {pods}");
    }
}

#[test]
fn local_size_is_a_file_or_a_trees_total() {
    let dir = TmpFile(std::env::temp_dir().join(format!("arena-cp-size-{}", std::process::id())));
    std::fs::create_dir_all(dir.0.join("sub/deeper")).unwrap();
    std::fs::write(dir.0.join("a.txt"), vec![0u8; 100]).unwrap();
    std::fs::write(dir.0.join("sub/b.bin"), vec![0u8; 2000]).unwrap();
    std::fs::write(dir.0.join("sub/deeper/c"), vec![0u8; 30]).unwrap();
    assert_eq!(local_size(&dir.0.join("a.txt")), 100);
    assert_eq!(local_size(&dir.0), 2130);
    assert_eq!(local_size(&dir.0.join("missing")), 0);
    #[cfg(unix)]
    {
        // A symlinked dir isn't followed (a loop can't hang it); a symlinked file counts.
        std::os::unix::fs::symlink(&dir.0, dir.0.join("sub/loop")).unwrap();
        std::os::unix::fs::symlink(dir.0.join("a.txt"), dir.0.join("sub/a-link")).unwrap();
        assert_eq!(local_size(&dir.0), 2230);
    }
}

#[tokio::test(start_paused = true)]
async fn cp_budget_follows_the_size_or_the_timeout_flag() {
    // 3 MB to 2 pods: base + 6s by default.
    let file = TmpFile(std::env::temp_dir().join(format!("arena-cp-budget-{}.bin", std::process::id())));
    std::fs::write(&file.0, vec![0u8; 3_000_000]).unwrap();
    let f = fleet_of(&[("apple", 22001), ("bloom", 22002)]);
    let copy_timeouts = |fake: &FakeRemote| -> Vec<Option<Duration>> {
        fake.calls()
            .into_iter()
            .filter_map(|c| match c {
                RemoteCall::Copy { timeout, .. } => Some(timeout),
                _ => None,
            })
            .collect()
    };
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [FakeReply::ok(), FakeReply::hang()]);
    fake.script(&host(22002), [FakeReply::ok(), FakeReply::ok(), FakeReply::stdout("OK 3000000")]);
    let start = Instant::now();
    let err = handle_copy(fake.clone(), &cfg(), &file.0, Some("/root/x.bin"), false, &everyone(&f), None, false, true)
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "1 pod(s) failed to receive the file");
    let want = CP_BASE_TIMEOUT + Duration::from_secs(6);
    assert_eq!(start.elapsed(), want);
    assert_eq!(copy_timeouts(&fake), [Some(want), Some(want)]);

    // `--timeout` overrides it.
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [FakeReply::ok(), FakeReply::hang()]);
    let start = Instant::now();
    let flag = Some(Duration::from_secs(30));
    handle_copy(fake.clone(), &cfg(), &file.0, Some("/root/x.bin"), false, &everyone(&f), flag, false, true)
        .await
        .unwrap_err();
    assert_eq!(start.elapsed(), Duration::from_secs(30));
    assert_eq!(copy_timeouts(&fake), [flag, flag]);
}

#[test]
fn cp_timeout_flag_parses_within_range() {
    let parse = |args: &[&str]| Cli::try_parse_from(args).map(|c| c.cmd);
    match parse(&["arena", "pods", "cp", "--timeout", "3600", "big.tar", "/root/"]).unwrap() {
        Cmd::Pods(PodCmd::Cp { timeout, .. }) => assert_eq!(timeout, Some(3600)),
        _ => panic!("not pods cp"),
    }
    assert!(matches!(parse(&["arena", "pods", "cp", "f"]).unwrap(), Cmd::Pods(PodCmd::Cp { timeout: None, .. })));
    for bad in ["0", "86401", "ten"] {
        assert!(parse(&["arena", "pods", "cp", "--timeout", bad, "f"]).is_err(), "{bad}");
    }
}

#[tokio::test(start_paused = true)]
async fn list_probe_overrides_the_gpu_where_a_pod_answers_and_a_wedged_pod_costs_only_its_budget() {
    let fake = Arc::new(FakeRemote::new());
    let smi = format!(
        "NVIDIA RTX A5000, 3, 10, 24564, 40\nNVIDIA RTX A5000, 0, 10, 24564, 39\n{}\n",
        arena_core::metrics::SENTINEL
    );
    fake.script(&host(22001), [FakeReply::stdout(&smi)]);
    fake.script(&host(22002), [FakeReply::hang()]);
    let remote: Arc<dyn Remote> = fake.clone();
    let mut pods = fleet().pods;
    let start = Instant::now();
    probe_gpus(&remote, &cfg(), &mut pods).await;
    assert_eq!(start.elapsed(), PROBE_TIMEOUT);
    let gpu = |i: usize| (pods[i].gpu_type.clone().unwrap(), pods[i].gpu_count.unwrap());
    assert_eq!(gpu(0), ("2×RTX A5000".to_string(), 2), "what nvidia-smi saw wins");
    assert_eq!(gpu(1), ("RTX A4000".to_string(), 1), "no answer: the provider's GPU stays");
    assert_eq!(gpu(2), ("RTX A4000".to_string(), 1), "no GPU rows: the provider's GPU stays");
    assert!(fake.calls().iter().all(|c| matches!(c, RemoteCall::Exec { timeout, .. } if *timeout == Some(PROBE_TIMEOUT))));
}

/// Finding #24: under MIG nvidia-smi names the parent card (`NVIDIA B300 SXM6 AC`), so
/// the table showed `1×B300 SXM6 AC`; a pod the provider lists as a MIG slice keeps the
/// slice's name (the count still comes from the probe).
#[tokio::test]
async fn list_probe_keeps_a_mig_slices_name() {
    let fake = Arc::new(FakeRemote::new());
    let smi = format!("NVIDIA B300 SXM6 AC, 0, 16, 31232, 43\n{}\n", arena_core::metrics::SENTINEL);
    fake.script(&host(22001), [FakeReply::stdout(&smi)]);
    let remote: Arc<dyn Remote> = fake.clone();
    let mut mig = pod("apple", 22001);
    mig.gpu_type = Some("B300 MIG 34GB".into());
    let mut pods = vec![mig];
    probe_gpus(&remote, &cfg(), &mut pods).await;
    assert_eq!((pods[0].gpu_type.as_deref(), pods[0].gpu_count), (Some("B300 MIG 34GB"), Some(1)));
    assert_eq!(arena_core::fleet::gpu_label(&pods[0]), "1×B300 MIG 34GB");
}

/// Two pods with one name (e.g. a half-finished replace) each keep their own GPU: the
/// probe used to key results by name, so one pod's readout landed on both rows.
#[tokio::test]
async fn list_probe_keeps_each_pods_gpu_when_two_pods_share_a_name() {
    let fake = Arc::new(FakeRemote::new());
    let smi = |gpu: &str| format!("{gpu}, 3, 10, 24564, 40\n{}\n", arena_core::metrics::SENTINEL);
    fake.script(&host(22001), [FakeReply::stdout(&smi("NVIDIA RTX A5000"))]);
    fake.script(&host(22002), [FakeReply::stdout(&smi("NVIDIA GeForce RTX 3070"))]);
    let remote: Arc<dyn Remote> = fake.clone();
    let mut twin = pod("apple", 22002);
    twin.id = "id-devtest-apple-2".into();
    let mut pods = vec![pod("apple", 22001), twin];
    probe_gpus(&remote, &cfg(), &mut pods).await;
    assert_eq!(pods[0].gpu_type.as_deref(), Some("1×RTX A5000"));
    assert_eq!(pods[1].gpu_type.as_deref(), Some("1×RTX 3070"));
}

#[tokio::test(start_paused = true)]
async fn replace_identity_and_marker_probes_fail_closed_and_are_bounded() {
    let f = fleet_of(&[("apple", 22001), ("apple-new", 22009)]);
    let t = SshTarget::from_pod(&f.pods[0], &cfg()).unwrap();
    let fake = FakeRemote::new();
    fake.script(&host(22001), [FakeReply::stdout("id-devtest-apple\n"), FakeReply::stdout("id-someone-else\n")]);
    assert!(target_is_pod(&fake, &t, "id-devtest-apple", &f).await);
    assert!(!target_is_pod(&fake, &t, "id-devtest-apple", &f).await, "a recycled ip:port reaching another pod");
    fake.script(&host(22001), [FakeReply::hang()]);
    let start = Instant::now();
    assert!(!target_is_pod(&fake, &t, "id-devtest-apple", &f).await, "couldn't confirm = not the pod");
    assert_eq!(start.elapsed(), PROBE_TIMEOUT);

    let dest = "id-devtest-apple-new";
    fake.script(
        &host(22009),
        [
            FakeReply::stdout(&format!("ID={dest}\nMARK=arena-replace-{dest}\n")),
            FakeReply::stdout(&format!("ID=\nMARK=arena-replace-{dest}\n")),
            FakeReply::hang(),
        ],
    );
    assert!(marker_present(&f, &fake, dest, &cfg()).await);
    assert!(!marker_present(&f, &fake, dest, &cfg()).await, "RunPod without a pod id fails closed");
    let start = Instant::now();
    assert!(!marker_present(&f, &fake, dest, &cfg()).await);
    assert_eq!(start.elapsed(), PROBE_TIMEOUT);
    assert!(fake.calls().iter().all(|c| matches!(c, RemoteCall::Exec { timeout, .. } if *timeout == Some(PROBE_TIMEOUT))));
}

/// The via-local copy (live findings #13 and #30), driven through `copy_pod_files` with a
/// stand-in rsync: two pods behind one public IP skip the direct copy (it hangs on the
/// hairpin) instead of losing minutes to it; a direct copy that fails says its own reason
/// and falls back; the staging copy — a participant's whole home on a shared box — is
/// private, REMOVED once the copy landed, KEPT (with where and how big) when it didn't so
/// a re-run continues, and never started when the disk can't take it.
#[tokio::test]
async fn the_via_local_copy_cleans_up_its_staging_and_never_fills_the_disk() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!("arena-via-local-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    // A stand-in rsync: a dry run reports `size`; a pull writes a file into the stage; a
    // push (to `user@host:`) exits `push`.
    let stub = |name: &str, size: &str, push: u8| {
        let path = root.join(name);
        arena_core::remote::write_test_script(
            &path,
            format!(
                "#!/bin/sh\ndry=; for a in \"$@\"; do last=$a; [ \"$a\" = --dry-run ] && dry=1; done\n\
                 if [ -n \"$dry\" ]; then printf 'Number of files: 3\\nTotal transferred file size: {size} bytes\\n'; exit 0; fi\n\
                 case \"$last\" in\n  *@*) [ {push} = 0 ] || echo 'rsync: connection unexpectedly closed' >&2; exit {push};;\n\
                 \x20 *) mkdir -p \"$last\" && printf 'work\\n' > \"$last/work.py\";;\nesac\n"
            ),
        )
        .unwrap();
        path.display().to_string()
    };
    let (src, dest) = ("id-devtest-apple", "id-devtest-apple-new");
    let fake_for = |dest_host: &str| {
        let fake = FakeRemote::new();
        // (every step on either pod carries its identity check: the specific answers first)
        fake.on(&host(22001), "arena-replica-sent", FakeReply::stdout("arena-replica-sent=yes\n"));
        fake.on(&host(22001), "rsync", FakeReply::exit(255, "ssh: connect to host 10.0.0.2 port 22002: Connection timed out"));
        fake.on(&host(22001), "RUNPOD_POD_ID", FakeReply::stdout(src));
        fake.on(dest_host, "arena-replica-ready", FakeReply::stdout("arena-replica-ready\n"));
        fake.on(dest_host, "MARK=", FakeReply::stdout(&format!("ID={dest}\nMARK=arena-replace-{dest}\n")));
        fake
    };
    let opts = |rsync: String| crate::pods::home_copy::CopyOpts { fresh: false, rsync, stage_root: root.clone() };
    let stage = root.join(format!("arena-replace-{src}"));

    // Same public IP: no direct attempt; the copy lands and the stage is gone.
    let f = fleet_of(&[("apple", 22001), ("apple-new", 22002)]);
    let fake = fake_for(&host(22002));
    copy_pod_files(&cfg(), &f, &fake, src, dest, &opts(stub("ok", "1,000", 0))).await.unwrap();
    assert!(!execs(&fake, 22001).iter().any(|(c, _)| c.contains("rsync ")), "no direct copy through a shared IP");
    assert!(!stage.exists(), "the staging copy is removed once the copy landed");

    // Its own IP: the direct copy is tried, fails, falls back; the push fails — the stage
    // is kept, private, and the error says where it is and how big.
    let mut f = fleet_of(&[("apple", 22001), ("apple-new", 22002)]);
    f.pods[1].ssh_ip = Some("10.0.0.2".into());
    let fake = fake_for("10.0.0.2:22002");
    let e = copy_pod_files(&cfg(), &f, &fake, src, dest, &opts(stub("push-fails", "1,000", 12))).await.unwrap_err();
    let msg = format!("{e:#}");
    assert!(execs(&fake, 22001).iter().filter(|(c, _)| c.contains("rsync ")).count() == 1, "one direct attempt");
    assert!(msg.contains(&format!("kept at {}", stage.display())) && msg.contains("rm -rf"), "{msg}");
    assert!(msg.contains("connection unexpectedly closed"), "{msg}");
    assert!(stage.join("work.py").exists());
    assert_eq!(std::fs::metadata(&stage).unwrap().permissions().mode() & 0o777, 0o700, "a participant's home: private");
    std::fs::remove_dir_all(&stage).unwrap();

    // More than the disk can spare (the one free-space judge, CONTROL_MIN_FREE_GB — here 0:
    // only the measured copy must fit): refused before anything is staged.
    let fake = fake_for("10.0.0.2:22002");
    let judged = Config::parse(&format!("{}CONTROL_MIN_FREE_GB=0\n", cfg_text()));
    let e = copy_pod_files(&judged, &f, &fake, src, dest, &opts(stub("huge", "999.00P", 0))).await.unwrap_err();
    let msg = format!("{e:#}");
    assert!(msg.contains(&format!("not copying via local staging ({}): skipped: ", root.display())), "{msg}");
    assert!(msg.contains("GB, measured) would leave less than the floor (0 GB, CONTROL_MIN_FREE_GB) — nothing copied; free at least "), "{msg}");
    // How much to free and where, what's there (arena's biggest dirs, if any), and another
    // filesystem to stage on: none has room for ~999 PB.
    let room = |msg: &str| msg.contains("(arena's biggest dirs there: ") || msg.contains("(`arena config check` names arena's biggest dirs there)");
    assert!(room(&msg), "{msg}");
    let anywhere = "; or stage it on another filesystem for this run: `TMPDIR=<a dir on a disk with room> arena …`";
    assert!(msg.ends_with(anywhere), "{msg}");
    assert!(!stage.exists(), "nothing staged, nothing left behind");
    // A box already under the floor: refused before the home is sized (an rsync dry run over
    // all of it, for nothing) or anything is made — saying how far below the floor it is.
    let fake = fake_for("10.0.0.2:22002");
    let full = Config::parse(&format!("{}CONTROL_MIN_FREE_GB=100000\n", cfg_text()));
    let ran = root.join("rsync-ran");
    let never = root.join("never");
    arena_core::remote::write_test_script(&never, format!("#!/bin/sh\ntouch {}\nexit 1\n", ran.display())).unwrap();
    let e = copy_pod_files(&full, &f, &fake, src, dest, &opts(never.display().to_string())).await.unwrap_err();
    let msg = format!("{e:#}");
    assert!(msg.contains("skipped: only ") && msg.contains("(floor 100000 GB, CONTROL_MIN_FREE_GB) — nothing copied; free at least "), "{msg}");
    assert!(room(&msg) && msg.ends_with(anywhere), "{msg}");
    assert!(!ran.exists(), "no rsync at all: nothing sized");
    assert!(!stage.exists());
    // A floor that can't be read fails the copy before anything is asked or copied (replace
    // and migrate refuse it before they create anything: `disk::floor_refusal`).
    let bad = Config::parse(&format!("{}CONTROL_MIN_FREE_GB=2.5\n", cfg_text()));
    let fake = fake_for("10.0.0.2:22002");
    let e = copy_pod_files(&bad, &f, &fake, src, dest, &opts(stub("bad", "1,000", 0))).await.unwrap_err();
    assert!(format!("{e:#}").contains("CONTROL_MIN_FREE_GB must be a whole number of GB"), "{e:#}");
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    // A stage another run holds (a second replace/migrate of the same pod): refused, and that
    // run's stage left exactly as it is.
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::write(stage.join("theirs.py"), "x").unwrap();
    let held = arena_core::disk::lock_dir(&stage).unwrap();
    let fake = fake_for("10.0.0.2:22002");
    let e = copy_pod_files(&cfg(), &f, &fake, src, dest, &opts(stub("held", "1,000", 0))).await.unwrap_err();
    assert!(format!("{e:#}").contains("another arena run is copying id-devtest-apple through"), "{e:#}");
    assert!(stage.join("theirs.py").exists() && !stage.join("work.py").exists());
    // An earlier attempt's stage is kept when a later copy lands directly — it may hold what
    // the source has lost since — and stays visible (`config check`, `teardown --check`).
    drop(held);
    let fake = FakeRemote::new();
    fake.on(&host(22001), "arena-replica-sent", FakeReply::stdout("arena-replica-sent=yes\n"));
    fake.on(&host(22001), "RUNPOD_POD_ID", FakeReply::stdout(src));
    fake.on("10.0.0.2:22002", "arena-replica-ready", FakeReply::stdout("arena-replica-ready\n"));
    fake.on("10.0.0.2:22002", "MARK=", FakeReply::stdout(&format!("ID={dest}\nMARK=arena-replace-{dest}\n")));
    copy_pod_files(&cfg(), &f, &fake, src, dest, &opts(stub("direct", "1,000", 0))).await.unwrap();
    assert!(execs(&fake, 22001).iter().any(|(c, _)| c.contains("rsync ")), "copied directly");
    assert!(stage.join("theirs.py").exists(), "an earlier attempt's stage is never removed by a later copy");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(start_paused = true)]
async fn a_direct_pod_copy_that_times_out_stops_instead_of_falling_back() {
    let mut f = fleet_of(&[("apple", 22001), ("apple-new", 22002)]);
    f.pods[1].ssh_ip = Some("10.0.0.2".into()); // its own host: the direct copy is tried
    let fake = FakeRemote::new();
    // identity check, plant the marker (+ the source's sync record), the direct rsync
    // (wedged), clean the marker.
    fake.script(&host(22001), [FakeReply::stdout("id-devtest-apple"), FakeReply::stdout("arena-replica-sent=yes\n"), FakeReply::hang()]);
    fake.on("10.0.0.2:22002", "arena-replica-ready", FakeReply::stdout("arena-replica-ready\n"));
    let start = Instant::now();
    let err = copy_pod_files(&cfg(), &f, &fake, "id-devtest-apple", "id-devtest-apple-new", &crate::pods::home_copy::CopyOpts::new(false))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.starts_with("direct pod-to-pod copy timed out after 7200s — NOT swapping"), "{msg}");
    assert_eq!(start.elapsed(), POD_COPY_TIMEOUT);
    let src = execs(&fake, 22001);
    assert_eq!(src.len(), 4, "{src:?}");
    assert!(src[2].0.contains("\nrsync ") && src[2].1 == Some(POD_COPY_TIMEOUT), "{src:?}");
    assert!(src[2].0.starts_with("if ! [ ") && src[2].0.contains("= 'id-devtest-apple' ]"), "only on the source itself: {src:?}");
    // The repo goes as its tree (a link onto a volume on either pod is resolved, not copied).
    assert!(src[2].0.contains("'/ARENA_materials'") && src[2].0.contains("'root@10.0.0.2:ARENA_materials/'"), "{src:?}");
    assert!(src[3].0.contains("rm -f ") && !src[3].0.contains("sent-to") && src[3].1 == Some(PROBE_TIMEOUT), "source marker cleaned, nothing recorded: {src:?}");
    // On the destination: no via-local push, no delivery check — the prepare, and the
    // tidy-up of what the partial sync moved aside, which keeps the old baseline.
    let dest = fake.calls_to("10.0.0.2:22002");
    assert!(
        matches!(&dest[..], [RemoteCall::Exec { cmd: prep, .. }, RemoteCall::Exec { cmd, .. }]
            if prep.contains("arena-replica-ready") && cmd.contains("arena-replica-kept") && cmd.contains("del=0") && !cmd.contains("touch")),
        "{dest:?}"
    );
    // It doesn't promise a plain re-run continues: `replace` refuses while `-new` exists.
    assert!(!msg.contains("Re-run"), "{msg}");
}

/// Review finding (critical): the original was reset since `-new`'s last sync — its home
/// is the image again — and the re-sync mirrored that over `-new`, deleting the
/// participant's files there (`migrate cutover` runs it by default). The source's record
/// of the sync went with its home (or its container is newer than the stamp): the sync is
/// refused before ANY transfer, `-new` is never tidied, and the error says what's left.
#[tokio::test]
async fn a_resync_from_a_reset_original_is_refused_before_anything_moves() {
    let (src, dest) = ("id-devtest-apple", "id-devtest-apple-new");
    let mut f = fleet_of(&[("apple", 22001), ("apple-new", 22002)]);
    f.pods[1].ssh_ip = Some("10.0.0.2".into());
    let dest_host = "10.0.0.2:22002";
    for (source, why) in [
        ("arena-replica-sent=no\narena-replica-container=1791000000\n", "has no record of devtest-apple-new's last sync"),
        ("arena-replica-sent=yes\narena-replica-container=1791500000\n", "was reset since devtest-apple-new's last sync"),
    ] {
        let fake = FakeRemote::new();
        fake.on(&host(22001), "arena-replica-sent", FakeReply::stdout(source));
        fake.on(&host(22001), "RUNPOD_POD_ID", FakeReply::stdout(src));
        fake.on(dest_host, "arena-replica-ready", FakeReply::stdout("arena-last-sync=1791457323\narena-replica-ready\n"));
        let e = copy_pod_files(&cfg(), &f, &fake, src, dest, &crate::pods::home_copy::CopyOpts::new(false)).await.unwrap_err().to_string();
        assert!(e.starts_with("NOT syncing devtest-apple -> devtest-apple-new") && e.contains(why), "{e}");
        assert!(e.contains("--no-final-sync") && e.contains("Nothing was copied"), "{e}");
        let on_src = execs(&fake, 22001);
        assert!(!on_src.iter().any(|(c, _)| c.contains("rsync ")), "no transfer: {on_src:?}");
        assert!(on_src.last().is_some_and(|(c, _)| c.contains("rm -f") && !c.contains("sent-to")), "marker cleaned, nothing recorded: {on_src:?}");
        let on_dest = fake.calls_to(dest_host);
        assert!(
            matches!(&on_dest[..], [RemoteCall::Exec { cmd, .. }] if cmd.contains("arena-replica-ready")),
            "only the prepare on -new — no tidy-up: {on_dest:?}"
        );
    }
    // A fresh -new (made by this command) has nothing to lose: no record needed.
    let fake = FakeRemote::new();
    fake.on(&host(22001), "arena-replica-sent", FakeReply::stdout("arena-replica-sent=no\n"));
    fake.on(&host(22001), "RUNPOD_POD_ID", FakeReply::stdout(src));
    fake.on(dest_host, "arena-replica-ready", FakeReply::stdout("arena-last-sync=1791457323\narena-replica-ready\n"));
    fake.on(dest_host, "MARK=", FakeReply::stdout(&format!("ID={dest}\nMARK=arena-replace-{dest}\n")));
    copy_pod_files(&cfg(), &f, &fake, src, dest, &crate::pods::home_copy::CopyOpts::new(true)).await.unwrap();
    assert!(execs(&fake, 22001).iter().any(|(c, _)| c.contains("\nrsync ") && c.contains("--delete")));
}

/// Live R: a failed direct copy printed rsync's `code 255` summary, not why — the ssh line
/// above it (here a missing pod-to-pod key) is the reason.
#[test]
fn a_failed_copy_names_the_ssh_error_not_rsyncs_summary() {
    let epilogue = "rsync: connection unexpectedly closed (0 bytes received so far) [sender]\n\
                    rsync error: unexplained error (code 255) at io.c(232) [sender=3.2.7]\n";
    for (stderr, want) in [
        (format!("Permission denied (publickey).\r\n{epilogue}"), "Permission denied (publickey)."),
        (format!("Warning: Identity file /root/.ssh/id_ed25519 not accessible: No such file or directory.\nPermission denied (publickey).\n{epilogue}"), "Permission denied (publickey)."),
        (format!("ssh: connect to host 10.0.0.2 port 22002: Connection timed out\n{epilogue}"), "ssh: connect to host 10.0.0.2 port 22002: Connection timed out"),
        (format!("Warning: Permanently added '10.0.0.2' (ED25519) to the list of known hosts.\nHost key verification failed.\n{epilogue}"), "Host key verification failed."),
        // rsync's own complaint is the reason when it is one…
        ("rsync: [receiver] mkdir \"/root/x\" failed: No space left on device (28)\nrsync error: error in file IO (code 11) at main.c(791)\n".into(), "rsync: [receiver] mkdir \"/root/x\" failed: No space left on device (28)"),
        // …and with nothing but the epilogue (and the known-hosts notice), it's what there is.
        (format!("Warning: Permanently added '10.0.0.2' (ED25519) to the list of known hosts.\n{epilogue}"), "rsync error: unexplained error (code 255) at io.c(232) [sender=3.2.7]"),
        (String::new(), "no error output"),
    ] {
        assert_eq!(copy_failure_reason(&stderr), want, "{stderr:?}");
    }
}

/// Live R: the reset refusal gave both times to the minute ("its container dates from
/// 14:54 UTC, the sync from 14:54 UTC"), which reads as no reset at all. To the second now.
#[test]
fn the_reset_refusal_gives_its_times_to_the_second() {
    use arena_core::replica::MirrorRefusal;
    // A sync at 11:02:03, the container reset 20 s later — the same minute.
    let r = MirrorRefusal::SourceReset { container: 1_791_457_343, last_sync: 1_791_457_323 };
    let e = mirror_refusal_text(&r, "devtest-apple", "devtest-apple-new");
    assert!(e.contains("(its container dates from 2026-10-08 11:02:23 UTC, the sync from 2026-10-08 11:02:03 UTC)"), "{e}");
    let e = mirror_refusal_text(&MirrorRefusal::NoRecord { last_sync: 1_791_457_323 }, "a", "b");
    assert!(e.contains("has no record of b's last sync (2026-10-08 11:02:03 UTC)"), "{e}");
}

/// Live R: `cutover --no-final-sync` said "(180s ago)" of a sync 235 s old — the age was
/// floored to minutes and printed in seconds. Now exact, in `up`'s format.
#[test]
fn the_last_sync_age_is_exact() {
    let at = 1_791_457_323; // 2026-10-08 11:02:03 UTC
    for (ago, want) in [
        (0, "2026-10-08 11:02 UTC (0s ago)"),
        (59, "2026-10-08 11:02 UTC (59s ago)"),
        (235, "2026-10-08 11:02 UTC (3m55s ago)"),
        (3 * 3600 + 120, "2026-10-08 11:02 UTC (3h02m ago)"),
    ] {
        assert_eq!(sync_time_label(at, at + ago), want, "{ago}");
    }
    // A clock behind the stamp is "0s", never an underflow.
    assert_eq!(sync_time_label(at, at - 5), "2026-10-08 11:02 UTC (0s ago)");
}

/// Review finding (critical): the destination's endpoint was used with no identity check
/// before the mirroring push and the tidy-up — a recycled endpoint would have another
/// pod's home mirrored over and its files tidied away — and a stopped `-new` (listed with
/// a stale endpoint) wasn't refused. Now every step on it runs behind its RUNPOD_POD_ID in
/// the same session: an endpoint that answers as another pod stops the copy at the
/// prepare, before any transfer, and nothing else runs there.
#[tokio::test]
async fn the_destination_is_identity_checked_before_anything_is_written_to_it() {
    let (src, dest) = ("id-devtest-apple", "id-devtest-apple-new");
    let mut f = fleet_of(&[("apple", 22001), ("apple-new", 22002)]);
    f.pods[1].ssh_ip = Some("10.0.0.2".into());
    let dest_host = "10.0.0.2:22002";
    let fake = FakeRemote::new();
    fake.on(&host(22001), "arena-replica-sent", FakeReply::stdout("arena-replica-sent=yes\n"));
    fake.on(&host(22001), "RUNPOD_POD_ID", FakeReply::stdout(src));
    fake.on(dest_host, "arena-replica-ready", FakeReply::exit(97, "arena: this endpoint is not the expected pod - refusing"));
    let e = copy_pod_files(&cfg(), &f, &fake, src, dest, &crate::pods::home_copy::CopyOpts::new(false)).await.unwrap_err().to_string();
    assert!(e.contains("couldn't prepare devtest-apple-new") && e.contains("doesn't answer as id-devtest-apple-new"), "{e}");
    assert!(e.contains("nothing was copied"), "{e}");
    assert!(!execs(&fake, 22001).iter().any(|(c, _)| c.contains("rsync ")), "no push");
    let on_dest = fake.calls_to(dest_host);
    assert!(
        matches!(&on_dest[..], [RemoteCall::Exec { cmd, .. }] if cmd.contains("= '\\''id-devtest-apple-new'\\'' ]")),
        "only the guarded prepare, nothing after it: {on_dest:?}"
    );
    // Every later step carries the same check: the push's remote rsync, and the tidy-up.
    let fake = FakeRemote::new();
    fake.on(&host(22001), "arena-replica-sent", FakeReply::stdout("arena-replica-sent=yes\n"));
    fake.on(&host(22001), "RUNPOD_POD_ID", FakeReply::stdout(src));
    fake.on(dest_host, "arena-replica-ready", FakeReply::stdout("arena-replica-ready\n"));
    fake.on(dest_host, "MARK=", FakeReply::stdout(&format!("ID={dest}\nMARK=arena-replace-{dest}\n")));
    copy_pod_files(&cfg(), &f, &fake, src, dest, &crate::pods::home_copy::CopyOpts::new(false)).await.unwrap();
    let push = execs(&fake, 22001).into_iter().find(|(c, _)| c.contains("\nrsync ")).unwrap().0;
    assert!(push.contains("--rsync-path=") && push.contains("id-devtest-apple-new"), "{push}");
    let on_dest = fake.calls_to(dest_host);
    assert!(
        on_dest.iter().any(|c| matches!(c, RemoteCall::Exec { cmd, .. } if cmd.contains("arena-replica-kept") && cmd.contains("RUNPOD_POD_ID"))),
        "{on_dest:?}"
    );
    // A stopped -new is refused outright: its listed endpoint is the one it had.
    f.pods[1].status = "EXITED".into();
    let fake = FakeRemote::new();
    let e = copy_pod_files(&cfg(), &f, &fake, src, dest, &crate::pods::home_copy::CopyOpts::new(false)).await.unwrap_err().to_string();
    assert!(e.contains("the destination devtest-apple-new is stopped (EXITED)") && e.contains("`arena pods start devtest-apple-new`"), "{e}");
    assert!(fake.calls().is_empty(), "nothing reached: {:?}", fake.calls());
}

/// The via-local copy's "2 h per leg" is the LEG's budget, not each rsync's: a leg split
/// into the home and a linked repo's tree must not get the budget twice. Real children
/// (`sh -c sleep`, standing in for rsync) on a real clock, scaled down: a 2s leg whose
/// first rsync takes 0.8s leaves the second 1.2s — too little for its 1.6s, which a
/// per-rsync budget would have let finish (3 s for the leg) — and once the leg is spent
/// the next rsync isn't started at all.
#[cfg(unix)]
#[tokio::test]
async fn a_via_local_legs_rsyncs_share_one_budget() {
    let nap = |secs: &str| vec!["-c".to_string(), format!("sleep {secs}")];
    let leg = CopyLeg::start(Duration::from_secs(2));
    let start = Instant::now();
    leg.rsync("sh", &nap("0.8")).await.expect("the home fits in the leg");
    let err = leg.rsync("sh", &nap("1.6")).await.unwrap_err().to_string();
    assert_eq!(err, "rsync timed out after 2s (the copy leg's budget, for all its rsyncs together)");
    let took = start.elapsed();
    assert!(took >= Duration::from_secs(2), "stopped at the leg's deadline, not before: {took:?}");
    assert!(took < Duration::from_millis(2800), "not the repo's own 1.6s on top of the home's: {took:?}");
    // Spent: refused without spawning (a missing program would be a spawn error instead).
    let err = leg.rsync("/nonexistent/rsync", &[]).await.unwrap_err().to_string();
    assert!(err.starts_with("rsync timed out after 2s"), "{err}");
    // A failed rsync is still its own error, with its stderr.
    let err = CopyLeg::start(Duration::from_secs(5))
        .rsync("sh", &["-c".into(), "echo nope >&2; exit 23".into()])
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "rsync failed: nope");
}

#[test]
fn a_failed_replace_copy_says_new_is_left_running_and_how_to_continue() {
    let msg = replace_copy_failed("devtest-apple", "devtest-apple-new");
    assert!(msg.contains("devtest-apple-new is left running (billed)"), "{msg}");
    assert!(msg.contains("`arena pods migrate copy devtest-apple`"), "{msg}");
    assert!(msg.contains("`arena pods migrate cutover devtest-apple`"), "{msg}");
    assert!(msg.contains("`arena pods terminate devtest-apple-new`"), "{msg}");
}

#[tokio::test(start_paused = true)]
async fn proxy_gate_probes_the_stable_port_within_the_probe_budget() {
    let cfg = Config::parse(
        "MACHINE_NAME_PREFIX=devtest\nSSH_PROXY_HOST=proxy.test\nSSH_PROXY_STARTING_PORT=9500\n\
         MACHINE_NAME_LIST=(\n  \"apple\"\n  \"bloom\"\n)\n",
    );
    let fake = FakeRemote::new();
    fake.script("proxy.test:9501", [FakeReply::stdout("id-new\n"), FakeReply::stdout("id-old\n"), FakeReply::hang()]);
    assert!(proxy_reaches_pod(&fake, &cfg, "devtest-bloom", "id-new").await.unwrap());
    assert!(!proxy_reaches_pod(&fake, &cfg, "devtest-bloom", "id-new").await.unwrap(), "still the old pod");
    let start = Instant::now();
    assert!(!proxy_reaches_pod(&fake, &cfg, "devtest-bloom", "id-new").await.unwrap());
    assert_eq!(start.elapsed(), PROBE_TIMEOUT);
    assert!(fake.calls().iter().all(|c| matches!(c, RemoteCall::Exec { timeout, .. } if *timeout == Some(PROBE_TIMEOUT))));
}

#[tokio::test]
async fn each_pod_reports_a_crashed_job_by_name() {
    let jobs = [("devtest-apple", false), ("devtest-bloom", true)]
        .into_iter()
        .map(|(name, crash)| {
            (name.to_string(), async move {
                if crash {
                    panic!("boom");
                }
                7
            })
        })
        .collect();
    let mut seen = Vec::new();
    each_pod(jobs, |_, total, name, result| seen.push((total, name.to_string(), result))).await;
    seen.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(seen[0], (2, "devtest-apple".to_string(), Ok(7)));
    assert!(matches!(&seen[1], (2, name, Err(e)) if name == "devtest-bloom" && e.starts_with("task crashed: ")), "{seen:?}");
}

// ---- `pods test --deep` (PLAN 1.A) ----

/// What the deep-check script prints on a healthy 2×A4000 pod, after some zshrc
/// chatter (which the parser must skip).
pub(crate) const DEEP_HEALTHY: &str = "\
Welcome back! conda env: arena-env
deep_check=1
load1=0.84
cpus=64
nproc=16
uptime_secs=1209600
smi=ok
smi_cuda=13.0
gpu.0.name=NVIDIA RTX A4000
gpu.0.driver=580.65.06
gpu.1.name=NVIDIA RTX A4000
gpu.1.driver=580.65.06
smi_gpus=2
python=/root/miniconda3/envs/arena-env/bin/python
torch=ok
torch_version=2.9.0+cu130
torch_cuda=13.0
cuda_available=true
device_count=2
tensor.0=ok
tensor.1=ok
peer.0-1=ok
peer.1-0=ok
nccl_ranks=2
nccl=ok
py_done=1
py_exit=0
disk.root_avail_kb=104857600
net.curl_exit=0
net.http=206
net.bytes=33554432
net.secs=0.712
deep_check_end=1
";

/// A bad host: nvidia-smi is fine, CUDA init fails with error 999.
pub(crate) const DEEP_CUINIT_999: &str = "\
deep_check=1
load1=1.20
cpus=64
smi=ok
smi_cuda=13.0
gpu.0.name=NVIDIA RTX A4000
gpu.0.driver=580.65.06
smi_gpus=1
python=/root/miniconda3/envs/arena-env/bin/python
torch=ok
torch_version=2.9.0+cu130
torch_cuda=13.0
cuda_available=false
cuda_error=error: RuntimeError: Unexpected error from cudaGetDeviceCount(). Did you run some cuda functions before calling NumCudaDevices() that might have already set an error? Error 999: unknown error
device_count=0
py_done=1
py_exit=0
disk.root_avail_kb=104857600
net.curl_exit=0
net.http=206
net.bytes=33554432
net.secs=0.712
deep_check_end=1
";

fn deep_cfg() -> Config {
    Config::parse(
        "MACHINE_NAME_PREFIX=devtest\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nALLOWED_CUDA_VERSIONS=\"13.0\"\n",
    )
}

/// apple passes, bloom is on a cuInit-999 host, cloud hangs past the budget.
fn script_deep(fake: &FakeRemote) {
    fake.script(&host(22001), [FakeReply::stdout(DEEP_HEALTHY).after(Duration::from_secs(40))]);
    fake.script(&host(22002), [FakeReply::stdout(DEEP_CUINIT_999).after(Duration::from_secs(25))]);
    fake.script(&host(22003), [FakeReply::hang()]);
}

#[tokio::test(start_paused = true)]
async fn pods_test_deep_reports_pass_fail_and_timeout() {
    use arena_core::health::{deep_check_command, render_report, Status, DEEP_CHECK_TIMEOUT};
    let fake = Arc::new(FakeRemote::new());
    script_deep(&fake);
    let remote: Arc<dyn Remote> = fake.clone();
    let start = Instant::now();
    let results = deep_check_fleet(&fleet(), &remote, &deep_cfg(), &everyone(&fleet())).await.unwrap();
    assert_eq!(start.elapsed(), DEEP_CHECK_TIMEOUT, "ends at the hung pod's budget, not the hang");

    // One exec per pod: the script (base64, inside the conda login wrap), bounded.
    let want = deep_check_command(Some("arena-env"));
    for port in [22001, 22002, 22003] {
        assert_eq!(execs(&fake, port), [(want.clone(), Some(DEEP_CHECK_TIMEOUT))], "port {port}");
    }

    let statuses: Vec<(&str, Status)> = results.iter().map(|h| (h.name.as_str(), h.status)).collect();
    assert_eq!(
        statuses,
        [("devtest-apple", Status::Pass), ("devtest-bloom", Status::Fail), ("devtest-cloud", Status::Fail)]
    );
    // The table, and — all three pods share 10.0.0.1 — the same-host hint for the two failures.
    assert_eq!(
        render_report(&results, false),
        "\
NAME           RESULT  GPUS         DRIVER     CUDA        NET  NOTES
devtest-apple  pass    2×RTX A4000  580.65.06  13.0  47.1 MB/s
devtest-bloom  fail    1×RTX A4000  580.65.06  13.0  47.1 MB/s  cuda: RuntimeError: Unexpected error from cudaGetDeviceCount(). Error 999: unknown error
devtest-cloud  fail    -            -          -             -  ssh: timed out after 150s

1 pass, 0 warn, 2 fail
same host? 10.0.0.1: 2 failing (devtest-bloom, devtest-cloud). A bad host breaks every pod on it; a different GPU type draws a different host.
"
    );
    // -v adds every check per pod.
    let verbose = render_report(&results, true);
    assert!(verbose.contains("── devtest-apple (pass)\n"), "{verbose}");
    assert!(verbose.contains("  ✓ driver        580.65.06 ≥ 580 (CUDA 13.0)\n"), "{verbose}");
    assert!(verbose.contains("  - peer_copy     needs CUDA\n"), "{verbose}");

    // --json: per-pod {name, provider, status, checks, facts}; no IPs.
    let v = serde_json::to_value(&results).unwrap();
    assert_eq!(v[0]["status"], "pass");
    assert_eq!(v[0]["facts"]["nccl"], "ok");
    assert_eq!(v[1]["provider"], "runpod");
    let cuda = v[1]["checks"].as_array().unwrap().iter().find(|c| c["name"] == "cuda").unwrap();
    assert_eq!(cuda["status"], "fail");
    assert!(cuda["detail"].as_str().unwrap().contains("Error 999: unknown error"));
    assert_eq!(v[2]["checks"], serde_json::json!([{"name": "ssh", "status": "fail", "detail": "timed out after 150s"}]));
    assert!(v[2]["facts"].is_null());
    assert!(!v.to_string().contains("10.0.0.1"));
}

#[tokio::test(start_paused = true)]
async fn pods_test_deep_exit_status_follows_failures_not_warnings() {
    use arena_core::health::DEEP_CHECK_TIMEOUT;
    let deep = |json: bool| PodCmd::Test { deep: true, sel: Select::default(), json, verbose: false, cohort: crate::cli::CohortScope::default() };
    // Any FAIL fails the command (table or JSON), at the hung pod's budget.
    for json in [false, true] {
        let fake = Arc::new(FakeRemote::new());
        script_deep(&fake);
        let start = Instant::now();
        let err = handle_pods(deep(json), &fleet(), fake.clone(), &deep_cfg(), true).await.unwrap_err();
        assert_eq!(err.to_string(), "2 pod(s) failed the deep check", "json={json}");
        assert_eq!(start.elapsed(), DEEP_CHECK_TIMEOUT);
    }
    // Warnings alone (a slow network on bloom) don't.
    let slow = DEEP_HEALTHY.replace("net.secs=0.712", "net.secs=30.001").replace("net.bytes=33554432", "net.bytes=12000000");
    let script_slow = |fake: &FakeRemote| {
        fake.script(&host(22001), [FakeReply::stdout(DEEP_HEALTHY)]);
        fake.script(&host(22002), [FakeReply::stdout(&slow)]);
        fake.script(&host(22003), [FakeReply::stdout(DEEP_HEALTHY)]);
    };
    let fake = Arc::new(FakeRemote::new());
    script_slow(&fake);
    handle_deep_test(&fleet(), fake.clone(), &deep_cfg(), &everyone(&fleet()), false, true).await.unwrap();
    // ...and bloom really was a warning, not a pass.
    let fake = Arc::new(FakeRemote::new());
    script_slow(&fake);
    let remote: Arc<dyn Remote> = fake.clone();
    let results = deep_check_fleet(&fleet(), &remote, &deep_cfg(), &everyone(&fleet())).await.unwrap();
    assert_eq!(results[1].status, arena_core::health::Status::Warn, "{:#?}", results[1].checks);
    // A malformed MIN_DRIVER_VERSION fails before any pod is reached.
    let fake = Arc::new(FakeRemote::new());
    let bad = Config::parse("MACHINE_NAME_PREFIX=devtest\nMIN_DRIVER_VERSION=newest\n");
    let err = handle_pods(deep(false), &fleet(), fake.clone(), &bad, true).await.unwrap_err();
    assert!(err.to_string().contains("MIN_DRIVER_VERSION"), "{err}");
    assert!(fake.calls().is_empty());
}

/// disk-guard: the daily health check (`pods test --deep` over the cohort, nothing named)
/// ends with this box — its disk, and the copies skipped for space in the last day, which
/// fail the check even when every pod passes. A check of named pods says nothing about it.
#[tokio::test]
async fn the_health_check_counts_copies_skipped_for_space_as_failures() {
    let dir = std::env::temp_dir().join(format!("arena-deep-disk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cfg = deep_cfg();
    cfg.values.insert("ARENA_STATE_DIR".into(), dir.display().to_string());
    cfg.values.insert("CONTROL_MIN_FREE_GB".into(), "0".into());
    cfg.values.insert("LOCAL_BACKUP_DIR".into(), dir.join("backup").display().to_string());
    let healthy = || {
        let fake = Arc::new(FakeRemote::new());
        for port in [22001, 22002, 22003] {
            fake.script(&host(port), [FakeReply::stdout(DEEP_HEALTHY)]);
        }
        fake
    };
    handle_deep_test(&fleet(), healthy(), &cfg, &everyone(&fleet()), false, false).await.unwrap();
    // A copy skipped an hour ago: the check fails, and says which and why.
    let now = arena_core::snapshot::unix_now();
    let why = "only 3.1 GB free on / (floor 5 GB, CONTROL_MIN_FREE_GB)";
    let record = dir.join("devtest").join(arena_core::disk::SKIPS_FILE);
    arena_core::disk::record_skips(&record, &[("devtest-bloom".into(), "copy".into(), why.into())], now - 3600, Duration::from_secs(5)).unwrap();
    let err = handle_deep_test(&fleet(), healthy(), &cfg, &everyone(&fleet()), false, false).await.unwrap_err();
    assert_eq!(err.to_string(), "copies of 1 pod(s) onto this box were skipped for space in the last day");
    let box_health = crate::disk::health(&cfg, now).unwrap();
    assert_eq!(box_health.skipped, 1);
    assert!(box_health.lines[0].contains("This box"), "{:?}", box_health.lines);
    assert!(box_health.lines.iter().any(|l| l.starts_with("  ✗ devtest-bloom: copy skipped at ") && l.ends_with(why)), "{:?}", box_health.lines);
    assert!(box_health.lines.iter().any(|l| l.starts_with("  ✓ ")), "the disk's own row: {:?}", box_health.lines);
    // Named pods: about them only.
    let apple = picked(&fleet(), &cfg, &["apple"]).await;
    handle_deep_test(&fleet(), healthy(), &cfg, &apple, false, false).await.unwrap();
    // A day on, no longer this check's.
    assert_eq!(crate::disk::health(&cfg, now + 86_400).unwrap().skipped, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `pods test --deep` leaves each verdict in the health cache `arena snapshot` reads —
/// keyed by provider + id — and drops the record of a pod the listing no longer has.
#[tokio::test(start_paused = true)]
async fn pods_test_deep_records_verdicts_in_the_health_cache() {
    use arena_core::health::{PodHealth, Status};
    use arena_core::snapshot::{HealthCache, Issue};
    let dir = std::env::temp_dir().join(format!("arena-deep-cache-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cfg = deep_cfg();
    cfg.values.insert("ARENA_STATE_DIR".into(), dir.display().to_string());
    let path = dir.join("devtest").join("health.json");
    // A record of a pod since terminated (runpod now lists without it).
    let gone = Pod { id: "id-gone".into(), name: "devtest-gone".into(), provider: "runpod".into(), ..Default::default() };
    let mut seed = HealthCache::new();
    seed.merge(&[PodHealth::unreachable(&gone, "timed out")], 1);
    seed.save(&path).unwrap();

    let fake = Arc::new(FakeRemote::new());
    script_deep(&fake);
    let err = handle_deep_test(&fleet(), fake.clone(), &cfg, &everyone(&fleet()), true, false).await.unwrap_err();
    assert_eq!(err.to_string(), "2 pod(s) failed the deep check", "recording never changes the verdict");

    let (cache, warning) = HealthCache::load(&path);
    assert_eq!(warning, None);
    let rec = |name: &str| cache.get(&pod(name, 0)).unwrap_or_else(|| panic!("{name}: {cache:#?}"));
    assert_eq!((rec("apple").status, rec("apple").issues.len()), (Status::Pass, 0));
    assert_eq!((rec("bloom").status, rec("bloom").issues.first()), (Status::Fail, Some(&Issue::GpuError)));
    assert_eq!((rec("cloud").status, rec("cloud").issues.as_slice()), (Status::Fail, &[Issue::Unreachable][..]));
    assert_eq!(rec("apple").gpu.as_deref(), Some("2×RTX A4000"), "the machine's own GPU view");
    assert!(cache.get(&gone).is_none(), "pruned: runpod listed OK without it");
    assert_eq!(cache.pods.len(), 3);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(start_paused = true)]
async fn pods_test_deep_names_scope_the_run_and_report_unreachable_pods() {
    use arena_core::health::Status;
    let remote = |fake: &Arc<FakeRemote>| -> Arc<dyn Remote> { fake.clone() };

    // Bare, full and id forms; only those pods are reached.
    let fake = Arc::new(FakeRemote::new());
    let sel = picked(&fleet(), &deep_cfg(), &["bloom", "id-devtest-cloud"]).await;
    let results = deep_check_fleet(&fleet(), &remote(&fake), &deep_cfg(), &sel).await.unwrap();
    assert_eq!(results.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["devtest-bloom", "devtest-cloud"]);
    assert!(fake.calls_to(&host(22001)).is_empty());

    // A typo fails loudly (through the command), touching nothing.
    let fake = Arc::new(FakeRemote::new());
    let err = handle_pods(pods(&["test", "--deep", "apple", "nope"]), &fleet(), fake.clone(), &deep_cfg(), true)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("`nope`: no pod has that name or id"), "{err}");
    assert!(fake.calls().is_empty());

    // A named pod with no endpoint yet is a FAIL (it was asked about), never silently
    // skipped; ssh refusing the connection is a FAIL with ssh's reason; a script that
    // never started (exit 0, nothing printed) is a FAIL with the pod's stderr.
    let mut f = fleet();
    f.pods[0].ssh_port = None;
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22002), [FakeReply::exit(255, "ssh: connect to host 10.0.0.1 port 22002: Connection refused\n")]);
    fake.script(&host(22003), [FakeReply::exit(0, "\nzsh:1: command not found: base64\n")]);
    let sel = picked(&f, &deep_cfg(), &["apple", "bloom", "cloud"]).await;
    let results = deep_check_fleet(&f, &remote(&fake), &deep_cfg(), &sel).await.unwrap();
    let got: Vec<(&str, Status, &str, &str)> = results
        .iter()
        .map(|h| (h.name.as_str(), h.status, h.checks[0].name.as_str(), h.checks[0].detail.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            ("devtest-apple", Status::Fail, "ssh", "no SSH endpoint yet (status RUNNING)"),
            (
                "devtest-bloom",
                Status::Fail,
                "ssh",
                "exit 255: ssh: connect to host 10.0.0.1 port 22002: Connection refused"
            ),
            (
                "devtest-cloud",
                Status::Fail,
                "script",
                "no output from the check script (zsh:1: command not found: base64)"
            ),
        ]
    );
    assert!(fake.calls_to(&host(22001)).is_empty());
    // A named STOPPED pod (its last endpoint still listed) is never probed: a FAIL that
    // says it's stopped and how to bring it back (live finding #15).
    let mut f = fleet();
    f.pods[0].status = "EXITED".into();
    let fake = Arc::new(FakeRemote::new());
    let sel = picked(&f, &deep_cfg(), &["apple"]).await;
    let results = deep_check_fleet(&f, &remote(&fake), &deep_cfg(), &sel).await.unwrap();
    assert_eq!(results[0].checks[0].detail, "stopped (EXITED) — `arena pods start devtest-apple` brings it back");
    assert!(fake.calls().is_empty());
    // Without names, a pod without an endpoint is skipped (with a note), not failed.
    let fake = Arc::new(FakeRemote::new());
    let all: Vec<_> = (0..3).map(|_| FakeReply::stdout(DEEP_HEALTHY)).collect();
    for port in [22002, 22003] {
        fake.script(&host(port), all.clone());
    }
    let results = deep_check_fleet(&f, &remote(&fake), &deep_cfg(), &everyone(&f)).await.unwrap();
    assert_eq!(results.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["devtest-bloom", "devtest-cloud"]);

    // A stopped pod, or one up with only RunPod's SSH proxy, says so — not "yet" (live:
    // `test`/`run` said "no SSH endpoint yet" for an EXITED pod).
    let mut f = fleet();
    (f.pods[0].ssh_ip, f.pods[0].ssh_port, f.pods[0].status) = (None, None, "EXITED".into());
    (f.pods[1].ssh_ip, f.pods[1].ssh_port, f.pods[1].proxy_only) = (None, None, true);
    let fake = Arc::new(FakeRemote::new());
    let sel = picked(&f, &deep_cfg(), &["apple", "bloom"]).await;
    let results = deep_check_fleet(&f, &remote(&fake), &deep_cfg(), &sel).await.unwrap();
    let why: Vec<&str> = results.iter().map(|h| h.checks[0].detail.as_str()).collect();
    assert_eq!(why[0], "stopped (EXITED) — `arena pods start devtest-apple` brings it back");
    assert!(why[1].starts_with("no public endpoint (RunPod ssh proxy only)"), "{why:?}");
    assert!(fake.calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn pods_test_deep_reports_every_pod_when_two_share_a_name() {
    use arena_core::health::Status;
    // Two `devtest-apple`s (a double create): one on a cuInit-999 host, one healthy.
    // Whichever finishes last, both get their own row and the broken one fails the run.
    for (broken_first, healthy_after, broken_after) in [(true, 5, 1), (false, 1, 5)] {
        let mut f = fleet_of(&[("apple", 22001), ("apple", 22002)]);
        f.pods[0].id = "id-a".into();
        f.pods[1].id = "id-b".into();
        let (healthy_port, broken_port) = if broken_first { (22001, 22002) } else { (22002, 22001) };
        let fake = Arc::new(FakeRemote::new());
        let script = |fake: &FakeRemote| {
            fake.script(&host(healthy_port), [FakeReply::stdout(DEEP_HEALTHY).after(Duration::from_secs(healthy_after))]);
            fake.script(&host(broken_port), [FakeReply::stdout(DEEP_CUINIT_999).after(Duration::from_secs(broken_after))]);
        };
        script(&fake);
        let remote: Arc<dyn Remote> = fake.clone();
        let results = deep_check_fleet(&f, &remote, &deep_cfg(), &everyone(&f)).await.unwrap();
        let got: Vec<(&str, &str, Status)> =
            results.iter().map(|h| (h.id.as_str(), h.name.as_str(), h.status)).collect();
        let verdict = |port| if port == healthy_port { Status::Pass } else { Status::Fail };
        assert_eq!(
            got,
            [("id-a", "devtest-apple", verdict(22001)), ("id-b", "devtest-apple", verdict(22002))],
            "broken_first={broken_first}"
        );
        script(&fake);
        let err = handle_deep_test(&f, fake.clone(), &deep_cfg(), &everyone(&f), false, false).await.unwrap_err();
        assert_eq!(err.to_string(), "1 pod(s) failed the deep check");
    }
    assert_eq!(
        duplicate_names(&fleet_of(&[("apple", 1), ("bloom", 2), ("apple", 3)]).pods),
        [("devtest-apple".to_string(), vec!["id-devtest-apple".to_string(), "id-devtest-apple".to_string()])]
    );
    assert!(duplicate_names(&fleet().pods).is_empty());
}

#[tokio::test(start_paused = true)]
async fn pods_test_deep_json_stdout_is_always_json() {
    // No pod with an endpoint: `[]` on stdout, the note on stderr.
    let (out, err) = render_deep_test(&[], true, false).unwrap();
    assert_eq!(out, "[]\n");
    assert_eq!(err, ["(no pods with an SSH endpoint)"]);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&out).unwrap(), serde_json::json!([]));
    // Without --json the note is the output.
    assert_eq!(render_deep_test(&[], false, false).unwrap(), ("(no pods with an SSH endpoint)\n".into(), vec![]));
    // With results: parseable JSON on stdout, the summary on stderr.
    let fake = Arc::new(FakeRemote::new());
    script_deep(&fake);
    let remote: Arc<dyn Remote> = fake.clone();
    let results = deep_check_fleet(&fleet(), &remote, &deep_cfg(), &everyone(&fleet())).await.unwrap();
    let (out, err) = render_deep_test(&results, true, false).unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 3);
    assert_eq!(err[0], "1 pass, 0 warn, 2 fail");
    // An empty fleet through the command itself: no failure, nothing reached.
    let empty = Fleet { kind: "runpod", pods: vec![] };
    let fake = Arc::new(FakeRemote::new());
    handle_deep_test(&empty, fake.clone(), &deep_cfg(), &everyone(&empty), true, false).await.unwrap();
    assert!(fake.calls().is_empty());
}

#[test]
fn pods_test_flags_parse() {
    let parse = |args: &[&str]| Cli::try_parse_from(args).map(|c| c.cmd);
    assert!(matches!(
        parse(&["arena", "pods", "test"]).unwrap(),
        Cmd::Pods(PodCmd::Test { deep: false, ref sel, json: false, verbose: false, .. }) if sel.targets.is_empty()
    ));
    match parse(&["arena", "pods", "test", "--deep", "apple", "bloom", "--json", "-v"]).unwrap() {
        Cmd::Pods(PodCmd::Test { deep, sel, json, verbose, .. }) => {
            assert!(deep && json && verbose);
            assert_eq!(sel.targets, ["apple", "bloom"]);
        }
        _ => panic!("not pods test"),
    }
    // Plain `pods test` (the torch check) takes targets too now (PLAN 1.C) …
    match parse(&["arena", "pods", "test", "apple..bloom", "--exclude", "cloud"]).unwrap() {
        Cmd::Pods(PodCmd::Test { deep: false, sel, .. }) => {
            assert_eq!((sel.targets, sel.opts.exclude), (vec!["apple..bloom".to_string()], vec!["cloud".to_string()]));
        }
        _ => panic!("not pods test"),
    }
    // … but --json/-v still belong to --deep.
    for args in [&["arena", "pods", "test", "--json"][..], &["arena", "pods", "test", "-v"]] {
        assert!(parse(args).is_err(), "{args:?}");
    }
}

/// Which pods a fleet-wide git backup command reaches, and the one-line summaries for the
/// pods it left out and the cohort pods it found on main (spec items 2 and 3).
mod cohort_scope {
    use crate::cli::{Cli, Cmd, CohortScope, PodCmd};
    use crate::cron::resolve_week_day;
    use crate::pods::backup::{on_protected_line, on_protected_lines};
    use crate::pods::branch::{restored_note, set_branch_reminder};
    use crate::pods::selection::{cohort_only, non_cohort_line, split_cohort, take_said};
    use super::*;

    /// A pod named exactly `full` (no prefix added) at `10.0.0.1:<port>`.
    fn box_pod(full: &str, port: u16) -> Pod {
        Pod { id: format!("id-{full}"), name: full.into(), ..pod("x", port) }
    }

    /// A shared account: two listed cohort pods, an extra `devtest-` pod off the list, a staff
    /// box listed as `@devtest-zebra` (it wears the prefix), a staff box `@staff-gpu`, a pod the
    /// provider named itself, and another cohort's pod.
    fn account() -> Fleet {
        Fleet {
            kind: "runpod",
            pods: vec![
                pod("apple", 22001),
                pod("bloom", 22002),
                pod("extra", 22003),
                box_pod("devtest-zebra", 22004),
                box_pod("staff-gpu", 22005),
                box_pod("quiet_blue_fox", 22006),
                box_pod("devtest2-apple", 22007),
            ],
        }
    }

    fn account_cfg(extra: &str) -> Config {
        Config::parse(&format!(
            "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom @devtest-zebra @staff-gpu)\n\
             BACKUP_REPO_PATH={REPO}\nGIT_SSH_KEY_REMOTE={KEY}\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\n{extra}"
        ))
    }

    /// The cohort's three; the line for the other four (in selection order: list slot, then name).
    const COHORT_PORTS: [u16; 3] = [22001, 22002, 22003];
    const LEFT_OUT: &str = "⊘ skipped 4 non-cohort pods (not devtest-…, or an @ staff entry): \
        devtest-zebra, staff-gpu, devtest2-apple, quiet_blue_fox — name one to include it, or pass --include-non-cohort";
    /// What a branch switch says first with [`account_cfg`]'s legacy `./backup` (no locks).
    const NO_LOCKS: &str = "(no per-pod backup locks: LOCAL_BACKUP_DIR (./backup) is relative — set an absolute path to keep \
        other backups out during the switch)";

    fn names(sel: &Selected) -> Vec<&str> {
        sel.pods.iter().map(|p| p.name.as_str()).collect()
    }

    /// A `split_cohort` case: the selection, whether it named pods, the pods kept, the names left out.
    type SplitCase = (Vec<Pod>, bool, Vec<&'static str>, Vec<&'static str>);

    /// An `on_protected_line` case: `(name, branch)` pods, whether the run was fleet-wide, the line.
    type OnMainCase = (&'static [(&'static str, &'static str)], bool, Option<&'static str>);

    #[test]
    fn a_fleet_wide_selection_keeps_this_cohorts_pods_and_names_the_rest() {
        let cfg = account_cfg("");
        let naming = Naming::from_config(&cfg);
        let pods = account().pods;
        // (selection, named?, kept, left out)
        let pick = |names: &[&str]| pods.iter().filter(|p| names.contains(&p.name.as_str())).cloned().collect::<Vec<_>>();
        let cases: Vec<SplitCase> = vec![
            // Fleet-wide: the `devtest-` pods, the off-list extra included; staff boxes (even
            // one wearing the prefix), the provider-named pod and another prefix are left out.
            (
                pods.clone(),
                false,
                vec!["devtest-apple", "devtest-bloom", "devtest-extra"],
                vec!["devtest-zebra", "staff-gpu", "quiet_blue_fox", "devtest2-apple"],
            ),
            // Named: exactly what was named, staff boxes included.
            (pick(&["devtest-zebra", "staff-gpu", "quiet_blue_fox"]), true, vec!["devtest-zebra", "staff-gpu", "quiet_blue_fox"], vec![]),
            (pick(&["devtest-apple"]), true, vec!["devtest-apple"], vec![]),
            // Fleet-wide with nothing to leave out, and an empty fleet.
            (pick(&["devtest-apple", "devtest-extra"]), false, vec!["devtest-apple", "devtest-extra"], vec![]),
            (vec![], false, vec![], vec![]),
        ];
        for (i, (pods, named, kept, out)) in cases.into_iter().enumerate() {
            let (sel, left) = split_cohort(&naming, Selected { pods, named });
            assert_eq!((names(&sel), sel.named, left), (kept, named, out.iter().map(|s| s.to_string()).collect::<Vec<_>>()), "case {i}");
        }
        // A pod without a name is left out by its id.
        let nameless = Pod { name: String::new(), ..box_pod("x", 22009) };
        assert_eq!(split_cohort(&naming, Selected::all(vec![nameless])).1, ["id-x"]);
    }

    /// The human (2026-10-19): fleet-wide = cohort pods only, with a way to take the others
    /// too. `--include-non-cohort` parses on all four commands (and `test`), and with it the selection
    /// comes back whole and nothing is said; without it, the cohort's and the one line.
    #[test]
    fn include_non_cohort_takes_every_pod_and_says_nothing() {
        let scope = |argv: &[&str]| -> CohortScope {
            use clap::Parser;
            let cli = Cli::try_parse_from(["arena", "pods"].iter().chain(argv).copied()).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            match cli.cmd {
                Cmd::Pods(PodCmd::Backup { cohort, .. })
                | Cmd::Pods(PodCmd::Pull { cohort, .. })
                | Cmd::Pods(PodCmd::InitBranches { cohort, .. })
                | Cmd::Pods(PodCmd::SetBranch { cohort, .. })
                | Cmd::Pods(PodCmd::Test { cohort, .. }) => cohort,
                _ => panic!("{argv:?}: not a cohort-scoped command"),
            }
        };
        for argv in [&["backup"][..], &["pull"], &["init-branches"], &["set-branch", "main", "--all"], &["test"], &["test", "--deep"]] {
            assert!(!scope(argv).include_non_cohort, "{argv:?}: off by default");
            let with = [argv, &["--include-non-cohort"]].concat();
            assert!(scope(&with).include_non_cohort, "{with:?}");
        }
        let cfg = account_cfg("");
        let every = || Selected::all(account().pods);
        let _ = take_said();
        let sel = cohort_only(&cfg, every(), CohortScope { include_non_cohort: true });
        assert_eq!(names(&sel), names(&every()), "every pod, in order");
        assert!(take_said().is_empty(), "nothing left out, nothing said");
        let sel = cohort_only(&cfg, every(), CohortScope::default());
        assert_eq!(names(&sel), ["devtest-apple", "devtest-bloom", "devtest-extra"]);
        // (In fleet order here: `select` orders a real run's by list slot, as LEFT_OUT has it.)
        let out = ["devtest-zebra", "staff-gpu", "quiet_blue_fox", "devtest2-apple"].map(String::from);
        assert_eq!(take_said(), [non_cohort_line("devtest", &out).unwrap()]);
    }

    /// The health check (spec health-cron) reads only, but runs CUDA, NCCL and a download on
    /// every GPU it reaches, and its daily cron line names no pod: fleet-wide (bare or `--all`)
    /// it reaches this cohort's pods only and says who it left out — never a staff box or
    /// another prefix's pod; the torch check (`pods test`) the same. `--include-non-cohort`
    /// takes every pod; a named staff box is checked, as named.
    #[tokio::test(start_paused = true)]
    async fn pods_test_deep_reaches_only_this_cohorts_pods_unless_told() {
        let cfg = account_cfg("");
        let run = |argv: &'static [&'static str]| {
            let cfg = cfg.clone();
            async move {
                let fake = Arc::new(FakeRemote::new());
                for port in 22001..=22007 {
                    fake.script(&host(port), [FakeReply::stdout(DEEP_HEALTHY)]);
                }
                let _ = take_said();
                handle_pods(pods(argv), &account(), fake.clone(), &cfg, true).await.unwrap();
                let reached: Vec<u16> = (22001..=22007).filter(|port| !fake.calls_to(&host(*port)).is_empty()).collect();
                (reached, take_said())
            }
        };
        for argv in [&["test", "--deep"][..], &["test", "--deep", "--all"], &["test"]] {
            assert_eq!(run(argv).await, (COHORT_PORTS.to_vec(), vec![LEFT_OUT.to_string()]), "{argv:?}");
        }
        assert_eq!(run(&["test", "--deep", "--include-non-cohort"]).await, ((22001..=22007).collect(), vec![]));
        assert_eq!(run(&["test", "--deep", "@staff-gpu"]).await, (vec![22005], vec![]));
    }

    #[test]
    fn the_left_out_pods_get_one_line() {
        let many: Vec<String> = (1..=8).map(|i| format!("box{i}")).collect();
        let cases: &[(&[String], Option<&str>)] = &[
            (&[], None),
            (
                &many[..1],
                Some("⊘ skipped 1 non-cohort pod (not devtest-…, or an @ staff entry): box1 — name one to include it, or pass --include-non-cohort"),
            ),
            (
                &many[..2],
                Some("⊘ skipped 2 non-cohort pods (not devtest-…, or an @ staff entry): box1, box2 — name one to include it, or pass --include-non-cohort"),
            ),
            (
                &many,
                Some(
                    "⊘ skipped 8 non-cohort pods (not devtest-…, or an @ staff entry): box1, box2, box3, box4, box5, box6 \
                     (+2 more) — name one to include it, or pass --include-non-cohort",
                ),
            ),
        ];
        for (names, want) in cases {
            assert_eq!(non_cohort_line("devtest", names).as_deref(), *want, "{names:?}");
        }
    }

    #[test]
    fn cohort_pods_on_main_get_one_line_naming_the_fix() {
        let on = |pods: &[(&str, &str)]| pods.iter().map(|(n, b)| (n.to_string(), b.to_string())).collect::<Vec<_>>();
        let cases: &[OnMainCase] = &[
            (&[], true, None),
            // The morning reset: the plain command.
            (
                &[("devtest-bloom", "main"), ("devtest-apple", "main"), ("devtest-extra", "main")],
                true,
                Some("⚠ 3 cohort pods on main: not backed up until `arena pods init-branches`"),
            ),
            // Some of the fleet, or a named run: the pods themselves, sorted.
            (
                &[("devtest-bloom", "main"), ("devtest-apple", "master")],
                false,
                Some("⚠ 2 cohort pods on main or master: not backed up until `arena pods init-branches devtest-apple devtest-bloom`"),
            ),
            (
                &[("devtest-apple", "HEAD")],
                false,
                Some("⚠ 1 cohort pod on a detached HEAD: not backed up until `arena pods init-branches devtest-apple`"),
            ),
        ];
        for (pods, fleet_wide, want) in cases {
            assert_eq!(on_protected_line(&on(pods), *fleet_wide).as_deref(), *want, "{pods:?}");
        }
        // In `pods backup`: a staff box on main (it was named) has its own line and is not
        // counted with the cohort's; each pod is said exactly once.
        let cfg = account_cfg("");
        let naming = Naming::from_config(&cfg);
        let found = on(&[("devtest-apple", "main"), ("devtest-zebra", "main"), ("quiet_blue_fox", "master")]);
        assert_eq!(
            on_protected_lines(&naming, &found, false),
            [
                "⊘ devtest-zebra (skipped — on protected branch main)",
                "⊘ quiet_blue_fox (skipped — on protected branch master)",
                "⚠ 1 cohort pod on main: not backed up until `arena pods init-branches devtest-apple`",
            ]
        );
        assert!(on_protected_lines(&naming, &[], true).is_empty());
    }

    #[test]
    fn set_branch_says_what_it_restored_and_reminds_on_main() {
        let cases: &[(&[&str], &str, &str)] = &[
            (&[], "main", ""),
            (
                &["Grokking"],
                "main",
                " — restored 1 nested repo that main doesn't track (Grokking): on disk and staged; \
                 the first backup on the next day branch commits them",
            ),
            (
                &["Grokking", "chapter4/exercises/shutdown_avoidance"],
                "master",
                " — restored 2 nested repos that master doesn't track (Grokking, chapter4/exercises/shutdown_avoidance): \
                 on disk and staged; the first backup on the next day branch commits them",
            ),
            (
                &["Grokking"],
                "feature-x",
                " — restored 1 nested repo that feature-x doesn't track (Grokking): on disk and staged; the next backup commits them",
            ),
        ];
        for (restored, branch, want) in cases {
            assert_eq!(restored_note(restored, branch), *want, "{restored:?} {branch}");
        }
        let cfg = account_cfg("");
        let naming = Naming::from_config(&cfg);
        let landed = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let reminder = |branch: &str, pods: &[&str], fleet_wide: bool| set_branch_reminder(&naming, branch, &landed(pods), fleet_wide);
        assert_eq!(
            reminder("main", &["devtest-apple", "devtest-bloom"], true).as_deref(),
            Some("⚠ 2 cohort pods on main: not backed up until `arena pods init-branches`")
        );
        // A staff box that landed on main isn't counted (init-branches isn't its fix).
        assert_eq!(
            reminder("master", &["staff-gpu", "devtest-bloom"], false).as_deref(),
            Some("⚠ 1 cohort pod on master: not backed up until `arena pods init-branches devtest-bloom`")
        );
        assert_eq!(reminder("main", &["staff-gpu"], false), None);
        assert_eq!(reminder("main", &[], true), None);
        assert_eq!(reminder("autocommit-devtest-w1d2-apple", &["devtest-apple"], true), None);
    }

    #[tokio::test(start_paused = true)]
    async fn backup_by_default_reaches_only_this_cohorts_pods_and_says_who_it_left_out() {
        let cfg = account_cfg("");
        take_said();
        // No targets, `all`, `--all`: the cohort's three (the off-list extra included) and one
        // line for the rest — not a failure (the run succeeds).
        for argv in [&["backup", "--no-pull"][..], &["backup", "all", "--no-pull"], &["backup", "--all", "--no-pull"]] {
            let fake = Arc::new(FakeRemote::new());
            handle_pods(pods(argv), &account(), fake.clone(), &cfg, true).await.unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            assert_eq!(ports(&fake), COHORT_PORTS, "{argv:?}");
            assert_eq!(take_said(), [LEFT_OUT], "{argv:?}");
        }
        // A filter narrows the cohort's pods; it doesn't widen the default.
        let fake = Arc::new(FakeRemote::new());
        handle_pods(pods(&["backup", "--exclude", "bloom", "--no-pull"]), &account(), fake.clone(), &cfg, true).await.unwrap();
        assert_eq!(ports(&fake), [22001, 22003]);
        assert_eq!(take_said(), [LEFT_OUT]);
        // Named — by name, @name or id, or a range: exactly those, staff boxes included, no line.
        let cases: &[(&[&str], &[u16])] = &[
            (&["backup", "staff-gpu", "@devtest-zebra", "quiet_blue_fox", "--no-pull"], &[22004, 22005, 22006]),
            (&["backup", "id-devtest2-apple", "apple", "--no-pull"], &[22001, 22007]),
            (&["backup", "apple..staff-gpu", "--no-pull"], &[22001, 22002, 22005]),
        ];
        for (argv, want) in cases {
            let fake = Arc::new(FakeRemote::new());
            handle_pods(pods(argv), &account(), fake.clone(), &cfg, true).await.unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            assert_eq!(ports(&fake), *want, "{argv:?}");
            assert_eq!(take_said(), Vec::<String>::new(), "{argv:?}");
        }
        // The non-cohort failures that used to fail every run are gone from it: here they
        // would all fail, and the default run still succeeds.
        let fake = Arc::new(FakeRemote::new());
        for port in [22004, 22005, 22006, 22007] {
            fake.script(&host(port), [FakeReply::exit(255, "Permission denied (publickey).")]);
        }
        handle_pods(pods(&["backup", "--no-pull"]), &account(), fake.clone(), &cfg, true).await.unwrap();
        assert_eq!(ports(&fake), COHORT_PORTS);
        take_said();
    }

    /// Review (fb2 R8): each of the four commands passes `--include-non-cohort` through. Without
    /// it the ⊘ line is said (the rest left out); with it nothing is left out, so nothing is said —
    /// and `backup` reaches every pod. (`pull` and the switches as dry runs: no rsync, no switch.)
    #[tokio::test(start_paused = true)]
    async fn include_non_cohort_reaches_every_pod_through_each_command() {
        let cfg = account_cfg("");
        take_said();
        let runs: &[&[&str]] = &[
            &["backup", "--no-pull"],
            &["pull", "--label", "w1d2", "--dry-run"],
            &["init-branches", "--week", "1", "--day", "2", "--dry-run"],
            &["set-branch", "main", "--all", "--dry-run"],
        ];
        for argv in runs {
            let fake = Arc::new(FakeRemote::new());
            handle_pods(pods(argv), &account(), fake.clone(), &cfg, true).await.unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            assert!(take_said().iter().any(|l| l == LEFT_OUT), "{argv:?}: the default leaves them out");
            let with = [*argv, &["--include-non-cohort"]].concat();
            let fake = Arc::new(FakeRemote::new());
            handle_pods(pods(&with), &account(), fake.clone(), &cfg, true).await.unwrap_or_else(|e| panic!("{with:?}: {e}"));
            assert!(!take_said().iter().any(|l| l.starts_with("⊘ skipped")), "{with:?}: nothing left out");
            if argv[0] == "backup" {
                assert_eq!(ports(&fake), [22001, 22002, 22003, 22004, 22005, 22006, 22007], "{with:?}");
            }
        }
    }

    /// Review ai-review claude #5: `--include-non-cohort` takes every pod on the account — fine
    /// for a pull, but `set-branch --all --hard` with it reset the staff boxes and the other
    /// cohort's pods too, with nothing in the way. Now that is refused before a pod is listed or
    /// touched (a dry run too, and with --init-branches), saying why; pods named outright (the
    /// flag then changes nothing), a gentle switch, and `--hard` on this cohort go on.
    #[tokio::test(start_paused = true)]
    async fn hard_with_include_non_cohort_is_refused_unless_the_pods_are_named() {
        use crate::pods::selection::hard_non_cohort_refusal;
        const REFUSED: &str = "set-branch --hard with --include-non-cohort refused: that takes every pod on the account, so \
            --hard would reset staff boxes and other cohorts' pods too, discarding their uncommitted work — name the pods to \
            reset (`arena pods set-branch <branch> <pod>... --hard`), or leave --include-non-cohort out to reset this cohort's";
        let cfg = account_cfg("");
        take_said();
        for argv in [
            &["set-branch", "main", "--all", "--hard", "--include-non-cohort"][..],
            &["set-branch", "main", "all", "--hard", "--include-non-cohort", "--dry-run"],
            &["set-branch", "main", "--all", "--hard", "--include-non-cohort", "--init-branches"],
            &["set-branch", "main", "--all", "--exclude", "staff-gpu", "--hard", "--include-non-cohort"],
        ] {
            let fake = Arc::new(FakeRemote::new());
            let err = handle_pods(pods(argv), &account(), fake.clone(), &cfg, true).await.unwrap_err();
            assert_eq!(err.to_string(), REFUSED, "{argv:?}");
            assert!(fake.calls().is_empty() && take_said().is_empty(), "{argv:?}: nothing touched or said");
        }
        // Named pods (a staff box, another cohort's): exactly those, the flag or not.
        for argv in [
            &["set-branch", "main", "@staff-gpu", "devtest2-apple", "--hard", "--include-non-cohort", "--dry-run"][..],
            // Gentle, fleet-wide: every pod, as before; --hard on this cohort's: as before.
            &["set-branch", "main", "--all", "--include-non-cohort", "--dry-run"],
            &["set-branch", "main", "--all", "--hard", "--dry-run"],
        ] {
            let fake = Arc::new(FakeRemote::new());
            handle_pods(pods(argv), &account(), fake.clone(), &cfg, true).await.unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        }
        take_said();
        let on = CohortScope { include_non_cohort: true };
        assert_eq!(hard_non_cohort_refusal(true, on, false).as_deref(), Some(REFUSED));
        for (hard, scope, named) in [(true, on, true), (false, on, false), (true, CohortScope::default(), false)] {
            assert_eq!(hard_non_cohort_refusal(hard, scope, named), None, "{hard} {scope:?} {named}");
        }
    }

    /// `pods ask --all` reads this cohort's pods (the others named in one line), every pod with
    /// `--include-non-cohort`; named pods exactly. Dry runs: nothing is sent.
    #[tokio::test(start_paused = true)]
    async fn ask_all_reads_the_cohort_and_include_non_cohort_every_pod() {
        let cfg = account_cfg("");
        take_said();
        let fake = Arc::new(FakeRemote::new());
        handle_pods(pods(&["ask", "--all", "--dry-run"]), &account(), fake.clone(), &cfg, false).await.unwrap();
        assert_eq!(take_said(), [LEFT_OUT]);
        assert_eq!(ports(&fake), COHORT_PORTS);
        let fake = Arc::new(FakeRemote::new());
        handle_pods(pods(&["ask", "--all", "--include-non-cohort", "--dry-run"]), &account(), fake.clone(), &cfg, false).await.unwrap();
        assert!(take_said().is_empty());
        assert_eq!(ports(&fake), [22001, 22002, 22003, 22004, 22005, 22006, 22007]);
        let fake = Arc::new(FakeRemote::new());
        handle_pods(pods(&["ask", "bloom", "@devtest-zebra", "is it busy?", "--dry-run"]), &account(), fake.clone(), &cfg, false).await.unwrap();
        assert_eq!(ports(&fake), [22002, 22004]);
    }

    /// `ASK_AI=off` refuses — whatever `--ai` says — and so does an unscoped ask or two
    /// questions, all before any pod is read.
    #[tokio::test(start_paused = true)]
    async fn ask_refuses_off_unscoped_or_two_questions_before_reading_a_pod() {
        let fake = Arc::new(FakeRemote::new());
        let cases: [(&str, &[&str], &str); 5] = [
            ("ASK_AI=off\n", &["ask", "apple"], "ASK_AI=off"),
            ("ASK_AI=off\n", &["ask", "apple", "--ai", "claude"], "ASK_AI=off"),
            ("ASK_AI=gpt\n", &["ask", "apple"], "ASK_AI must be claude, codex or off"),
            ("", &["ask"], "name the pods to ask about"),
            ("", &["ask", "apple", "is it up?", "and busy?"], "one question at a time"),
        ];
        for (extra, argv, want) in cases {
            let e = handle_pods(pods(argv), &account(), fake.clone(), &account_cfg(extra), false).await.unwrap_err();
            assert!(e.to_string().contains(want), "{argv:?}: {e}");
        }
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    }

    #[tokio::test(start_paused = true)]
    async fn backup_says_once_which_cohort_pods_sit_on_main_and_still_exits_zero() {
        let cfg = account_cfg("");
        take_said();
        // The morning reset: every cohort pod on main — one line, the plain fix, exit 0.
        let fake = Arc::new(FakeRemote::new());
        for port in COHORT_PORTS {
            fake.script(&host(port), [FakeReply::stdout("SKIP main\n")]);
        }
        handle_pods(pods(&["backup", "--no-pull"]), &account(), fake.clone(), &cfg, true).await.unwrap();
        assert_eq!(take_said(), [LEFT_OUT, "⚠ 3 cohort pods on main: not backed up until `arena pods init-branches`"]);

        // Some on main: the line names them.
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::stdout("PUSHED autocommit-devtest-w1d2-apple\n")]);
        fake.script(&host(22002), [FakeReply::stdout("SKIP main\n")]);
        fake.script(&host(22003), [FakeReply::stdout("SKIP master\n")]);
        handle_pods(pods(&["backup", "--no-pull"]), &account(), fake.clone(), &cfg, true).await.unwrap();
        assert_eq!(
            take_said(),
            [
                LEFT_OUT,
                "⚠ 2 cohort pods on main or master: not backed up until `arena pods init-branches devtest-bloom devtest-extra`"
            ]
        );

        // A staff box named next to a cohort pod, both on main: each said once, in its own line.
        let fake = Arc::new(FakeRemote::new());
        for port in [22001, 22004] {
            fake.script(&host(port), [FakeReply::stdout("SKIP main\n")]);
        }
        handle_pods(pods(&["backup", "apple", "@devtest-zebra", "--no-pull"]), &account(), fake.clone(), &cfg, true).await.unwrap();
        assert_eq!(
            take_said(),
            [
                "⊘ devtest-zebra (skipped — on protected branch main)",
                "⚠ 1 cohort pod on main: not backed up until `arena pods init-branches devtest-apple`"
            ]
        );

        // A real failure beside them still fails the run — for that pod only.
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::stdout("SKIP main\n")]);
        fake.script(&host(22002), [FakeReply::exit(1, "error: failed to push some refs")]);
        fake.script(&host(22003), [FakeReply::stdout("SKIP main\n")]);
        let err = handle_pods(pods(&["backup", "--no-pull"]), &account(), fake.clone(), &cfg, true).await.unwrap_err();
        assert_eq!(err.to_string(), "1 pod(s) failed to back up");
        assert_eq!(take_said(), [LEFT_OUT, "⚠ 2 cohort pods on main: not backed up until `arena pods init-branches`"]);
    }

    #[tokio::test(start_paused = true)]
    async fn set_branch_to_main_says_what_it_restored_and_reminds_until_init_branches() {
        let cfg = account_cfg("");
        take_said();
        // `--all --hard`: the cohort's pods only; the pod with adopted clones says what was put
        // back; the run ends with the reminder. (Each pod's branch is backed up before its
        // switch: the first reply is its backup's.)
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [FakeReply::ok(), FakeReply::stdout("RESTORED Grokking\nRESTORED chapter4/x\n")]);
        handle_pods(pods(&["set-branch", "main", "--all", "--hard"]), &account(), fake.clone(), &cfg, true).await.unwrap();
        assert_eq!(ports(&fake), COHORT_PORTS);
        let said = take_said();
        assert_eq!((said.len(), said[0].as_str(), said[1].as_str()), (6, LEFT_OUT, NO_LOCKS), "{said:?}");
        let mut per_pod = unnumbered(&said[2..5]);
        per_pod.sort();
        assert_eq!(
            per_pod,
            [
                "✓ devtest-apple (on main) — restored 2 nested repos that main doesn't track (Grokking, chapter4/x): \
                 on disk and staged; the first backup on the next day branch commits them",
                "✓ devtest-bloom (on main)",
                "✓ devtest-extra (on main)",
            ]
        );
        assert_eq!(said[5], "⚠ 3 cohort pods on main: not backed up until `arena pods init-branches`");

        // Named, a staff box among them: the reminder names the cohort pod only.
        let fake = Arc::new(FakeRemote::new());
        handle_pods(pods(&["set-branch", "main", "bloom", "staff-gpu"]), &account(), fake.clone(), &cfg, true).await.unwrap();
        assert_eq!(ports(&fake), [22002, 22005]);
        let said = take_said();
        assert_eq!(said.last().map(String::as_str), Some("⚠ 1 cohort pod on main: not backed up until `arena pods init-branches devtest-bloom`"));
        assert_eq!(said.len(), 4, "{said:?}");

        // A failed switch: the reminder still names the pods that did land on main.
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22002), [FakeReply::ok(), FakeReply::exit(1, "error: Your local changes would be overwritten")]);
        let err = handle_pods(pods(&["set-branch", "main", "apple", "bloom"]), &account(), fake.clone(), &cfg, true).await.unwrap_err();
        assert_eq!(err.to_string(), "1 pod(s) failed to switch branch");
        assert_eq!(take_said().last().map(String::as_str), Some("⚠ 1 cohort pod on main: not backed up until `arena pods init-branches devtest-apple`"));

        // Any other branch: no reminder.
        let fake = Arc::new(FakeRemote::new());
        handle_pods(pods(&["set-branch", "feature-x", "apple"]), &account(), fake.clone(), &cfg, true).await.unwrap();
        assert_eq!(take_said(), [NO_LOCKS, "[1/1] ✓ devtest-apple (on feature-x)"]);
    }

    #[tokio::test(start_paused = true)]
    async fn set_branch_init_branches_inits_the_pods_that_switched() {
        let cfg = account_cfg("ARENA_START_DATE=2026-10-05\n");
        let (week, day) = resolve_week_day(&cfg, None, None).unwrap();
        let bcfg = BackupConfig::from_config(&cfg, week, day);
        let hard = checkout_command(REPO, "main", Some(KEY), true);
        take_said();
        // Each cohort pod: the hard reset, then its day branch — each after its branch is backed
        // up — nothing on the staff boxes.
        let fake = Arc::new(FakeRemote::new());
        handle_pods(pods(&["set-branch", "main", "--all", "--hard", "--init-branches"]), &account(), fake.clone(), &cfg, true).await.unwrap();
        assert_eq!(ports(&fake), COHORT_PORTS);
        for (name, port) in [("devtest-apple", 22001), ("devtest-bloom", 22002), ("devtest-extra", 22003)] {
            let init = init_branch_command(&bcfg, name);
            let backup = (backup_command(REPO, Some(KEY), &format!("arena backup {name}"), None), Some(BACKUP_TIMEOUT));
            assert_eq!(
                execs(&fake, port),
                [backup.clone(), (hard.clone(), Some(BRANCH_TIMEOUT)), backup, (init, Some(BRANCH_TIMEOUT))],
                "{name}"
            );
        }
        take_said();
        // A failed switch: that pod gets no day branch — it isn't on main — the others do, and the
        // run fails naming it (review pr4: one failed pod used to keep every pod on main).
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22002), [FakeReply::ok(), FakeReply::exit(1, "fatal: unable to access")]);
        let err = handle_pods(pods(&["set-branch", "main", "--all", "--hard", "--init-branches"]), &account(), fake.clone(), &cfg, true)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "1 pod(s) failed to switch branch");
        assert_eq!(execs(&fake, 22002).len(), 2, "bloom: its backup and the switch, no init: {:?}", fake.calls());
        for (name, port) in [("devtest-apple", 22001), ("devtest-extra", 22003)] {
            let init = (init_branch_command(&bcfg, name), Some(BRANCH_TIMEOUT));
            assert_eq!(execs(&fake, port).last(), Some(&init), "{name}: init'd");
        }
        // No start date to name the day: refused before any pod is switched.
        let fake = Arc::new(FakeRemote::new());
        let err = handle_pods(pods(&["set-branch", "main", "--all", "--hard", "--init-branches"]), &account(), fake.clone(), &account_cfg(""), true)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("ARENA_START_DATE"), "{err}");
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
        // A dry run previews both and reaches nobody.
        let fake = Arc::new(FakeRemote::new());
        handle_pods(pods(&["set-branch", "main", "--all", "--init-branches", "--dry-run"]), &account(), fake.clone(), &cfg, true).await.unwrap();
        assert!(fake.calls().is_empty());
        take_said();
    }

    #[tokio::test(start_paused = true)]
    async fn init_branches_by_default_leaves_out_non_cohort_pods_too() {
        let cfg = account_cfg("");
        take_said();
        let fake = Arc::new(FakeRemote::new());
        handle_pods(pods(&["init-branches", "--week", "1", "--day", "2"]), &account(), fake.clone(), &cfg, true).await.unwrap();
        assert_eq!(ports(&fake), COHORT_PORTS);
        // The one line, then each cohort pod's own.
        let said = take_said();
        assert_eq!((said.len(), said[0].as_str(), said[1].as_str()), (5, LEFT_OUT, NO_LOCKS), "{said:?}");
        let fake = Arc::new(FakeRemote::new());
        handle_pods(pods(&["init-branches", "staff-gpu", "--week", "1", "--day", "2"]), &account(), fake.clone(), &cfg, true).await.unwrap();
        assert_eq!(ports(&fake), [22005]);
        assert_eq!(take_said(), [NO_LOCKS, "[1/1] ✓ staff-gpu -> autocommit-devtest-w1d2-staff-gpu"], "named: no ⊘ line");
    }

    #[tokio::test(start_paused = true)]
    async fn pull_by_default_leaves_out_non_cohort_pods_too() {
        let cfg = account_cfg("");
        take_said();
        // A dry run (it reaches no pod either way) says the same one line; named, none.
        let fake = Arc::new(FakeRemote::new());
        handle_pods(pods(&["pull", "--label", "w1d2", "--dry-run"]), &account(), fake.clone(), &cfg, true).await.unwrap();
        assert_eq!(take_said(), [LEFT_OUT]);
        handle_pods(pods(&["pull", "staff-gpu", "--label", "w1d2", "--dry-run"]), &account(), fake.clone(), &cfg, true).await.unwrap();
        assert!(take_said().is_empty());
        assert!(fake.calls().is_empty());
    }
}
