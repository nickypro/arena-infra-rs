use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arena_core::openrouter::{CreatedKey, KeyApi, KeyInfo};
use arena_core::provider::ContainerReset;
use arena_core::remote::{FakeRemote, FakeReply, RemoteCall, PROBE_TIMEOUT};
use arena_core::setup::name_file_command;
use arena_core::volume::{probe_command, RepoSite};
use arena_core::{Config, Error, Pod, PodSpec, Provider, Result};
use async_trait::async_trait;
use clap::Parser;

use crate::cli::{Cli, Cmd, PlaceFlags};
use crate::pods::create::SpecOverrides;
use crate::pods::home_copy::staging_dir;
use crate::pods::lifecycle::{afterwards_line, disk_fate, handle_reimage, handle_restart, handle_start, handle_terminate, no_setup_line, reprovision_line, restart_fate, stop_lines, stopped_restart_refusal, wipe_refusal, DiskFate, DiskOp, RestartOpts, SettleWait, TerminateOpts};
use crate::pods::lock::{take_would_refuse, LOCKED_REPLACEMENT_NOTE};
use crate::pods::migrate::{cutover_revert, migrate_copy_on, migrate_cutover_on, migrate_finish_on, revert_followup, stopped_old_refusal, CutoverOpts, FinishOpts, MigrateCmd, MigrateCopyJob};
use crate::pods::rename::{handle_rename, machine_name_note, plan_csv_move, rename_followup_steps, rename_report, rewrite_name_files, CsvMove, NameMissed, PlannedRename, RenameRequest};
use crate::pods::replace::{replace_on, ReplaceJob, ReplaceOpts};
use crate::pods::selection::Selected;
use super::{handle_copy_keys, handle_pods, Interrupt, KeyBook, PodCmd};

/// `pods terminate`'s switches without `--unlock` (most tests), positionally as before.
fn topts(revoke_key: bool, skip_proxy: bool, dry_run: bool, yes: bool) -> TerminateOpts {
    TerminateOpts { unlock: false, revoke_key, skip_proxy, dry_run, yes }
}

/// A fleet that records every mutation and applies renames/terminates to its listing.
struct Fleet {
    pods: Mutex<Vec<Pod>>,
    calls: Mutex<Vec<String>>,
    /// Persistent volume (GB) per pod id, as `pod_spec` reports it (absent: none).
    volumes: Vec<(&'static str, u32)>,
    /// Pod ids whose spec can't be read (an API error): volume unknown.
    no_spec: Vec<&'static str>,
    /// Pod ids whose provider rename fails (stopping a batch part-way).
    fail_rename: Vec<&'static str>,
    /// Pod ids the API holds locked — what it refuses stop/restart/terminate on with the
    /// recorded 400. Starts as the listing's locked pods; a test adds one the listing
    /// doesn't show (locked since). `set_locked` keeps both in step.
    server_locked: Mutex<Vec<String>>,
    /// Pod ids whose rename the API refuses as locked (unverified live — see `lock`).
    rename_locked: Vec<&'static str>,
    /// Pod ids whose unlock fails.
    fail_unlock: Vec<&'static str>,
    /// What `create_pod` hands out, in order (and adds to the listing); none left = the
    /// test didn't expect a create.
    to_create: Mutex<Vec<Pod>>,
    /// `list_pods` calls so far, and the calls (1-based) that list every pod WITHOUT its
    /// endpoint — RunPod's passing "running, no endpoint" glitch.
    lists: Mutex<usize>,
    blank_lists: Vec<usize>,
    /// Host maintenance windows `enrich` reports, by pod id.
    maintenance: Vec<(&'static str, arena_core::pod::Maintenance)>,
    /// What a GPU pod's restart is known to do: `Yes` as RunPod v2 (the default),
    /// `Unknown` as v1 — whose restart was seen keeping the disk (live 2026-10-09).
    restart_resets: ContainerReset,
}

impl Fleet {
    fn new(pods: Vec<Pod>) -> Self {
        let server_locked = Mutex::new(pods.iter().filter(|p| p.locked == Some(true)).map(|p| p.id.clone()).collect());
        Self {
            pods: Mutex::new(pods),
            calls: Mutex::default(),
            volumes: vec![],
            no_spec: vec![],
            fail_rename: vec![],
            server_locked,
            rename_locked: vec![],
            fail_unlock: vec![],
            to_create: Mutex::default(),
            lists: Mutex::default(),
            blank_lists: vec![],
            maintenance: vec![],
            restart_resets: ContainerReset::Yes,
        }
    }
    /// A container reset as the providers do it: the pod comes back on a NEW endpoint
    /// (port +1000), so a setup that reaches the new one ran after the reset.
    fn reset(&self, id: &str) {
        for p in self.pods.lock().unwrap().iter_mut().filter(|p| p.id == id) {
            p.ssh_port = p.ssh_port.map(|port| port + 1000);
        }
    }
    /// Refuse `ctx` on `id` as the API does a locked pod's.
    fn refuse_locked(&self, id: &str, ctx: &str) -> Result<()> {
        if self.server_locked.lock().unwrap().iter().any(|l| l == id) {
            return Err(recorded_locked(ctx));
        }
        Ok(())
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn record(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }
}

#[async_trait]
impl Provider for Fleet {
    fn name(&self) -> &'static str {
        "runpod"
    }
    fn describe(&self, _spec: &PodSpec) -> String {
        String::new()
    }
    async fn list_pods(&self) -> Result<Vec<Pod>> {
        let n = {
            let mut lists = self.lists.lock().unwrap();
            *lists += 1;
            *lists
        };
        let mut pods = self.pods.lock().unwrap().clone();
        if self.blank_lists.contains(&n) {
            for p in &mut pods {
                (p.ssh_ip, p.ssh_port) = (None, None);
            }
        }
        Ok(pods)
    }
    async fn enrich(&self, pods: &mut [Pod]) -> Result<()> {
        for p in pods.iter_mut() {
            if let Some((_, m)) = self.maintenance.iter().find(|(id, _)| *id == p.id) {
                p.maintenance = Some(m.clone());
            }
        }
        Ok(())
    }
    async fn create_pod(&self, spec: &PodSpec) -> Result<Pod> {
        let mut queue = self.to_create.lock().unwrap();
        assert!(!queue.is_empty(), "not exercised by this test: create {}", spec.name);
        let mut pod = queue.remove(0);
        pod.name = spec.name.clone();
        self.record(format!("create {} {}", spec.name, spec.gpu_type));
        self.pods.lock().unwrap().push(pod.clone());
        Ok(pod)
    }
    async fn stop_pod(&self, id: &str) -> Result<()> {
        self.refuse_locked(id, "stop pod")?;
        self.record(format!("stop {id}"));
        Ok(())
    }
    /// Like a real container reset, the restarted pod comes back on a NEW endpoint (port
    /// +1000): setup reaching the new one proves it ran after the restart, not before.
    async fn restart_pod(&self, id: &str) -> Result<()> {
        self.refuse_locked(id, "restart pod")?;
        self.record(format!("restart {id}"));
        self.reset(id);
        Ok(())
    }
    /// A stopped pod comes back running, as a fresh container on a new endpoint.
    async fn start_pod(&self, id: &str) -> Result<()> {
        self.record(format!("start {id}"));
        for p in self.pods.lock().unwrap().iter_mut().filter(|p| p.id == id) {
            p.status = "RUNNING".into();
        }
        self.reset(id);
        Ok(())
    }
    async fn reimage_pod(&self, id: &str, image: &str, _env: &[(String, String)]) -> Result<()> {
        self.record(format!("reimage {id} {image}"));
        self.reset(id);
        Ok(())
    }
    async fn terminate_pod(&self, id: &str) -> Result<()> {
        self.refuse_locked(id, "terminate pod")?;
        self.record(format!("terminate {id}"));
        self.pods.lock().unwrap().retain(|p| p.id != id);
        Ok(())
    }
    async fn set_locked(&self, id: &str, locked: bool) -> Result<()> {
        if !locked && self.fail_unlock.contains(&id) {
            return Err(Error::provider("unlock pod HTTP 500 Internal Server Error: boom"));
        }
        self.record(format!("lock {id} {locked}"));
        let mut server = self.server_locked.lock().unwrap();
        server.retain(|l| l != id);
        if locked {
            server.push(id.to_string());
        }
        for p in self.pods.lock().unwrap().iter_mut().filter(|p| p.id == id) {
            p.locked = Some(locked);
        }
        Ok(())
    }
    async fn rename_pod(&self, id: &str, new_name: &str) -> Result<()> {
        if self.fail_rename.contains(&id) {
            return Err(Error::provider("rename refused"));
        }
        if self.rename_locked.contains(&id) {
            return Err(recorded_locked("rename pod"));
        }
        self.record(format!("rename {id} {new_name}"));
        for p in self.pods.lock().unwrap().iter_mut().filter(|p| p.id == id) {
            p.name = new_name.to_string();
        }
        Ok(())
    }
    /// As the fleet routes it: the capability table's answer for the pod's provider. A `lambda`
    /// pod stands for a backend that can't rename (nor anything else).
    fn supports(&self, op: arena_core::capability::Op, pod: Option<&Pod>) -> arena_core::capability::Support {
        use arena_core::capability::{support, Backend, Support};
        match pod.map(|p| p.provider.as_str()) {
            Some("lambda") => Support::No { why: "rename not supported on provider `lambda`", instead: "rename it in its console" },
            p => p.and_then(Backend::named).map_or(Support::Yes, |b| support(b, op)),
        }
    }
    async fn pod_spec(&self, id: &str) -> Result<PodSpec> {
        if self.no_spec.contains(&id) {
            return Err(Error::provider("get pod: HTTP 500"));
        }
        let mut spec = PodSpec::from_config(&Config::parse(""));
        spec.volume_gb = self.volumes.iter().find(|(i, _)| *i == id).map_or(0, |(_, v)| *v);
        Ok(spec)
    }
    fn restart_wipes_container_disk(&self, pod: &Pod) -> bool {
        pod.provider != "hetzner" // as the fleet provider routes it
    }
    fn restart_resets_container(&self, pod: &Pod) -> ContainerReset {
        if pod.provider == "hetzner" {
            ContainerReset::No
        } else {
            self.restart_resets
        }
    }
}

fn pod(id: &str, name: &str, provider: &str, endpoint: Option<(&str, u16)>) -> Pod {
    Pod {
        id: id.into(),
        name: name.into(),
        provider: provider.into(),
        status: "RUNNING".into(),
        ssh_ip: endpoint.map(|(ip, _)| ip.to_string()),
        ssh_port: endpoint.map(|(_, port)| port),
        ..Default::default()
    }
}

/// alpha + bravo (GPU pods, reachable), charlie (no endpoint yet), delta (a Hetzner VM).
fn fleet() -> Fleet {
    Fleet::new(vec![
        pod("r-alpha", "devtest-alpha", "runpod", Some(("10.0.0.1", 22001))),
        pod("r-bravo", "devtest-bravo", "runpod", Some(("10.0.0.1", 22002))),
        pod("r-charlie", "devtest-charlie", "runpod", None),
        pod("h-delta", "devtest-delta", "hetzner", Some(("10.0.0.9", 22))),
    ])
}

/// A temp dir (per process + test), removed when dropped.
struct TmpDir(PathBuf);
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn tmpdir(tag: &str) -> TmpDir {
    let d = std::env::temp_dir().join(format!("arena-lifecycle-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    TmpDir(d)
}

/// Setup + a write-only proxy in `dir` (never reloads nginx, never SSHes anywhere). The
/// repo is in its default place, /root/r — on the container disk.
fn cfg(dir: &Path) -> Config {
    Config::parse(&cfg_text(dir))
}

fn cfg_text(dir: &Path) -> String {
    format!(
        "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(alpha bravo charlie delta echo @solo-gpu)\n\
         ARENA_REPO_OWNER=o\nARENA_REPO_NAME=r\nGIT_SSH_KEY_LOCAL=/nonexistent/devtest_deploy_key\n\
         SSH_PROXY_HOST=localhost\nSSH_PROXY_NGINX_CONFIG_PATH={}\nSSH_PROXY_RELOAD_CMD=\"\"\nVSCODE_PREINSTALL=0\nREPO_ON_VOLUME=0\n",
        dir.join("proxy.conf").display()
    )
}

fn proxy_file(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("proxy.conf")).unwrap_or_default()
}

/// The cohort keys file plus the `openrouter_api_keys.csv` symlink to it, as `keys gen`
/// leaves them. Returns the symlink (what the commands are pointed at).
fn keys_csv(dir: &Path, rows: &str) -> PathBuf {
    std::fs::write(dir.join("devtest_openrouter_keys.csv"), format!("# OpenRouter API keys — arena iteration: devtest\n{rows}"))
        .unwrap();
    let link = dir.join("openrouter_api_keys.csv");
    std::os::unix::fs::symlink("devtest_openrouter_keys.csv", &link).unwrap();
    link
}

fn cohort_csv(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("devtest_openrouter_keys.csv")).unwrap()
}

/// The provisioning API: keys by name, recording creates/deletes/renames; deleting a
/// key whose name is in `broken` fails. Shared with `selection_tests` (`keys` commands).
#[derive(Default)]
pub(crate) struct FakeKeys {
    pub(crate) keys: Mutex<Vec<KeyInfo>>,
    pub(crate) calls: Mutex<Vec<String>>,
    pub(crate) broken: Vec<&'static str>,
}

impl FakeKeys {
    pub(crate) fn with(names: &[(&str, &str)]) -> Self {
        let keys = names
            .iter()
            .map(|(hash, name)| KeyInfo {
                hash: hash.to_string(),
                name: Some(name.to_string()),
                limit: Some(5.0),
                ..Default::default()
            })
            .collect();
        Self { keys: Mutex::new(keys), ..Default::default() }
    }
    pub(crate) fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    pub(crate) fn names(&self) -> Vec<String> {
        let mut n: Vec<String> = self.keys.lock().unwrap().iter().filter_map(|k| k.name.clone()).collect();
        n.sort();
        n
    }
}

#[async_trait]
impl KeyApi for FakeKeys {
    async fn list_keys(&self) -> Result<Vec<KeyInfo>> {
        Ok(self.keys.lock().unwrap().clone())
    }
    async fn create_key(&self, name: &str, limit: Option<f64>) -> Result<CreatedKey> {
        self.calls.lock().unwrap().push(format!("create {name}"));
        let hash = format!("h-new-{name}");
        let info = KeyInfo { hash: hash.clone(), name: Some(name.into()), limit, ..Default::default() };
        self.keys.lock().unwrap().push(info);
        Ok(CreatedKey { secret: format!("sk-new-{name}"), hash, name: name.into() })
    }
    async fn delete_key(&self, hash: &str) -> Result<()> {
        self.calls.lock().unwrap().push(format!("delete {hash}"));
        let mut keys = self.keys.lock().unwrap();
        if keys.iter().any(|k| k.hash == hash && self.broken.contains(&k.name.as_deref().unwrap_or(""))) {
            return Err(Error::provider("openrouter delete key: HTTP 500"));
        }
        keys.retain(|k| k.hash != hash);
        Ok(())
    }
    async fn rename_key(&self, hash: &str, new_name: &str) -> Result<()> {
        self.calls.lock().unwrap().push(format!("rename {hash} {new_name}"));
        for k in self.keys.lock().unwrap().iter_mut().filter(|k| k.hash == hash) {
            k.name = Some(new_name.to_string());
        }
        Ok(())
    }
}

fn no_api(csv: &Path) -> KeyBook<'static> {
    KeyBook { csv: csv.to_path_buf(), api: None }
}

fn one(old: &str, new: &str) -> RenameRequest {
    RenameRequest::One { old: old.into(), new: new.into() }
}

fn name_write(host: &str, short: &str) -> RemoteCall {
    RemoteCall::Exec { host: host.into(), cmd: name_file_command(short), timeout: Some(PROBE_TIMEOUT) }
}

#[tokio::test]
async fn rename_rewrites_name_file_moves_the_key_and_syncs_the_proxy() {
    let dir = tmpdir("rename-one");
    let (cfg, f, remote) = (cfg(&dir.0), fleet(), Arc::new(FakeRemote::new()));
    let csv = keys_csv(&dir.0, "devtest-alpha,sk-or-a\ndevtest-bravo,sk-or-b\n");
    let keys = FakeKeys::with(&[("h-a", "devtest-alpha"), ("h-b", "devtest-bravo"), ("h-x", "other-cohort-x")]);
    let book = KeyBook::new(&csv, Some(&keys));
    handle_rename(&f, remote.clone(), &cfg, &book, one("alpha", "echo"), false, false, true).await.unwrap();
    assert_eq!(f.calls(), ["rename r-alpha devtest-echo"]);
    // ~/.name: rewritten on alpha's host only, with setup's exact line and the SHORT name.
    assert_eq!(remote.calls(), [name_write("10.0.0.1:22001", "echo")]);
    // The CSV row moved in the cohort file the symlink points at (the link kept); the
    // key itself and the other rows are untouched.
    let text = cohort_csv(&dir.0);
    assert!(text.contains("devtest-echo,sk-or-a") && text.contains("devtest-bravo,sk-or-b"), "{text}");
    assert!(!text.contains("devtest-alpha") && text.starts_with("# OpenRouter API keys"), "{text}");
    assert!(std::fs::symlink_metadata(&csv).unwrap().file_type().is_symlink());
    // OpenRouter: the same key, renamed in place (no delete, no re-mint).
    assert_eq!(keys.calls(), ["rename h-a devtest-echo"]);
    assert_eq!(keys.names(), ["devtest-bravo", "devtest-echo", "other-cohort-x"]);
    // The proxy followed: the new name has a forward, the old one is gone.
    let px = proxy_file(&dir.0);
    assert!(px.contains("name=devtest-echo") && !px.contains("name=devtest-alpha"), "{px}");
}

#[tokio::test]
async fn an_unreachable_pod_is_a_warning_not_a_failure_and_absolute_names_work() {
    let dir = tmpdir("rename-unreachable");
    let cfg = cfg(&dir.0);
    let csv = keys_csv(&dir.0, "devtest-charlie,sk-or-c\n");
    // charlie has no endpoint: renamed anyway (to an absolute `@solo-gpu` entry), no SSH
    // attempted; no provisioning key → OpenRouter skipped quietly, the CSV still moves.
    let (f, remote) = (fleet(), Arc::new(FakeRemote::new()));
    handle_rename(&f, remote.clone(), &cfg, &no_api(&csv), one("charlie", "solo-gpu"), true, false, true).await.unwrap();
    assert_eq!(f.calls(), ["rename r-charlie solo-gpu"]);
    assert!(remote.calls().is_empty());
    assert!(cohort_csv(&dir.0).contains("solo-gpu,sk-or-c"));
    // A reachable pod whose ssh fails: still renamed, still Ok — reported for a re-setup.
    let (f, remote) = (fleet(), Arc::new(FakeRemote::new()));
    remote.script("10.0.0.1:22002", [FakeReply::exit(255, "ssh: connect to host 10.0.0.1 port 22002: Connection refused")]);
    handle_rename(&f, remote.clone(), &cfg, &no_api(&csv), one("bravo", "@solo-gpu"), true, false, true).await.unwrap();
    assert_eq!(f.calls(), ["rename r-bravo solo-gpu"]);
    // An absolute name is its own short name.
    assert_eq!(remote.calls(), [name_write("10.0.0.1:22002", "solo-gpu")]);
    // What rename hands its summary for both: exactly these pods, with why.
    let (f, remote) = (fleet(), Arc::new(FakeRemote::new()));
    remote.script("10.0.0.1:22002", [FakeReply::exit(255, "ssh: connect to host 10.0.0.1 port 22002: Connection refused")]);
    let pods = f.pods.lock().unwrap().clone();
    let plan = [
        PlannedRename { id: "r-charlie".into(), old: "devtest-charlie".into(), new: "solo-gpu".into() },
        PlannedRename { id: "r-bravo".into(), old: "devtest-bravo".into(), new: "devtest-echo".into() },
        PlannedRename { id: "r-alpha".into(), old: "devtest-alpha".into(), new: "devtest-delta".into() },
    ];
    let done: Vec<&PlannedRename> = plan.iter().collect();
    let mut missed = rewrite_name_files(remote.clone(), &cfg, "devtest", &done, &pods).await;
    missed.name_file.sort();
    assert_eq!(
        missed.name_file,
        [
            ("devtest-echo".to_string(), "exit 255: ssh: connect to host 10.0.0.1 port 22002: Connection refused".to_string()),
            ("solo-gpu".to_string(), "no SSH endpoint".to_string()),
        ]
    );
    assert!(missed.hostname.is_empty(), "RunPod pods aren't named by their hostname: {missed:?}");
    // What the operator is told about them.
    let note = machine_name_note(&done, &pods);
    let report = rename_report(note.as_deref(), &missed, &["devtest-echo already has its own OpenRouter key".into()]);
    assert!(report[0].starts_with("note: each renamed pod's MACHINE_NAME env var keeps the old name") && report[0].contains("reimage"), "{report:?}");
    assert!(report[1].contains("re-run `arena pods setup devtest-echo solo-gpu`") && report[1].contains("no SSH endpoint"), "{report:?}");
    assert_eq!(report[2], "⚠ keys: devtest-echo already has its own OpenRouter key");
    assert!(rename_report(None, &NameMissed::default(), &[]).is_empty(), "nothing to say");
}

/// Prod 2026-10-30, `pods rename arena9-rainbow keepsake` on a Hetzner VM: its prompt, login
/// banner and `~/.name` fallback come from the hostname, so the follow-up sets that too —
/// after `~/.name`, on the VM alone, with setup's own command — and a miss is reported like
/// `~/.name`'s, with the command that fixes it; never fatal. A RunPod pod's hostname is its
/// container id: left alone.
#[tokio::test]
async fn a_hetzner_rename_sets_the_hostname_too() {
    use arena_core::setup::hostname_command;
    let dir = tmpdir("rename-hostname");
    let cfg = cfg(&dir.0);
    let csv = keys_csv(&dir.0, "");
    let (f, remote) = (fleet(), Arc::new(FakeRemote::new()));
    handle_rename(&f, remote.clone(), &cfg, &no_api(&csv), one("delta", "echo"), true, false, true).await.unwrap();
    assert_eq!(f.calls(), ["rename h-delta devtest-echo"]);
    let hostname = RemoteCall::Exec {
        host: "10.0.0.9:22".into(),
        cmd: hostname_command("devtest-echo").unwrap(),
        timeout: Some(PROBE_TIMEOUT),
    };
    assert_eq!(remote.calls(), [name_write("10.0.0.9:22", "echo"), hostname]);
    // The hostname can't be set: renamed anyway, reported with what to run.
    let (f, remote) = (fleet(), Arc::new(FakeRemote::new()));
    remote.script("10.0.0.9:22", [FakeReply::ok(), FakeReply::exit(1, "Could not set static hostname: Access denied")]);
    let pods = f.pods.lock().unwrap().clone();
    let plan = [
        PlannedRename { id: "h-delta".into(), old: "devtest-delta".into(), new: "devtest-echo".into() },
        PlannedRename { id: "r-alpha".into(), old: "devtest-alpha".into(), new: "devtest-bravo".into() },
    ];
    let done: Vec<&PlannedRename> = plan.iter().collect();
    let missed = rewrite_name_files(remote.clone(), &cfg, "devtest", &done, &pods).await;
    assert!(missed.name_file.is_empty(), "{missed:?}");
    assert_eq!(missed.hostname, [("devtest-echo".to_string(), "exit 1: Could not set static hostname: Access denied".to_string())]);
    let report = rename_report(None, &missed, &[]);
    assert_eq!(
        report,
        ["⚠ the hostname is still the old name on devtest-echo (devtest-echo: exit 1: Could not set static hostname: Access \
          denied) — re-run `arena pods setup devtest-echo` once it answers"]
    );
    // No endpoint at all: both are said.
    let mut gone = pods.clone();
    gone.iter_mut().filter(|p| p.id == "h-delta").for_each(|p| p.ssh_ip = None);
    let missed = rewrite_name_files(Arc::new(FakeRemote::new()), &cfg, "devtest", &done[..1], &gone).await;
    let why = ("devtest-echo".to_string(), "no SSH endpoint".to_string());
    assert_eq!((missed.name_file, missed.hostname), (vec![why.clone()], vec![why]));
    // The dry run and prompt say it, for the Hetzner VM only.
    let steps = rename_followup_steps("devtest", &plan, &pods, &no_api(&csv), true).join("\n");
    assert!(steps.contains("and set the hostname (a Hetzner VM is named by it; kept across a reboot): devtest-echo\n"), "{steps}");
}

/// The MACHINE_NAME note is about a RunPod or Vast container's env var: said for those, and
/// only those, by name in a mixed batch — never for a Hetzner (or Shadeform) VM, which has
/// none (prod 2026-10-30 printed it for a Hetzner rename). A pod not listed gets it: extra
/// only.
#[test]
fn the_machine_name_note_is_for_pods_with_that_env_var() {
    let pods = [
        pod("r1", "devtest-alpha", "runpod", None),
        pod("v1", "devtest-bravo", "vast", None),
        pod("h1", "devtest-charlie", "hetzner", None),
        pod("s1", "devtest-delta", "shadeform", None),
    ];
    let plan: Vec<PlannedRename> =
        [("r1", "devtest-echo"), ("v1", "devtest-foxtrot"), ("h1", "devtest-golf"), ("s1", "devtest-hotel"), ("x1", "devtest-india")]
            .iter()
            .map(|(id, new)| PlannedRename { id: id.to_string(), old: String::new(), new: new.to_string() })
            .collect();
    let note = |ids: &[&str]| {
        let renamed: Vec<&PlannedRename> = plan.iter().filter(|r| ids.contains(&r.id.as_str())).collect();
        machine_name_note(&renamed, &pods)
    };
    assert_eq!(note(&["h1"]), None);
    assert_eq!(note(&["h1", "s1"]), None);
    let all = note(&["r1", "v1"]).unwrap();
    assert!(all.starts_with("note: each renamed pod's MACHINE_NAME env var keeps the old name — env only changes with a reimage"), "{all}");
    let mixed = note(&["r1", "h1", "x1"]).unwrap();
    assert!(mixed.starts_with("note: the MACHINE_NAME env var of devtest-echo, devtest-india keeps the old name"), "{mixed}");
}

/// A pod whose backend can't rename ([`Provider::supports`]) is refused before
/// anything is asked, renamed or reached — the whole batch, as `plan_renames`' problems are.
#[tokio::test]
async fn a_pod_its_backend_cannot_rename_is_refused_up_front() {
    let dir = tmpdir("rename-unsupported");
    let cfg = cfg(&dir.0);
    let csv = keys_csv(&dir.0, "");
    let f = Fleet::new(vec![
        pod("r-alpha", "devtest-alpha", "runpod", Some(("10.0.0.1", 22001))),
        pod("l-bravo", "devtest-bravo", "lambda", Some(("10.0.0.2", 22))),
    ]);
    let remote = Arc::new(FakeRemote::new());
    let e = handle_rename(&f, remote.clone(), &cfg, &no_api(&csv), one("bravo", "echo"), true, false, true).await.unwrap_err();
    assert_eq!(e.to_string(), "refusing to rename anything:\n  devtest-bravo (lambda): rename not supported on provider `lambda` — rename it in its console");
    // The dry run says so too.
    let e = handle_rename(&f, remote.clone(), &cfg, &no_api(&csv), one("bravo", "echo"), true, true, true).await.unwrap_err();
    assert!(e.to_string().contains("rename not supported on provider `lambda`"), "{e}");
    assert!(f.calls().is_empty() && remote.calls().is_empty());
    // Another pod in the same fleet is renamed as ever.
    handle_rename(&f, remote.clone(), &cfg, &no_api(&csv), one("alpha", "echo"), true, false, true).await.unwrap();
    assert_eq!(f.calls(), ["rename r-alpha devtest-echo"]);
}

#[tokio::test]
async fn from_prefix_moves_every_pod_its_name_file_and_its_key() {
    let dir = tmpdir("rename-prefix");
    let cfg = cfg(&dir.0);
    let f = Fleet::new(vec![
        pod("a", "arena8-alpha", "runpod", Some(("10.0.0.1", 22001))),
        pod("b", "arena8-bravo", "runpod", Some(("10.0.0.1", 22002))),
        pod("c", "arena8-charlie", "runpod", Some(("10.0.0.1", 22003))),
    ]);
    let remote = Arc::new(FakeRemote::new());
    let csv = keys_csv(&dir.0, "arena8-alpha,sk-a\narena8-bravo,sk-b\narena8-charlie,sk-c\nsolo-gpu,sk-s\n");
    // charlie's new name already has a (stale) key on OpenRouter: its mapping is left
    // alone on BOTH sides, so the CSV and OpenRouter never disagree.
    let keys = FakeKeys::with(&[("h-a", "arena8-alpha"), ("h-b", "arena8-bravo"), ("h-c", "arena8-charlie"), ("h-old", "devtest-charlie")]);
    let book = KeyBook::new(&csv, Some(&keys));
    handle_rename(&f, remote.clone(), &cfg, &book, RenameRequest::FromPrefix("arena8".into()), false, false, true)
        .await
        .unwrap();
    assert_eq!(f.calls(), ["rename a devtest-alpha", "rename b devtest-bravo", "rename c devtest-charlie"]);
    let mut writes = remote.calls();
    writes.sort_by(|x, y| x.host().cmp(y.host()));
    assert_eq!(
        writes,
        [name_write("10.0.0.1:22001", "alpha"), name_write("10.0.0.1:22002", "bravo"), name_write("10.0.0.1:22003", "charlie")]
    );
    let rows = arena_core::apikeys::parse_csv(&cohort_csv(&dir.0));
    let hosts: Vec<&str> = rows.iter().map(|(h, _)| h.as_str()).collect();
    assert_eq!(hosts, ["devtest-alpha", "devtest-bravo", "arena8-charlie", "solo-gpu"]);
    assert_eq!(keys.calls(), ["rename h-a devtest-alpha", "rename h-b devtest-bravo"]);
    let px = proxy_file(&dir.0);
    assert!(["devtest-alpha", "devtest-bravo", "devtest-charlie"].iter().all(|n| px.contains(&format!("name={n}"))), "{px}");
}

#[tokio::test]
async fn after_a_prefix_change_the_rows_move_into_the_current_cohorts_keys_file() {
    // The link still points at the OLD cohort's file (keys were minted as arena8). The
    // renamed rows must land in devtest's file, with the link pointing at it — else the
    // next `keys gen` (which repoints the link and skips keys that exist by name) would
    // leave these machines with no deliverable key.
    let dir = tmpdir("rename-cross");
    let cfg = cfg(&dir.0);
    let old_file = dir.0.join("arena8_openrouter_keys.csv");
    std::fs::write(&old_file, "# OpenRouter API keys — arena iteration: arena8\narena8-alpha,sk-a\narena8-bravo,sk-b\nsolo-gpu,sk-s\narena8-gone,sk-g\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&old_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let link = dir.0.join("openrouter_api_keys.csv");
    std::os::unix::fs::symlink("arena8_openrouter_keys.csv", &link).unwrap();
    let f = Fleet::new(vec![
        pod("a", "arena8-alpha", "runpod", Some(("10.0.0.1", 22001))),
        pod("b", "arena8-bravo", "runpod", Some(("10.0.0.1", 22002))),
        pod("s", "solo-gpu", "vast", None),
    ]);
    let keys = FakeKeys::with(&[("h-a", "arena8-alpha"), ("h-b", "arena8-bravo")]);
    let book = KeyBook::new(&link, Some(&keys));
    // The dry run says where the rows go.
    let pods = f.pods.lock().unwrap().clone();
    let plan = vec![
        PlannedRename { id: "a".into(), old: "arena8-alpha".into(), new: "devtest-alpha".into() },
        PlannedRename { id: "b".into(), old: "arena8-bravo".into(), new: "devtest-bravo".into() },
    ];
    let steps = rename_followup_steps("devtest", &plan, &pods, &book, true).join("\n");
    assert!(steps.contains("move rows out of the previous cohort's file into") && steps.contains("devtest_openrouter_keys.csv"), "{steps}");
    assert!(steps.contains("arena8-alpha → devtest-alpha, arena8-bravo → devtest-bravo (carrying along: solo-gpu)"), "{steps}");
    handle_rename(&f, Arc::new(FakeRemote::new()), &cfg, &book, RenameRequest::FromPrefix("arena8".into()), true, false, true)
        .await
        .unwrap();
    // The current cohort's file: the moved rows + the other live pod's row, seeded header.
    let new_text = std::fs::read_to_string(dir.0.join("devtest_openrouter_keys.csv")).unwrap();
    assert!(new_text.starts_with("# OpenRouter API keys — arena iteration: devtest\n"), "{new_text}");
    let rows = arena_core::apikeys::parse_csv(&new_text);
    assert_eq!(rows, [("devtest-alpha".into(), "sk-a".into()), ("devtest-bravo".into(), "sk-b".into()), ("solo-gpu".to_string(), "sk-s".to_string())]);
    // ...which copy-keys now reads through the link; a new file keeps the old one's mode.
    assert_eq!(std::fs::read_link(&link).unwrap(), PathBuf::from("devtest_openrouter_keys.csv"));
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.0.join("devtest_openrouter_keys.csv")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    // The old file loses only the moved rows (the gone pod's record stays with its cohort).
    let old_text = std::fs::read_to_string(&old_file).unwrap();
    let old_rows: Vec<String> = arena_core::apikeys::parse_csv(&old_text).into_iter().map(|(h, _)| h).collect();
    assert_eq!(old_rows, ["solo-gpu", "arena8-gone"]);
    assert_eq!(keys.calls(), ["rename h-a devtest-alpha", "rename h-b devtest-bravo"]);
}

#[tokio::test]
async fn rename_moves_a_key_named_the_keys_gen_way() {
    // vast-777 is off the prefix: its key is devtest-vast-777 (as `keys gen` named it).
    let dir = tmpdir("rename-keyname");
    let cfg = cfg(&dir.0);
    let csv = keys_csv(&dir.0, "vast-777,sk-v\n");
    let keys = FakeKeys::with(&[("h-v", "devtest-vast-777")]);
    let f = Fleet::new(vec![pod("v-777", "vast-777", "vast", None)]);
    handle_rename(&f, Arc::new(FakeRemote::new()), &cfg, &KeyBook::new(&csv, Some(&keys)), one("vast-777", "echo"), true, false, true)
        .await
        .unwrap();
    assert_eq!(keys.calls(), ["rename h-v devtest-echo"]);
    assert!(cohort_csv(&dir.0).contains("devtest-echo,sk-v"));
}

#[test]
fn csv_moves_in_place_or_into_the_current_cohort_file() {
    let src = "# h\na,sk-a\nb,sk-b\nx,sk-x\ngone,sk-g\n";
    let live = ["A".to_string(), "B".into(), "x".into()];
    // In place (the link already points at this cohort's file): rows renamed there.
    let mv = plan_csv_move(src, None, &[("a", "A"), ("b", "x")], &live);
    assert_eq!(mv.dst.as_deref(), Some("# h\nA,sk-a\nb,sk-b\nx,sk-x\ngone,sk-g\n"));
    assert_eq!((mv.moved, mv.taken, mv.src), (vec!["a → A".to_string()], vec!["x".to_string()], None));
    // Cross-cohort: moved into dst (taken out of src), other live rows carried (copied).
    let mv = plan_csv_move(src, Some("# new\n"), &[("a", "A"), ("b", "B"), ("nope", "N")], &live);
    assert_eq!(
        mv,
        CsvMove {
            dst: Some("# new\nA,sk-a\nB,sk-b\nx,sk-x\n".into()),
            src: Some("# h\nx,sk-x\ngone,sk-g\n".into()),
            moved: vec!["a → A".into(), "b → B".into()],
            taken: vec![],
            carried: vec!["x".into()],
        }
    );
    // A new name with a row already (either file): left alone; dst rows never overwritten.
    let mv = plan_csv_move(src, Some("# new\nA,sk-other\nx,sk-x2\n"), &[("a", "A"), ("b", "B")], &live);
    assert_eq!(mv.taken, ["A"]);
    assert_eq!(mv.dst.as_deref(), Some("# new\nA,sk-other\nx,sk-x2\nB,sk-b\n"));
    assert!(mv.carried.is_empty());
    // Nothing of ours in the old file: nothing written anywhere (the link stays put).
    assert_eq!(plan_csv_move(src, Some("# new\n"), &[("q", "Q")], &live), CsvMove::default());
    assert_eq!(plan_csv_move(src, None, &[("q", "Q")], &live), CsvMove::default());
}

#[tokio::test]
async fn a_batch_that_stops_part_way_still_moves_what_it_renamed_but_not_the_proxy() {
    let dir = tmpdir("rename-partial");
    let cfg = cfg(&dir.0);
    let mut f = Fleet::new(vec![
        pod("a", "arena8-alpha", "runpod", Some(("10.0.0.1", 22001))),
        pod("b", "arena8-bravo", "runpod", Some(("10.0.0.1", 22002))),
    ]);
    f.fail_rename = vec!["b"];
    let remote = Arc::new(FakeRemote::new());
    let csv = keys_csv(&dir.0, "arena8-alpha,sk-a\narena8-bravo,sk-b\n");
    let keys = FakeKeys::with(&[("h-a", "arena8-alpha"), ("h-b", "arena8-bravo")]);
    let book = KeyBook::new(&csv, Some(&keys));
    let e = handle_rename(&f, remote.clone(), &cfg, &book, RenameRequest::FromPrefix("arena8".into()), false, false, true)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("arena8-bravo → devtest-bravo failed") && e.contains("1 of 2 rename(s) done"), "{e}");
    // alpha (renamed) got its ~/.name and key; bravo (not renamed) kept its own.
    assert_eq!(remote.calls(), [name_write("10.0.0.1:22001", "alpha")]);
    assert!(cohort_csv(&dir.0).contains("devtest-alpha,sk-a\narena8-bravo,sk-b"));
    assert_eq!(keys.calls(), ["rename h-a devtest-alpha"]);
    // No proxy write on a partial prefix batch (it would drop the arena8-* forwards).
    assert_eq!(proxy_file(&dir.0), "");
}

#[test]
fn rename_dry_run_lists_every_follow_up_step() {
    let dir = tmpdir("rename-steps");
    let f = fleet();
    let pods = f.pods.lock().unwrap().clone();
    let plan = vec![
        PlannedRename { id: "r-alpha".into(), old: "devtest-alpha".into(), new: "devtest-echo".into() },
        PlannedRename { id: "r-charlie".into(), old: "devtest-charlie".into(), new: "solo-gpu".into() },
    ];
    let missing = dir.0.join("openrouter_api_keys.csv");
    let steps = rename_followup_steps("devtest", &plan, &pods, &no_api(&missing), false).join("\n");
    assert!(steps.contains("1. rewrite ~/.name") && steps.contains("devtest-echo ← 'echo'") && steps.contains("solo-gpu ← 'solo-gpu'"), "{steps}");
    assert!(steps.contains("no SSH endpoint right now") && steps.contains(": solo-gpu"), "{steps}");
    assert!(steps.contains("2. OpenRouter key mapping: no ") && steps.contains("no OPENROUTER_PROVISIONING_KEY"), "{steps}");
    assert!(steps.contains("3. redeploy the proxy"), "{steps}");
    // With a CSV and the API: what moves, what's left alone, and the key rename.
    let csv = keys_csv(&dir.0, "devtest-alpha,sk-a\ndevtest-charlie,sk-c\nsolo-gpu,sk-s\n");
    let keys = FakeKeys::default();
    let steps = rename_followup_steps("devtest", &plan, &pods, &KeyBook::new(&csv, Some(&keys)), true).join("\n");
    assert!(steps.contains("move rows in") && steps.contains("devtest-alpha → devtest-echo"), "{steps}");
    assert!(steps.contains("already have their own row (left alone): solo-gpu"), "{steps}");
    assert!(steps.contains("rename each machine's key on OpenRouter") && steps.contains("proxy NOT synced"), "{steps}");
    assert!(!steps.contains("sk-"), "never prints a key: {steps}");
    assert!(keys.calls().is_empty(), "a preview makes no API call");
}

#[tokio::test]
async fn terminate_revoke_key_revokes_and_drops_the_row_with_or_without_a_key() {
    let dir = tmpdir("terminate-revoke");
    let cfg = cfg(&dir.0);
    let csv = keys_csv(&dir.0, "devtest-alpha,sk-a\ndevtest-bravo,sk-b\n");
    let keys = FakeKeys::with(&[("h-a", "devtest-alpha"), ("h-a2", "devtest-alpha"), ("h-b", "devtest-bravo")]);
    let book = KeyBook::new(&csv, Some(&keys));
    let f = fleet();
    handle_terminate(&f, &cfg, &book, Some("alpha"), topts(true, true, false, true)).await.unwrap();
    assert_eq!(f.calls(), ["terminate r-alpha"]);
    // Every key named after the machine is revoked; its row dropped, the others kept.
    assert_eq!(keys.calls(), ["delete h-a", "delete h-a2"]);
    assert_eq!(cohort_csv(&dir.0), "# OpenRouter API keys — arena iteration: devtest\ndevtest-bravo,sk-b\n");
    // A machine with no key and no row: terminated, nothing to revoke — not a failure.
    handle_terminate(&f, &cfg, &book, Some("charlie"), topts(true, true, false, true)).await.unwrap();
    assert_eq!(f.calls(), ["terminate r-alpha", "terminate r-charlie"]);
    assert_eq!(keys.calls().len(), 2);
    // A dry run touches nothing, on either side.
    handle_terminate(&f, &cfg, &book, Some("bravo"), topts(true, true, true, true)).await.unwrap();
    assert_eq!((f.calls().len(), keys.calls().len()), (2, 2));
    // Without --revoke-key the key and row stay.
    handle_terminate(&f, &cfg, &book, Some("bravo"), topts(false, true, false, true)).await.unwrap();
    assert!(cohort_csv(&dir.0).contains("devtest-bravo,sk-b") && keys.names() == ["devtest-bravo"]);
}

#[tokio::test]
async fn a_failed_revoke_never_blocks_terminate_but_fails_the_exit() {
    let dir = tmpdir("terminate-all");
    let cfg = cfg(&dir.0);
    let csv = keys_csv(&dir.0, "devtest-alpha,sk-a\ndevtest-bravo,sk-b\n");
    let keys = FakeKeys { broken: vec!["devtest-bravo"], ..FakeKeys::with(&[("h-a", "devtest-alpha"), ("h-b", "devtest-bravo")]) };
    let book = KeyBook::new(&csv, Some(&keys));
    let f = fleet();
    super::sync_proxy(&cfg, &f, "test setup").await; // forwards for the fleet, to see them go
    assert!(proxy_file(&dir.0).contains("name=devtest-alpha"));
    let e = handle_terminate(&f, &cfg, &book, None, topts(true, false, false, true)).await.unwrap_err().to_string();
    // Every pod went, bravo's revoke failed: named, non-zero, and its row is kept as the
    // record that the key still works.
    assert_eq!(f.calls().len(), 4, "{:?}", f.calls());
    assert!(e.contains("revoking the OpenRouter key failed for devtest-bravo") && e.contains("ARE terminated"), "{e}");
    assert!(e.contains("arena keys revoke devtest-bravo"), "{e}");
    let text = cohort_csv(&dir.0);
    assert!(!text.contains("devtest-alpha") && text.contains("devtest-bravo,sk-b"), "{text}");
    assert_eq!(keys.names(), ["devtest-bravo"]);
    // The proxy was still synced after the terminates (the revoke failure came after).
    let px = proxy_file(&dir.0);
    assert!(!px.contains("name=devtest-alpha") && !px.contains("name=devtest-bravo"), "{px}");
}

#[tokio::test]
async fn revoke_key_without_a_provisioning_key_is_refused_before_anything() {
    let dir = tmpdir("terminate-nokey");
    let f = fleet();
    let e = handle_terminate(&f, &cfg(&dir.0), &no_api(&dir.0.join("k.csv")), Some("alpha"), topts(true, true, false, true))
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("--revoke-key needs OPENROUTER_PROVISIONING_KEY"), "{e}");
    assert!(f.calls().is_empty());
    // Through the command line too (no OPENROUTER_PROVISIONING_KEY in this config).
    let r = handle_pods(pods_cmd(&["terminate", "alpha", "--revoke-key"]), &f, Arc::new(FakeRemote::new()), &cfg(&dir.0), true).await;
    assert!(r.unwrap_err().to_string().contains("OPENROUTER_PROVISIONING_KEY"));
    assert!(f.calls().is_empty());
}

#[tokio::test]
async fn a_name_another_pod_still_holds_keeps_its_key() {
    // A double create left two pods named devtest-alpha. Removing the extra one by id
    // (as the ambiguity error advises) must not revoke the key the survivor uses.
    let dir = tmpdir("terminate-twin");
    let cfg = cfg(&dir.0);
    let csv = keys_csv(&dir.0, "devtest-alpha,sk-a\n");
    let keys = FakeKeys::with(&[("h-a", "devtest-alpha")]);
    let book = KeyBook::new(&csv, Some(&keys));
    let twins = || {
        let f = fleet();
        f.pods.lock().unwrap().push(pod("r-alpha-dup", "devtest-alpha", "runpod", Some(("10.0.0.1", 22009))));
        f
    };
    let f = twins();
    handle_terminate(&f, &cfg, &book, Some("r-alpha-dup"), topts(true, true, true, true)).await.unwrap(); // dry run
    assert!(f.calls().is_empty() && keys.calls().is_empty());
    handle_terminate(&f, &cfg, &book, Some("r-alpha-dup"), topts(true, true, false, true)).await.unwrap();
    assert_eq!(f.calls(), ["terminate r-alpha-dup"]);
    assert!(keys.calls().is_empty(), "the survivor's key is kept: {:?}", keys.calls());
    assert!(cohort_csv(&dir.0).contains("devtest-alpha,sk-a"), "and so is its row");
    // --all with both twins going: the shared key is revoked ONCE (not a second delete
    // of a hash already gone, which the real API would fail).
    let f = twins();
    handle_terminate(&f, &cfg, &book, None, topts(true, true, false, true)).await.unwrap();
    assert_eq!(keys.calls(), ["delete h-a"]);
    assert!(!cohort_csv(&dir.0).contains("devtest-alpha"));
}

#[tokio::test]
async fn revoke_finds_the_key_by_the_name_keys_gen_gave_it() {
    // Vast's fallback name `vast-777` is off the prefix: `keys gen` named its key
    // devtest-vast-777 (key_name), with the CSV row under the pod name.
    let dir = tmpdir("terminate-keyname");
    let cfg = cfg(&dir.0);
    let csv = keys_csv(&dir.0, "vast-777,sk-v\ndevtest-bravo,sk-b\n");
    let keys = FakeKeys::with(&[("h-v", "devtest-vast-777"), ("h-b", "devtest-bravo")]);
    let book = KeyBook::new(&csv, Some(&keys));
    let f = Fleet::new(vec![pod("v-777", "vast-777", "vast", None)]);
    handle_terminate(&f, &cfg, &book, Some("vast-777"), topts(true, true, false, true)).await.unwrap();
    assert_eq!(keys.calls(), ["delete h-v"]);
    assert_eq!(cohort_csv(&dir.0), "# OpenRouter API keys — arena iteration: devtest\ndevtest-bravo,sk-b\n");
}

#[tokio::test]
async fn a_csv_row_whose_key_is_already_gone_is_dropped_quietly() {
    // `keys revoke` leaves the CSV as-is; terminating the pod later finds no key under
    // its name (the full listing) and drops the dead row — not a failure.
    let dir = tmpdir("terminate-row-only");
    let cfg = cfg(&dir.0);
    let csv = keys_csv(&dir.0, "devtest-alpha,sk-dead\ndevtest-bravo,sk-b\n");
    let keys = FakeKeys::with(&[("h-b", "devtest-bravo")]);
    let book = KeyBook::new(&csv, Some(&keys));
    let f = fleet();
    handle_terminate(&f, &cfg, &book, Some("alpha"), topts(true, true, false, true)).await.unwrap();
    assert!(keys.calls().is_empty());
    assert_eq!(cohort_csv(&dir.0), "# OpenRouter API keys — arena iteration: devtest\ndevtest-bravo,sk-b\n");
}

#[tokio::test]
async fn a_csv_that_cant_be_rewritten_is_not_reported_as_a_failed_revoke() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tmpdir("terminate-csv-ro");
    let cfg = cfg(&dir.0);
    let csv = keys_csv(&dir.0, "devtest-alpha,sk-a\n");
    let keys = FakeKeys::with(&[("h-a", "devtest-alpha")]);
    let book = KeyBook::new(&csv, Some(&keys));
    // The keys file and its directory read-only: the revoke works, the row can't go.
    let file = dir.0.join("devtest_openrouter_keys.csv");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o444)).unwrap();
    std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o555)).unwrap();
    let f = fleet();
    let r = handle_terminate(&f, &cfg, &book, Some("alpha"), topts(true, true, false, true)).await;
    std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let e = r.unwrap_err().to_string();
    assert_eq!(keys.calls(), ["delete h-a"]);
    assert!(e.contains("devtest-alpha ARE revoked (or were already gone), but their rows couldn't be removed from"), "{e}");
    assert!(e.contains("delete those rows by hand"), "{e}");
    assert!(!e.contains("retry with `arena keys revoke"), "keys revoke can't fix a local file: {e}");
    assert_eq!(f.calls(), ["terminate r-alpha"]);
}

const FAST: SettleWait = SettleWait { stable_secs: 0, timeout_secs: 5, poll: Duration::ZERO };

fn restart_opts(wipe_ok: bool) -> RestartOpts {
    RestartOpts { wipe_ok, yes: true, ..Default::default() }
}

#[tokio::test]
async fn restart_of_a_volumeless_gpu_pod_is_refused_without_wipe_ok_even_with_yes() {
    let dir = tmpdir("restart-refused");
    let (f, remote) = (fleet(), Arc::new(FakeRemote::new()));
    let keys_dir = dir.0.display().to_string();
    let e = handle_restart(&f, remote.clone(), &cfg(&dir.0), "alpha", restart_opts(false), FAST, &keys_dir)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("refusing to restart devtest-alpha without --wipe-ok") && e.contains("--yes alone isn't enough"), "{e}");
    // An unreadable spec (volume unknown) counts as no volume.
    let mut g = fleet();
    g.volumes = vec![("r-alpha", 50)];
    g.no_spec = vec!["r-alpha"];
    assert!(handle_restart(&g, remote.clone(), &cfg(&dir.0), "alpha", restart_opts(false), FAST, &keys_dir).await.is_err());
    assert!(f.calls().is_empty() && g.calls().is_empty() && remote.calls().is_empty(), "nothing restarted");
    // Through the command line: the same refusal (the gate comes before any wait).
    let r = handle_pods(pods_cmd(&["restart", "alpha"]), &f, remote.clone(), &cfg(&dir.0), true).await;
    assert!(r.unwrap_err().to_string().contains("without --wipe-ok"));
    handle_pods(pods_cmd(&["restart", "alpha", "--dry-run"]), &f, remote.clone(), &cfg(&dir.0), true).await.unwrap();
    assert!(f.calls().is_empty() && remote.calls().is_empty());
}

#[tokio::test]
async fn restart_with_wipe_ok_comes_back_set_up_and_proxied() {
    let dir = tmpdir("restart-ok");
    let cfg = cfg(&dir.0);
    let (f, remote) = (fleet(), Arc::new(FakeRemote::new()));
    let keys_dir = dir.0.display().to_string();
    handle_restart(&f, remote.clone(), &cfg, "alpha", restart_opts(true), FAST, &keys_dir).await.unwrap();
    assert_eq!(f.calls(), ["restart r-alpha"]);
    // Setup ran again on alpha (and only alpha) — on the endpoint it came back on (the
    // fake moves it on restart), so setup ran AFTER the restart, not before: deploy key,
    // then the config command, which writes ~/.name again.
    let calls = remote.calls();
    assert!(!calls.is_empty() && calls.iter().all(|c| c.host() == "10.0.0.1:23001"), "{calls:?}");
    assert!(matches!(&calls[0], RemoteCall::Copy { remote, .. } if remote == "/root/.ssh/id_ed25519"), "{calls:?}");
    assert!(matches!(&calls[1], RemoteCall::Exec { cmd, .. } if cmd.contains(&name_file_command("alpha"))), "{calls:?}");
    // …and the proxy was synced afterwards, to the new endpoint.
    let px = proxy_file(&dir.0);
    assert!(px.contains("name=devtest-alpha") && px.contains("23001") && !px.contains("22001"), "{px}");
}

#[tokio::test]
async fn restart_needs_no_wipe_ok_only_when_the_work_survives() {
    let dir = tmpdir("restart-volume");
    let cfg = cfg(&dir.0);
    let keys_dir = dir.0.display().to_string();
    // A pod WITH a volume but the repo in its default place (/root/r): the restart still
    // wipes the participants' work — refused without --wipe-ok, even with --yes. The pod
    // was asked where its repo is (read-only, bounded); it said nothing usable.
    let mut f = fleet();
    f.volumes = vec![("r-bravo", 50)];
    let remote = Arc::new(FakeRemote::new());
    let e = handle_restart(&f, remote.clone(), &cfg, "bravo", restart_opts(false), FAST, &keys_dir)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("refusing to restart devtest-bravo without --wipe-ok") && e.contains("the repo at /root/r"), "{e}");
    assert!(f.calls().is_empty());
    assert_eq!(
        remote.calls(),
        [RemoteCall::Exec { host: "10.0.0.1:22002".into(), cmd: probe_command("/root/r"), timeout: Some(PROBE_TIMEOUT) }],
        "only the probe"
    );
    // The repo on the volume (BACKUP_REPO_PATH under /workspace): it survives, so no
    // --wipe-ok — still re-set-up afterwards.
    let on_volume = Config::parse(&format!("{}BACKUP_REPO_PATH=/workspace/r\n", cfg_text(&dir.0)));
    handle_restart(&f, remote.clone(), &on_volume, "bravo", restart_opts(false), FAST, &keys_dir).await.unwrap();
    assert_eq!(f.calls(), ["restart r-bravo"]);
    assert!(!remote.calls_to("10.0.0.1:23002").is_empty(), "setup ran");
    // ...but not on a volume-less pod, wherever the repo is.
    let (g, remote) = (fleet(), Arc::new(FakeRemote::new()));
    assert!(handle_restart(&g, remote.clone(), &on_volume, "bravo", restart_opts(false), FAST, &keys_dir).await.is_err());
    assert!(g.calls().is_empty());
    // A Hetzner VM keeps its disk: not refused, nothing re-provisioned.
    let (f, remote) = (fleet(), Arc::new(FakeRemote::new()));
    handle_restart(&f, remote.clone(), &cfg, "delta", restart_opts(false), FAST, &keys_dir).await.unwrap();
    assert_eq!(f.calls(), ["restart h-delta"]);
    assert!(remote.calls().is_empty());
    // --no-setup: restarted (with --wipe-ok), not provisioned.
    let (f, remote) = (fleet(), Arc::new(FakeRemote::new()));
    let opts = RestartOpts { no_setup: true, ..restart_opts(true) };
    handle_restart(&f, remote.clone(), &cfg, "alpha", opts, FAST, &keys_dir).await.unwrap();
    assert_eq!(f.calls(), ["restart r-alpha"]);
    assert!(remote.calls().is_empty());
}

/// The gate follows where the repo REALLY is: setup links `/root/r` onto the volume, which
/// the configured path can't show — so a pod with a volume is asked. Linked onto a mounted
/// volume: no --wipe-ok needed. The volume not mounted, a plain directory, or no answer at
/// all: refused as before.
#[tokio::test]
async fn restart_asks_the_pod_where_its_repo_really_is() {
    let dir = tmpdir("restart-real-path");
    let cfg = cfg(&dir.0);
    let keys_dir = dir.0.display().to_string();
    let said = |real: &str, mount: &str| {
        FakeReply::stdout(&format!("arena-repo-home=/root\narena-repo-real={real}\narena-repo-mount={mount}\n"))
    };
    // Linked onto the mounted volume: restarted without --wipe-ok, then set up again.
    let mut f = fleet();
    f.volumes = vec![("r-bravo", 50)];
    let remote = Arc::new(FakeRemote::new());
    remote.script("10.0.0.1:22002", [said("/workspace/r", "yes")]);
    handle_restart(&f, remote.clone(), &cfg, "bravo", restart_opts(false), FAST, &keys_dir).await.unwrap();
    assert_eq!(f.calls(), ["restart r-bravo"]);
    assert!(matches!(&remote.calls_to("10.0.0.1:22002")[..], [RemoteCall::Exec { cmd, .. }] if *cmd == probe_command("/root/r")));
    assert!(!remote.calls_to("10.0.0.1:23002").is_empty(), "setup ran on the restarted pod");
    // Refused: (the pod's answer) — nothing restarted.
    for answer in [
        said("/workspace/r", "no"),                     // the volume isn't mounted there
        said("/root/r", "yes"),                          // still a plain directory
        FakeReply::error("ssh: connect to host 10.0.0.1 port 22002: Connection refused"),
        FakeReply::exit(255, "Permission denied (publickey)"),
        FakeReply::stdout("garbage\n"),
    ] {
        let mut f = fleet();
        f.volumes = vec![("r-bravo", 50)];
        let remote = Arc::new(FakeRemote::new());
        remote.script("10.0.0.1:22002", [answer.clone()]);
        let r = handle_restart(&f, remote.clone(), &cfg, "bravo", restart_opts(false), FAST, &keys_dir).await;
        let e = r.unwrap_err().to_string();
        assert!(e.contains("refusing to restart devtest-bravo without --wipe-ok"), "{answer:?}: {e}");
        assert!(f.calls().is_empty(), "{answer:?}");
    }
    // The dry run says what the pod said.
    let mut f = fleet();
    f.volumes = vec![("r-bravo", 50)];
    let remote = Arc::new(FakeRemote::new());
    remote.script("10.0.0.1:22002", [said("/workspace/r", "yes")]);
    let opts = RestartOpts { dry_run: true, ..restart_opts(false) };
    handle_restart(&f, remote.clone(), &cfg, "bravo", opts, FAST, &keys_dir).await.unwrap();
    assert!(f.calls().is_empty());
    // A pod without a volume is never asked (nothing on it survives anyway).
    let (f, remote) = (fleet(), Arc::new(FakeRemote::new()));
    assert!(handle_restart(&f, remote.clone(), &cfg, "alpha", restart_opts(false), FAST, &keys_dir).await.is_err());
    assert!(remote.calls().is_empty());
}

/// A wedged pod doesn't hold the gate: the probe is bounded, and no answer is "refuse".
#[tokio::test(start_paused = true)]
async fn a_wedged_pod_is_refused_after_the_probe_budget() {
    let dir = tmpdir("restart-wedged");
    let mut f = fleet();
    f.volumes = vec![("r-bravo", 50)];
    let remote = Arc::new(FakeRemote::new());
    remote.script("10.0.0.1:22002", [FakeReply::hang()]);
    let start = tokio::time::Instant::now();
    let e = handle_restart(&f, remote.clone(), &cfg(&dir.0), "bravo", restart_opts(false), FAST, &dir.0.display().to_string())
        .await
        .unwrap_err();
    assert!(e.to_string().contains("without --wipe-ok"), "{e}");
    assert!(start.elapsed() <= PROBE_TIMEOUT + Duration::from_secs(1), "{:?}", start.elapsed());
    assert!(f.calls().is_empty());
}

#[tokio::test]
async fn a_restart_whose_setup_fails_says_not_to_restart_again() {
    let dir = tmpdir("restart-setup-fails");
    let cfg = cfg(&dir.0);
    let (f, remote) = (fleet(), Arc::new(FakeRemote::new()));
    // (on the endpoint the restart moved it to)
    remote.script("10.0.0.1:23001", [FakeReply::ok(), FakeReply::exit(1, "fatal: not a git repository")]);
    let e = handle_restart(&f, remote.clone(), &cfg, "alpha", restart_opts(true), FAST, &dir.0.display().to_string())
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("devtest-alpha restarted, but it is NOT set up again"), "{e}");
    assert!(e.contains("arena pods setup devtest-alpha") && e.contains("not another restart"), "{e}");
    assert_eq!(f.calls(), ["restart r-alpha"]);
    assert!(proxy_file(&dir.0).contains("name=devtest-alpha"), "the proxy is synced anyway");
    // Setup that can't run at all (no repo config) is caught BEFORE the wipe.
    let bare = Config::parse("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(alpha)\n");
    let f = fleet();
    let e = handle_restart(&f, remote.clone(), &bare, "alpha", restart_opts(true), FAST, "/nonexistent").await.unwrap_err();
    assert!(format!("{e:#}").contains("pass --no-setup"), "{e:#}");
    assert!(f.calls().is_empty());
}

#[tokio::test]
async fn disk_fate_follows_the_backend_and_the_volume() {
    let mut f = fleet();
    f.volumes = vec![("r-bravo", 50)];
    f.no_spec = vec!["r-charlie"];
    let pods = f.pods.lock().unwrap().clone();
    let fate = |i: usize| disk_fate(&f, &pods[i]);
    assert_eq!(fate(0).await, DiskFate::Wiped { volume_gb: Some(0) });
    assert_eq!(fate(1).await, DiskFate::Wiped { volume_gb: Some(50) });
    assert_eq!(fate(2).await, DiskFate::Wiped { volume_gb: None });
    assert_eq!(fate(3).await, DiskFate::Kept);
}

/// Live R (2026-10-09): a RunPod v1 restart kept the container disk (marker files and
/// `/.dockerenv` survived), while the output said "the restart reset them to the image".
/// v1 stays gated (cheap safety; v1 retires 2026-11-16) and is set up again in case — but
/// nothing says a reset happened. v2's restart, which does reset, still says so.
#[tokio::test]
async fn a_restart_not_known_to_reset_is_gated_and_set_up_again_but_never_said_to_reset() {
    let dir = tmpdir("restart-v1");
    let cfg = cfg(&dir.0);
    let keys_dir = dir.0.display().to_string();
    let mut f = fleet();
    f.restart_resets = ContainerReset::Unknown;
    f.volumes = vec![("r-bravo", 50)];
    let pods = f.pods.lock().unwrap().clone();
    // The fate: gated like a wipe, with or without a volume; a VM is still kept.
    assert_eq!(restart_fate(&f, &pods[0]).await, DiskFate::MayWipe { volume_gb: Some(0) });
    assert_eq!(restart_fate(&f, &pods[1]).await, DiskFate::MayWipe { volume_gb: Some(50) });
    assert_eq!(restart_fate(&f, &pods[3]).await, DiskFate::Kept);
    // The gate holds (--yes alone isn't enough), worded as a possibility.
    let remote = Arc::new(FakeRemote::new());
    let e = handle_restart(&f, remote.clone(), &cfg, "alpha", restart_opts(false), FAST, &keys_dir).await.unwrap_err().to_string();
    assert!(e.contains("refusing to restart devtest-alpha without --wipe-ok: this may reset the container disk"), "{e}");
    assert!(f.calls().is_empty() && remote.calls().is_empty());
    // With --wipe-ok: restarted, and set up again on the endpoint it came back on.
    handle_restart(&f, remote.clone(), &cfg, "alpha", restart_opts(true), FAST, &keys_dir).await.unwrap();
    assert_eq!(f.calls(), ["restart r-alpha"]);
    assert!(!remote.calls_to("10.0.0.1:23001").is_empty(), "set up again, in case");
    // What it says, before and after — never that the reset happened.
    let may = DiskFate::MayWipe { volume_gb: Some(0) };
    let text = may.describe(DiskOp::Restart, &RepoSite::configured("/root/r"));
    assert!(text.starts_with("⚠ may WIPE the container disk") && text.contains("NO persistent volume"), "{text}");
    assert_eq!(may.reset(), ContainerReset::Unknown);
    let line = reprovision_line(1, "restart", ContainerReset::Unknown);
    assert_eq!(line, "Re-provisioning 1 pod(s) (in case the restart reset them to the image — it may not have)…");
    assert!(afterwards_line(ContainerReset::Unknown, false, false).contains("(no setup: if it was reset, it comes back as the bare image)"));
    assert!(afterwards_line(ContainerReset::Unknown, false, true).contains("if it was reset"));
    assert_eq!(afterwards_line(ContainerReset::Unknown, true, false), afterwards_line(ContainerReset::Yes, true, false));
    assert!(no_setup_line(&pods[..1], ContainerReset::Unknown).starts_with("(--no-setup: devtest-alpha may be back as the bare image"));
    // A known reset (v2, reimage, start) still says it plainly.
    assert_eq!(reprovision_line(2, "restart", ContainerReset::Yes), "Re-provisioning 2 pod(s) (the restart reset them to the image)…");
    assert!(no_setup_line(&pods[..1], ContainerReset::Yes).starts_with("(--no-setup: devtest-alpha back as the bare image"));
    let v2 = DiskFate::Wiped { volume_gb: Some(0) }.describe(DiskOp::Restart, &RepoSite::configured("/root/r"));
    assert!(v2.starts_with("⚠ WIPES the container disk (reset to the image)"), "{v2}");
    let r = wipe_refusal(DiskOp::Restart, false, &["devtest-a"], "/root/r");
    assert!(r.contains("this resets the container disk to the image"), "{r}");
    // A restart whose setup fails: don't restart again — it "may" wipe, not "would again".
    let mut g = fleet();
    g.restart_resets = ContainerReset::Unknown;
    let remote = Arc::new(FakeRemote::new());
    remote.script("10.0.0.1:23001", [FakeReply::ok(), FakeReply::exit(1, "fatal: not a git repository")]);
    let e = handle_restart(&g, remote, &cfg, "alpha", restart_opts(true), FAST, &keys_dir).await.unwrap_err().to_string();
    assert!(e.contains("not another restart — that may wipe it)") && !e.contains("wipe it again"), "{e}");
}

/// Live R: `restart` on a stopped Hetzner VM said `pods start` brings it back "as a fresh
/// image and is set up again" — a VM keeps its disk and isn't set up again.
#[tokio::test]
async fn restart_on_a_stopped_pod_says_what_a_start_brings_back() {
    let dir = tmpdir("restart-stopped-vm");
    let cfg = cfg(&dir.0);
    let f = fleet();
    f.pods.lock().unwrap()[3].status = "OFF".into(); // delta, the Hetzner VM
    let remote = Arc::new(FakeRemote::new());
    let e = handle_restart(&f, remote.clone(), &cfg, "delta", restart_opts(false), FAST, &dir.0.display().to_string())
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("devtest-delta is stopped (OFF)") && e.contains("`arena pods start devtest-delta`"), "{e}");
    assert!(e.contains("its disk was kept") && !e.contains("fresh image"), "{e}");
    assert!(f.calls().is_empty() && remote.calls().is_empty());
    // A RunPod pod's start does bring it back as a fresh image.
    let gpu = pod("r-x", "devtest-x", "runpod", None);
    assert!(stopped_restart_refusal(&gpu, true).contains("(it comes back as a fresh image and is set up again)"));
}

#[test]
fn disk_fate_text_and_gate() {
    // The gate follows the WORK (the repo), not just "is there a volume".
    const ROOT: &str = "/root/ARENA_materials";
    const VOL: &str = "/workspace/ARENA_materials";
    let v50 = DiskFate::Wiped { volume_gb: Some(50) };
    let cfg_only = RepoSite::configured;
    // What the pod said: its repo's real path, and whether /workspace is mounted.
    let pod_said = |real: &str, mounted: bool| RepoSite { configured: ROOT.into(), on_pod: Some((real.into(), mounted)) };
    let linked = pod_said(VOL, true);
    let cases: &[(DiskFate, RepoSite, bool, &[&str])] = &[
        (DiskFate::Kept, cfg_only(ROOT), false, &["disk kept"]),
        // a volume, but the repo in the default place: lost — gated
        (v50, cfg_only(ROOT), true, &["WIPES the container disk", "the ARENA repo at /root/ARENA_materials (participants' work) is NOT on the volume", "only the 50 GB volume at /workspace survives"]),
        // the repo on the volume: survives — not gated
        (v50, cfg_only(VOL), false, &["everything outside /workspace is lost", "the repo at /workspace/ARENA_materials is on the 50 GB volume at /workspace and survives"]),
        // setup linked it there, the pod says so: survives — not gated, and the prompt says where
        (v50, linked.clone(), false, &["the repo at /root/ARENA_materials → /workspace/ARENA_materials on the pod is on the 50 GB volume"]),
        // linked, but the volume isn't mounted on the pod / still a plain directory: gated
        (v50, pod_said(VOL, false), true, &["isn't mounted there", "is NOT on the volume"]),
        (v50, pod_said(ROOT, true), true, &["the ARENA repo at /root/ARENA_materials (participants' work) is NOT on the volume"]),
        // no volume at all: whatever the pod says, nothing survives
        (DiskFate::Wiped { volume_gb: Some(0) }, linked.clone(), true, &["NO persistent volume"]),
        (DiskFate::Wiped { volume_gb: Some(0) }, cfg_only(VOL), true, &["WIPES the container disk", "NO persistent volume", "~/.name", "repo at /workspace/ARENA_materials"]),
        (DiskFate::Wiped { volume_gb: None }, cfg_only(ROOT), true, &["couldn't read whether it has a volume", "assume nothing survives"]),
        (DiskFate::Wiped { volume_gb: None }, cfg_only(VOL), true, &["assume nothing survives"]),
    ];
    for (fate, site, gated, says) in cases {
        assert_eq!(fate.needs_wipe_ok(site), *gated, "{fate:?} {site:?}");
        let text = fate.describe(DiskOp::Restart, site);
        for s in *says {
            assert!(text.contains(s), "{fate:?} {site:?}: {text}");
        }
    }
    assert!(DiskFate::Wiped { volume_gb: Some(0) }.describe(DiskOp::Stop, &cfg_only(ROOT)).contains("starts again as a fresh image"));
    assert!(DiskFate::Kept.describe(DiskOp::Stop, &cfg_only(ROOT)).contains("powered off"));
    // A stopped pod comes back only when started — the prompt says to re-run setup then.
    assert!(v50.describe(DiskOp::Stop, &linked).contains("`arena pods start` brings it back and re-links the repo"));
    assert!(!v50.describe(DiskOp::Restart, &linked).contains("re-links"));
    let r = wipe_refusal(DiskOp::Restart, false, &["devtest-a", "devtest-b"], ROOT);
    assert!(r.starts_with("refusing to restart devtest-a, devtest-b without --wipe-ok"), "{r}");
    assert!(r.contains("the repo at /root/ARENA_materials") && r.contains("outside /workspace"), "{r}");
    assert!(r.contains("`arena pods backup devtest-a devtest-b`") && r.contains("--yes alone isn't enough"), "{r}");
    assert!(wipe_refusal(DiskOp::Stop, false, &["x"], ROOT).contains("terminate instead"));
}

#[test]
fn only_a_repo_at_or_under_the_mount_is_on_the_volume() {
    for (repo, on) in [
        ("/workspace", true),
        ("/workspace/ARENA", true),
        ("/workspace/ARENA/", true),
        ("/root/ARENA_materials", false),
        ("/workspacex/ARENA", false),
        ("workspace/ARENA", false),
        ("~/ARENA", false),
        ("/workspace/../root/ARENA", false),
        ("", false),
    ] {
        assert_eq!(RepoSite::configured(repo).on_volume(), on, "{repo}");
    }
}

#[tokio::test]
async fn stop_follows_the_same_work_survives_rule() {
    let dir = tmpdir("stop-volume");
    let mut f = fleet();
    f.volumes = vec![("r-bravo", 50)];
    // bravo has a volume, but the repo (/root/r) isn't on it: refused without --wipe-ok.
    let e = handle_pods(pods_cmd(&["stop", "bravo"]), &f, Arc::new(FakeRemote::new()), &cfg(&dir.0), true)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("refusing to stop devtest-bravo without --wipe-ok") && e.contains("the repo at /root/r"), "{e}");
    assert!(f.calls().is_empty());
    // With the repo on the volume it goes ahead (and a Hetzner VM always does).
    let on_volume = Config::parse(&format!("{}BACKUP_REPO_PATH=/workspace/r\n", cfg_text(&dir.0)));
    handle_pods(pods_cmd(&["stop", "bravo", "delta"]), &f, Arc::new(FakeRemote::new()), &on_volume, true).await.unwrap();
    assert_eq!(f.calls(), ["stop r-bravo", "stop h-delta"]);
    // …and so it does when setup linked it onto the volume — the pod says so. Only the pod
    // with a volume is asked; alpha (no volume) is refused whatever it would say.
    let mut f = fleet();
    f.volumes = vec![("r-bravo", 50)];
    let remote = Arc::new(FakeRemote::new());
    remote.script("10.0.0.1:22002", [FakeReply::stdout("arena-repo-real=/workspace/r\narena-repo-mount=yes\n")]);
    handle_pods(pods_cmd(&["stop", "bravo", "delta"]), &f, remote.clone(), &cfg(&dir.0), true).await.unwrap();
    assert_eq!(f.calls(), ["stop r-bravo", "stop h-delta"]);
    assert_eq!(remote.calls().len(), 1, "one probe, to bravo");
    let remote = Arc::new(FakeRemote::new());
    remote.script("10.0.0.1:22002", [FakeReply::stdout("arena-repo-real=/workspace/r\narena-repo-mount=yes\n")]);
    let e = handle_pods(pods_cmd(&["stop", "alpha", "bravo"]), &f, remote.clone(), &cfg(&dir.0), true).await.unwrap_err();
    assert!(e.to_string().contains("refusing to stop devtest-alpha without --wipe-ok"), "{e}");
    assert!(remote.calls_to("10.0.0.1:22001").is_empty());
    // The prompt / dry-run line per pod names it and what happens to its disk.
    let pods = f.pods.lock().unwrap().clone();
    let sites = [RepoSite::configured("/root/r"), RepoSite::configured("/root/r")];
    let lines = stop_lines(&[&pods[1], &pods[3]], &[DiskFate::Wiped { volume_gb: Some(50) }, DiskFate::Kept], &sites);
    assert!(lines[0].starts_with("  devtest-bravo (id=r-bravo, runpod): ⚠ discards the container disk"), "{lines:?}");
    assert!(lines[0].contains("the ARENA repo at /root/r (participants' work) is NOT on the volume"), "{lines:?}");
    // (a VM is shut down gracefully now — powered off only if it hangs: live R)
    assert_eq!(lines[1], "  devtest-delta (id=h-delta, hetzner): disk kept — shut down (powered off only if it hangs), files stay");
}

fn pods_cmd(argv: &[&str]) -> PodCmd {
    match Cli::try_parse_from(["arena", "pods"].iter().chain(argv).copied()).map(|c| c.cmd) {
        Ok(Cmd::Pods(p)) => p,
        other => panic!("{argv:?}: {:?}", other.err()),
    }
}

/// `cfg` plus a readable public key (reimage re-seeds PUBLIC_KEY and refuses without
/// one), an image and a GPU type (a replacement spec needs one: the fakes report none).
fn keyed_cfg(dir: &Path, extra: &str) -> Config {
    std::fs::write(dir.join("k"), "private").unwrap();
    std::fs::write(dir.join("k.pub"), "ssh-ed25519 AAAAtest devtest-test").unwrap();
    Config::parse(&format!(
        "{}SHARED_SSH_KEY_PATH={}\nIMAGE=img:9\nGPU_TYPE=\"NVIDIA RTX A4000\"\n{extra}",
        cfg_text(dir),
        dir.join("k").display()
    ))
}

fn named(pods: &[Pod]) -> Selected {
    Selected { pods: pods.to_vec(), named: true }
}

/// Live findings #2 + #8: `reimage` wiped a volume-less pod with only `-y` (stop and
/// restart beside it refused), and left it unprovisioned (no ~/.name, the public git
/// remote, no deploy key). Now the same `--wipe-ok` gate — refused up front, the dry run
/// says so, a repo on the volume passes — and afterwards it settles, runs setup again ON
/// THE NEW CONTAINER and syncs the proxy to it.
#[tokio::test]
async fn reimage_is_gated_like_restart_and_sets_the_pod_up_again() {
    let dir = tmpdir("reimage");
    let cfg = keyed_cfg(&dir.0, "");
    let keys_dir = dir.0.display().to_string();
    // Refused without --wipe-ok, even with --yes (handle_pods passes it): nothing reimaged.
    let (f, remote) = (fleet(), Arc::new(FakeRemote::new()));
    let e = handle_pods(pods_cmd(&["reimage", "alpha", "bravo"]), &f, remote.clone(), &cfg, true).await.unwrap_err().to_string();
    assert!(e.contains("refusing to reimage devtest-alpha, devtest-bravo without --wipe-ok"), "{e}");
    assert!(e.contains("--yes alone isn't enough") && e.contains("`arena pods backup devtest-alpha devtest-bravo`"), "{e}");
    assert!(f.calls().is_empty() && remote.calls().is_empty());
    // The dry run says it would be refused (and changes nothing).
    take_would_refuse();
    let dry = RestartOpts { dry_run: true, yes: true, ..Default::default() };
    let pods = f.pods.lock().unwrap().clone();
    handle_reimage(&f, remote.clone(), &cfg, &named(&pods[..1]), None, dry, FAST, &keys_dir).await.unwrap();
    let notes = take_would_refuse();
    assert!(notes.len() == 1 && notes[0].contains("refusing to reimage devtest-alpha without --wipe-ok"), "{notes:?}");
    assert!(f.calls().is_empty());
    // --wipe-ok: reimaged, set up again on the endpoint it came back on, proxied there.
    let opts = RestartOpts { wipe_ok: true, yes: true, ..Default::default() };
    handle_reimage(&f, remote.clone(), &cfg, &named(&pods[..1]), None, opts, FAST, &keys_dir).await.unwrap();
    assert_eq!(f.calls(), ["reimage r-alpha img:9"]);
    let calls = remote.calls();
    assert!(!calls.is_empty() && calls.iter().all(|c| c.host() == "10.0.0.1:23001"), "{calls:?}");
    assert!(calls.iter().any(|c| matches!(c, RemoteCall::Exec { cmd, .. } if cmd.contains(&name_file_command("alpha")))), "{calls:?}");
    let px = proxy_file(&dir.0);
    assert!(px.contains("name=devtest-alpha") && px.contains("23001") && !px.contains("22001"), "{px}");
    // The repo on a volume survives a reimage: no --wipe-ok needed.
    let mut g = fleet();
    g.volumes = vec![("r-bravo", 50)];
    let on_volume = keyed_cfg(&dir.0, "BACKUP_REPO_PATH=/workspace/r\n");
    let bravo = g.pods.lock().unwrap()[1].clone();
    let opts = RestartOpts { no_setup: true, skip_proxy: true, yes: true, ..Default::default() };
    handle_reimage(&g, Arc::new(FakeRemote::new()), &on_volume, &named(&[bravo]), None, opts, FAST, &keys_dir).await.unwrap();
    assert_eq!(g.calls(), ["reimage r-bravo img:9"]);
}

/// Live finding #15: a stopped pod had no way back — `restart` answered a raw 409 (v2)
/// or "restarted" it without starting it (v1), and `run` said "no SSH endpoint yet". Now
/// `restart` points at `pods start`, which starts it, sets it up again (it's a fresh
/// image) and syncs the proxy; the SSH commands say it's stopped.
#[tokio::test]
async fn a_stopped_pod_is_started_set_up_and_proxied() {
    let dir = tmpdir("start");
    let cfg = cfg(&dir.0);
    let keys_dir = dir.0.display().to_string();
    let f = fleet();
    f.pods.lock().unwrap()[0].status = "EXITED".into(); // alpha (its last endpoint still listed)
    let remote = Arc::new(FakeRemote::new());
    let e = handle_restart(&f, remote.clone(), &cfg, "alpha", restart_opts(true), FAST, &keys_dir).await.unwrap_err().to_string();
    assert!(e.contains("devtest-alpha is stopped (EXITED)") && e.contains("`arena pods start devtest-alpha`"), "{e}");
    let e = handle_pods(pods_cmd(&["run", "-t", "alpha", "hostname"]), &f, remote.clone(), &cfg, true).await.unwrap_err().to_string();
    assert!(e.contains("stopped (devtest-alpha)") && e.contains("arena pods start devtest-alpha"), "{e}");
    assert!(f.calls().is_empty() && remote.calls().is_empty(), "nothing restarted, nothing reached");
    // Start: the stopped one only (bravo is running: skipped).
    let pods = f.pods.lock().unwrap().clone();
    let opts = RestartOpts { yes: true, ..Default::default() };
    handle_start(&f, remote.clone(), &cfg, &named(&pods[..2]), opts, FAST, &keys_dir).await.unwrap();
    assert_eq!(f.calls(), ["start r-alpha"]);
    let calls = remote.calls();
    assert!(!calls.is_empty() && calls.iter().all(|c| c.host() == "10.0.0.1:23001"), "set up on the new container: {calls:?}");
    assert!(proxy_file(&dir.0).contains("23001"));
    // Through the command line: needs targets; `--dry-run` starts nothing.
    assert!(handle_pods(pods_cmd(&["start"]), &f, remote.clone(), &cfg, true).await.is_err());
    let g = fleet();
    g.pods.lock().unwrap()[1].status = "EXITED".into();
    handle_pods(pods_cmd(&["start", "bravo", "--dry-run"]), &g, remote.clone(), &cfg, true).await.unwrap();
    assert!(g.calls().is_empty());
}

/// Live R: `copy-keys` on a stopped pod printed the right skip line, then failed with
/// "none of the selected pods … has an SSH endpoint yet"; every other SSH command says
/// the pods are stopped, and how to start them.
#[tokio::test]
async fn copy_keys_on_a_stopped_pod_says_it_is_stopped() {
    let dir = tmpdir("copy-keys-stopped");
    let cfg = cfg(&dir.0);
    std::fs::write(dir.0.join("openai_api_keys.csv"), "host,key\ndevtest-alpha,sk-a\ndevtest-charlie,sk-c\n").unwrap();
    let keys_dir = dir.0.display().to_string();
    let f = fleet();
    f.pods.lock().unwrap()[0].status = "EXITED".into(); // alpha
    let pods = f.pods.lock().unwrap().clone();
    let remote = Arc::new(FakeRemote::new());
    let e = handle_copy_keys(remote.clone(), &cfg, &keys_dir, None, None, &named(&pods[..1]), false, true)
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(e, "the selected pod(s) are stopped (devtest-alpha) — `arena pods start devtest-alpha` first");
    // With a pod that has no endpoint yet among them (charlie), that is what's said.
    let both = named(&[pods[0].clone(), pods[2].clone()]);
    let e = handle_copy_keys(remote.clone(), &cfg, &keys_dir, None, None, &both, false, true).await.unwrap_err().to_string();
    assert!(e.starts_with("none of the selected pods (devtest-alpha, devtest-charlie) has an SSH endpoint yet"), "{e}");
    assert!(remote.calls().is_empty(), "nothing reached");
}

/// Live (wave 6): after a restart the endpoint flapped — the proxy sync right after setup
/// caught a moment the listing showed the pod with no endpoint, and its forward stayed
/// on the dead one. The sync now waits for a settled endpoint (and re-syncs once).
#[tokio::test]
async fn the_post_restart_proxy_sync_waits_for_a_settled_endpoint() {
    let dir = tmpdir("restart-flap");
    let cfg = cfg(&dir.0);
    let mut f = fleet();
    // List calls: 1 the restart's lookup, 2-3 the settle wait, 4 setup's listing — so 5 is
    // the first listing after setup: the one the sync used to take as it came.
    f.blank_lists = vec![5];
    let remote = Arc::new(FakeRemote::new());
    handle_restart(&f, remote, &cfg, "alpha", restart_opts(true), FAST, &dir.0.display().to_string()).await.unwrap();
    let px = proxy_file(&dir.0);
    assert!(px.contains("name=devtest-alpha") && px.contains("23001") && !px.contains("22001"), "{px}");
}

/// Live: RunPod listed a running pod without an endpoint for ~20 s, `pull` skipped it and
/// exited 1 — a backup cron tick silently missed that pod. `run` / `pull` / `backup` ask
/// once more first.
#[tokio::test(start_paused = true)]
async fn run_and_backup_ask_again_for_a_running_pod_listed_without_an_endpoint() {
    let dir = tmpdir("endpoint-retry");
    let cfg = cfg(&dir.0);
    for argv in [&["run", "-t", "alpha", "hostname"][..], &["backup", "alpha", "--no-pull"]] {
        let mut f = fleet();
        f.blank_lists = vec![1]; // the selection's listing
        let remote = Arc::new(FakeRemote::new());
        handle_pods(pods_cmd(argv), &f, remote.clone(), &cfg, true).await.unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        assert_eq!(remote.calls().len(), 1, "{argv:?}: {:?}", remote.calls());
        assert_eq!(remote.calls()[0].host(), "10.0.0.1:22001", "{argv:?}");
    }
}

/// Live finding #6: `migrate finish` terminated `<name>-old` while the proxy still routed
/// the participant's port to it (after a `--skip-proxy` cutover). Now it syncs first and
/// refuses unless the name's stable port routes to the live pod.
#[tokio::test]
async fn finish_keeps_old_until_the_proxy_routes_the_name_to_the_live_pod() {
    let dir = tmpdir("finish-proxy");
    let cfg = cfg(&dir.0);
    let pods = |live_endpoint: Option<(&str, u16)>| {
        vec![
            pod("r-bravo-new", "devtest-bravo", "runpod", live_endpoint),
            pod("r-bravo", "devtest-bravo-old", "runpod", Some(("10.0.0.1", 22002))),
        ]
    };
    // The live pod has no endpoint yet: the sync can't route bravo to it.
    let f = Fleet::new(pods(None));
    let listed = f.pods.lock().unwrap().clone();
    let e = migrate_finish_on(&FakeRemote::new(), &f, &cfg, &f, "devtest-bravo", &listed, FinishOpts { dry_run: false, yes: true, ..Default::default() }).await.unwrap_err().to_string();
    assert!(e.contains("NOT terminating devtest-bravo-old") && e.contains("arena proxy apply"), "{e}");
    assert!(f.calls().is_empty(), "{:?}", f.calls());
    // Routed: it goes.
    let f = Fleet::new(pods(Some(("10.0.0.3", 22009))));
    let listed = f.pods.lock().unwrap().clone();
    migrate_finish_on(&FakeRemote::new(), &f, &cfg, &f, "devtest-bravo", &listed, FinishOpts { dry_run: false, yes: true, ..Default::default() }).await.unwrap();
    assert_eq!(f.calls(), ["terminate r-bravo"]);
    assert!(proxy_file(&dir.0).contains("10.0.0.3"));
}

/// Without a proxy, `migrate finish` syncs nothing before its terminate — so `SSH_CONFIG_FILE`
/// is rewritten after it, from the listing without `-old`: its block goes.
#[tokio::test]
async fn finish_without_a_proxy_drops_old_from_the_ssh_config_file() {
    let dir = tmpdir("finish-sshconfig");
    let path = dir.0.join("arena.conf");
    let cfg = Config::parse(&format!(
        "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(alpha bravo)\nSSH_CONFIG_FILE={}\n",
        path.display()
    ));
    let f = Fleet::new(vec![
        pod("r-bravo-new", "devtest-bravo", "runpod", Some(("10.0.0.3", 22009))),
        pod("r-bravo", "devtest-bravo-old", "runpod", Some(("10.0.0.1", 22002))),
    ]);
    let listed = f.pods.lock().unwrap().clone();
    std::fs::write(&path, arena_core::sshconfig::managed_text(&arena_core::sshconfig::render_manual("devtest", "root", "~/.ssh/shared_infra_key", &listed))).unwrap();
    migrate_finish_on(&FakeRemote::new(), &f, &cfg, &f, "devtest-bravo", &listed, FinishOpts { yes: true, ..Default::default() }).await.unwrap();
    assert_eq!(f.calls(), ["terminate r-bravo"]);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("Host devtest-bravo\n    HostName 10.0.0.3\n") && !text.contains("devtest-bravo-old"), "{text}");
}

/// The cutover's live findings, on a STOPPED original: #16 — the final sync is refused
/// saying the source is stopped and offering `--no-final-sync`, which then cuts over to
/// the copy already on `-new`; #26 — `~/.name` on the promoted pod in setup's own format;
/// #5 — its per-host API keys re-distributed under the canonical name; #29 — a write-only
/// proxy honoured up front: no 45 s wait "for nginx", no probe through it, no revert.
#[tokio::test(start_paused = true)]
async fn cutover_onto_the_copy_of_a_stopped_original() {
    let dir = tmpdir("cutover");
    let cfg = cfg(&dir.0);
    std::fs::write(dir.0.join("openai_api_keys.csv"), "devtest-alpha,sk-openai-alpha\n").unwrap();
    let keys_dir = dir.0.display().to_string();
    let mut src = pod("r-alpha", "devtest-alpha", "runpod", Some(("10.0.0.1", 22001)));
    src.status = "EXITED".into();
    let f = Fleet::new(vec![src, pod("r-alpha-new", "devtest-alpha-new", "runpod", Some(("10.0.0.2", 22001)))]);
    let new_host = "10.0.0.2:22001";
    let remote = Arc::new(FakeRemote::new());
    remote.on(new_host, "SSH_OK", FakeReply::stdout("NVIDIA RTX A4000\nSSH_OK\n"));
    remote.on(new_host, "arena-last-sync", FakeReply::stdout("arena-last-sync=1791457323\n"));
    let pods = f.pods.lock().unwrap().clone();
    let opts = |no_final_sync| CutoverOpts { skip_proxy: false, no_final_sync, dry_run: false, yes: true };
    let e = migrate_cutover_on(remote.clone(), &f, &cfg, &f, "devtest-alpha", &pods, opts(false), &keys_dir).await.unwrap_err();
    let msg = format!("{e:#}");
    assert!(msg.contains("the source devtest-alpha is stopped (EXITED)") && msg.contains("--no-final-sync"), "{msg}");
    assert!(f.calls().is_empty() && remote.calls().is_empty(), "nothing swapped, nothing reached");

    let start = tokio::time::Instant::now();
    migrate_cutover_on(remote.clone(), &f, &cfg, &f, "devtest-alpha", &pods, opts(true), &keys_dir).await.unwrap();
    assert_eq!(f.calls(), ["rename r-alpha devtest-alpha-old", "rename r-alpha-new devtest-alpha"]);
    assert!(start.elapsed() < Duration::from_secs(45), "no wait for an nginx arena doesn't reload: {:?}", start.elapsed());
    assert!(remote.calls_to("10.0.0.1:22001").is_empty(), "the stopped original isn't contacted");
    assert!(remote.calls().iter().all(|c| c.host() == new_host), "nothing probed through the proxy: {:?}", remote.calls());
    let on_new: Vec<String> = remote
        .calls_to(new_host)
        .into_iter()
        .filter_map(|c| match c {
            RemoteCall::Exec { cmd, .. } => Some(cmd),
            RemoteCall::Copy { .. } => None,
        })
        .collect();
    assert!(on_new.iter().any(|c| c.ends_with(&name_file_command("alpha"))), "~/.name in setup's format: {on_new:?}");
    assert!(on_new.iter().any(|c| c.contains("OPENAI_API_KEY") && c.contains("sk-openai-alpha")), "keys under the canonical name: {on_new:?}");
    let px = proxy_file(&dir.0);
    assert!(px.contains("name=devtest-alpha ") || px.contains("name=devtest-alpha\n") || px.contains("10.0.0.2"), "{px}");
    assert!(px.contains("10.0.0.2") && !px.contains("10.0.0.1"), "routed to the new pod: {px}");
}

/// `pods replace` end to end on fakes: the new pod's host has a maintenance window (live
/// finding #28) — said before the swap, and the original is KEPT, not terminated; the
/// promoted pod gets `~/.name` and its API keys under the canonical name (#26, #5), the
/// parked one its `-old` name; the copy is a replica's (deletions mirrored, moved aside).
#[tokio::test(start_paused = true)]
async fn replace_onto_a_host_with_maintenance_keeps_the_original() {
    let dir = tmpdir("replace-maint");
    let cfg = keyed_cfg(&dir.0, "");
    std::fs::write(dir.0.join("openai_api_keys.csv"), "devtest-alpha,sk-openai-alpha\n").unwrap();
    let keys_dir = dir.0.display().to_string();
    let mut f = fleet();
    *f.to_create.lock().unwrap() = vec![pod("r-new", "", "runpod", Some(("10.0.0.2", 22001)))];
    f.maintenance = vec![(
        "r-new",
        arena_core::pod::Maintenance { start: Some("2026-10-12T04:00:00Z".into()), end: Some("2026-10-12T12:00:00Z".into()), note: None },
    )];
    let (old_host, new_host) = ("10.0.0.1:22001", "10.0.0.2:22001");
    let remote = Arc::new(FakeRemote::new());
    remote.on(old_host, "arena-replica-sent", FakeReply::stdout("arena-replica-sent=no\n"));
    remote.on(old_host, "RUNPOD_POD_ID", FakeReply::stdout("r-alpha"));
    remote.on(new_host, "arena-replica-ready", FakeReply::stdout("arena-replica-ready\n"));
    remote.on(new_host, "MARK=", FakeReply::stdout("ID=r-new\nMARK=arena-replace-r-new\n"));
    remote.on(new_host, "SSH_OK", FakeReply::stdout("NVIDIA RTX A4000\nSSH_OK\n"));
    let (ov, place, interrupt) = (SpecOverrides::default(), PlaceFlags::default(), Interrupt::manual());
    let opts = ReplaceOpts { keep_old: false, skip_proxy: false, dry_run: false, yes: true };
    let timeouts = arena_core::setup::SetupTimeouts::from_config(&cfg, None).unwrap();
    let job = ReplaceJob { setup_timeouts: timeouts, ov: &ov, place: &place, opts, interrupt: &interrupt, keys_dir: &keys_dir, balance: None };
    replace_on(remote.clone(), &f, &cfg, &f, "r-alpha".into(), "devtest-alpha".into(), job).await.unwrap();
    assert_eq!(
        f.calls(),
        ["create devtest-alpha-new NVIDIA RTX A4000", "rename r-alpha devtest-alpha-old", "rename r-new devtest-alpha"],
        "swapped, but the original NOT terminated"
    );
    let execs = |host: &str| -> Vec<String> {
        remote.calls_to(host).into_iter().filter_map(|c| if let RemoteCall::Exec { cmd, .. } = c { Some(cmd) } else { None }).collect()
    };
    let (on_old, on_new) = (execs(old_host), execs(new_host));
    let wrote = |cmds: &[String], short: &str| cmds.iter().any(|c| c.ends_with(&name_file_command(short)) && c.contains("RUNPOD_POD_ID"));
    assert!(wrote(&on_new, "alpha") && wrote(&on_old, "alpha-old"), "each on the pod itself: {on_new:?} {on_old:?}");
    assert!(on_new.iter().any(|c| c.contains("OPENAI_API_KEY") && c.contains("sk-openai-alpha")), "{on_new:?}");
    // The copy ran on the original, as a replica: deletions mirrored into the kept folder,
    // the remote rsync started only on the new pod itself.
    assert!(
        on_old.iter().any(|c| c.contains("\nrsync ")
            && c.contains("'--delete'")
            && c.contains("--backup-dir=arena-sync-kept/")
            && c.contains("--filter=P arena-sync-kept/")
            && c.contains("--rsync-path=")
            && c.contains("r-new")),
        "{on_old:?}"
    );
    // …the new pod's baseline was started before it (a pod made for this copy), then sorted
    // after; and the sync that landed is on record on the original.
    let baseline = on_new.iter().position(|c| c.contains("arena-replica-ready") && c.contains("touch \"$s\"")).expect("baseline");
    let settle = on_new.iter().position(|c| c.contains("arena-replica-kept") && c.contains("del=1")).expect("settle");
    assert!(baseline < settle);
    assert!(on_old.iter().any(|c| c.contains("sent-to") && c.contains("'r-new'") && c.contains("rm -f")), "{on_old:?}");
    assert!(proxy_file(&dir.0).contains("10.0.0.2"), "the proxy routes alpha to the new pod");
}

/// Review finding (untested branch): a `replace` that terminated the original removes the
/// leftover local staging copy of its home (a participant's whole home, on the shared box).
#[tokio::test(start_paused = true)]
async fn a_finished_replace_removes_the_originals_leftover_staging_copy() {
    let dir = tmpdir("replace-stage");
    let cfg = keyed_cfg(&dir.0, "");
    let keys_dir = dir.0.display().to_string();
    let src_id: &'static str = Box::leak(format!("r-replace-{}", std::process::id()).into_boxed_str());
    let f = Fleet::new(vec![pod(src_id, "devtest-alpha", "runpod", Some(("10.0.0.1", 22001)))]);
    *f.to_create.lock().unwrap() = vec![pod("r-new", "", "runpod", Some(("10.0.0.2", 22001)))];
    let (old_host, new_host) = ("10.0.0.1:22001", "10.0.0.2:22001");
    let remote = Arc::new(FakeRemote::new());
    remote.on(old_host, "arena-replica-sent", FakeReply::stdout("arena-replica-sent=no\n"));
    remote.on(old_host, "RUNPOD_POD_ID", FakeReply::stdout(src_id));
    remote.on(new_host, "arena-replica-ready", FakeReply::stdout("arena-replica-ready\n"));
    remote.on(new_host, "MARK=", FakeReply::stdout("ID=r-new\nMARK=arena-replace-r-new\n"));
    remote.on(new_host, "SSH_OK", FakeReply::stdout("NVIDIA RTX A4000\nSSH_OK\n"));
    // Where `replace` looks for it (the system temp dir), removed by the guard even if an
    // assertion fails first — a failed or mutated run leaked these (live R, S11).
    let stage = TmpDir(staging_dir(&std::env::temp_dir(), src_id));
    std::fs::create_dir_all(stage.0.join("ARENA_materials")).unwrap();
    std::fs::write(stage.0.join("ARENA_materials/work.py"), "an earlier failed copy's stage").unwrap();
    let (ov, place, interrupt) = (SpecOverrides::default(), PlaceFlags::default(), Interrupt::manual());
    let opts = ReplaceOpts { keep_old: false, skip_proxy: false, dry_run: false, yes: true };
    let timeouts = arena_core::setup::SetupTimeouts::from_config(&cfg, None).unwrap();
    let job = ReplaceJob { setup_timeouts: timeouts, ov: &ov, place: &place, opts, interrupt: &interrupt, keys_dir: &keys_dir, balance: None };
    replace_on(remote, &f, &cfg, &f, src_id.into(), "devtest-alpha".into(), job).await.unwrap();
    assert_eq!(
        f.calls(),
        [
            "create devtest-alpha-new NVIDIA RTX A4000".to_string(),
            format!("rename {src_id} devtest-alpha-old"),
            "rename r-new devtest-alpha".to_string(),
            format!("terminate {src_id}")
        ]
    );
    assert!(!stage.0.exists(), "removed once the original is gone");
}

/// A balance that remembers prices into `dir` for the RunPod account (a fake: no call).
fn priced_balance(dir: &Path) -> crate::money::BalanceCtx {
    use arena_core::balance::{Account, FakeBalanceApi};
    let path = dir.join("state").join("devtest").join(arena_core::snapshot::BALANCE_FILE);
    crate::money::BalanceCtx { api: Arc::new(FakeBalanceApi::new().with(Account::Runpod)), path: Some(Ok(path)) }
}

/// The price the balance state remembers for pod `id`, if any.
fn remembered(bal: &crate::money::BalanceCtx, id: &str) -> Option<f64> {
    let (state, _) = arena_core::balance::load(bal.state_path().unwrap());
    state.accounts.get(&arena_core::balance::Account::Runpod).and_then(|a| a.prices.get(id)).map(|p| p.0)
}

/// `pods replace` remembers the new pod's price the moment it's made, as `create`/`up`
/// do — by id, so the promoted pod (renamed to the canonical name) keeps it, and a later
/// listing with no price (RunPod v1's) still prices it.
#[tokio::test(start_paused = true)]
async fn replace_remembers_the_new_pods_price_for_the_balance() {
    let dir = tmpdir("replace-price");
    let cfg = keyed_cfg(&dir.0, "");
    let keys_dir = dir.0.display().to_string();
    let f = fleet();
    let mut new = pod("r-new", "", "runpod", Some(("10.0.0.2", 22001)));
    new.cost_per_hr = Some(0.29);
    *f.to_create.lock().unwrap() = vec![new];
    let (old_host, new_host) = ("10.0.0.1:22001", "10.0.0.2:22001");
    let remote = Arc::new(FakeRemote::new());
    remote.on(old_host, "arena-replica-sent", FakeReply::stdout("arena-replica-sent=no\n"));
    remote.on(old_host, "RUNPOD_POD_ID", FakeReply::stdout("r-alpha"));
    remote.on(new_host, "arena-replica-ready", FakeReply::stdout("arena-replica-ready\n"));
    remote.on(new_host, "MARK=", FakeReply::stdout("ID=r-new\nMARK=arena-replace-r-new\n"));
    remote.on(new_host, "SSH_OK", FakeReply::stdout("NVIDIA RTX A4000\nSSH_OK\n"));
    let (ov, place, interrupt) = (SpecOverrides::default(), PlaceFlags::default(), Interrupt::manual());
    // --keep-old: the original isn't terminated (no staging clean-up of a shared id).
    let opts = ReplaceOpts { keep_old: true, skip_proxy: false, dry_run: false, yes: true };
    let timeouts = arena_core::setup::SetupTimeouts::from_config(&cfg, None).unwrap();
    let bal = priced_balance(&dir.0);
    let job = ReplaceJob { setup_timeouts: timeouts, ov: &ov, place: &place, opts, interrupt: &interrupt, keys_dir: &keys_dir, balance: Some(&bal) };
    replace_on(remote, &f, &cfg, &f, "r-alpha".into(), "devtest-alpha".into(), job).await.unwrap();
    assert_eq!(f.calls(), ["create devtest-alpha-new NVIDIA RTX A4000", "rename r-alpha devtest-alpha-old", "rename r-new devtest-alpha"]);
    assert_eq!(remembered(&bal, "r-new"), Some(0.29));
}

/// `migrate copy` remembers `-new`'s price the moment it's created — before the wait for
/// it to come up, so a copy that then fails (here: it never gets an endpoint) still leaves
/// the billing pod priced. An unpriced create remembers nothing.
#[tokio::test(start_paused = true)]
async fn migrate_copy_remembers_the_new_pods_price_even_when_the_copy_fails() {
    let dir = tmpdir("migrate-price");
    let cfg = keyed_cfg(&dir.0, "");
    let (ov, place, interrupt) = (SpecOverrides::default(), PlaceFlags::default(), Interrupt::manual());
    let timeouts = arena_core::setup::SetupTimeouts::from_config(&cfg, None).unwrap();
    let bal = priced_balance(&dir.0);
    for (id, price) in [("r-new", Some(0.33)), ("r-bare", None)] {
        let f = fleet();
        let mut new = pod(id, "", "runpod", None); // never gets an endpoint
        new.cost_per_hr = price;
        *f.to_create.lock().unwrap() = vec![new];
        let pods = f.pods.lock().unwrap().clone();
        let job = MigrateCopyJob {
            setup_timeouts: timeouts,
            ov: &ov,
            place: &place,
            dry_run: false,
            yes: true,
            interrupt: &interrupt,
            keys_dir: "/nonexistent",
            balance: Some(&bal),
        };
        let e = migrate_copy_on(Arc::new(FakeRemote::new()), &cfg, &f, "devtest-alpha", &pods, job).await.unwrap_err();
        assert!(format!("{e:#}").contains("devtest-alpha-new never came up"), "{e:#}");
        assert_eq!(f.calls(), ["create devtest-alpha-new NVIDIA RTX A4000"]);
        assert_eq!(remembered(&bal, id), price, "{id}");
    }
}

/// Review finding: `replace` / `migrate copy` created (and billed) the new pod, set it up,
/// and only then found the original stopped — leaving `-new` running and advising a
/// `migrate copy` that fails the same way. Refused while planning now, and the dry runs
/// say so; nothing is created.
#[tokio::test]
async fn a_stopped_original_is_refused_before_anything_is_created() {
    let dir = tmpdir("stopped-src");
    let cfg = keyed_cfg(&dir.0, "");
    let f = fleet(); // `create_pod` panics: reaching a create fails the test
    f.pods.lock().unwrap()[0].status = "EXITED".into();
    let (ov, place, interrupt) = (SpecOverrides::default(), PlaceFlags::default(), Interrupt::manual());
    let timeouts = arena_core::setup::SetupTimeouts::from_config(&cfg, None).unwrap();
    let replace = |dry_run| {
        let opts = ReplaceOpts { keep_old: false, skip_proxy: true, dry_run, yes: true };
        ReplaceJob { setup_timeouts: timeouts, ov: &ov, place: &place, opts, interrupt: &interrupt, keys_dir: "/nonexistent", balance: None }
    };
    let e = replace_on(Arc::new(FakeRemote::new()), &f, &cfg, &f, "r-alpha".into(), "devtest-alpha".into(), replace(false))
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("devtest-alpha is stopped (EXITED)") && e.contains("`arena pods start devtest-alpha`") && e.contains("nothing was created"), "{e}");
    take_would_refuse();
    replace_on(Arc::new(FakeRemote::new()), &f, &cfg, &f, "r-alpha".into(), "devtest-alpha".into(), replace(true)).await.unwrap();
    assert!(take_would_refuse().iter().any(|r| r.contains("devtest-alpha is stopped")));
    // migrate copy: the same, whether -new would be created or re-synced.
    let pods = f.pods.lock().unwrap().clone();
    let copy = |dry_run| MigrateCopyJob { setup_timeouts: timeouts, ov: &ov, place: &place, dry_run, yes: true, interrupt: &interrupt, keys_dir: "/nonexistent", balance: None };
    let e = migrate_copy_on(Arc::new(FakeRemote::new()), &cfg, &f, "devtest-alpha", &pods, copy(false)).await.unwrap_err().to_string();
    assert!(e.contains("devtest-alpha is stopped (EXITED)"), "{e}");
    migrate_copy_on(Arc::new(FakeRemote::new()), &cfg, &f, "devtest-alpha", &pods, copy(true)).await.unwrap();
    assert!(take_would_refuse().iter().any(|r| r.contains("devtest-alpha is stopped")));
    let mut with_new = pods.clone();
    with_new.push(pod("r-alpha-new", "devtest-alpha-new", "runpod", Some(("10.0.0.2", 22001))));
    let remote = Arc::new(FakeRemote::new());
    let e = migrate_copy_on(remote.clone(), &cfg, &f, "devtest-alpha", &with_new, copy(false)).await.unwrap_err().to_string();
    assert!(e.contains("devtest-alpha is stopped (EXITED)"), "{e}");
    assert!(f.calls().is_empty() && remote.calls().is_empty(), "{:?} {:?}", f.calls(), remote.calls());
}

/// Review finding: after a revert the summary said `arena pods pull <new>` while pull's
/// positional argument was the backup LABEL — that pulled EVERY pod's home into a folder
/// named after the new pod, onto the disk the live cohort shares. Positionals are pods now
/// (`--label` takes the label; `-t` is the hidden old spelling), and the hint names the pod
/// that way: it resolves to that pod alone, with no label read from it.
#[test]
fn the_revert_followup_backs_up_only_the_new_pod() {
    let line = revert_followup("devtest-alpha", "devtest-alpha-new");
    assert!(line.contains("`arena pods pull devtest-alpha-new`"), "{line}");
    let PodCmd::Pull { label, sel, target_flag, .. } = pods_cmd(&["pull", "devtest-alpha-new"]) else { panic!("not a pull") };
    let cfg = Config::parse("MACHINE_NAME_PREFIX=devtest\n");
    let naming = arena_core::selector::Naming::from_config(&cfg);
    let args = crate::pods::pull::pull_label_and_targets(label, &sel.targets, &target_flag, &naming, &[]).unwrap();
    assert_eq!(args, crate::pods::pull::PullArgs { label: None, targets: vec!["devtest-alpha-new".to_string()], note: None });
}

/// Review finding: a revert wrote `~/.name` through the listed endpoint of a STOPPED
/// original (after a `--no-final-sync` cutover) — whatever pod answers there would be
/// labelled as this machine. A stopped pod isn't written, and every write runs only on
/// the pod it names (RUNPOD_POD_ID, in the same session).
#[tokio::test(start_paused = true)]
async fn a_revert_never_writes_through_a_stopped_originals_stale_endpoint() {
    let dir = tmpdir("revert-stopped");
    let cfg = cfg(&dir.0);
    let mut old = pod("r-alpha", "devtest-alpha-old", "runpod", Some(("10.0.0.1", 22001)));
    old.status = "EXITED".into();
    let f = Fleet::new(vec![old, pod("r-alpha-new", "devtest-alpha", "runpod", Some(("10.0.0.2", 22001)))]);
    let remote = FakeRemote::new();
    cutover_revert(&remote, &cfg, &f, &f, "devtest-alpha", "r-alpha-new", "r-alpha", false).await;
    assert_eq!(f.calls(), ["rename r-alpha-new devtest-alpha-new", "rename r-alpha devtest-alpha"]);
    assert!(remote.calls_to("10.0.0.1:22001").is_empty(), "{:?}", remote.calls());
    let on_new = remote.calls_to("10.0.0.2:22001");
    assert!(
        matches!(&on_new[..], [RemoteCall::Exec { cmd, .. }] if cmd.ends_with(&name_file_command("alpha-new")) && cmd.contains("= 'r-alpha-new' ]")),
        "{on_new:?}"
    );
}

/// Review finding: `come_back` waited for each pod to settle, then `settled_sync` waited
/// for ALL of them again, and once more before its re-sync — a pod that never got an
/// endpoint back cost 3 × 600 s before `start`/`restart`/`reimage` reported it. One wait.
#[tokio::test(start_paused = true)]
async fn a_pod_that_never_comes_back_costs_one_settle_wait() {
    let dir = tmpdir("start-dead");
    let cfg = cfg(&dir.0);
    let mut f = fleet();
    f.pods.lock().unwrap()[0].status = "EXITED".into();
    f.blank_lists = (1..10_000).collect(); // never an endpoint again
    let pods = f.pods.lock().unwrap().clone();
    let settle = SettleWait { stable_secs: 30, timeout_secs: 600, poll: Duration::from_secs(15) };
    let start = tokio::time::Instant::now();
    let opts = RestartOpts { yes: true, ..Default::default() };
    let e = handle_start(&f, Arc::new(FakeRemote::new()), &cfg, &named(&pods[..1]), opts, settle, &dir.0.display().to_string())
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("NOT set up again") && e.contains("never stabilized within 600s"), "{e}");
    let took = start.elapsed();
    assert!(took >= Duration::from_secs(600) && took < Duration::from_secs(700), "one wait, not three: {took:?}");
}

/// Lane P × lane R: `up` calls a pod that stays up behind RunPod's SSH proxy only (no public
/// IP) a host failure after its 5-min grace. `replace`'s new pod used to wait its whole
/// 900 s window for an endpoint such a host never gives, then say only "timed out". It
/// now gives up at the grace, naming the host's problem, with `-new` left in place and
/// nothing renamed or copied.
#[tokio::test(start_paused = true)]
async fn a_replacement_behind_the_ssh_proxy_only_is_given_up_on_after_ups_grace() {
    let dir = tmpdir("replace-proxy-only");
    let cfg = keyed_cfg(&dir.0, "");
    let f = fleet();
    let mut new = pod("r-new", "", "runpod", None);
    new.proxy_only = true;
    *f.to_create.lock().unwrap() = vec![new];
    let remote = Arc::new(FakeRemote::new());
    let (ov, place, interrupt) = (SpecOverrides::default(), PlaceFlags::default(), Interrupt::manual());
    let opts = ReplaceOpts { keep_old: false, skip_proxy: true, dry_run: false, yes: true };
    let timeouts = arena_core::setup::SetupTimeouts::from_config(&cfg, None).unwrap();
    let job = ReplaceJob { setup_timeouts: timeouts, ov: &ov, place: &place, opts, interrupt: &interrupt, keys_dir: "/nonexistent", balance: None };
    let start = tokio::time::Instant::now();
    let e = replace_on(remote.clone(), &f, &cfg, &f, "r-alpha".into(), "devtest-alpha".into(), job).await.unwrap_err();
    let e = format!("{e:#}");
    let took = start.elapsed();
    assert!(e.contains("devtest-alpha-new never came up") && e.contains(arena_core::fleet::PROXY_ONLY) && e.contains("no public IP"), "{e}");
    assert!(took >= Duration::from_secs(300) && took < Duration::from_secs(400), "up's grace, not the 900 s window: {took:?}");
    assert_eq!(f.calls(), ["create devtest-alpha-new NVIDIA RTX A4000"], "nothing renamed, nothing terminated");
    assert!(remote.calls().is_empty(), "nothing copied: {:?}", remote.calls());
}

/// The same for the settle of `start`/`restart`/`reimage`: a pod that comes back behind the
/// SSH proxy only is named as such after the grace, not after the whole settle window as a
/// bare "never stabilized". A listing that lacks the pod meanwhile doesn't restart the clock.
#[tokio::test(start_paused = true)]
async fn a_pod_started_behind_the_ssh_proxy_only_is_named_after_ups_grace() {
    let dir = tmpdir("start-proxy-only");
    let cfg = cfg(&dir.0);
    let f = fleet();
    {
        let mut pods = f.pods.lock().unwrap();
        pods[0].status = "EXITED".into();
        (pods[0].ssh_ip, pods[0].ssh_port, pods[0].proxy_only) = (None, None, true);
    }
    let pods = f.pods.lock().unwrap().clone();
    let settle = SettleWait { stable_secs: 30, timeout_secs: 600, poll: Duration::from_secs(15) };
    let gap = async {
        // A listing gap two minutes in (the aggregate list drops a failed backend's pods).
        tokio::time::sleep(Duration::from_secs(120)).await;
        let gone = f.pods.lock().unwrap().remove(0);
        tokio::time::sleep(Duration::from_secs(40)).await;
        f.pods.lock().unwrap().insert(0, gone);
    };
    let start = tokio::time::Instant::now();
    let opts = RestartOpts { yes: true, ..Default::default() };
    let (sel, keys_dir) = (named(&pods[..1]), dir.0.display().to_string());
    let (res, ()) = tokio::join!(handle_start(&f, Arc::new(FakeRemote::new()), &cfg, &sel, opts, settle, &keys_dir), gap);
    let e = res.unwrap_err().to_string();
    assert!(e.contains("NOT set up again") && e.contains(arena_core::fleet::PROXY_ONLY) && e.contains("no public IP"), "{e}");
    let took = start.elapsed();
    assert!(took >= Duration::from_secs(300) && took < Duration::from_secs(400), "the grace, through the gap: {took:?}");
}

/// The endpoint-flap fix's second half (review: no test held it): when the listing the
/// first post-restart sync takes shows the pod without an endpoint, the sync doesn't
/// route it — it waits for that pod again and syncs once more.
#[tokio::test]
async fn a_post_restart_sync_that_missed_the_pod_waits_and_syncs_again() {
    let dir = tmpdir("restart-resync");
    let cfg = cfg(&dir.0);
    let mut f = fleet();
    // List calls: 1 the restart's lookup, 2-3 the settle wait, 4 setup's listing, 5-6 the
    // sync's settle wait — so 7 is the listing the first sync routes from.
    f.blank_lists = vec![7];
    handle_restart(&f, Arc::new(FakeRemote::new()), &cfg, "alpha", restart_opts(true), FAST, &dir.0.display().to_string()).await.unwrap();
    assert!(*f.lists.lock().unwrap() >= 10, "a second wait and sync: {}", f.lists.lock().unwrap());
    let px = proxy_file(&dir.0);
    assert!(px.contains("name=devtest-alpha") && px.contains("23001"), "{px}");
}

/// Review finding (untested branch): `pods start` of a powered-off VM — its disk kept, so
/// no setup — still syncs the proxy to it.
#[tokio::test]
async fn starting_a_powered_off_vm_syncs_the_proxy_without_setup() {
    let dir = tmpdir("start-vm");
    let cfg = cfg(&dir.0);
    let f = fleet();
    f.pods.lock().unwrap()[3].status = "off".into(); // delta: a Hetzner VM
    let pods = f.pods.lock().unwrap().clone();
    let remote = Arc::new(FakeRemote::new());
    let opts = RestartOpts { yes: true, ..Default::default() };
    handle_start(&f, remote.clone(), &cfg, &named(&pods[3..4]), opts, FAST, &dir.0.display().to_string()).await.unwrap();
    assert_eq!(f.calls(), ["start h-delta"]);
    assert!(remote.calls().is_empty(), "its disk was kept: no setup");
    let px = proxy_file(&dir.0);
    assert!(px.contains("name=devtest-delta") && px.contains("1022"), "{px}");
}

/// Review finding (untested branch): the cutover reverts when the proxy sync went through
/// but didn't route the name to the new pod (listed without an endpoint at that moment) —
/// nobody could reach it.
#[tokio::test(start_paused = true)]
async fn a_cutover_whose_sync_doesnt_route_the_new_pod_reverts() {
    let dir = tmpdir("cutover-unrouted");
    let cfg = cfg(&dir.0);
    let owner = Fleet::new(vec![
        pod("r-alpha", "devtest-alpha", "runpod", Some(("10.0.0.1", 22001))),
        pod("r-alpha-new", "devtest-alpha-new", "runpod", Some(("10.0.0.2", 22001))),
    ]);
    // What the proxy sync lists right after the swap: the promoted pod, without an endpoint.
    let seen = Fleet::new(vec![
        pod("r-alpha", "devtest-alpha-old", "runpod", Some(("10.0.0.1", 22001))),
        pod("r-alpha-new", "devtest-alpha", "runpod", None),
    ]);
    let remote = Arc::new(FakeRemote::new());
    remote.on("10.0.0.2:22001", "SSH_OK", FakeReply::stdout("NVIDIA RTX A4000\nSSH_OK\n"));
    let pods = owner.pods.lock().unwrap().clone();
    let opts = CutoverOpts { skip_proxy: false, no_final_sync: true, dry_run: false, yes: true };
    let e = migrate_cutover_on(remote, &seen, &cfg, &owner, "devtest-alpha", &pods, opts, &dir.0.display().to_string())
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("cutover reverted") && e.contains("doesn't route devtest-alpha to the new pod"), "{e}");
    assert_eq!(
        owner.calls(),
        ["rename r-alpha devtest-alpha-old", "rename r-alpha-new devtest-alpha", "rename r-alpha-new devtest-alpha-new", "rename r-alpha devtest-alpha"]
    );
}

/// Review finding: after a `--no-final-sync` cutover, `migrate finish` terminated a STOPPED
/// `-old` — and with it its volume, holding what changed on it after the last sync. It's
/// kept unless `--discard-stopped`, saying what it may hold and since when; with the flag
/// it goes, and its leftover local staging copy with it (that removal was untested too).
#[tokio::test]
async fn finish_keeps_a_stopped_old_unless_told_to_discard_it() {
    let dir = tmpdir("finish-stopped");
    let cfg = cfg(&dir.0);
    let old_id: &'static str = Box::leak(format!("r-finish-{}", std::process::id()).into_boxed_str());
    // (removed by the guard even if an assertion fails first — see the replace test)
    let stage = TmpDir(staging_dir(&std::env::temp_dir(), old_id));
    let mut old = pod(old_id, "devtest-bravo-old", "runpod", Some(("10.0.0.1", 22002)));
    old.status = "EXITED".into();
    let mut f = Fleet::new(vec![pod("r-bravo-new", "devtest-bravo", "runpod", Some(("10.0.0.3", 22009))), old]);
    f.volumes = vec![(old_id, 20)];
    let remote = FakeRemote::new();
    remote.on("10.0.0.3:22009", "arena-last-sync", FakeReply::stdout("arena-last-sync=1791457323\n"));
    let listed = f.pods.lock().unwrap().clone();
    let e = migrate_finish_on(&remote, &f, &cfg, &f, "devtest-bravo", &listed, FinishOpts { yes: true, ..Default::default() })
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("NOT terminating devtest-bravo-old: it is stopped (EXITED)") && e.contains("its 20 GB volume"), "{e}");
    assert!(e.contains("2026-10-08 11:02 UTC") && e.contains("--discard-stopped") && e.contains("`arena pods pull devtest-bravo-old`"), "{e}");
    assert!(f.calls().is_empty());
    take_would_refuse();
    migrate_finish_on(&remote, &f, &cfg, &f, "devtest-bravo", &listed, FinishOpts { dry_run: true, yes: true, ..Default::default() }).await.unwrap();
    assert!(take_would_refuse().iter().any(|r| r.contains("it is stopped")));
    // --discard-stopped: it goes, and the leftover staging copy of its home with it.
    std::fs::create_dir_all(stage.0.join("ARENA_materials")).unwrap();
    std::fs::write(stage.0.join("ARENA_materials/work.py"), "a participant's home").unwrap();
    let opts = FinishOpts { discard_stopped: true, yes: true, ..Default::default() };
    migrate_finish_on(&remote, &f, &cfg, &f, "devtest-bravo", &listed, opts).await.unwrap();
    assert_eq!(f.calls(), [format!("terminate {old_id}")]);
    assert!(!stage.0.exists(), "the leftover staging copy is removed once its source is gone");
    // The refusal's words for the volume it may (not) have.
    assert!(stopped_old_refusal("o", "EXITED", "c", Some(0), "t").contains("--discard-stopped is safe"));
    assert!(stopped_old_refusal("o", "EXITED", "c", None, "t").contains("couldn't read whether it has a volume"));
}

#[test]
fn the_new_flags_parse() {
    assert!(matches!(
        pods_cmd(&["migrate", "finish", "alpha", "--discard-stopped"]),
        PodCmd::Migrate { cmd: MigrateCmd::Finish { discard_stopped: true, .. } }
    ));
    assert!(matches!(pods_cmd(&["migrate", "finish", "alpha"]), PodCmd::Migrate { cmd: MigrateCmd::Finish { discard_stopped: false, .. } }));
    assert!(matches!(
        pods_cmd(&["restart", "alpha", "--wipe-ok", "--no-setup", "--skip-proxy"]),
        PodCmd::Restart { wipe_ok: true, no_setup: true, skip_proxy: true, dry_run: false, .. }
    ));
    assert!(matches!(pods_cmd(&["restart", "alpha"]), PodCmd::Restart { wipe_ok: false, no_setup: false, .. }));
    assert!(matches!(pods_cmd(&["stop", "alpha", "--wipe-ok"]), PodCmd::Stop { wipe_ok: true, .. }));
    assert!(matches!(pods_cmd(&["terminate", "alpha", "--revoke-key"]), PodCmd::Terminate { revoke_key: true, all: false, .. }));
    assert!(matches!(pods_cmd(&["terminate", "--all", "--revoke-key"]), PodCmd::Terminate { revoke_key: true, all: true, .. }));
    assert!(matches!(pods_cmd(&["terminate", "--all", "--unlock"]), PodCmd::Terminate { unlock: true, all: true, .. }));
    assert!(matches!(pods_cmd(&["terminate", "alpha"]), PodCmd::Terminate { unlock: false, .. }));
    assert!(matches!(pods_cmd(&["lock", "alpha", "--dry-run"]), PodCmd::Lock { dry_run: true, .. }));
    assert!(matches!(pods_cmd(&["unlock", "--all"]), PodCmd::Unlock { dry_run: false, .. }));
}

/// The recorded refusal of a locked pod (`locked_stop_refused.json`, live 2026-10-08),
/// replayed through the v2 backend's own error classification — exactly what the real
/// backend returns for it (`Locked`).
fn recorded_locked(ctx: &str) -> Error {
    let e = arena_core::provider::runpod_v2::recorded::locked_refusal(ctx);
    assert!(e.is_locked() && e.to_string().contains("HTTP 400") && e.to_string().contains("Pod is locked"), "{e:?}");
    e
}

/// The fleet with alpha locked (as the v2 listing reports it).
fn locked_fleet() -> Fleet {
    let f = fleet();
    let mut pods = f.pods.lock().unwrap().clone();
    pods[0].locked = Some(true);
    for p in pods.iter_mut().skip(1).filter(|p| p.provider == "runpod") {
        p.locked = Some(false);
    }
    Fleet::new(pods)
}

/// Every lifecycle command refuses a pod the listing reports locked — up front, the
/// whole command, before any call (a batch never half-runs) — with the one sentence that
/// says how to unlock it. A dry run says it would be refused, and does nothing.
#[tokio::test]
async fn every_mutating_command_refuses_a_locked_pod_up_front() {
    let dir = tmpdir("locked-refusals");
    let cfg = cfg(&dir.0);
    let hint = "devtest-alpha is locked — `arena pods unlock devtest-alpha` first";
    // (argv, the refusal)
    let cases: &[(&[&str], String)] = &[
        (&["stop", "alpha", "--wipe-ok"], format!("refusing to stop: {hint}")),
        (&["stop", "--all", "--wipe-ok"], format!("refusing to stop: {hint}")),
        (&["restart", "alpha", "--wipe-ok"], format!("refusing to restart: {hint}")),
        (&["reimage", "alpha", "bravo"], format!("refusing to reimage: {hint}")),
        (&["terminate", "alpha"], format!("refusing to terminate: {hint} — or pass --unlock to lift the lock as part of the terminate")),
        (&["terminate", "--all"], format!("refusing to terminate: {hint} — or pass --unlock")),
    ];
    for (argv, want) in cases {
        let f = locked_fleet();
        let e = handle_pods(pods_cmd(argv), &f, Arc::new(FakeRemote::new()), &cfg, true).await.unwrap_err().to_string();
        assert!(e.contains(want.as_str()), "{argv:?}: `{want}` in {e}");
        assert!(f.calls().is_empty(), "{argv:?}: nothing touched: {:?}", f.calls());
        // The dry run says so and changes nothing. (Reimage's needs an image and keys
        // from config first; its refusal comes before those, as checked above.)
        if argv[0] == "reimage" {
            continue;
        }
        let mut dry = argv.to_vec();
        dry.push("--dry-run");
        take_would_refuse();
        handle_pods(pods_cmd(&dry), &f, Arc::new(FakeRemote::new()), &cfg, true).await.unwrap();
        assert!(f.calls().is_empty(), "{dry:?}");
        let notes = take_would_refuse();
        assert!(
            notes.len() == 1 && notes[0].starts_with("(without --dry-run: ") && notes[0].contains(want.as_str()),
            "{dry:?}: the dry run says it would be refused: {notes:?}"
        );
    }
    // Unlocked pods next to it go ahead as before.
    let f = locked_fleet();
    handle_pods(pods_cmd(&["stop", "bravo", "--wipe-ok"]), &f, Arc::new(FakeRemote::new()), &cfg, true).await.unwrap();
    assert_eq!(f.calls(), ["stop r-bravo"]);
}

/// `replace` (with and without `--keep-old`), `migrate cutover` of a locked original and
/// `migrate finish` of a locked `-old`: refused up front with the unlock sentence (plus,
/// for the two that retire the pod for a new one, that the new one isn't locked by
/// itself) — nothing created, renamed or terminated. The dry run prints the plan and
/// says it would be refused, touching nothing either.
#[tokio::test]
async fn replace_and_migrate_refuse_a_locked_pod() {
    let dir = tmpdir("locked-replace");
    // A GPU type for the replacement spec (the fake's pod_spec reports none).
    let cfg = Config::parse(&format!("{}GPU_TYPE=\"NVIDIA RTX A4000\"\n", cfg_text(&dir.0)));
    let hint = |name: &str| format!("{name} is locked — `arena pods unlock {name}` first");
    /// `pods replace <src>` on `f`, `-y`, no proxy.
    async fn replace(f: &Fleet, cfg: &Config, src: &str, keep_old: bool, dry_run: bool) -> anyhow::Result<()> {
        let timeouts = arena_core::setup::SetupTimeouts::from_config(cfg, None).unwrap();
        let opts = ReplaceOpts { keep_old, skip_proxy: true, dry_run, yes: true };
        let id = f.pods.lock().unwrap().iter().find(|p| p.name == src).unwrap().id.clone();
        let (ov, place, interrupt) = (SpecOverrides::default(), PlaceFlags::default(), Interrupt::manual());
        let job = ReplaceJob { setup_timeouts: timeouts, ov: &ov, place: &place, opts, interrupt: &interrupt, keys_dir: "/nonexistent", balance: None };
        replace_on(Arc::new(FakeRemote::new()), f, cfg, f, id, src.to_string(), job).await
    }
    let want = format!("refusing to replace: {}{LOCKED_REPLACEMENT_NOTE}", hint("devtest-alpha"));
    for keep_old in [false, true] {
        // Fleet::create_pod panics: reaching a create fails the test.
        let f = locked_fleet();
        let e = replace(&f, &cfg, "devtest-alpha", keep_old, false).await.unwrap_err().to_string();
        assert_eq!(e, want, "--keep-old={keep_old}");
        assert!(f.calls().is_empty(), "--keep-old={keep_old}: {:?}", f.calls());
        take_would_refuse();
        replace(&f, &cfg, "devtest-alpha", keep_old, true).await.unwrap();
        assert!(f.calls().is_empty(), "--keep-old={keep_old} (dry run): {:?}", f.calls());
        assert_eq!(take_would_refuse(), [format!("(without --dry-run: {want})")], "--keep-old={keep_old}");
    }
    // An unlocked source gets past the lock check (to the plan; dry run: nothing else).
    let f = locked_fleet();
    replace(&f, &cfg, "devtest-bravo", false, true).await.unwrap();
    assert!(take_would_refuse().is_empty() && f.calls().is_empty());

    // migrate cutover: the original (alpha) locked, its -new built.
    let f = locked_fleet();
    f.pods.lock().unwrap().push(pod("r-alpha-new", "devtest-alpha-new", "runpod", Some(("10.0.0.2", 22001))));
    let pods = f.pods.lock().unwrap().clone();
    let want = format!("refusing to cut over: {}{LOCKED_REPLACEMENT_NOTE}", hint("devtest-alpha"));
    let opts = |dry_run| CutoverOpts { skip_proxy: true, no_final_sync: false, dry_run, yes: true };
    let e = migrate_cutover_on(Arc::new(FakeRemote::new()), &f, &cfg, &f, "devtest-alpha", &pods, opts(false), "/nonexistent")
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(e, want);
    migrate_cutover_on(Arc::new(FakeRemote::new()), &f, &cfg, &f, "devtest-alpha", &pods, opts(true), "/nonexistent").await.unwrap();
    assert_eq!(take_would_refuse(), [format!("(without --dry-run: {want})")]);
    assert!(f.calls().is_empty(), "{:?}", f.calls());

    // migrate finish: the parked -old locked (the live pod isn't).
    let mut pods = fleet().pods.lock().unwrap().clone();
    let mut old = pod("r-bravo-old", "devtest-bravo-old", "runpod", None);
    old.locked = Some(true);
    pods.push(old);
    let f = Fleet::new(pods.clone());
    let want = format!("refusing to terminate: {}", hint("devtest-bravo-old"));
    let e = migrate_finish_on(&FakeRemote::new(), &f, &cfg, &f, "devtest-bravo", &pods, FinishOpts { dry_run: false, yes: true, ..Default::default() }).await.unwrap_err().to_string();
    assert_eq!(e, want);
    migrate_finish_on(&FakeRemote::new(), &f, &cfg, &f, "devtest-bravo", &pods, FinishOpts { dry_run: true, yes: true, ..Default::default() }).await.unwrap();
    assert_eq!(take_would_refuse(), [format!("(without --dry-run: {want})")]);
    assert!(f.calls().is_empty(), "{:?}", f.calls());
    // Locked since it was listed: the API's refusal reads as the same sentence.
    let mut unlisted = pods.clone();
    unlisted.last_mut().unwrap().locked = None;
    let e = migrate_finish_on(&FakeRemote::new(), &f, &cfg, &f, "devtest-bravo", &unlisted, FinishOpts { dry_run: false, yes: true, ..Default::default() }).await.unwrap_err().to_string();
    assert_eq!(e, format!("terminating devtest-bravo-old: {}", hint("devtest-bravo-old")));
    assert!(f.calls().is_empty());
    // Unlocked: it goes.
    f.server_locked.lock().unwrap().clear();
    migrate_finish_on(&FakeRemote::new(), &f, &cfg, &f, "devtest-bravo", &unlisted, FinishOpts { dry_run: false, yes: true, ..Default::default() }).await.unwrap();
    assert_eq!(f.calls(), ["terminate r-bravo-old"]);
}

/// disk-nits (review #42): a `CONTROL_MIN_FREE_GB` the via-local copy can't read refuses
/// replace, migrate copy and a cutover's final sync before anything is created or asked —
/// found only at the copy, it left the new pod built, set up and billing. Its dry runs say
/// they would be refused; a cutover without a final sync copies nothing, so it doesn't care.
#[tokio::test]
async fn replace_and_migrate_refuse_a_bad_floor_before_creating_anything() {
    let dir = tmpdir("bad-floor");
    let cfg = Config::parse(&format!("{}GPU_TYPE=\"NVIDIA RTX A4000\"\nCONTROL_MIN_FREE_GB=2.5\n", cfg_text(&dir.0)));
    let timeouts = || arena_core::setup::SetupTimeouts::from_config(&cfg, None).unwrap();
    let (ov, place, interrupt) = (SpecOverrides::default(), PlaceFlags::default(), Interrupt::manual());
    let want = |what: &str| {
        format!(
            "CONTROL_MIN_FREE_GB must be a whole number of GB, 0..=100000 (got `2.5`) — not starting the {what}: its copy via \
             local staging needs it (fix it, or unset it for the default)"
        )
    };
    for dry_run in [false, true] {
        // Fleet::create_pod panics: reaching a create fails the test.
        let f = locked_fleet();
        let id = f.pods.lock().unwrap().iter().find(|p| p.name == "devtest-bravo").unwrap().id.clone();
        let opts = ReplaceOpts { keep_old: false, skip_proxy: true, dry_run, yes: true };
        let job = ReplaceJob { setup_timeouts: timeouts(), ov: &ov, place: &place, opts, interrupt: &interrupt, keys_dir: "/nonexistent", balance: None };
        take_would_refuse();
        let r = replace_on(Arc::new(FakeRemote::new()), &f, &cfg, &f, id, "devtest-bravo".into(), job).await;
        if dry_run {
            r.unwrap();
            assert_eq!(take_would_refuse(), [format!("(without --dry-run: {})", want("replace"))]);
        } else {
            assert_eq!(r.unwrap_err().to_string(), want("replace"));
        }
        assert!(f.calls().is_empty(), "dry run {dry_run}: {:?}", f.calls());

        let pods = f.pods.lock().unwrap().clone();
        let job = MigrateCopyJob { setup_timeouts: timeouts(), ov: &ov, place: &place, dry_run, yes: true, interrupt: &interrupt, keys_dir: "/nonexistent", balance: None };
        let r = migrate_copy_on(Arc::new(FakeRemote::new()), &cfg, &f, "devtest-bravo", &pods, job).await;
        if dry_run {
            r.unwrap();
            assert_eq!(take_would_refuse(), [format!("(without --dry-run: {})", want("migrate"))]);
        } else {
            assert_eq!(r.unwrap_err().to_string(), want("migrate"));
        }
        assert!(f.calls().is_empty(), "dry run {dry_run}: {:?}", f.calls());
    }
    // A cutover's final sync: refused alike; without one, nothing to copy — not refused.
    let f = locked_fleet();
    f.pods.lock().unwrap().push(pod("r-bravo-new", "devtest-bravo-new", "runpod", Some(("10.0.0.2", 22001))));
    let pods = f.pods.lock().unwrap().clone();
    let cutover = |no_final_sync, dry_run| CutoverOpts { skip_proxy: true, no_final_sync, dry_run, yes: true };
    let e = migrate_cutover_on(Arc::new(FakeRemote::new()), &f, &cfg, &f, "devtest-bravo", &pods, cutover(false, false), "/nonexistent")
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(e, want("cut over"));
    migrate_cutover_on(Arc::new(FakeRemote::new()), &f, &cfg, &f, "devtest-bravo", &pods, cutover(false, true), "/nonexistent").await.unwrap();
    assert_eq!(take_would_refuse(), [format!("(without --dry-run: {})", want("cut over"))]);
    migrate_cutover_on(Arc::new(FakeRemote::new()), &f, &cfg, &f, "devtest-bravo", &pods, cutover(true, true), "/nonexistent").await.unwrap();
    assert!(take_would_refuse().is_empty(), "no final sync: no copy, no floor needed");
    assert!(f.calls().is_empty(), "{:?}", f.calls());
}

/// A pod locked since it was listed (or on a backend whose listing doesn't say): the
/// API's recorded 400 reads as the same sentence — for restart, terminate and rename.
#[tokio::test]
async fn the_apis_locked_refusal_reads_as_the_unlock_hint() {
    let dir = tmpdir("locked-api");
    let cfg = cfg(&dir.0);
    let hint = "devtest-bravo is locked — `arena pods unlock devtest-bravo` first";
    let f = fleet();
    f.server_locked.lock().unwrap().push("r-bravo".into());
    let e = handle_pods(pods_cmd(&["terminate", "bravo", "--skip-proxy"]), &f, Arc::new(FakeRemote::new()), &cfg, true)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains(hint), "{e}");
    let restart = handle_pods(
        pods_cmd(&["restart", "bravo", "--wipe-ok", "--no-setup", "--skip-proxy"]),
        &f,
        Arc::new(FakeRemote::new()),
        &cfg,
        true,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(restart.contains(hint), "{restart}");
    let e = handle_pods(pods_cmd(&["stop", "bravo", "--wipe-ok"]), &f, Arc::new(FakeRemote::new()), &cfg, true)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("1 pod(s) failed to stop"), "{e}");
    assert!(f.calls().is_empty(), "{:?}", f.calls());
    // Rename: allowed on a locked pod by the spec's wording (assumed — see `lock`), so
    // it isn't refused up front; if the API refuses it after all, it reads the same.
    let f = locked_fleet();
    let book = no_api(&dir.0.join("none.csv"));
    handle_rename(&f, Arc::new(FakeRemote::new()), &cfg, &book, one("alpha", "echo"), true, false, true).await.unwrap();
    assert_eq!(f.calls(), ["rename r-alpha devtest-echo"]);
    let mut f = locked_fleet();
    f.rename_locked = vec!["r-bravo"];
    let e = handle_rename(&f, Arc::new(FakeRemote::new()), &cfg, &book, one("bravo", "echo"), true, false, true)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains(hint), "{e}");
}

/// `terminate --unlock` (the end-of-program `--all --unlock`): each locked pod is
/// unlocked right before its own terminate, the others just terminated; one whose unlock
/// fails is NOT terminated. Never without the flag (see the refusals above).
#[tokio::test]
async fn terminate_unlock_lifts_each_lock_right_before_its_terminate() {
    let dir = tmpdir("terminate-unlock");
    let cfg = cfg(&dir.0);
    let f = locked_fleet();
    handle_pods(pods_cmd(&["terminate", "--all", "--unlock", "--skip-proxy"]), &f, Arc::new(FakeRemote::new()), &cfg, true)
        .await
        .unwrap();
    assert_eq!(
        f.calls(),
        ["lock r-alpha false", "terminate r-alpha", "terminate r-bravo", "terminate r-charlie", "terminate h-delta"]
    );
    // A failed unlock: that pod stays (locked), and the command says so.
    let mut f = locked_fleet();
    f.fail_unlock = vec!["r-alpha"];
    let e = handle_pods(pods_cmd(&["terminate", "alpha", "--unlock", "--skip-proxy"]), &f, Arc::new(FakeRemote::new()), &cfg, true)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("devtest-alpha: unlocking failed, so it was NOT terminated"), "{e}");
    assert!(f.calls().is_empty(), "{:?}", f.calls());
    // --unlock on a pod that isn't locked: just the terminate.
    let f = locked_fleet();
    handle_pods(pods_cmd(&["terminate", "bravo", "--unlock", "--skip-proxy"]), &f, Arc::new(FakeRemote::new()), &cfg, true)
        .await
        .unwrap();
    assert_eq!(f.calls(), ["terminate r-bravo"]);
}

/// The fleet with alpha locked, plus two locked boxes that aren't the cohort's on the same
/// account: `solo-gpu` (the `@solo-gpu` staff entry) and `james-gpu` (off the list).
fn fleet_with_locked_staff() -> Fleet {
    let mut pods = locked_fleet().pods.lock().unwrap().clone();
    for (id, name) in [("r-solo", "solo-gpu"), ("r-james", "james-gpu")] {
        let mut p = pod(id, name, "runpod", Some(("10.0.0.1", 22009)));
        p.locked = Some(true);
        pods.push(p);
    }
    Fleet::new(pods)
}

/// `--all --unlock` (the end-of-program one-liner) lifts only the cohort's locks: a locked
/// staff box and a locked pod off the list are KEPT — never unlocked, never terminated —
/// the cohort goes, and the exit fails naming them. Said up front in the dry run. Named
/// alone, a staff box is the operator's explicit choice: unlocked and terminated.
#[tokio::test]
async fn terminate_all_unlock_never_unlocks_a_box_that_isnt_the_cohorts() {
    let dir = tmpdir("terminate-unlock-staff");
    let cfg = cfg(&dir.0);
    let f = fleet_with_locked_staff();
    let e = handle_pods(pods_cmd(&["terminate", "--all", "--unlock", "--skip-proxy"]), &f, Arc::new(FakeRemote::new()), &cfg, true)
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        f.calls(),
        ["lock r-alpha false", "terminate r-alpha", "terminate r-bravo", "terminate r-charlie", "terminate h-delta"],
        "the staff box and the off-list pod: no unlock, no terminate"
    );
    assert!(
        e.contains("james-gpu, solo-gpu KEPT, not terminated: locked, and not this cohort's (`devtest-…`)")
            && e.contains("`arena pods terminate james-gpu --unlock`"),
        "{e}"
    );
    let left: Vec<String> = f.pods.lock().unwrap().iter().map(|p| p.name.clone()).collect();
    assert_eq!(left, ["solo-gpu", "james-gpu"]);
    assert_eq!(*f.server_locked.lock().unwrap(), ["r-solo", "r-james"], "still locked");

    // The dry run says so, and does nothing.
    let f = fleet_with_locked_staff();
    take_would_refuse();
    handle_pods(pods_cmd(&["terminate", "--all", "--unlock", "--dry-run"]), &f, Arc::new(FakeRemote::new()), &cfg, true)
        .await
        .unwrap();
    assert!(f.calls().is_empty(), "{:?}", f.calls());
    let notes = take_would_refuse();
    assert!(
        notes.len() == 1 && notes[0].contains("james-gpu, solo-gpu KEPT, not terminated") && notes[0].ends_with("the exit fails)"),
        "{notes:?}"
    );

    // Without --unlock the whole command is refused, and the refusal says --unlock
    // wouldn't take those two either.
    let e = handle_pods(pods_cmd(&["terminate", "--all"]), &f, Arc::new(FakeRemote::new()), &cfg, true)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("refusing to terminate: devtest-alpha, james-gpu, solo-gpu are locked"), "{e}");
    assert!(e.contains("(with --all it unlocks only the cohort's: james-gpu, solo-gpu would be KEPT, locked"), "{e}");
    assert!(f.calls().is_empty());

    // Only kept pods left: nothing is touched, and it fails.
    let f = fleet_with_locked_staff();
    f.pods.lock().unwrap().retain(|p| !p.name.starts_with("devtest-"));
    let e = handle_pods(pods_cmd(&["terminate", "--all", "--unlock"]), &f, Arc::new(FakeRemote::new()), &cfg, true)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("KEPT, not terminated"), "{e}");
    assert!(f.calls().is_empty());

    // Named on its own: the operator's choice — unlocked, then terminated.
    for target in ["solo-gpu", "james-gpu"] {
        let f = fleet_with_locked_staff();
        handle_pods(pods_cmd(&["terminate", target, "--unlock", "--skip-proxy"]), &f, Arc::new(FakeRemote::new()), &cfg, true)
            .await
            .unwrap();
        let id = if target == "solo-gpu" { "r-solo" } else { "r-james" };
        assert_eq!(f.calls(), [format!("lock {id} false"), format!("terminate {id}")]);
    }
}

/// A pod locked after the listing (it said unlocked): under `--unlock` the API's Locked
/// refusal is the lock the operator asked to lift — unlocked, terminated on the one retry.
/// Never for a box `--all --unlock` mustn't unlock, and never without the flag (the hint).
#[tokio::test]
async fn terminate_unlock_lifts_a_lock_taken_since_the_listing() {
    let dir = tmpdir("terminate-unlock-race");
    let cfg = cfg(&dir.0);
    let f = fleet();
    f.server_locked.lock().unwrap().push("r-bravo".into());
    handle_pods(pods_cmd(&["terminate", "bravo", "--unlock", "--skip-proxy"]), &f, Arc::new(FakeRemote::new()), &cfg, true)
        .await
        .unwrap();
    assert_eq!(f.calls(), ["lock r-bravo false", "terminate r-bravo"]);

    // --all --unlock: the cohort pod is unlocked and goes; an off-list box locked since
    // the listing is NOT unlocked — it fails with the hint, the rest still go.
    let f = fleet();
    f.pods.lock().unwrap().push(pod("r-james", "james-gpu", "runpod", None));
    f.server_locked.lock().unwrap().extend(["r-bravo".to_string(), "r-james".to_string()]);
    let e = handle_pods(pods_cmd(&["terminate", "--all", "--unlock", "--skip-proxy"]), &f, Arc::new(FakeRemote::new()), &cfg, true)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("1 pod(s) failed to terminate"), "{e}");
    assert_eq!(
        f.calls(),
        ["terminate r-alpha", "lock r-bravo false", "terminate r-bravo", "terminate r-charlie", "terminate h-delta"],
    );
    assert!(f.pods.lock().unwrap().iter().any(|p| p.name == "james-gpu"));

    // The retried unlock fails: not terminated, said so.
    let mut f = fleet();
    f.fail_unlock = vec!["r-bravo"];
    f.server_locked.lock().unwrap().push("r-bravo".into());
    let e = handle_pods(pods_cmd(&["terminate", "bravo", "--unlock", "--skip-proxy"]), &f, Arc::new(FakeRemote::new()), &cfg, true)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("devtest-bravo: locked since it was listed, and unlocking failed, so it was NOT terminated"), "{e}");
    assert!(f.calls().is_empty(), "{:?}", f.calls());
}

/// `pods lock`/`unlock`: the lockable pods are (un)locked, one already in that state is
/// left alone, one on a backend that can't lock is reported and fails the exit. A dry
/// run and an unscoped selection change nothing.
#[tokio::test]
async fn lock_and_unlock_act_on_what_can_be_locked_and_fail_on_the_rest() {
    let dir = tmpdir("lock-cmd");
    let cfg = cfg(&dir.0);
    let f = locked_fleet();
    let run = |argv: &'static [&'static str]| handle_pods(pods_cmd(argv), &f, Arc::new(FakeRemote::new()), &cfg, true);
    let e = run(&["lock", "alpha", "bravo", "delta"]).await.unwrap_err().to_string();
    assert!(e.contains("1 pod(s) can't be locked: devtest-delta"), "{e}");
    assert_eq!(f.calls(), ["lock r-bravo true"], "alpha already was; delta can't be");
    run(&["lock", "bravo"]).await.unwrap();
    assert_eq!(f.calls().len(), 1, "already locked: nothing sent");
    run(&["unlock", "bravo", "--dry-run"]).await.unwrap();
    assert_eq!(f.calls().len(), 1);
    run(&["unlock", "alpha", "bravo"]).await.unwrap();
    assert_eq!(f.calls(), ["lock r-bravo true", "lock r-alpha false", "lock r-bravo false"]);
    let e = run(&["lock", "delta"]).await.unwrap_err().to_string();
    assert!(e.contains("can't be locked: devtest-delta (leave other providers' pods out with --on runpod)"), "{e}");
    run(&["lock", "delta", "--dry-run"]).await.unwrap(); // says it would fail; changes nothing
    let e = run(&["lock"]).await.unwrap_err().to_string();
    assert!(e.contains("name the pods to lock"), "{e}");
    assert_eq!(f.calls().len(), 3);
    // The listing now says unlocked: a stop goes through again.
    run(&["stop", "alpha", "--wipe-ok"]).await.unwrap();
    assert_eq!(f.calls().last().unwrap(), "stop r-alpha");
}
