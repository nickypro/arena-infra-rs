//! Moving a Hetzner VM (prod 2026-10-30: cx33 → cx43 by a hand-written "swapper"): `replace` /
//! `migrate` with `--server-type`, `--onto`, `--onto-spare`, the "anyone on it?" checks and the
//! VM's own identity — end to end over a fake Hetzner backend (with spares) and `FakeRemote`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use arena_core::idle::fixture::Reply;
use arena_core::remote::{FakeRemote, FakeReply, RemoteCall};
use arena_core::spare::SpareCloud;
use arena_core::{Config, Error, Pod, PodSpec, Provider, Result};
use async_trait::async_trait;

use crate::cli::PlaceFlags;
use crate::pods::adopt::Source;
use crate::pods::create::SpecOverrides;
use crate::pods::migrate::{migrate_copy_on, migrate_cutover_on, CutoverOpts, MigrateCopyJob};
use crate::pods::replace::{replace_on, ReplaceJob, ReplaceOpts};
use crate::Interrupt;

/// A Hetzner project: fleet servers, capacity spares (left out of the fleet listing, as the
/// real backend does), every mutation recorded.
struct Vms {
    pods: Mutex<Vec<Pod>>,
    spares: Mutex<Vec<Pod>>,
    to_create: Mutex<Vec<Pod>>,
    calls: Mutex<Vec<String>>,
}

impl Vms {
    fn new(pods: Vec<Pod>, spares: Vec<Pod>) -> Self {
        Self { pods: Mutex::new(pods), spares: Mutex::new(spares), to_create: Mutex::default(), calls: Mutex::default() }
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn record(&self, c: String) {
        self.calls.lock().unwrap().push(c);
    }
}

#[async_trait]
impl Provider for Vms {
    fn name(&self) -> &'static str {
        "hetzner"
    }
    fn describe(&self, spec: &PodSpec) -> String {
        format!("{} CPU VM", spec.server_type.as_deref().unwrap_or("cx33"))
    }
    async fn list_pods(&self) -> Result<Vec<Pod>> {
        Ok(self.pods.lock().unwrap().clone())
    }
    async fn create_pod(&self, spec: &PodSpec) -> Result<Pod> {
        let mut queue = self.to_create.lock().unwrap();
        assert!(!queue.is_empty(), "not exercised by this test: create {}", spec.name);
        let mut pod = queue.remove(0);
        pod.name = spec.name.clone();
        pod.gpu_type = spec.server_type.clone();
        // A create labelled a spare lands among the spares (left out of the fleet listing).
        let spare = spec.api_extra.as_ref().and_then(|x| x.get("labels")).and_then(|l| l.get(arena_core::spare::SPARE_LABEL)).and_then(|v| v.as_str()).map(String::from);
        let mark = spare.as_ref().map(|v| format!(" [spare={v}]")).unwrap_or_default();
        self.record(format!("create {} {}{mark}", spec.name, spec.server_type.as_deref().unwrap_or("-")));
        if spare.is_some() {
            self.spares.lock().unwrap().push(pod.clone());
        } else {
            self.pods.lock().unwrap().push(pod.clone());
        }
        Ok(pod)
    }
    async fn stop_pod(&self, _id: &str) -> Result<()> {
        unreachable!("no stop here")
    }
    async fn restart_pod(&self, _id: &str) -> Result<()> {
        unreachable!("no restart here")
    }
    async fn terminate_pod(&self, id: &str) -> Result<()> {
        self.record(format!("terminate {id}"));
        self.pods.lock().unwrap().retain(|p| p.id != id);
        self.spares.lock().unwrap().retain(|p| p.id != id);
        Ok(())
    }
    async fn rename_pod(&self, id: &str, new_name: &str) -> Result<()> {
        self.record(format!("rename {id} {new_name}"));
        for p in self.pods.lock().unwrap().iter_mut().filter(|p| p.id == id) {
            p.name = new_name.to_string();
        }
        Ok(())
    }
    fn restart_wipes_container_disk(&self, _pod: &Pod) -> bool {
        false
    }
    async fn pod_spec(&self, id: &str) -> Result<PodSpec> {
        let ty = self.pods.lock().unwrap().iter().find(|p| p.id == id).and_then(|p| p.gpu_type.clone());
        Ok(PodSpec { server_type: ty, ..PodSpec::default() })
    }
}

#[async_trait]
impl SpareCloud for Vms {
    async fn spares(&self) -> Result<Vec<Pod>> {
        Ok(self.spares.lock().unwrap().clone())
    }
    async fn adopt(&self, id: &str, name: &str, new_name: &str, _retry: bool) -> Result<()> {
        let mut spares = self.spares.lock().unwrap();
        let Some(i) = spares.iter().position(|p| p.id == id && p.name == name) else { return Err(Error::provider("isn't a spare")) };
        let mut p = spares.remove(i);
        p.name = new_name.to_string();
        self.record(format!("adopt {id} {new_name}"));
        self.pods.lock().unwrap().push(p);
        Ok(())
    }
}

fn vm(id: &str, name: &str, ty: &str, ip: &str, location: &str) -> Pod {
    Pod {
        id: id.into(),
        name: name.into(),
        provider: "hetzner".into(),
        status: "RUNNING".into(),
        gpu_type: Some(ty.into()),
        ssh_ip: Some(ip.into()),
        ssh_port: Some(22),
        location: Some(location.into()),
        ..Default::default()
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
    let d = std::env::temp_dir().join(format!("arena-move-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    TmpDir(d)
}

/// A Hetzner fleet's config with a write-only proxy in `dir` (never reloads nginx).
fn cfg(dir: &Path) -> Config {
    std::fs::write(dir.join("k"), "private").unwrap();
    std::fs::write(dir.join("k.pub"), "ssh-ed25519 AAAAtest devtest-test").unwrap();
    Config::parse(&format!(
        "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(alpha bravo charlie)\nARENA_REPO_OWNER=o\nARENA_REPO_NAME=r\n\
         GIT_SSH_KEY_LOCAL=/nonexistent/devtest_deploy_key\nSSH_PROXY_HOST=localhost\nSSH_PROXY_NGINX_CONFIG_PATH={}\n\
         SSH_PROXY_RELOAD_CMD=\"\"\nVSCODE_PREINSTALL=0\nREPO_ON_VOLUME=0\nSHARED_SSH_KEY_PATH={}\nHETZNER_LOCATION=nbg1,any\n",
        dir.join("proxy.conf").display(),
        dir.join("k").display()
    ))
}

const OLD: &str = "10.0.0.1:22";
const NEW: &str = "10.0.0.2:22";

/// No one on `host` (the idle probe's reply), the copy's bookkeeping, a delivery marker and a
/// health check that pass for the VM `new_id` at `new_host`.
fn scripted(remote: &FakeRemote, src_id: &str, new_host: &str, new_id: &str) {
    remote.on(OLD, "ARENA_IDLE_NOW", FakeReply::stdout(&Reply { gpu: None, ..Reply::default() }.render()));
    remote.on(OLD, "arena-replica-sent", FakeReply::stdout("arena-replica-sent=no\n"));
    remote.on(OLD, "instance-id", FakeReply::stdout(src_id));
    remote.on(new_host, "arena-replica-ready", FakeReply::stdout("arena-replica-ready\n"));
    remote.on(new_host, "MARK=", FakeReply::stdout(&format!("ID={new_id}\nMARK=arena-replace-{new_id}\n")));
    remote.on(new_host, "SSH_OK", FakeReply::stdout("SSH_OK\n"));
}

fn execs(remote: &FakeRemote, host: &str) -> Vec<String> {
    remote.calls_to(host).into_iter().filter_map(|c| if let RemoteCall::Exec { cmd, .. } = c { Some(cmd) } else { None }).collect()
}

#[allow(clippy::too_many_arguments)]
fn replace_job<'a>(
    ov: &'a SpecOverrides,
    place: &'a PlaceFlags,
    interrupt: &'a Interrupt,
    keys: &'a str,
    source: Source,
    spares: Option<&'a dyn SpareCloud>,
    force: bool,
    cfg: &Config,
) -> ReplaceJob<'a> {
    ReplaceJob {
        setup_timeouts: arena_core::setup::SetupTimeouts::from_config(cfg, None).unwrap(),
        ov,
        place,
        opts: ReplaceOpts { keep_old: false, skip_proxy: false, dry_run: false, yes: true, force },
        interrupt,
        keys_dir: keys,
        balance: None,
        source,
        spares,
        volumes: None,
    }
}

/// The swapper's job as one command: `replace alpha --server-type cx43` on a cx33 in nbg1. The new
/// VM lands in fsn1 (said), is set up, gets the copy — every step on a VM behind its own server
/// id, not RunPod's — then the swap renames old-first, the hostname follows `~/.name` on both,
/// the proxy routes alpha to it, and the old VM is terminated. No 90 s settle waits on a VM.
#[tokio::test(start_paused = true)]
async fn replace_moves_a_vm_to_a_bigger_type_and_its_hostname_follows() {
    let dir = tmpdir("replace-type");
    let cfg = cfg(&dir.0);
    let f = Vms::new(vec![vm("101", "devtest-alpha", "cx33", "10.0.0.1", "nbg1 (Nuremberg, DE)")], vec![]);
    *f.to_create.lock().unwrap() = vec![vm("202", "", "", "10.0.0.2", "fsn1 (Falkenstein, DE)")];
    let remote = Arc::new(FakeRemote::new());
    scripted(&remote, "101", NEW, "202");
    let (ov, interrupt) = (SpecOverrides::default(), Interrupt::manual());
    let place = PlaceFlags { server_type: Some("cx43".into()), ..Default::default() };
    let keys = dir.0.display().to_string();
    let started = tokio::time::Instant::now();
    let job = replace_job(&ov, &place, &interrupt, &keys, Source::Create, None, false, &cfg);
    replace_on(remote.clone(), &f, &cfg, &f, "101".into(), "devtest-alpha".into(), job).await.unwrap();
    assert_eq!(
        f.calls(),
        ["create devtest-alpha-new cx43", "rename 101 devtest-alpha-old", "rename 202 devtest-alpha", "terminate 101"]
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(90), "no settle/persist wait on a VM: {:?}", started.elapsed());
    let (on_old, on_new) = (execs(&remote, OLD), execs(&remote, NEW));
    // Asked who's on it before anything was made, before the prompt, after it and after the
    // final sync.
    assert_eq!(on_old.iter().filter(|c| c.contains("ARENA_IDLE_NOW")).count(), 4, "{on_old:?}");
    // The copy, then the final sync of what changed since, right before the swap.
    assert_eq!(on_old.iter().filter(|c| c.contains("\nrsync ") && !c.contains("--dry-run")).count(), 2, "{on_old:?}");
    // The copy ran on the old VM, its remote rsync only on the new VM itself (by server id).
    assert!(
        on_old.iter().any(|c| c.contains("\nrsync ") && c.contains("--rsync-path=") && c.contains("instance-id") && c.contains("202")),
        "{on_old:?}"
    );
    // ~/.name and the hostname on both, each behind that VM's own id.
    let named = |cmds: &[String], name: &str, id: &str| {
        cmds.iter().any(|c| {
            c.contains(&arena_core::setup::name_file_command(arena_core::setup::short_name("devtest", name)))
                && c.contains(&format!("n='{name}'"))
                && c.contains(&format!("= '{id}' ]"))
        })
    };
    assert!(named(&on_new, "devtest-alpha", "202"), "{on_new:?}");
    assert!(named(&on_old, "devtest-alpha-old", "101"), "{on_old:?}");
    let px = std::fs::read_to_string(dir.0.join("proxy.conf")).unwrap_or_default();
    assert!(px.contains("10.0.0.2") && !px.contains("10.0.0.1"), "{px}");
}

/// The swapper took only idle boxes; `replace` refuses one someone is on — SSH, or a VS Code
/// server still running — before anything is made, pointing at `migrate`; `--force` goes on.
#[tokio::test(start_paused = true)]
async fn replace_refuses_a_vm_someone_is_on() {
    let dir = tmpdir("replace-busy");
    let cfg = cfg(&dir.0);
    let (ov, place, interrupt) = (SpecOverrides::default(), PlaceFlags::default(), Interrupt::manual());
    let keys = dir.0.display().to_string();
    for (reply, why) in [
        (Reply { gpu: None, sessions: vec![[10, 9, 9, 9]], ..Reply::default() }, "1 SSH session"),
        (Reply { gpu: None, editors: Some(1), ..Reply::default() }, "a VS Code (or Cursor) server running"),
        (Reply { gpu: None, editors: None, ..Reply::default() }, "couldn't tell whether anyone is on it"),
    ] {
        let f = Vms::new(vec![vm("101", "devtest-alpha", "cx33", "10.0.0.1", "nbg1")], vec![]);
        let remote = Arc::new(FakeRemote::new());
        remote.on(OLD, "ARENA_IDLE_NOW", FakeReply::stdout(&reply.render()));
        let job = replace_job(&ov, &place, &interrupt, &keys, Source::Create, None, false, &cfg);
        let e = replace_on(remote.clone(), &f, &cfg, &f, "101".into(), "devtest-alpha".into(), job).await.unwrap_err().to_string();
        assert!(e.contains("refusing to replace devtest-alpha") && e.contains(why), "{e}");
        assert!(e.contains("arena pods migrate copy devtest-alpha") && e.contains("--force"), "{e}");
        assert!(f.calls().is_empty(), "nothing made: {:?}", f.calls());
    }
}

/// Someone came on while it copied — or while its prompt waited, or during the final sync
/// (reviews r2, r3: asked again after each): no swap — the original untouched, `-new` built — and the way on is `migrate cutover` (it
/// checks again).
#[tokio::test(start_paused = true)]
async fn replace_doesnt_swap_when_someone_came_on_during_the_copy() {
    for (busy_from, when) in [(1, "while it copied"), (2, "while the prompt waited"), (3, "during the final sync")] {
        let dir = tmpdir(&format!("replace-came-on-{busy_from}"));
        let cfg = cfg(&dir.0);
        let f = Vms::new(vec![vm("101", "devtest-alpha", "cx33", "10.0.0.1", "nbg1")], vec![]);
        *f.to_create.lock().unwrap() = vec![vm("202", "", "", "10.0.0.2", "nbg1")];
        let remote = Arc::new(FakeRemote::new());
        let probes = Arc::new(AtomicUsize::new(0));
        let seen = probes.clone();
        remote.on(
            OLD,
            "ARENA_IDLE_NOW",
            FakeReply::from_cmd(move |_| {
                let sessions = if seen.fetch_add(1, Ordering::SeqCst) < busy_from { vec![] } else { vec![[10, 9, 9, 9]] };
                FakeReply::stdout(&Reply { gpu: None, sessions, ..Reply::default() }.render())
            }),
        );
        scripted(&remote, "101", NEW, "202");
        let (ov, place, interrupt) = (SpecOverrides::default(), PlaceFlags::default(), Interrupt::manual());
        let keys = dir.0.display().to_string();
        let job = replace_job(&ov, &place, &interrupt, &keys, Source::Create, None, false, &cfg);
        let e = replace_on(remote.clone(), &f, &cfg, &f, "101".into(), "devtest-alpha".into(), job).await.unwrap_err().to_string();
        assert!(e.contains("NOT swapping") && e.contains("1 SSH session") && e.contains(when), "{e}");
        assert!(e.contains("arena pods migrate cutover devtest-alpha"), "{e}");
        assert_eq!(f.calls(), ["create devtest-alpha-new cx33"], "no rename, no terminate");
        assert_eq!(probes.load(Ordering::SeqCst), busy_from + 1);
        // No final sync once someone's on it before it: only the copy ran.
        let syncs = if busy_from >= 3 { 2 } else { 1 };
        assert_eq!(execs(&remote, OLD).iter().filter(|c| c.contains("\nrsync ") && !c.contains("--dry-run")).count(), syncs);
    }
}

/// `--server-type` is Hetzner's; GPU flags aren't a VM's — each refused before anything, by the
/// capability table (this fake is "hetzner" by name only, so the table's answers for its
/// provider name are what's asked: `--gpu`'s; `--disk`'s needs the real backend, and is the
/// table's own test).
#[tokio::test(start_paused = true)]
async fn flags_a_vm_cant_take_and_a_server_type_off_hetzner_are_refused() {
    let dir = tmpdir("replace-flags");
    let cfg = cfg(&dir.0);
    let f = Vms::new(vec![vm("101", "devtest-alpha", "cx33", "10.0.0.1", "nbg1")], vec![]);
    let remote = Arc::new(FakeRemote::new());
    scripted(&remote, "101", NEW, "202");
    let interrupt = Interrupt::manual();
    let keys = dir.0.display().to_string();
    let place = PlaceFlags::default();
    let ov = SpecOverrides { gpu: Some("A4000".into()), ..Default::default() };
    let job = replace_job(&ov, &place, &interrupt, &keys, Source::Create, None, false, &cfg);
    let e = replace_on(remote.clone(), &f, &cfg, &f, "101".into(), "devtest-alpha".into(), job).await.unwrap_err().to_string();
    assert!(e.contains("--gpu") && e.contains("Hetzner creates CPU VMs sized by server type") && e.contains("--server-type"), "{e}");
    assert!(f.calls().is_empty());
    let e = crate::pods::replace::server_types("runpod", Some("cx43")).unwrap_err().to_string();
    assert!(e.contains("--server-type") && e.contains("only Hetzner VMs are sized by server type"), "{e}");
    assert_eq!(crate::pods::replace::server_types("hetzner", Some("CX43, cx53,cx43")).unwrap(), ["cx43", "cx53"]);
    assert!(crate::pods::replace::server_types("hetzner", Some("cx43;rm")).is_err());
    assert!(crate::pods::replace::server_types("hetzner", None).unwrap().is_empty());
}

fn copy_job<'a>(
    ov: &'a SpecOverrides,
    place: &'a PlaceFlags,
    interrupt: &'a Interrupt,
    source: Source,
    spares: Option<&'a dyn SpareCloud>,
    force: bool,
    cfg: &Config,
) -> MigrateCopyJob<'a> {
    MigrateCopyJob {
        setup_timeouts: arena_core::setup::SetupTimeouts::from_config(cfg, None).unwrap(),
        ov,
        place,
        dry_run: false,
        yes: true,
        interrupt,
        keys_dir: "/nonexistent",
        balance: None,
        source,
        force,
        spares,
        volumes: None,
    }
}

/// The swapper's spares, taken by `migrate copy alpha --onto-spare --server-type cx43`: the
/// first READY cx43 spare (not the one still booting, not the cx53) becomes `alpha-new` by one
/// adopt — renamed and unmarked, nothing created — is set up, and gets a fresh-baseline copy.
#[tokio::test(start_paused = true)]
async fn migrate_copy_takes_the_first_ready_spare_of_the_type() {
    let dir = tmpdir("onto-spare");
    let cfg = cfg(&dir.0);
    let mut booting = vm("301", "devtest-spare-1", "cx43", "10.0.0.3", "fsn1");
    booting.status = "INITIALIZING".into();
    booting.ssh_ip = None;
    let spares = vec![booting, vm("302", "devtest-spare-2", "cx53", "10.0.0.4", "fsn1"), vm("303", "devtest-spare-3", "cx43", "10.0.0.2", "hel1")];
    let f = Vms::new(vec![vm("101", "devtest-alpha", "cx33", "10.0.0.1", "nbg1")], spares);
    let remote = Arc::new(FakeRemote::new());
    scripted(&remote, "101", NEW, "303");
    let (ov, interrupt) = (SpecOverrides::default(), Interrupt::manual());
    let place = PlaceFlags { server_type: Some("cx43".into()), ..Default::default() };
    let pods = f.list_pods().await.unwrap();
    let job = copy_job(&ov, &place, &interrupt, Source::Spare, Some(&f), false, &cfg);
    migrate_copy_on(remote.clone(), &cfg, &f, "devtest-alpha", &pods, job).await.unwrap();
    assert_eq!(f.calls(), ["adopt 303 devtest-alpha-new"]);
    let on_new = execs(&remote, NEW);
    assert!(on_new.iter().any(|c| c.contains("arena-replica-ready") && c.contains("touch \"$s\"")), "a spare's baseline starts now: {on_new:?}");
    // No spare of the type ready: said, with the way to make one; nothing taken.
    let f = Vms::new(vec![vm("101", "devtest-alpha", "cx33", "10.0.0.1", "nbg1")], vec![vm("302", "devtest-spare-2", "cx53", "10.0.0.4", "fsn1")]);
    let job = copy_job(&ov, &place, &interrupt, Source::Spare, Some(&f), false, &cfg);
    let e = migrate_copy_on(remote.clone(), &cfg, &f, "devtest-alpha", &pods, job).await.unwrap_err().to_string();
    assert!(e.contains("no ready spare of type cx43") && e.contains("arena pods spare up 1 --server-type cx43"), "{e}");
    assert!(f.calls().is_empty());
}

/// `--onto bravo`: a cohort machine is someone's — refused saying what's on it, nothing renamed;
/// with --force it's taken (renamed `alpha-new`) and its own files are moved aside, never dropped
/// (no fresh baseline). `--onto` itself, a `-new`, or a machine on no listing are refused.
#[tokio::test(start_paused = true)]
async fn onto_a_cohort_machine_needs_force_and_keeps_its_files_aside() {
    let dir = tmpdir("onto-bravo");
    let cfg = cfg(&dir.0);
    let fleet = || {
        Vms::new(
            vec![vm("101", "devtest-alpha", "cx33", "10.0.0.1", "nbg1"), vm("202", "devtest-bravo", "cx43", "10.0.0.2", "nbg1")],
            vec![],
        )
    };
    let remote = Arc::new(FakeRemote::new());
    scripted(&remote, "101", NEW, "202");
    remote.on(NEW, "ARENA_IDLE_NOW", FakeReply::stdout(&Reply { gpu: None, sessions: vec![[10, 9, 9, 9]], newest_age: Some(3600), ..Reply::default() }.render()));
    let (ov, place, interrupt) = (SpecOverrides::default(), PlaceFlags::default(), Interrupt::manual());
    let f = fleet();
    let pods = f.list_pods().await.unwrap();
    let job = copy_job(&ov, &place, &interrupt, Source::Onto("bravo".into()), Some(&f), false, &cfg);
    let e = migrate_copy_on(remote.clone(), &cfg, &f, "devtest-alpha", &pods, job).await.unwrap_err().to_string();
    assert!(e.contains("--onto devtest-bravo: it's a cohort machine") && e.contains("1 SSH session") && e.contains("changed 1h ago"), "{e}");
    assert!(f.calls().is_empty());
    let job = copy_job(&ov, &place, &interrupt, Source::Onto("bravo".into()), Some(&f), true, &cfg);
    migrate_copy_on(remote.clone(), &cfg, &f, "devtest-alpha", &pods, job).await.unwrap();
    assert_eq!(f.calls(), ["rename 202 devtest-alpha-new"]);
    let on_new = execs(&remote, NEW);
    let prepare: Vec<&String> = on_new.iter().filter(|c| c.contains("arena-replica-ready")).collect();
    assert!(!prepare.is_empty() && prepare.iter().all(|c| !c.contains("touch \"$s\" || exit 1")), "no fresh baseline: {prepare:?}");
    for (onto, why) in [("alpha", "itself"), ("devtest-zulu", "only a machine on hetzner")] {
        let f = fleet();
        let job = copy_job(&ov, &place, &interrupt, Source::Onto(onto.into()), Some(&f), true, &cfg);
        let e = migrate_copy_on(remote.clone(), &cfg, &f, "devtest-alpha", &pods, job).await.unwrap_err().to_string();
        assert!(e.contains(why), "{onto}: {e}");
        assert!(f.calls().is_empty());
    }
    // A shaping flag next to --onto is refused, not dropped.
    let f = fleet();
    let shaped = PlaceFlags { server_type: Some("cx53".into()), ..Default::default() };
    let job = copy_job(&ov, &shaped, &interrupt, Source::Onto("bravo".into()), Some(&f), true, &cfg);
    let e = migrate_copy_on(remote.clone(), &cfg, &f, "devtest-alpha", &pods, job).await.unwrap_err().to_string();
    assert!(e.contains("--server-type shapes a new pod"), "{e}");
}

/// The cutover refuses while someone is on the original (what they write after the final sync
/// would stay on `-old`); `--force` cuts over. On a VM the swap sets the hostname too.
#[tokio::test(start_paused = true)]
async fn the_cutover_waits_for_no_one_on_the_original() {
    let dir = tmpdir("cutover-busy");
    let cfg = cfg(&dir.0);
    let f = Vms::new(
        vec![vm("101", "devtest-alpha", "cx33", "10.0.0.1", "nbg1"), vm("202", "devtest-alpha-new", "cx43", "10.0.0.2", "nbg1")],
        vec![],
    );
    let remote = Arc::new(FakeRemote::new());
    // (Before `scripted`'s own answer: the first rule that matches wins.)
    remote.on(OLD, "ARENA_IDLE_NOW", FakeReply::stdout(&Reply { gpu: None, editors: Some(2), ..Reply::default() }.render()));
    scripted(&remote, "101", NEW, "202");
    let pods = f.list_pods().await.unwrap();
    let opts = |force| CutoverOpts { volumes: None, skip_proxy: false, no_final_sync: true, dry_run: false, yes: true, force };
    let e = migrate_cutover_on(remote.clone(), &f, &cfg, &f, "devtest-alpha", &pods, opts(false), "/nonexistent").await.unwrap_err().to_string();
    assert!(e.contains("refusing to cut over devtest-alpha") && e.contains("2 VS Code (or Cursor) server processes"), "{e}");
    assert!(f.calls().is_empty());
    migrate_cutover_on(remote.clone(), &f, &cfg, &f, "devtest-alpha", &pods, opts(true), "/nonexistent").await.unwrap();
    assert_eq!(f.calls(), ["rename 101 devtest-alpha-old", "rename 202 devtest-alpha"]);
    assert!(execs(&remote, NEW).iter().any(|c| c.contains("n='devtest-alpha'") && c.contains("hostnamectl")), "the hostname follows");

    // No one on it at first, someone by the time the swap comes (review r2): asked again then.
    let f = Vms::new(
        vec![vm("101", "devtest-alpha", "cx33", "10.0.0.1", "nbg1"), vm("202", "devtest-alpha-new", "cx43", "10.0.0.2", "nbg1")],
        vec![],
    );
    let remote = Arc::new(FakeRemote::new());
    let probes = Arc::new(AtomicUsize::new(0));
    let seen = probes.clone();
    remote.on(
        OLD,
        "ARENA_IDLE_NOW",
        FakeReply::from_cmd(move |_| {
            let sessions = if seen.fetch_add(1, Ordering::SeqCst) == 0 { vec![] } else { vec![[10, 9, 9, 9]] };
            FakeReply::stdout(&Reply { gpu: None, sessions, ..Reply::default() }.render())
        }),
    );
    scripted(&remote, "101", NEW, "202");
    let pods = f.list_pods().await.unwrap();
    let e = migrate_cutover_on(remote.clone(), &f, &cfg, &f, "devtest-alpha", &pods, opts(false), "/nonexistent").await.unwrap_err().to_string();
    assert!(e.contains("NOT swapping") && e.contains("1 SSH session") && e.contains("nothing was renamed"), "{e}");
    assert!(f.calls().is_empty(), "{:?}", f.calls());
    assert_eq!(probes.load(Ordering::SeqCst), 2);
}

/// The cutover's through-the-proxy check on a VM reads its server id (cloud-init's instance id),
/// failing closed on an empty or other answer, as RunPod's does with RUNPOD_POD_ID.
#[tokio::test]
async fn the_proxy_check_reads_a_vms_own_server_id() {
    use crate::pods::migrate::proxy_reaches_pod;
    let dir = tmpdir("proxy-id");
    let cfg = cfg(&dir.0);
    let fake = FakeRemote::new();
    fake.script("localhost:7000", [FakeReply::stdout("202\n"), FakeReply::stdout("101\n"), FakeReply::stdout("")]);
    assert!(proxy_reaches_pod(&fake, &cfg, "hetzner", "devtest-alpha", "202").await.unwrap());
    assert!(!proxy_reaches_pod(&fake, &cfg, "hetzner", "devtest-alpha", "202").await.unwrap());
    assert!(!proxy_reaches_pod(&fake, &cfg, "hetzner", "devtest-alpha", "202").await.unwrap());
    assert!(fake.calls().iter().all(|c| matches!(c, RemoteCall::Exec { cmd, .. } if cmd.contains("/var/lib/cloud/data/instance-id"))));
    // A backend whose machines show no id can't be confirmed (and isn't asked).
    assert!(!proxy_reaches_pod(&fake, &cfg, "vast", "devtest-alpha", "202").await.unwrap());
    assert_eq!(fake.calls().len(), 3);
}

/// The swapper's step 1 as a command: `spare up 2 --server-type cx43` names them after the free
/// spare numbers (taken names skipped, fleet ones too), labels them spares (so the fleet never
/// lists them), waits for them; `spare down` deletes only spares — a fleet name is refused.
#[tokio::test(start_paused = true)]
async fn spares_are_made_labelled_and_deleted_only_as_spares() {
    use crate::pods::spare::{spare_on, SpareCmd};
    let dir = tmpdir("spares");
    let cfg = cfg(&dir.0);
    let f = Vms::new(vec![vm("101", "devtest-alpha", "cx33", "10.0.0.1", "nbg1")], vec![vm("301", "devtest-spare-1", "cx43", "10.0.0.3", "fsn1")]);
    *f.to_create.lock().unwrap() = vec![vm("302", "", "", "10.0.0.4", "fsn1"), vm("303", "", "", "10.0.0.5", "hel1")];
    let interrupt = Interrupt::manual();
    let up = SpareCmd::Up {
        count: 2,
        server_type: Some("cx43".into()),
        image: None,
        keep_trying: false,
        retry_mins: 0,
        retry_secs: 60,
        no_wait: false,
        dry_run: false,
    };
    spare_on(up, &cfg, &f, &f, true, &interrupt).await.unwrap();
    assert_eq!(f.calls(), ["create devtest-spare-2 cx43 [spare=devtest]", "create devtest-spare-3 cx43 [spare=devtest]"]);
    assert_eq!(f.list_pods().await.unwrap().len(), 1, "never in the fleet listing");
    // Down: by short name and id; a fleet machine is no spare.
    let down = |names: &[&str]| SpareCmd::Down { names: names.iter().map(|n| n.to_string()).collect(), all: false, dry_run: false };
    let e = spare_on(down(&["alpha"]), &cfg, &f, &f, true, &interrupt).await.unwrap_err().to_string();
    assert!(e.contains("isn't one of this cohort's spares"), "{e}");
    spare_on(down(&["spare-2", "303"]), &cfg, &f, &f, true, &interrupt).await.unwrap();
    assert_eq!(&f.calls()[2..], ["terminate 302", "terminate 303"]);
    spare_on(SpareCmd::Down { names: vec![], all: true, dry_run: false }, &cfg, &f, &f, true, &interrupt).await.unwrap();
    assert_eq!(f.calls().last().unwrap(), "terminate 301");
    assert!(f.spares().await.unwrap().is_empty());
    // At most 20 at a time; a server type off the list is refused before anything.
    let many = SpareCmd::Up { count: 21, server_type: None, image: None, keep_trying: false, retry_mins: 0, retry_secs: 60, no_wait: true, dry_run: false };
    assert!(spare_on(many, &cfg, &f, &f, true, &interrupt).await.unwrap_err().to_string().contains("1 to 20"));
}

/// "migrating from arena9-rarity → arena9-scoot": `copy-home rarity scoot` copies the home
/// straight across — nothing renamed, nothing created — after a dry run of that very copy says
/// what of scoot's own it would replace: refused (named) unless --overwrite, which moves it
/// aside, never deletes it. Someone on either machine stops it unless --force.
#[tokio::test(start_paused = true)]
async fn copy_home_moves_a_person_without_renaming_anything() {
    use crate::pods::copy_home::{copy_home_on, CopyHomeOpts};
    let dir = tmpdir("copy-home");
    let cfg = cfg(&dir.0);
    let f = Vms::new(vec![vm("101", "devtest-alpha", "cx33", "10.0.0.1", "nbg1"), vm("202", "devtest-bravo", "cx43", "10.0.0.2", "nbg1")], vec![]);
    let listed = f.pods.lock().unwrap().clone();
    let (src, dest) = (listed[0].clone(), listed[1].clone());
    let remote = Arc::new(FakeRemote::new());
    // (Before `scripted`'s rules: the first rule that matches wins.) The repo (`~/r`) goes as its own
    // tree after the home, its items named from the repo.
    remote.on(OLD, "--dry-run", FakeReply::stdout("*deleting   notes/mine.txt\n<f+++++++++ new.py\nARENA_TREE_ITEMS\n<f.st...... day1/x.py\n"));
    remote.on(NEW, "ARENA_IDLE_NOW", FakeReply::stdout(&Reply { gpu: None, ..Reply::default() }.render()));
    scripted(&remote, "101", NEW, "202");
    let opts = |overwrite, force| CopyHomeOpts { overwrite, force, dry_run: false, yes: true };
    let e = copy_home_on(remote.clone(), &cfg, &f, &src, &dest, opts(false, false)).await.unwrap_err().to_string();
    assert!(e.contains("devtest-bravo has files of its own") && e.contains("(2 files: notes/mine.txt, r/day1/x.py)"), "{e}");
    assert!(!execs(&remote, OLD).iter().any(|c| c.contains("\nrsync ") && !c.contains("--dry-run")), "nothing copied");
    copy_home_on(remote.clone(), &cfg, &f, &src, &dest, opts(true, false)).await.unwrap();
    assert!(f.calls().is_empty(), "nothing renamed, created or deleted: {:?}", f.calls());
    let on_old = execs(&remote, OLD);
    assert!(on_old.iter().any(|c| c.contains("\nrsync ") && !c.contains("--dry-run") && c.contains("--backup-dir=arena-sync-kept/")), "{on_old:?}");
    let on_new = execs(&remote, NEW);
    assert!(on_new.iter().any(|c| c.contains("arena-replica-ready") && !c.contains("touch \"$s\" || exit 1")), "not a fresh baseline: {on_new:?}");
    // Someone on the destination: refused, unless --force.
    let busy = Arc::new(FakeRemote::new());
    busy.on(NEW, "ARENA_IDLE_NOW", FakeReply::stdout(&Reply { gpu: None, sessions: vec![[10, 9, 9, 9]], ..Reply::default() }.render()));
    scripted(&busy, "101", NEW, "202");
    let e = copy_home_on(busy.clone(), &cfg, &f, &src, &dest, opts(true, false)).await.unwrap_err().to_string();
    assert!(e.contains("devtest-bravo — someone is on it (1 SSH session)") && e.contains("--force"), "{e}");
    copy_home_on(busy, &cfg, &f, &src, &dest, opts(true, true)).await.unwrap();

    // Nothing in the way at the preview, a file of bravo's own by the time it copies (review
    // r3: the dry run runs again after the prompt): refused, nothing copied.
    let later = Arc::new(FakeRemote::new());
    let looks = Arc::new(AtomicUsize::new(0));
    let seen = looks.clone();
    later.on(
        OLD,
        "--dry-run",
        FakeReply::from_cmd(move |_| {
            FakeReply::stdout(if seen.fetch_add(1, Ordering::SeqCst) == 0 { "<f+++++++++ new.py\n" } else { "*deleting   appeared.txt\n" })
        }),
    );
    later.on(NEW, "ARENA_IDLE_NOW", FakeReply::stdout(&Reply { gpu: None, ..Reply::default() }.render()));
    scripted(&later, "101", NEW, "202");
    let e = copy_home_on(later.clone(), &cfg, &f, &src, &dest, opts(false, false)).await.unwrap_err().to_string();
    assert!(e.contains("appeared.txt") && e.contains("appeared while the prompt waited"), "{e}");
    assert_eq!(looks.load(Ordering::SeqCst), 2);
    assert!(!execs(&later, OLD).iter().any(|c| c.contains("\nrsync ") && !c.contains("--dry-run")), "nothing copied");
}

/// A machine taken with `--onto` that another move promoted meanwhile (review r3: renamed to a
/// participant's name while this one waited at its prompt) is never taken from the stale look.
#[tokio::test]
async fn an_onto_machine_renamed_meanwhile_isnt_taken() {
    use crate::pods::adopt::{take, Onto};
    let dir = tmpdir("onto-stale");
    let cfg = cfg(&dir.0);
    let helper = vm("303", "devtest-helper", "cx43", "10.0.0.3", "nbg1");
    let f = Vms::new(vec![helper.clone()], vec![]);
    let onto = Onto { pod: helper, spare: false };
    f.rename_pod("303", "devtest-alpha").await.unwrap();
    let e = take(&f, Some(&f), &cfg, &onto, "devtest-bravo-new").await.unwrap_err();
    assert!(format!("{e:#}").contains("it's called devtest-alpha now"), "{e:#}");
    assert_eq!(f.calls(), ["rename 303 devtest-alpha"], "no second rename");
    // Still under its name: taken.
    let f = Vms::new(vec![vm("304", "devtest-helper", "cx43", "10.0.0.3", "nbg1")], vec![]);
    take(&f, Some(&f), &cfg, &Onto { pod: vm("304", "devtest-helper", "cx43", "10.0.0.3", "nbg1"), spare: false }, "devtest-bravo-new").await.unwrap();
    assert_eq!(f.calls(), ["rename 304 devtest-bravo-new"]);
}

/// Two moves that picked the same spare (review r2): the first takes it; the second is refused
/// — never handed the machine the first is copying onto — and renames nothing.
#[tokio::test]
async fn two_claims_on_one_spare_take_it_once() {
    use crate::pods::adopt::{take, Onto};
    let dir = tmpdir("two-claims");
    let cfg = cfg(&dir.0);
    let spare = vm("301", "devtest-spare-1", "cx43", "10.0.0.3", "fsn1");
    let f = Vms::new(vec![], vec![spare.clone()]);
    let onto = Onto { pod: spare, spare: true };
    take(&f, Some(&f), &cfg, &onto, "devtest-alpha-new").await.unwrap();
    let e = take(&f, Some(&f), &cfg, &onto, "devtest-bravo-new").await.unwrap_err();
    assert!(format!("{e:#}").contains("taking devtest-spare-1 as devtest-bravo-new — nothing was renamed"), "{e:#}");
    assert_eq!(f.calls(), ["adopt 301 devtest-alpha-new"]);
}

/// A Hetzner project for the volume tests: the fleet's VMs as servers (a volume created
/// attached takes its location from its server), and one volume of 10 GB on `on` (101).
fn volume_project(servers: &[(&str, &str)], on: u64, location: &str) -> arena_core::archive::cloud::FakeArchiveCloud {
    use arena_core::archive::cloud::{CloudServer, CloudVolume, FakeArchiveCloud, Inventory};
    FakeArchiveCloud::with(Inventory {
        servers: servers
            .iter()
            .map(|(id, loc)| CloudServer {
                id: id.parse().unwrap(),
                name: format!("s{id}"),
                status: "running".into(),
                location: loc.to_string(),
                ipv4: None,
                labels: Default::default(),
                delete_protected: false,
                server_type: "cx23".into(),
            })
            .collect(),
        volumes: vec![CloudVolume {
            id: 900,
            name: "devtest-alpha-data".into(),
            size_gb: 10,
            location: location.into(),
            server: Some(on),
            format: Some("ext4".into()),
            linux_device: "/dev/disk/by-id/scsi-0HC_Volume_900".into(),
            labels: [("arena-volume".to_string(), "devtest".to_string())].into(),
            delete_protected: false,
        }],
        ..Default::default()
    })
}

/// The volume steps answer on both VMs: where it's mounted, unmounted, mounted (unless
/// `mount_fails`). Registered before `scripted`'s rules (the first that matches wins: its
/// identity rule would answer every guarded step).
fn volume_steps(remote: &FakeRemote, mount_fails: bool) {
    for host in [OLD, NEW] {
        remote.on(host, "ARENA_VOL_AT", FakeReply::stdout("ARENA_VOL_AT /mnt/data\n"));
        remote.on(host, "ARENA_VOL_UNMOUNTED", FakeReply::stdout("ARENA_VOL_UNMOUNTED\n"));
    }
    remote.on(OLD, "ARENA_VOL_NO_DEVICE", FakeReply::stdout("ARENA_VOL_MOUNTED 10501771264\n"));
    let on_new = if mount_fails { "ARENA_VOL_FAIL /mnt/data holds files (x)\n" } else { "ARENA_VOL_MOUNTED 10501771264\n" };
    remote.on(NEW, "ARENA_VOL_NO_DEVICE", FakeReply::stdout(on_new));
    remote.on(OLD, "rsync -aHAX", FakeReply::stdout(""));
}

/// A volume on the old VM moves with it when the new one is in the same location: unmounted
/// there, detached, attached to the new VM and mounted where it was — before the names swap.
#[tokio::test(start_paused = true)]
async fn replace_moves_a_volume_in_the_same_location() {
    use arena_core::archive::cloud::CloudCall;
    let dir = tmpdir("vol-move");
    let cfg = cfg(&dir.0);
    let f = Vms::new(vec![vm("101", "devtest-alpha", "cx33", "10.0.0.1", "nbg1 (Nuremberg, DE)")], vec![]);
    *f.to_create.lock().unwrap() = vec![vm("202", "", "", "10.0.0.2", "nbg1 (Nuremberg, DE)")];
    let cloud = volume_project(&[("101", "nbg1"), ("202", "nbg1")], 101, "nbg1");
    let remote = Arc::new(FakeRemote::new());
    volume_steps(&remote, false);
    scripted(&remote, "101", NEW, "202");
    let (ov, place, interrupt) = (SpecOverrides::default(), PlaceFlags::default(), Interrupt::manual());
    let keys = dir.0.display().to_string();
    let mut job = replace_job(&ov, &place, &interrupt, &keys, Source::Create, None, false, &cfg);
    job.volumes = Some(&cloud);
    replace_on(remote.clone(), &f, &cfg, &f, "101".into(), "devtest-alpha".into(), job).await.unwrap();
    assert_eq!(cloud.mutations(), [CloudCall::Detach(900), CloudCall::Attach(900, 202)]);
    assert_eq!(cloud.state().volumes[0].server, Some(202));
    assert!(execs(&remote, OLD).iter().any(|c| c.contains("ARENA_VOL_UNMOUNTED") && c.contains("scsi-0HC_Volume_900")));
    assert!(execs(&remote, NEW).iter().any(|c| c.contains("ARENA_VOL_NO_DEVICE") && c.contains("'/mnt/data'")), "mounted where it was");
    assert_eq!(f.calls(), ["create devtest-alpha-new cx33", "rename 101 devtest-alpha-old", "rename 202 devtest-alpha", "terminate 101"]);
}

/// The move fails on the new VM (its mount point holds files): the volume goes back onto the
/// original, mounted there, and nothing is swapped.
#[tokio::test(start_paused = true)]
async fn a_volume_that_wont_move_goes_back_and_nothing_swaps() {
    let dir = tmpdir("vol-move-fail");
    let cfg = cfg(&dir.0);
    let f = Vms::new(vec![vm("101", "devtest-alpha", "cx33", "10.0.0.1", "nbg1")], vec![]);
    *f.to_create.lock().unwrap() = vec![vm("202", "", "", "10.0.0.2", "nbg1")];
    let cloud = volume_project(&[("101", "nbg1"), ("202", "nbg1")], 101, "nbg1");
    let remote = Arc::new(FakeRemote::new());
    volume_steps(&remote, true);
    scripted(&remote, "101", NEW, "202");
    let (ov, place, interrupt) = (SpecOverrides::default(), PlaceFlags::default(), Interrupt::manual());
    let keys = dir.0.display().to_string();
    let mut job = replace_job(&ov, &place, &interrupt, &keys, Source::Create, None, false, &cfg);
    job.volumes = Some(&cloud);
    let e = format!("{:#}", replace_on(remote.clone(), &f, &cfg, &f, "101".into(), "devtest-alpha".into(), job).await.unwrap_err());
    assert!(e.contains("NOT swapping") && e.contains("holds files") && e.contains("back on devtest-alpha"), "{e}");
    assert_eq!(cloud.state().volumes[0].server, Some(101), "back on the original");
    assert!(execs(&remote, OLD).iter().any(|c| c.contains("ARENA_VOL_NO_DEVICE")), "mounted again on the original");
    assert_eq!(f.calls(), ["create devtest-alpha-new cx33"], "no rename, no terminate");
}

/// In another location the volume can't move: a volume of the same size is made on the new VM
/// (labelled as the copy), mounted at the same place, and synced straight from the old VM —
/// with the copy, and again right before the swap. The original stays where it is.
#[tokio::test(start_paused = true)]
async fn replace_copies_a_volume_to_another_location() {
    use arena_core::archive::cloud::CloudCall;
    let dir = tmpdir("vol-copy");
    let cfg = cfg(&dir.0);
    let f = Vms::new(vec![vm("101", "devtest-alpha", "cx33", "10.0.0.1", "nbg1")], vec![]);
    *f.to_create.lock().unwrap() = vec![vm("202", "", "", "10.0.0.2", "fsn1 (Falkenstein, DE)")];
    let cloud = volume_project(&[("101", "nbg1"), ("202", "fsn1")], 101, "nbg1");
    let remote = Arc::new(FakeRemote::new());
    volume_steps(&remote, false);
    scripted(&remote, "101", NEW, "202");
    let (ov, place, interrupt) = (SpecOverrides::default(), PlaceFlags::default(), Interrupt::manual());
    let keys = dir.0.display().to_string();
    let mut job = replace_job(&ov, &place, &interrupt, &keys, Source::Create, None, false, &cfg);
    job.volumes = Some(&cloud);
    replace_on(remote.clone(), &f, &cfg, &f, "101".into(), "devtest-alpha".into(), job).await.unwrap();
    let created: Vec<serde_json::Value> = cloud.mutations().into_iter().filter_map(|c| if let CloudCall::CreateVolume(b) = c { Some(b) } else { None }).collect();
    assert_eq!(created.len(), 1, "made once, found again for the final sync");
    assert_eq!((created[0]["size"].as_u64(), created[0]["server"].as_u64()), (Some(10), Some(202)));
    assert_eq!(created[0]["name"], "devtest-alpha-data-2");
    assert_eq!(created[0]["labels"]["arena-volume-copy-of"], "900");
    let copies: Vec<String> = execs(&remote, OLD).into_iter().filter(|c| c.contains("rsync -aHAX")).collect();
    assert_eq!(copies.len(), 2, "{copies:?}");
    assert!(copies[0].contains("'/mnt/data'/ 'root@10.0.0.2:/mnt/data/'") && copies[0].contains("--rsync-path="), "{}", copies[0]);
    let state = cloud.state();
    // The original stays (on its old VM — this fake project doesn't see that VM's terminate).
    assert_eq!(state.volumes.iter().find(|v| v.id == 900).unwrap().server, Some(101));
    assert!(state.volumes.iter().any(|v| v.name == "devtest-alpha-data-2" && v.server == Some(202)));
}
