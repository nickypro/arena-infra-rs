use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arena_core::capability::{support, Backend, Op, Support};
use arena_core::idle::fixture::Reply;
use arena_core::provider::Resize;
use arena_core::remote::{FakeRemote, FakeReply, RemoteCall};
use arena_core::{Config, Error, Pod, PodSpec, Provider, Result};
use async_trait::async_trait;
use clap::Parser;

use super::reimage_disk::{flags, host_lacks_room, no_room, plan_words, volume_refusal, words, Sizes};
use super::{handle_pods, PodCmd};
use crate::cli::{Cli, Cmd};
use crate::output_test::rendered;
use crate::pods::lifecycle::{afterwards_line, handle_reimage, DiskFate, DiskOp, RestartOpts, SettleWait};
use crate::pods::lock::take_would_refuse;
use crate::pods::selection::Selected;

/// What RunPod answers a resize its host hasn't the room for (live 2026-11-02, v2: 9000 GB asked).
const NO_ROOM: &str = "reimage pod HTTP 400 Bad Request: {\"detail\":\"This machine does not have enough disk available, please try to \
                       allocate less container or volume disk.\",\"status\":400,\"title\":\"Bad Request\"}";

/// A fleet of one provider each: two RunPod pods (alpha with a 20 GB volume, bravo with none), a
/// Hetzner VM, a Vast instance and a Shadeform one. Records every reimage with its sizes; a pod
/// in `no_room` refuses a resize as a host without the room does, and is left as it was.
struct Fleet {
    pods: Mutex<Vec<Pod>>,
    calls: Mutex<Vec<String>>,
    no_room: Vec<&'static str>,
}

impl Fleet {
    fn new() -> Self {
        let pod = |id: &str, name: &str, provider: &str, port: u16| Pod {
            id: id.into(),
            name: name.into(),
            provider: provider.into(),
            status: "RUNNING".into(),
            ssh_ip: Some("10.0.0.1".into()),
            ssh_port: Some(port),
            ..Default::default()
        };
        Self {
            pods: Mutex::new(vec![
                pod("r-alpha", "devtest-alpha", "runpod", 22001),
                pod("r-bravo", "devtest-bravo", "runpod", 22002),
                pod("h-vm", "devtest-vm", "hetzner", 22),
                pod("v-vast", "devtest-vast", "vast", 22004),
                pod("s-shade", "devtest-shade", "shadeform", 22005),
            ]),
            calls: Mutex::default(),
            no_room: vec![],
        }
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn get(&self, name: &str) -> Pod {
        self.pods.lock().unwrap().iter().find(|p| p.name == name).cloned().unwrap()
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
    /// As the fleet routes it: the capability table's cell for the pod's provider.
    fn supports(&self, op: Op, pod: Option<&Pod>) -> Support {
        pod.and_then(|p| Backend::named(&p.provider)).map_or(Support::Yes, |b| support(b, op))
    }
    async fn list_pods(&self) -> Result<Vec<Pod>> {
        Ok(self.pods.lock().unwrap().clone())
    }
    async fn create_pod(&self, spec: &PodSpec) -> Result<Pod> {
        unreachable!("no create: {}", spec.name)
    }
    async fn stop_pod(&self, _id: &str) -> Result<()> {
        unreachable!()
    }
    async fn restart_pod(&self, _id: &str) -> Result<()> {
        unreachable!()
    }
    async fn terminate_pod(&self, _id: &str) -> Result<()> {
        unreachable!()
    }
    /// alpha: a 60 GB disk and a 20 GB volume; bravo: a 50 GB disk, no volume.
    async fn pod_spec(&self, id: &str) -> Result<PodSpec> {
        let mut spec = PodSpec::from_config(&Config::parse(""));
        (spec.disk_gb, spec.volume_gb) = if id == "r-alpha" { (60, 20) } else { (50, 0) };
        Ok(spec)
    }
    async fn reimage_pod(&self, id: &str, image: &str, env: &[(String, String)]) -> Result<()> {
        self.reimage_pod_resized(id, image, env, Resize::default()).await
    }
    /// A container comes back on a new endpoint (port +1000); a host without the room refuses.
    async fn reimage_pod_resized(&self, id: &str, image: &str, _env: &[(String, String)], resize: Resize) -> Result<()> {
        if resize.any() && self.no_room.contains(&id) {
            return Err(Error::provider(NO_ROOM));
        }
        let sizes = format!("{}{}", resize.disk_gb.map(|g| format!(" disk={g}")).unwrap_or_default(), resize.volume_gb.map(|g| format!(" volume={g}")).unwrap_or_default());
        self.calls.lock().unwrap().push(format!("reimage {id} {image}{sizes}"));
        for p in self.pods.lock().unwrap().iter_mut().filter(|p| p.id == id) {
            p.ssh_port = p.ssh_port.map(|port| port + 1000);
        }
        Ok(())
    }
}

/// A temp dir (per process + test), removed when dropped.
struct TmpDir(PathBuf);
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn tmpdir(tag: &str) -> TmpDir {
    let d = std::env::temp_dir().join(format!("arena-reimage-disk-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    TmpDir(d)
}

/// Setup, a write-only proxy and a readable public key in `dir`; the repo on the container disk.
fn cfg(dir: &Path) -> Config {
    std::fs::write(dir.join("k"), "private").unwrap();
    std::fs::write(dir.join("k.pub"), "ssh-ed25519 AAAAtest devtest-test").unwrap();
    Config::parse(&format!(
        "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(alpha bravo vm vast shade)\n\
         ARENA_REPO_OWNER=o\nARENA_REPO_NAME=r\nGIT_SSH_KEY_LOCAL=/nonexistent/devtest_deploy_key\n\
         SSH_PROXY_HOST=localhost\nSSH_PROXY_NGINX_CONFIG_PATH={}\nSSH_PROXY_RELOAD_CMD=\"\"\nVSCODE_PREINSTALL=0\nAGENT_NOTES=0\n\
         REPO_ON_VOLUME=0\nSHARED_SSH_KEY_PATH={}\nIMAGE=img:9\nGPU_TYPE=\"NVIDIA RTX A4000\"\n",
        dir.join("proxy.conf").display(),
        dir.join("k").display()
    ))
}

/// No one on any pod.
fn idle_remote() -> Arc<FakeRemote> {
    let remote = Arc::new(FakeRemote::new());
    for port in [22001, 22002, 22004, 22005] {
        remote.on(&format!("10.0.0.1:{port}"), "ARENA_IDLE_NOW", FakeReply::stdout(&Reply::default().render()));
    }
    remote
}

fn pods_cmd(argv: &[&str]) -> PodCmd {
    match Cli::try_parse_from(["arena", "pods"].iter().chain(argv).copied()).map(|c| c.cmd) {
        Ok(Cmd::Pods(p)) => p,
        other => panic!("{argv:?}: {:?}", other.err()),
    }
}

const FAST: SettleWait = SettleWait { stable_secs: 0, timeout_secs: 5, poll: Duration::ZERO };

fn one(f: &Fleet, name: &str) -> Selected {
    Selected { pods: vec![f.get(name)], named: true }
}

/// `pods reimage NAME --disk GB`: the reimage's own request carries the size, the image is
/// reimage's default (RUNPOD_DOCKER_IMAGE / IMAGE: `--disk` alone keeps it), and the pod comes
/// back as from any reimage — set up again on its new endpoint, proxied. The gates stay:
/// refused without --wipe-ok (the disk is wiped), and a pod someone is on is refused unless
/// --force, its retry repeating the size.
#[tokio::test]
async fn reimage_disk_resizes_in_the_same_request_and_sets_the_pod_up_again() {
    let dir = tmpdir("wired");
    let cfg = cfg(&dir.0);
    let keys_dir = dir.0.display().to_string();
    let f = Fleet::new();
    let remote = idle_remote();
    // The wipe gate: --disk doesn't lift it, even with --yes.
    let e = handle_pods(pods_cmd(&["reimage", "alpha", "--disk", "500"]), &f, remote.clone(), &cfg, true).await.unwrap_err().to_string();
    assert!(e.contains("refusing to reimage devtest-alpha without --wipe-ok"), "{e}");
    assert!(f.calls().is_empty());
    // Through the command line: the size goes in the reimage's request.
    handle_pods(pods_cmd(&["reimage", "alpha", "--disk", "500", "--wipe-ok", "--no-setup", "--skip-proxy"]), &f, remote.clone(), &cfg, true)
        .await
        .unwrap();
    assert_eq!(f.calls(), ["reimage r-alpha img:9 disk=500"]);
    // With setup: it runs on the endpoint the pod came back on, and the proxy points there.
    let g = Fleet::new();
    let before = remote.calls().len();
    let opts = RestartOpts { wipe_ok: true, yes: true, ..Default::default() };
    let resize = Resize { disk_gb: Some(200), volume_gb: Some(40) };
    handle_reimage(&g, remote.clone(), &cfg, &one(&g, "devtest-alpha"), None, resize, false, opts, FAST, &keys_dir).await.unwrap();
    assert_eq!(g.calls(), ["reimage r-alpha img:9 disk=200 volume=40"]);
    let after: Vec<RemoteCall> = remote.calls()[before..].to_vec();
    assert!(after.iter().any(|c| c.host() == "10.0.0.1:23001"), "set up on the new container: {after:?}");
    let px = std::fs::read_to_string(dir.0.join("proxy.conf")).unwrap_or_default();
    assert!(px.contains("name=devtest-alpha") && px.contains("23001"), "{px}");
    // Someone on it: refused, and the default line's retry repeats the size.
    let h = Fleet::new();
    let busy = Arc::new(FakeRemote::new());
    busy.on("10.0.0.1:22001", "ARENA_IDLE_NOW", FakeReply::stdout(&Reply { sessions: vec![[10, 9, 9, 9]], ..Reply::default() }.render()));
    let opts = RestartOpts { no_setup: true, skip_proxy: true, ..opts };
    let resize = Resize { disk_gb: Some(500), volume_gb: None };
    let e = handle_reimage(&h, busy.clone(), &cfg, &one(&h, "devtest-alpha"), None, resize, false, opts, FAST, &keys_dir).await.unwrap_err().to_string();
    assert!(e.contains("refusing to reimage devtest-alpha: someone is on it") && e.contains("--force"), "{e}");
    assert!(h.calls().is_empty());
    // --force goes on.
    handle_reimage(&h, busy, &cfg, &one(&h, "devtest-alpha"), None, resize, true, opts, FAST, &keys_dir).await.unwrap();
    assert_eq!(h.calls(), ["reimage r-alpha img:9 disk=500"]);
}

/// Where it can't be done in place, it's refused before anything is read or asked — dry run or
/// not — with the command that does work, from the capability table: Hetzner's volume or a
/// bigger type, and Vast and Shadeform, which can't reimage at all. A RunPod volume that would
/// shrink, or a pod without one, is refused once its sizes are read; a smaller container disk is
/// fine (it's wiped anyway).
#[tokio::test]
async fn reimage_disk_is_refused_up_front_where_it_cant_be_done_in_place() {
    let dir = tmpdir("refused");
    let cfg = cfg(&dir.0);
    let f = Fleet::new();
    let remote = idle_remote();
    let refused = |argv: &[&str]| {
        let mut args = vec!["reimage"];
        args.extend_from_slice(argv);
        let (f, remote, cfg) = (&f, remote.clone(), &cfg);
        let cmd = pods_cmd(&args);
        async move { handle_pods(cmd, f, remote, cfg, true).await.unwrap_err().to_string() }
    };
    for dry in [false, true] {
        let extra: &[&str] = if dry { &["--wipe-ok", "--dry-run"] } else { &["--wipe-ok"] };
        let e = refused(&[&["vm", "--disk", "500"][..], extra].concat()).await;
        assert!(e.starts_with("refusing to resize the disk of devtest-vm (hetzner): a Hetzner VM's disk comes with its server type"), "{e}");
        assert!(e.contains("`arena pods volume add devtest-vm 500`") && e.contains("`arena pods replace devtest-vm --server-type <type>`"), "{e}");
        let e = refused(&[&["vm", "--volume", "100"][..], extra].concat()).await;
        assert!(e.contains("refusing to grow the volume of devtest-vm (hetzner)") && e.contains("`arena pods volume add devtest-vm 100`"), "{e}");
        let e = refused(&[&["vast", "--disk", "500"][..], extra].concat()).await;
        assert!(e.starts_with("refusing to reimage devtest-vast (vast): Vast can't swap an instance's image in place"), "{e}");
        let e = refused(&[&["shade", "--disk", "500"][..], extra].concat()).await;
        assert!(e.starts_with("refusing to reimage devtest-shade (shadeform)"), "{e}");
        // A RunPod pod beside a VM: the whole selection, and how to leave the VM out.
        let e = refused(&[&["alpha", "vm", "--disk", "500"][..], extra].concat()).await;
        assert!(e.contains("1 of the 2 pod(s) can't be") && e.contains("--exclude devtest-vm") && e.contains("<name>"), "{e}");
        // The volume: never shrunk, never added.
        let e = refused(&[&["alpha", "--volume", "10"][..], extra].concat()).await;
        assert_eq!(
            e,
            "refusing to shrink devtest-alpha's volume from 20 GB to 10 GB: RunPod only grows one — pass --volume 20 or more, \
             or `arena pods replace devtest-alpha --volume 10` (a new pod, files copied)"
        );
        let e = refused(&[&["bravo", "--volume", "50"][..], extra].concat()).await;
        assert!(e.starts_with("refusing to grow devtest-bravo's volume: it has none of its own"), "{e}");
        assert!(e.contains("`arena pods replace devtest-bravo --volume 50`"), "{e}");
    }
    assert!(f.calls().is_empty(), "{:?}", f.calls());
    // `--disk 0` is no size.
    assert!(Cli::try_parse_from(["arena", "pods", "reimage", "alpha", "--disk", "0"]).is_err());
    // A smaller disk, the same volume: fine.
    handle_pods(pods_cmd(&["reimage", "alpha", "--disk", "20", "--volume", "20", "--wipe-ok", "--no-setup", "--skip-proxy"]), &f, remote, &cfg, true)
        .await
        .unwrap();
    assert_eq!(f.calls(), ["reimage r-alpha img:9 disk=20 volume=20"]);
}

/// docs/OUTPUT.md: by default the image and one line that says the size and that the disk is
/// wiped — the prompt, or the dry run's; `-v` adds each pod's sizes to its plan. A host without
/// the room: one line pointing at `pods replace` with the same size (the whole error with
/// `-v`), and the exit fails; the pod was left as it was.
#[tokio::test]
async fn reimage_disk_says_the_size_and_the_wipe_in_one_line_and_the_host_without_room_in_one() {
    let dir = tmpdir("output");
    let cfg = cfg(&dir.0);
    let keys_dir = dir.0.display().to_string();
    let opts = RestartOpts { wipe_ok: true, no_setup: true, skip_proxy: true, ..Default::default() };
    let resize = Resize { disk_gb: Some(500), volume_gb: None };
    let plan = DiskFate::Wiped { volume_gb: Some(20) }.describe(DiskOp::Reimage, &arena_core::volume::RepoSite::configured("/root/r"));
    let then = afterwards_line(arena_core::provider::ContainerReset::Yes, false, true);
    for (verbose, dry_run, want) in [
        (false, false, "image: img:9\nReimage devtest-alpha with a 500 GB disk (wipes its disk and the work on it)? [y/N] y\n".to_string()),
        (
            true,
            false,
            format!(
                "image: img:9\nkeys:  devtest-test\nWill reimage 1 pod(s):\n  devtest-alpha (id=r-alpha, runpod): {plan}; disk 60 → 500 GB\n  \
                 image: img:9\n  {then}\nProceed? [y/N] y\n"
            ),
        ),
        (false, true, "image: img:9\n[dry-run] would reimage devtest-alpha with a 500 GB disk (wipes its disk and the work on it)\n".to_string()),
        (
            true,
            true,
            format!("image: img:9\nkeys:  devtest-test\n[dry-run] would reimage 1 pod(s):\n  devtest-alpha (id=r-alpha, runpod): {plan}; disk 60 → 500 GB\n  {then}\n"),
        ),
    ] {
        let f = Fleet::new();
        let opts = RestartOpts { dry_run, ..opts };
        let alpha = one(&f, "devtest-alpha");
        let run = handle_reimage(&f, idle_remote(), &cfg, &alpha, None, resize, false, opts, FAST, &keys_dir);
        let got = rendered(verbose, &["y"], run).await;
        // What follows a reimage that went through is reimage's own, as without a resize.
        let after = "[reimaged] devtest-alpha\n(--no-setup: devtest-alpha back as the bare image — `arena pods setup devtest-alpha` \
                     restores ~/.name, the git remote and keys)\n";
        let want = if dry_run { want } else { format!("{want}{after}") };
        assert_eq!(got, want, "verbose {verbose}, dry run {dry_run}");
        assert_eq!(f.calls().len(), usize::from(!dry_run), "{:?}", f.calls());
    }
    take_would_refuse();
    // The host hasn't the room: one line, the fix in it; the whole error with -v.
    for verbose in [false, true] {
        let mut f = Fleet::new();
        f.no_room = vec!["r-alpha"];
        let opts = RestartOpts { yes: true, ..opts };
        let alpha = one(&f, "devtest-alpha");
        let run = handle_reimage(&f, idle_remote(), &cfg, &alpha, None, resize, false, opts, FAST, &keys_dir);
        let got = rendered(verbose, &[], run).await;
        let line = if verbose {
            format!("[failed] devtest-alpha: {}", Error::provider(NO_ROOM))
        } else {
            "[devtest-alpha] FAILED reimage: its host hasn't the room for a 500 GB disk (instead: arena pods replace devtest-alpha --disk 500)".to_string()
        };
        assert!(got.contains(&format!("\n{line}\n")), "verbose {verbose}:\n{got}");
        assert!(got.ends_with("Error: 1 pod(s) failed to reimage: devtest-alpha\n"), "{got}");
        assert!(f.calls().is_empty());
    }
    // Someone came on it since the prompt: one line, its retry with the size.
    let f = Fleet::new();
    let remote = Arc::new(FakeRemote::new());
    let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = probes.clone();
    remote.on(
        "10.0.0.1:22001",
        "ARENA_IDLE_NOW",
        FakeReply::from_cmd(move |_| {
            let sessions = if seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 { vec![] } else { vec![[10, 9, 9, 9]] };
            FakeReply::stdout(&Reply { sessions, ..Reply::default() }.render())
        }),
    );
    let opts = RestartOpts { yes: true, ..opts };
    let alpha = one(&f, "devtest-alpha");
    let got = rendered(false, &[], handle_reimage(&f, remote, &cfg, &alpha, None, resize, false, opts, FAST, &keys_dir)).await;
    let line = "[devtest-alpha] not reimaged: someone is on it (1 SSH session) (retry: arena pods reimage devtest-alpha --wipe-ok \
                --no-setup --skip-proxy --disk 500, or add --force)";
    assert!(got.contains(&format!("\n{line}\n")), "{got}");
    assert!(f.calls().is_empty());
}

/// The words, pure: the prompt's, `-v`'s, a retry's flags, the volume's refusals and the
/// errors that mean "no room on the host".
#[test]
fn reimage_disk_words() {
    let disk = Resize { disk_gb: Some(500), volume_gb: None };
    let both = Resize { disk_gb: Some(500), volume_gb: Some(100) };
    assert_eq!(words(Resize::default(), 1), "");
    assert_eq!(words(disk, 1), " with a 500 GB disk");
    assert_eq!(words(both, 1), " with a 500 GB disk and a 100 GB volume");
    assert_eq!(words(both, 2), " with 500 GB disks and 100 GB volumes");
    assert_eq!(words(Resize { disk_gb: None, volume_gb: Some(80) }, 1), " with an 80 GB volume");
    for (gb, a) in [(8, "an"), (11, "an"), (18, "an"), (80, "an"), (110, "a"), (180, "a"), (800, "an"), (11000, "an"), (100, "a"), (500, "a"), (1000, "a")] {
        assert_eq!(words(Resize { disk_gb: Some(gb), volume_gb: None }, 1), format!(" with {a} {gb} GB disk"), "{gb}");
    }
    assert_eq!(plan_words(Sizes::default()), "");
    assert_eq!(plan_words(Sizes { disk: Some((60, 500)), volume: Some((20, 100)) }), "; disk 60 → 500 GB, volume 20 → 100 GB");
    assert_eq!((flags(Resize::default()), flags(both)), (String::new(), " --disk 500 --volume 100".to_string()));
    assert_eq!(volume_refusal("devtest-a", 20, disk), None, "no volume asked for");
    assert_eq!(volume_refusal("devtest-a", 20, Resize { disk_gb: None, volume_gb: Some(20) }), None, "the same size");
    assert_eq!(volume_refusal("devtest-a", 20, both), None);
    assert!(volume_refusal("devtest-a", 200, both).unwrap().contains("from 200 GB to 100 GB"));
    assert!(volume_refusal("devtest-a", 0, both).unwrap().contains("it has none of its own"));
    assert!(host_lacks_room(NO_ROOM));
    assert!(host_lacks_room("reimage pod HTTP 400: insufficient disk space on host"));
    for other in [
        "reimage pod: HTTP 400 Bad Request: Pod is locked",
        "reimage pod: HTTP 500",
        "can't reach the provider",
        "reimage pod HTTP 403 Forbidden: insufficient permissions",
        "reimage pod HTTP 402: not enough balance to resize",
    ] {
        assert!(!host_lacks_room(other), "{other}");
    }
    let e = Error::provider(NO_ROOM);
    assert!(no_room("devtest-a", disk, &e).is_some());
    assert_eq!(no_room("devtest-a", Resize::default(), &e), None, "a plain reimage isn't a resize");
    assert_eq!(no_room("devtest-a", disk, &Error::provider("reimage pod: HTTP 500")), None);
}
