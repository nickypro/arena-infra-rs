use std::sync::Arc;
use std::time::Duration;

use arena_core::remote::{FakeRemote, FakeReply, Remote, RemoteCall};
use arena_core::setup::{provisioning_steps, BootRetry, SetupConfig, SetupTimeouts};
use arena_core::ssh::{SshOutput, SshTarget};
use arena_core::{Config, Pod, PodSpec, Provider, Result};
use async_trait::async_trait;
use tokio::time::Instant;

use crate::cli::{Cli, Cmd};
use crate::keys::{handle_copy_keys, COPY_KEYS_TIMEOUT};
use crate::pods::handle_pods;
use crate::pods::selection::Selected;
use super::{handle_setup, provision_fleet, SetupJob};

fn target(port: u16) -> SshTarget {
    SshTarget { user: "root".into(), host: "10.0.0.1".into(), port, key_paths: vec![], connect_timeout_secs: 10, host_keys: Default::default() }
}

fn scfg() -> SetupConfig {
    SetupConfig {
        key_local: "/local/key".into(),
        key_remote: "/root/.ssh/id_ed25519".into(),
        repo_path: "/root/ARENA_materials".into(),
        repo_url: "git@github.com:o/r.git".into(),
        upstream_url: None,
        branch: "main".into(),
        prefix: "devtest".into(),
        authorized_pubkeys: vec![],
        broadcast_exports: vec![],
        zsh_install: false,
        vscode: None,
        // The fleet runner is under test here, not the move onto the volume.
        relocate: false,
        swap_gb: arena_core::setup::DEFAULT_SWAP_GB,
    }
}

fn job(name: &str, port: u16) -> SetupJob {
    SetupJob {
        name: name.into(),
        target: target(port),
        steps: provisioning_steps("runpod", &scfg(), name, false, "", &SetupTimeouts::default()),
        after_repair: false,
    }
}

#[tokio::test(start_paused = true)]
async fn one_hung_pod_times_out_while_the_others_finish() {
    // (a) bloom's config step hangs "forever"; apple and cloud are healthy.
    let fake = Arc::new(FakeRemote::new());
    fake.script("10.0.0.1:22002", [FakeReply::ok(), FakeReply::hang()]);
    let jobs = vec![job("devtest-apple", 22001), job("devtest-bloom", 22002), job("devtest-cloud", 22003)];
    let mut lines = Vec::new();
    let start = Instant::now();
    let (mut provisioned, failed, _) =
        provision_fleet(fake.clone(), jobs, BootRetry::default(), |l| lines.push(l.to_string())).await;

    provisioned.sort();
    assert_eq!((provisioned, failed), (vec!["devtest-apple".to_string(), "devtest-cloud".to_string()], 1));
    assert_eq!(start.elapsed(), Duration::from_secs(300), "ends at the stuck step's budget, not the hang");
    // Healthy pods are reported first (they never waited on bloom), in either order.
    assert!(lines[0].starts_with("[1/3] ✓ ") && lines[1].starts_with("[2/3] ✓ "), "{lines:?}");
    let mut healthy: Vec<&str> = lines[..2].iter().filter_map(|l| l.split_once(" ✓ ").map(|(_, n)| n)).collect();
    healthy.sort();
    assert_eq!(healthy, ["devtest-apple", "devtest-cloud"], "{lines:?}");
    assert_eq!(lines[2], "[3/3] ✗ devtest-bloom (timed out at repo + keys config after 300s)");
    // Every pod got both steps; nothing was retried.
    for port in [22001, 22002, 22003] {
        assert_eq!(fake.calls_to(&format!("10.0.0.1:{port}")).len(), 2, "port {port}");
    }
}

#[tokio::test(start_paused = true)]
async fn failures_are_counted_and_described() {
    let fake = Arc::new(FakeRemote::new());
    fake.script("10.0.0.1:22001", [FakeReply::exit(1, "scp: /root/.ssh: Permission denied")]);
    let mut lines = Vec::new();
    let (provisioned, failed, _) =
        provision_fleet(fake, vec![job("devtest-apple", 22001)], BootRetry::default(), |l| lines.push(l.to_string())).await;
    assert_eq!((provisioned.len(), failed), (0, 1));
    assert_eq!(lines, ["[1/1] ✗ devtest-apple (failed at copy deploy key, exit 1): scp: /root/.ssh: Permission denied"]);
}

struct Fleet(Vec<Pod>);

#[async_trait]
impl Provider for Fleet {
    fn name(&self) -> &'static str {
        "runpod"
    }
    fn describe(&self, _spec: &PodSpec) -> String {
        String::new()
    }
    async fn list_pods(&self) -> Result<Vec<Pod>> {
        Ok(self.0.clone())
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

fn fleet() -> Fleet {
    let pod = |name: &str, port: u16| Pod {
        id: format!("id-{name}"),
        name: name.into(),
        provider: "runpod".into(),
        status: "RUNNING".into(),
        ssh_ip: Some("10.0.0.1".into()),
        ssh_port: Some(port),
        ..Default::default()
    };
    Fleet(vec![pod("devtest-apple", 22001), pod("devtest-bloom", 22002), pod("devtest-cloud", 22003)])
}

/// The whole test fleet, nothing named.
fn everyone() -> Selected {
    Selected::all(fleet().0)
}

/// Setup's config, VS Code warm-up off: these tests script each pod's exact replies
/// (the warm-up has its own, below — `vscode_cfg`).
fn setup_cfg(extra: &str) -> Config {
    Config::parse(&format!(
        "MACHINE_NAME_PREFIX=devtest\nARENA_REPO_OWNER=o\nARENA_REPO_NAME=r\n\
         GIT_SSH_KEY_LOCAL=/nonexistent/devtest_deploy_key\nVSCODE_PREINSTALL=0\nREPO_ON_VOLUME=0\n{extra}"
    ))
}

/// `setup_cfg` as shipped: the VS Code warm-up on (the default).
fn vscode_cfg(extra: &str) -> Config {
    Config::parse(&format!(
        "MACHINE_NAME_PREFIX=devtest\nARENA_REPO_OWNER=o\nARENA_REPO_NAME=r\n\
         GIT_SSH_KEY_LOCAL=/nonexistent/devtest_deploy_key\n{extra}"
    ))
}

/// A Remote whose calls panic — to check a crashed setup task is still reported.
struct PanickyRemote;

#[async_trait]
impl Remote for PanickyRemote {
    async fn exec(&self, _: &SshTarget, _: &str, _: Option<Duration>) -> Result<SshOutput> {
        panic!("boom")
    }
    async fn copy(&self, _: &SshTarget, _: &str, _: &str, _: Option<Duration>) -> Result<SshOutput> {
        panic!("boom")
    }
    async fn copy_recursive(&self, _: &SshTarget, _: &str, _: &str, _: Option<Duration>) -> Result<SshOutput> {
        panic!("boom")
    }
}

#[tokio::test]
async fn a_crashed_setup_task_is_a_named_failure() {
    let mut lines = Vec::new();
    let (provisioned, failed, _) = provision_fleet(
        Arc::new(PanickyRemote),
        vec![job("devtest-apple", 22001)],
        BootRetry::default(),
        |l| lines.push(l.to_string()),
    )
    .await;
    assert_eq!((provisioned.len(), failed), (0, 1));
    assert!(lines[0].starts_with("[1/1] ✗ devtest-apple (its setup task crashed: "), "{lines:?}");
}

/// The resolved budgets for a test config (`--timeout` = `flag`).
fn budgets(cfg: &Config, flag: Option<u64>) -> SetupTimeouts {
    SetupTimeouts::from_config(cfg, flag).unwrap()
}

/// No API-key CSVs: setup skips key distribution.
const NO_KEYS: &str = "/nonexistent/arena-test-keys";

#[tokio::test(start_paused = true)]
async fn handle_setup_runs_over_the_given_remote_with_configured_budget() {
    // End to end through the command: SETUP_TIMEOUT_SECS sets the config-step budget,
    // the hung pod fails the command (non-zero exit) after that budget, the others
    // were fully provisioned with the narrow fetch.
    let fake = Arc::new(FakeRemote::new());
    fake.script("10.0.0.1:22002", [FakeReply::ok(), FakeReply::hang()]);
    let start = Instant::now();
    let cfg = setup_cfg("SETUP_TIMEOUT_SECS=120");
    let err = handle_setup(fake.clone(), &cfg, true, false, None, None, false, false, budgets(&cfg, None), &everyone(), NO_KEYS, None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("1 pod(s) failed to set up"), "{err}");
    assert_eq!(start.elapsed(), Duration::from_secs(120));
    for port in [22001, 22002, 22003] {
        let calls = fake.calls_to(&format!("10.0.0.1:{port}"));
        assert!(
            matches!(&calls[..], [RemoteCall::Copy { local, .. }, RemoteCall::Exec { cmd, timeout, .. }]
                if local.ends_with("devtest_deploy_key")
                    && cmd.contains("git fetch --no-tags origin '+refs/heads/main:refs/remotes/origin/main'")
                    && *timeout == Some(Duration::from_secs(120))),
            "port {port}: {calls:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn handle_setup_timeout_flag_wins_and_names_scope_the_run() {
    let fake = Arc::new(FakeRemote::new());
    let only = Selected { pods: vec![fleet().0.remove(0)], named: true };
    let cfg = setup_cfg("SETUP_TIMEOUT_SECS=120");
    handle_setup(fake.clone(), &cfg, true, false, None, None, false, false, budgets(&cfg, Some(45)), &only, NO_KEYS, None)
        .await
        .unwrap();
    let calls = fake.calls();
    assert!(calls.iter().all(|c| c.host() == "10.0.0.1:22001"), "only the named pod: {calls:?}");
    assert!(matches!(calls.last(), Some(RemoteCall::Exec { timeout, .. }) if *timeout == Some(Duration::from_secs(45))));
}

/// The test fleet behind a provider with a key API (like Vast): records which pods it
/// was asked to re-attach keys to.
#[derive(Default)]
struct KeyApi {
    seen: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl Provider for KeyApi {
    fn name(&self) -> &'static str {
        "vast"
    }
    fn describe(&self, _spec: &PodSpec) -> String {
        String::new()
    }
    async fn list_pods(&self) -> Result<Vec<Pod>> {
        Ok(fleet().0)
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
    async fn authorize_ssh_keys(&self, pod: &Pod, _keys: &[String]) -> Result<()> {
        self.seen.lock().unwrap().push(pod.id.clone());
        Ok(())
    }
}

/// A temp dir holding a readable cohort key with its `.pub` — something to re-attach.
fn pubkey_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("arena-setup-pubkey-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("devtest_key"), "not a real key").unwrap();
    std::fs::write(d.join("devtest_key.pub"), "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAbc devtest\n").unwrap();
    d
}

/// `pods setup`: a pod that refuses our key gets it re-attached through its provider's
/// API, then only that pod runs setup again — once the key lands, it's provisioned.
#[tokio::test(start_paused = true)]
async fn handle_setup_reattaches_a_refused_key_and_reruns_that_pod() {
    let denied = "root@10.0.0.1: Permission denied (publickey).\nscp: Connection closed\r\n";
    let fake = Arc::new(FakeRemote::new());
    fake.script("10.0.0.1:22002", [FakeReply::exit(255, denied)]);
    let dir = pubkey_dir("repair");
    let cfg = setup_cfg(&format!("SHARED_SSH_KEY_PATH={}/devtest_key", dir.display()));
    let api = KeyApi::default();
    let start = Instant::now();
    handle_setup(fake.clone(), &cfg, true, false, None, None, false, false, budgets(&cfg, None), &everyone(), NO_KEYS, Some(&api))
        .await
        .unwrap();
    assert_eq!(*api.seen.lock().unwrap(), ["id-devtest-bloom"], "only the pod that refused");
    assert_eq!(start.elapsed(), arena_core::setup::KEY_REPAIR_EVERY, "one wait, then it took the key");
    assert_eq!(fake.calls_to("10.0.0.1:22002").len(), 3, "refused copy, then copy + config");
    for port in [22001, 22003] {
        assert_eq!(fake.calls_to(&format!("10.0.0.1:{port}")).len(), 2, "port {port}: set up once");
    }
    let _ = std::fs::remove_dir_all(&dir);

    // A provider without a key API: the refusal stands, nothing is retried.
    let fake = Arc::new(FakeRemote::new());
    fake.script("10.0.0.1:22002", [FakeReply::exit(255, denied)]);
    let runpod = fleet();
    let err = handle_setup(fake.clone(), &cfg, true, false, None, None, false, false, budgets(&cfg, None), &everyone(), NO_KEYS, Some(&runpod))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("1 pod(s) failed to set up"), "{err}");
    assert_eq!(fake.calls_to("10.0.0.1:22002").len(), 1);
}

/// A temp keys dir holding an OpenAI per-host CSV for the whole test fleet.
fn keys_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("arena-setup-keys-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("openai_api_keys.csv"),
        "host,key\ndevtest-apple,sk-a\ndevtest-bloom,sk-b\ndevtest-cloud,sk-c\n",
    )
    .unwrap();
    d
}

#[tokio::test(start_paused = true)]
async fn keys_go_only_to_provisioned_pods_so_a_wedged_pod_cannot_hang_setup() {
    // Review finding: once `keys gen` has run, a whole-fleet setup hands off to
    // copy-keys — which used to target every reachable pod (incl. the one that just
    // timed out) over an unbounded ssh, hanging the command forever. Now: bloom
    // (wedged at its config step) is never contacted again, the others get their keys,
    // and the command ends at bloom's step budget.
    let dir = keys_dir("wedged");
    let fake = Arc::new(FakeRemote::new());
    fake.script("10.0.0.1:22002", [FakeReply::ok(), FakeReply::hang(), FakeReply::hang()]);
    let cfg = setup_cfg("SETUP_TIMEOUT_SECS=120");
    let start = Instant::now();
    let keys = dir.to_string_lossy();
    let err = handle_setup(fake.clone(), &cfg, true, false, None, None, false, false, budgets(&cfg, None), &everyone(), &keys, None)
        .await
        .unwrap_err();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(err.to_string().contains("1 pod(s) failed to set up"), "{err}");
    assert_eq!(start.elapsed(), Duration::from_secs(120), "ends at bloom's budget, not the hang");
    assert_eq!(fake.calls_to("10.0.0.1:22002").len(), 2, "the timed-out pod is not re-contacted for keys");
    for port in [22001, 22003] {
        let calls = fake.calls_to(&format!("10.0.0.1:{port}"));
        assert_eq!(calls.len(), 3, "copy, config, keys: {calls:?}");
        assert!(
            matches!(&calls[2], RemoteCall::Exec { cmd, timeout, .. }
                if cmd.contains("OPENAI_API_KEY") && *timeout == Some(COPY_KEYS_TIMEOUT)),
            "{calls:?}"
        );
    }
}

/// W7 live (A-ckl9): `setup` with no key file but a backup archive still hands off to
/// copy-keys — every provisioned pod gets the archive's access (its block, the read key).
#[tokio::test(start_paused = true)]
async fn setup_writes_the_archive_access_without_any_key_file() {
    let dir = std::env::temp_dir().join(format!("arena-setup-archive-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("keys")).unwrap();
    let cfg = setup_cfg(&format!("ARENA_STATE_DIR={}\n", dir.join("state").display()));
    crate::archive::seed_pod_access(&cfg);
    let fake = Arc::new(FakeRemote::new());
    let keys = dir.join("keys").display().to_string();
    let r = handle_setup(fake.clone(), &cfg, true, false, None, None, false, false, budgets(&cfg, None), &everyone(), &keys, None).await;
    let _ = std::fs::remove_dir_all(&dir);
    r.unwrap();
    for port in [22001, 22002, 22003] {
        let calls = fake.calls_to(&format!("10.0.0.1:{port}"));
        assert!(calls.iter().any(|c| matches!(c, RemoteCall::Exec { cmd, .. } if cmd.contains("# BEGIN arena-archive"))), "{calls:#?}");
        assert!(calls.iter().any(|c| matches!(c, RemoteCall::Copy { remote, .. } if remote == ".ssh/arena_archive_key")), "{calls:#?}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_pod_wedging_during_key_distribution_times_out() {
    // Every pod provisions, then apple hangs on the key write: copy-keys reports it
    // after COPY_KEYS_TIMEOUT (best-effort: setup itself still succeeds).
    let dir = keys_dir("keyhang");
    let fake = Arc::new(FakeRemote::new());
    fake.script("10.0.0.1:22001", [FakeReply::ok(), FakeReply::ok(), FakeReply::hang()]);
    let cfg = setup_cfg("");
    let start = Instant::now();
    let keys = dir.to_string_lossy().into_owned();
    handle_setup(fake.clone(), &cfg, true, false, None, None, false, false, budgets(&cfg, None), &everyone(), &keys, None)
        .await
        .unwrap();
    assert_eq!(start.elapsed(), COPY_KEYS_TIMEOUT);
    for port in [22001, 22002, 22003] {
        assert_eq!(fake.calls_to(&format!("10.0.0.1:{port}")).len(), 3, "port {port}");
    }

    // copy-keys on its own (`pods copy-keys`) is bounded the same way.
    let fake = Arc::new(FakeRemote::new());
    fake.script("10.0.0.1:22003", [FakeReply::hang()]);
    let start = Instant::now();
    let err = handle_copy_keys(fake.clone(), &cfg, &keys, None, None, &everyone(), false, true)
        .await
        .unwrap_err();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(err.to_string().contains("1 pod(s) failed"), "{err}");
    assert_eq!(start.elapsed(), COPY_KEYS_TIMEOUT);
    assert_eq!(fake.calls().len(), 3);
}

#[tokio::test]
async fn create_then_setup_commands_reject_a_bad_budget_before_doing_anything() {
    // Review finding: `replace` / `migrate copy` create (and bill) a pod, wait up to
    // ~15 min for it, and only then ran setup — which is where a malformed
    // SETUP_TIMEOUT_SECS used to be caught, stranding the new pod. Now they fail first.
    // (The config has no provider keys, so getting past the check would fail
    // differently — and never reach a provider.)
    let cfg = setup_cfg("SETUP_TIMEOUT_SECS=5m");
    let ov = crate::SpecOverrides::default();
    for dry_run in [true, false] {
        let fake = Arc::new(FakeRemote::new());
        let (place, interrupt) = (crate::cli::PlaceFlags::default(), crate::Interrupt::manual());
        let opts = crate::pods::replace::ReplaceOpts { keep_old: false, skip_proxy: true, dry_run, yes: true, force: true };
        let bal = crate::money::BalanceCtx::from_config(&cfg); // no account, no state file: a test
        let err = crate::pods::replace::handle_replace(fake.clone(), &fleet(), &cfg, "apple", &ov, &place, crate::pods::adopt::Source::Create, opts, &interrupt, &bal).await.unwrap_err();
        assert!(err.to_string().contains("SETUP_TIMEOUT_SECS"), "replace (dry_run={dry_run}): {err}");
        let err = crate::pods::migrate::handle_migrate_copy(fake.clone(), &cfg, "apple", &ov, &place, crate::pods::migrate::CopyHow { source: crate::pods::adopt::Source::Create, force: false, dry_run, yes: true }, &interrupt, &bal).await.unwrap_err();
        assert!(fake.calls().is_empty(), "nothing reached a pod");
        assert!(err.to_string().contains("SETUP_TIMEOUT_SECS"), "migrate copy (dry_run={dry_run}): {err}");
    }
}

/// `pods <args…>` parsed as the CLI would.
fn pods(args: &[&str]) -> crate::cli::PodCmd {
    use clap::Parser;
    let argv = ["arena", "pods"].iter().chain(args).copied();
    match Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{args:?}: {e}")).cmd {
        Cmd::Pods(p) => p,
        _ => unreachable!(),
    }
}

/// The hosts (`ip:port`) that got any call, sorted.
fn hosts(fake: &FakeRemote) -> Vec<String> {
    let mut h: Vec<String> = fake.calls().iter().map(|c| c.host().to_string()).collect();
    h.sort();
    h.dedup();
    h
}

#[tokio::test(start_paused = true)]
async fn setup_and_copy_keys_run_on_exactly_the_selected_pods() {
    // PLAN 1.C through the commands: the shared selector decides who is provisioned /
    // gets keys — names, ranges, --exclude — and a typo touches nobody.
    let dir = keys_dir("select");
    let keys = dir.to_string_lossy().into_owned();
    let cfg = setup_cfg("MACHINE_NAME_LIST=(apple bloom cloud)\n");
    let cases: &[(&[&str], &[u16])] = &[
        (&["setup", "apple", "cloud"], &[22001, 22003]),
        (&["setup", "bloom..cloud", "--exclude", "cloud"], &[22002]),
        (&["setup", "--include", "apple"], &[22001]), // the old spelling
    ];
    for (args, ports) in cases {
        let fake = Arc::new(FakeRemote::new());
        handle_pods(pods(args), &fleet(), fake.clone(), &cfg, true).await.unwrap_or_else(|e| panic!("{args:?}: {e}"));
        let want: Vec<String> = ports.iter().map(|p| format!("10.0.0.1:{p}")).collect();
        assert_eq!(hosts(&fake), want, "{args:?}");
    }
    for (args, ports) in [
        (vec!["copy-keys", "--keys-dir", keys.as_str(), "--all", "--exclude", "bloom"], [22001, 22003]),
        (vec!["copy-keys", "--keys-dir", keys.as_str(), "apple", "--include", "cloud"], [22001, 22003]),
    ] {
        let fake = Arc::new(FakeRemote::new());
        handle_pods(pods(&args), &fleet(), fake.clone(), &cfg, true).await.unwrap_or_else(|e| panic!("{args:?}: {e}"));
        let want: Vec<String> = ports.iter().map(|p| format!("10.0.0.1:{p}")).collect();
        assert_eq!(hosts(&fake), want, "{args:?}");
    }
    for args in [&["setup", "apple", "clod"][..], &["copy-keys", "--exclude", "bloon"]] {
        let fake = Arc::new(FakeRemote::new());
        let err = handle_pods(pods(args), &fleet(), fake.clone(), &cfg, true).await.unwrap_err().to_string();
        assert!(err.contains("matched no pod") && err.contains("did you mean devtest-"), "{args:?}: {err}");
        assert!(fake.calls().is_empty(), "{args:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// `pods setup` with the defaults — the move onto the volume and the warm-up on: the move
/// runs before the config step and the warm-up last, each on its own budget; a pod whose
/// move or warm-up fails or hangs is still provisioned (and gets its keys) — the command
/// succeeds; `--no-vscode` leaves the warm-up out.
#[tokio::test(start_paused = true)]
async fn the_vscode_warm_up_runs_last_and_never_fails_setup() {
    let dir = keys_dir("vscode");
    let keys = dir.to_string_lossy().into_owned();
    let cfg = vscode_cfg("");
    let fake = Arc::new(FakeRemote::new());
    // apple: the warm-up reports a problem; bloom: it hangs; cloud: the move onto the
    // volume reports one.
    fake.script(
        "10.0.0.1:22001",
        [FakeReply::ok(), FakeReply::ok(), FakeReply::ok(), FakeReply::exit(3, "vscode warm-up incomplete: server: checksum mismatch")],
    );
    fake.script("10.0.0.1:22002", [FakeReply::ok(), FakeReply::ok(), FakeReply::ok(), FakeReply::hang()]);
    fake.script("10.0.0.1:22003", [FakeReply::ok(), FakeReply::stdout("arena-warning: repo not moved onto the /workspace volume: in use (pid 7)\n")]);
    let start = Instant::now();
    handle_setup(fake.clone(), &cfg, true, false, None, None, false, false, budgets(&cfg, None), &everyone(), &keys, None)
        .await
        .unwrap();
    assert_eq!(start.elapsed(), Duration::from_secs(300), "the hung warm-up costs its own budget, no more");
    for port in [22001, 22002, 22003] {
        let calls = fake.calls_to(&format!("10.0.0.1:{port}"));
        assert!(
            matches!(&calls[..], [RemoteCall::Copy { .. }, RemoteCall::Exec { cmd: reloc, timeout: t, .. }, RemoteCall::Exec { cmd: config, .. }, RemoteCall::Exec { cmd, timeout, .. }, RemoteCall::Exec { cmd: keys, .. }]
                if reloc.contains("arena-aside") && *t == Some(Duration::from_secs(900))
                    && config.contains("git fetch")
                    && cmd.contains("arena-vscode-warmup")
                    && cmd.contains("ARENA_VSCODE_EXTENSIONS='ms-python.python,ms-python.vscode-pylance,ms-toolsai.jupyter'")
                    && *timeout == Some(Duration::from_secs(300))
                    && keys.contains("OPENAI_API_KEY")),
            "port {port}: every pod is set up and gets its keys: {calls:?}"
        );
    }

    // --no-vscode: the step is gone.
    let fake = Arc::new(FakeRemote::new());
    handle_setup(fake.clone(), &cfg, true, false, None, None, false, true, budgets(&cfg, None), &everyone(), NO_KEYS, None)
        .await
        .unwrap();
    for port in [22001, 22002, 22003] {
        let calls = fake.calls_to(&format!("10.0.0.1:{port}"));
        assert_eq!(calls.len(), 3, "port {port}: {calls:?}");
        assert!(!format!("{calls:?}").contains("arena-vscode-warmup"));
    }
    // A bad extension list fails before any pod is touched.
    let fake = Arc::new(FakeRemote::new());
    let bad = vscode_cfg("VSCODE_EXTENSIONS=ms-python.python;reboot");
    let err = handle_setup(fake.clone(), &bad, true, false, None, None, false, false, budgets(&bad, None), &everyone(), NO_KEYS, None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("VSCODE_EXTENSIONS"), "{err}");
    assert!(fake.calls().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_dry_run_shows_what_the_warm_up_does_not_its_blob() {
    use arena_core::setup::SetupTimeouts;
    let mut sc = scfg();
    sc.vscode = arena_core::vscode::VscodeSetup::from_config(&vscode_cfg(""), &sc.repo_path).unwrap();
    sc.relocate = true;
    let steps = provisioning_steps("runpod", &sc, "devtest-apple", false, "", &SetupTimeouts::default());
    let lines: Vec<String> = steps.iter().flat_map(|s| super::step_preview(s, &target(22001))).collect();
    assert_eq!(lines.len(), 8, "{lines:#?}");
    assert_eq!(lines[0], "## copy deploy key (timeout 60s)");
    assert!(lines[1].starts_with("scp "), "{}", lines[1]);
    // The move onto the volume: what it does, not its script.
    assert_eq!(lines[2], "## repo onto volume (timeout 900s, best-effort: a failure is a warning, never a failed setup)");
    assert!(lines[3].starts_with("if /workspace is a mounted volume: move /root/ARENA_materials onto it"), "{}", lines[3]);
    assert_eq!(lines[4], "## repo + keys config (timeout 300s)");
    assert!(lines[5].starts_with("ssh ") && lines[5].contains("git fetch"), "{}", lines[5]);
    assert_eq!(lines[6], "## vscode warm-up (timeout 300s, best-effort: a failure is a warning, never a failed setup)");
    assert!(lines[7].starts_with("latest stable VS Code server") && lines[7].contains("ms-toolsai.jupyter"), "{}", lines[7]);
    assert!(lines[7].contains("/opt/arena-env/bin/python") && !lines[7].contains("base64"), "{}", lines[7]);
}

/// disk-guard: the bare-VM setup script is staged once per run (the pid in its name) and
/// removed when the run ends (`main`), never left in the temp dir.
#[test]
fn the_staged_bare_vm_script_goes_with_the_run() {
    assert_eq!(super::stage_hetzner_script(false).unwrap(), "", "nothing staged when no pod needs it");
    let staged = super::stage_hetzner_script(true).unwrap();
    assert_eq!(std::path::PathBuf::from(&staged), super::hetzner_script_path());
    assert!(staged.ends_with(&format!("arena-hetzner-setup-{}.sh", std::process::id())), "{staged}");
    // What `main` does at the end, here in a dir of this test's own.
    let dir = std::env::temp_dir().join(format!("arena-unstage-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(super::hetzner_script_in(&dir), super::HETZNER_SETUP).unwrap();
    std::fs::write(dir.join("arena-hetzner-setup-0.sh"), "another run's").unwrap();
    super::unstage_hetzner_script(&dir);
    assert!(!super::hetzner_script_in(&dir).exists() && dir.join("arena-hetzner-setup-0.sh").exists(), "only this run's");
    super::unstage_hetzner_script(&dir); // none left: nothing to do
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn setup_takes_no_vscode() {
    let parsed = pods(&["setup", "apple", "--no-vscode"]);
    assert!(matches!(parsed, crate::cli::PodCmd::Setup { no_vscode: true, .. }));
    assert!(matches!(pods(&["setup"]), crate::cli::PodCmd::Setup { no_vscode: false, .. }));
}

#[test]
fn setup_takes_a_positive_timeout() {
    use crate::cli::{Cli, Cmd, PodCmd};
    use clap::Parser;
    let parsed = Cli::try_parse_from(["arena", "pods", "setup", "apple", "--timeout", "900"]).unwrap().cmd;
    assert!(matches!(parsed, Cmd::Pods(PodCmd::Setup { timeout: Some(900), .. })));
    let parsed = Cli::try_parse_from(["arena", "pods", "setup"]).unwrap().cmd;
    assert!(matches!(parsed, Cmd::Pods(PodCmd::Setup { timeout: None, .. })));
    assert!(Cli::try_parse_from(["arena", "pods", "setup", "--timeout", "0"]).is_err());
    // Bounded above (no overflow from "no limit" spelled as a huge number).
    assert!(Cli::try_parse_from(["arena", "pods", "setup", "--timeout", "86400"]).is_ok());
    for big in ["86401", "18446744073709551615"] {
        assert!(Cli::try_parse_from(["arena", "pods", "setup", "--timeout", big]).is_err(), "{big}");
    }
}
