//! Scenario tests (spec §13): a scripted fleet, a scripted remote that renders probe replies
//! from each pod's simulated state, and a recording rsync stub — all calls in one ordered log.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use arena_core::reap::probe::fixture::Reply;
use arena_core::ssh::SshOutput;
use arena_core::{Error, PodSpec};
use async_trait::async_trait;
use clap::Parser;

use super::*;

const T0: u64 = 1_791_460_800;

/// What one simulated pod does and says.
#[derive(Debug, Clone)]
struct Sim {
    /// The id the pod reports (its own, unless a test fakes a recycled endpoint).
    id: String,
    provider: &'static str,
    boot: String,
    pid1: String,
    cpu: u64,
    net: u64,
    sessions: u32,
    files_hit: bool,
    keep: bool,
    workspace: &'static str,
    repo: &'static str,
    gpu_util: u32,
    job_running: bool,
    /// Every exec fails (no route).
    unreachable: bool,
    /// The git step's reply.
    git: (i32, String),
    /// Change the reported id after the backup's git step (a recycled endpoint mid-reap).
    id_after_git: Option<String>,
    /// A session appears after the backup (someone logged in meanwhile).
    session_after_git: bool,
    /// The probe fails after the git step (unreachable for the post-backup probe).
    unreachable_after_git: bool,
    /// What happens in the world while the backup runs (fired by the git step).
    on_git: OnGit,
    git_done: bool,
}

/// A change to the world in the middle of a reap (between the decision and the terminate).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnGit {
    Nothing,
    /// The pod leaves its provider's listing.
    Gone,
    Renamed,
    Stopped,
    Locked,
    /// An operator pins it.
    Pinned,
    /// An operator extends its expiry.
    Extended,
    /// The backup takes 6 minutes (the control clock moves on).
    Slow,
}

impl Sim {
    fn new(id: &str, provider: &'static str) -> Sim {
        Sim {
            id: id.into(),
            provider,
            boot: "db9bb781-8521-46cd-a74a-63c0e46d9be7".into(),
            pid1: "4242".into(),
            cpu: 1_000_000_000,
            net: 5_000_000,
            sessions: 0,
            files_hit: false,
            keep: false,
            workspace: "absent",
            repo: "under_home",
            gpu_util: 0,
            job_running: false,
            unreachable: false,
            git: (0, "PUSHED autocommit-devtest-w0d1-x\n".into()),
            id_after_git: None,
            session_after_git: false,
            unreachable_after_git: false,
            on_git: OnGit::Nothing,
            git_done: false,
        }
    }
}

/// The scripted remote: one [`Sim`] per `host:port`; every call recorded in the shared log.
struct Remote2 {
    sims: Mutex<HashMap<String, Sim>>,
    /// The fleet's listing (shared with [`Fleet`]), for the mid-reap changes.
    listing: Listing2,
    store: Store,
    clock: Arc<AtomicU64>,
    log: PathBuf,
    calls: Mutex<Vec<(String, &'static str)>>,
}

fn kind_of(cmd: &str) -> &'static str {
    if cmd.contains("arena-idle-probe") {
        "probe"
    } else if cmd.contains("ARENA_JOBS_END") {
        "jobs"
    } else if cmd.contains("arena-backup") {
        "git"
    } else if cmd.contains(".arena/NOTICE") {
        "notice"
    } else if cmd.contains("| wall") {
        "wall"
    } else {
        "other"
    }
}

/// The probe's nonce and `since` from its command line.
fn probe_args(cmd: &str) -> (String, Since) {
    let tail = cmd.split("arena-idle-probe ").nth(1).unwrap();
    let quoted: Vec<&str> = tail.split('\'').collect();
    let (nonce, since) = (quoted[1], quoted[3]);
    let since = match since.split_at(1) {
        ("@", n) => Since::At(n.parse().unwrap()),
        ("-", n) => Since::Back(n.parse().unwrap()),
        _ => panic!("since {since}"),
    };
    (nonce.to_string(), since)
}

impl Remote2 {
    fn append(&self, line: &str) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.log).unwrap();
        writeln!(f, "{line}").unwrap();
    }

    fn reply(&self, host: &str, cmd: &str) -> arena_core::Result<SshOutput> {
        let kind = kind_of(cmd);
        self.calls.lock().unwrap().push((host.to_string(), kind));
        self.append(&format!("remote {kind} {host}"));
        let mut sims = self.sims.lock().unwrap();
        let sim = sims.get_mut(host).unwrap_or_else(|| panic!("no pod at {host}"));
        let ok = |stdout: String| Ok(SshOutput { success: true, code: Some(0), stdout, stderr: String::new() });
        if sim.unreachable || (sim.unreachable_after_git && sim.git_done) {
            return Err(Error::Timeout { what: format!("ssh {host}"), after: Duration::from_secs(90) });
        }
        match kind {
            "probe" => {
                let (nonce, since) = probe_args(cmd);
                let now = self.clock.load(Ordering::SeqCst);
                let id = match (&sim.id_after_git, sim.git_done) {
                    (Some(other), true) => other.clone(),
                    _ => sim.id.clone(),
                };
                let mut sshd = Reply::default().sshd;
                for i in 0..sim.sessions + u32::from(sim.session_after_git && sim.git_done) {
                    sshd.push((7000 + u64::from(i), "sshd: root@pts/0".into()));
                }
                let gpu = (sim.provider == "runpod").then(|| (0..5).map(|_| (0, vec![format!("{}, 3", sim.gpu_util)])).collect());
                let r = Reply {
                    nonce,
                    since,
                    id,
                    boot_id: sim.boot.clone(),
                    pid1_start: sim.pid1.clone(),
                    t0: now - 22,
                    cpu: (Some(sim.cpu), Some(sim.cpu + 5_000)),
                    net: (Some(sim.net), Some(sim.net + 100)),
                    gpu,
                    files: if sim.files_hit { "hit" } else { "none" },
                    sshd,
                    keep: sim.keep,
                    workspace: sim.workspace,
                    repo: sim.repo,
                    ..Default::default()
                };
                ok(r.render())
            }
            "jobs" => {
                let line = if sim.job_running {
                    "ARENA_JOB\t20261008-142301-train\t2026-10-08T14:23:01Z\trunning\t-\t4321\tpython train.py\n"
                } else {
                    ""
                };
                ok(format!("{line}ARENA_JOBS_END\n"))
            }
            "git" => {
                sim.git_done = true;
                let id = sim.id.clone();
                let mut listing = self.listing.lock().unwrap();
                let pod = listing.iter_mut().filter_map(|(_, r)| r.as_mut().ok()).flatten().find(|p| p.id == id);
                let name = pod.as_ref().map(|p| p.name.clone()).unwrap_or_default();
                match (sim.on_git, pod) {
                    (OnGit::Renamed, Some(p)) => p.name = "devtest-echo".into(),
                    (OnGit::Stopped, Some(p)) => p.status = "EXITED".into(),
                    (OnGit::Locked, Some(p)) => p.locked = Some(true),
                    (OnGit::Gone, Some(_)) => {
                        for (_, r) in listing.iter_mut() {
                            if let Ok(pods) = r {
                                pods.retain(|p| p.id != id);
                            }
                        }
                    }
                    (OnGit::Pinned, _) => {
                        self.store
                            .update(|st| {
                                st.pins.push(Pin { name: name.clone(), id: None, until: None, note: None, set_at: 0 });
                                Ok(())
                            })
                            .unwrap();
                    }
                    (OnGit::Slow, _) => {
                        self.clock.fetch_add(360, Ordering::SeqCst);
                    }
                    (OnGit::Extended, _) => {
                        let now = self.clock.load(Ordering::SeqCst);
                        self.store
                            .update(|st| {
                                for p in st.pods.values_mut() {
                                    p.expires_at = Some(now + 7200);
                                    p.first_due_seen = None;
                                    p.idle_after_secs = None;
                                }
                                Ok(())
                            })
                            .unwrap();
                    }
                    _ => {}
                }
                drop(listing);
                let (code, out) = sim.git.clone();
                Ok(SshOutput { success: code == 0, code: Some(code), stdout: out, stderr: String::new() })
            }
            "notice" | "wall" => ok(String::new()),
            _ => panic!("unexpected remote command: {cmd}"),
        }
    }
}

#[async_trait]
impl Remote for Remote2 {
    async fn exec(&self, t: &SshTarget, cmd: &str, _timeout: Option<Duration>) -> arena_core::Result<SshOutput> {
        self.reply(&format!("{}:{}", t.host, t.port), cmd)
    }
    async fn copy(&self, _t: &SshTarget, _l: &str, _r: &str, _to: Option<Duration>) -> arena_core::Result<SshOutput> {
        panic!("the reaper never copies to a pod")
    }
    async fn copy_recursive(&self, _t: &SshTarget, _l: &str, _r: &str, _to: Option<Duration>) -> arena_core::Result<SshOutput> {
        panic!("the reaper never copies to a pod")
    }
}

type Listing2 = Arc<Mutex<Vec<(String, std::result::Result<Vec<Pod>, String>)>>>;

/// The fleet: per-backend listings (or failures); terminates recorded. Spec I5: in `strict`
/// mode anything but `list_by_provider` and `terminate_pod_on` panics.
struct Fleet {
    backends: Listing2,
    terminated: Mutex<Vec<(String, String)>>,
    /// Scripted terminate answers (default Ok, and the pod leaves the listing).
    replies: Mutex<VecDeque<arena_core::Result<()>>>,
    /// An Ok terminate leaves the pod listed (F3).
    stays: Mutex<bool>,
    strict: Mutex<bool>,
    log: PathBuf,
}

#[async_trait]
impl Provider for Fleet {
    fn name(&self) -> &'static str {
        "runpod"
    }
    fn describe(&self, _spec: &PodSpec) -> String {
        String::new()
    }
    async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
        assert!(!*self.strict.lock().unwrap(), "I5: a reap run calls only list_by_provider and terminate_pod_on");
        Ok(self.backends.lock().unwrap().iter().filter_map(|(_, r)| r.as_ref().ok()).flatten().cloned().collect())
    }
    async fn list_by_provider(&self) -> Vec<(String, arena_core::Result<Vec<Pod>>)> {
        self.backends.lock().unwrap().iter().map(|(n, r)| (n.clone(), r.clone().map_err(Error::provider))).collect()
    }
    async fn enrich(&self, _pods: &mut [Pod]) -> arena_core::Result<()> {
        panic!("I5: enrich")
    }
    async fn create_pod(&self, _spec: &PodSpec) -> arena_core::Result<Pod> {
        panic!("I5: create")
    }
    async fn stop_pod(&self, _id: &str) -> arena_core::Result<()> {
        panic!("I5: stop")
    }
    async fn restart_pod(&self, _id: &str) -> arena_core::Result<()> {
        panic!("I5: restart")
    }
    async fn start_pod(&self, _id: &str) -> arena_core::Result<()> {
        panic!("I5: start")
    }
    async fn terminate_pod(&self, _id: &str) -> arena_core::Result<()> {
        panic!("I5: terminate only through the ticket (terminate_pod_on)")
    }
    async fn terminate_pod_on(&self, provider: &str, id: &str) -> arena_core::Result<()> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.log).unwrap();
        writeln!(f, "terminate {provider}:{id}").unwrap();
        self.terminated.lock().unwrap().push((provider.to_string(), id.to_string()));
        let r = self.replies.lock().unwrap().pop_front().unwrap_or(Ok(()));
        if r.is_ok() && !*self.stays.lock().unwrap() {
            for (n, pods) in self.backends.lock().unwrap().iter_mut() {
                if n == provider {
                    if let Ok(p) = pods {
                        p.retain(|x| x.id != id);
                    }
                }
            }
        }
        r
    }
    async fn rename_pod(&self, _id: &str, _n: &str) -> arena_core::Result<()> {
        panic!("I5: rename")
    }
    async fn set_locked(&self, _id: &str, _l: bool) -> arena_core::Result<()> {
        panic!("I5: lock/unlock — the reaper never unlocks")
    }
    async fn reimage_pod(&self, _id: &str, _i: &str, _e: &[(String, String)]) -> arena_core::Result<()> {
        panic!("I5: reimage")
    }
    async fn authorize_ssh_keys(&self, _p: &Pod, _k: &[String]) -> arena_core::Result<()> {
        panic!("I5: keys")
    }
}

/// A RunPod v2 pod at `10.0.0.1:<port>`.
fn rp(name: &str, id: &str, port: u16) -> Pod {
    Pod {
        id: id.into(),
        name: name.into(),
        provider: "runpod".into(),
        status: "RUNNING".into(),
        ssh_ip: Some("10.0.0.1".into()),
        ssh_port: Some(port),
        locked: Some(false),
        ..Default::default()
    }
}

fn hz(name: &str, id: &str, port: u16) -> Pod {
    Pod { provider: "hetzner".into(), locked: None, delete_protected: Some(false), ..rp(name, id, port) }
}

fn host(port: u16) -> String {
    format!("10.0.0.1:{port}")
}

/// The whole scripted world for one test.
struct World {
    dir: PathBuf,
    cfg: Config,
    fleet: Fleet,
    remote: Arc<Remote2>,
    clock: Arc<AtomicU64>,
    ntp: Arc<Mutex<Option<bool>>>,
    out: Arc<Mutex<Vec<String>>>,
    ntp_fn: Box<dyn Fn() -> Option<bool> + Send + Sync>,
    out_fn: Box<dyn Fn(&str) + Send + Sync>,
    rsync: String,
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl World {
    fn new(tag: &str, extra_cfg: &str) -> World {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("arena-reap-cli-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("snap/big")).unwrap();
        std::fs::write(dir.join("snap/.arena-backup-tier"), "snapshot\n").unwrap();
        std::fs::write(dir.join("snap/big/.arena-backup-tier"), "big\n").unwrap();
        let rsync = dir.join("rsync");
        std::fs::write(
            &rsync,
            r#"#!/bin/sh
d=$(dirname "$0")
case "$1" in --version) echo "rsync  version 3.2.7"; exit 0;; esac
for a in "$@"; do last="$a"; done
case " $* " in
  *" --dry-run "*) echo "rsync verify $last" >> "$d/calls.log"; echo "Number of regular files transferred: $(cat "$d/pending" 2>/dev/null || echo 0)"; exit "$(cat "$d/exit_verify" 2>/dev/null || echo 0)";;
esac
case "$last" in
  */snap/big/*) echo "rsync big $last" >> "$d/calls.log"; [ -f "$d/hang_big" ] && sleep 5; exit "$(cat "$d/exit_big" 2>/dev/null || echo 0)";;
esac
echo "rsync snapshot $last" >> "$d/calls.log"
echo "Number of regular files transferred: 1"
exit "$(cat "$d/exit_snap" 2>/dev/null || echo 0)"
"#,
        )
        .unwrap();
        std::fs::set_permissions(&rsync, std::fs::Permissions::from_mode(0o755)).unwrap();
        let text = format!(
            "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=( alpha bravo charlie delta echo foxtrot \"@devtest-boss\" )\n\
             RUNPOD_API=v2\nARENA_STATE_DIR={d}/state\nLOCAL_BACKUP_DIR={d}/snap\nARENA_START_DATE=2026-10-05\n\
             REAP_ENABLED=1\nREAP_IDLE_FLOOR_MINS=15\nREAP_MIN_FREE_GB=1\nREAP_NOTIFY_CMD=\"cat >> {d}/notified\"\n{extra_cfg}\n",
            d = dir.display()
        );
        let clock = Arc::new(AtomicU64::new(T0));
        let log = dir.join("calls.log");
        let cfg = Config::parse(&text);
        let listing: Listing2 = Arc::new(Mutex::new(vec![("runpod".into(), Ok(Vec::new())), ("hetzner".into(), Ok(Vec::new()))]));
        let store = Store::new(dir.join("state").join("devtest"));
        let ntp = Arc::new(Mutex::new(Some(true)));
        let out = Arc::new(Mutex::new(Vec::new()));
        World {
            cfg,
            fleet: Fleet {
                backends: listing.clone(),
                terminated: Mutex::new(Vec::new()),
                replies: Mutex::new(VecDeque::new()),
                stays: Mutex::new(false),
                strict: Mutex::new(true),
                log: log.clone(),
            },
            remote: Arc::new(Remote2 { sims: Mutex::new(HashMap::new()), listing, store, clock: clock.clone(), log, calls: Mutex::new(Vec::new()) }),
            clock,
            ntp_fn: {
                let n = ntp.clone();
                Box::new(move || *n.lock().unwrap())
            },
            out_fn: {
                let o = out.clone();
                Box::new(move |l: &str| o.lock().unwrap().push(l.to_string()))
            },
            ntp,
            out,
            rsync: rsync.display().to_string(),
            dir,
        }
    }

    /// Add a listed pod and its simulation.
    fn add(&self, pod: Pod, sim: Sim) {
        let port = pod.ssh_port.unwrap();
        let provider = pod.provider.clone();
        for (n, r) in self.fleet.backends.lock().unwrap().iter_mut() {
            if *n == provider {
                if let Ok(p) = r {
                    p.push(pod.clone());
                }
            }
        }
        self.remote.sims.lock().unwrap().insert(host(port), sim);
    }

    fn sim(&self, port: u16, f: impl FnOnce(&mut Sim)) {
        f(self.remote.sims.lock().unwrap().get_mut(&host(port)).unwrap());
    }

    fn store(&self) -> Store {
        store_for(&self.cfg).unwrap()
    }

    fn state(&self) -> ReapState {
        self.store().load().unwrap()
    }

    /// Give `pod` a policy directly (as `reap set` would).
    fn policy(&self, pod: &Pod, expires_in: Option<u64>, idle: Option<u64>) {
        let now = self.now();
        self.store()
            .update(|s| {
                s.pods.insert(core_reap::key(&pod.provider, &pod.id), PodPolicy::new(pod, expires_in.map(|e| now + e), idle, now, "set", None));
                Ok(())
            })
            .unwrap();
    }

    fn now(&self) -> u64 {
        self.clock.load(Ordering::SeqCst)
    }

    /// Time passes: the clock, and every pod's counters at a quiet rate (0.001 cores, 10 B/s).
    fn tick(&self, secs: u64) {
        self.clock.fetch_add(secs, Ordering::SeqCst);
        for s in self.remote.sims.lock().unwrap().values_mut() {
            s.cpu += secs * 1000;
            s.net += secs * 10;
            s.git_done = false;
        }
    }

    fn env(&self) -> ReapEnv<'_> {
        let c = self.clock.clone();
        ReapEnv {
            provider: &self.fleet,
            remote: self.remote.clone(),
            cfg: &self.cfg,
            yes: true,
            clock: Arc::new(move || c.load(Ordering::SeqCst)),
            ntp: &*self.ntp_fn,
            tz: 0,
            rsync: self.rsync.clone(),
            nonce: Arc::new(|| "N0NCE".to_string()),
            resolve_local: &|_| None,
            out: &*self.out_fn,
            sync_proxy: false,
        }
    }

    /// `pods reap <args>`, parsed as the CLI parses it.
    async fn reap(&self, args: &[&str]) -> Result<()> {
        let argv = ["arena", "pods", "reap"].iter().chain(args).map(|s| s.to_string());
        let cli = super::super::Cli::try_parse_from(argv).unwrap();
        let super::super::Cmd::Pods(super::super::PodCmd::Reap(cmd)) = cli.cmd else { panic!("not a reap command") };
        handle_reap(cmd, &self.env()).await
    }

    /// [`World::run`] where only the effects matter.
    async fn step(&self, mode: &str) {
        let _ = self.run(mode).await;
    }

    async fn run(&self, mode: &str) -> (Result<()>, String) {
        self.out.lock().unwrap().clear();
        let args: &[&str] = match mode {
            "apply" => &["run", "--apply"],
            "rehearse" => &["run", "--rehearse"],
            _ => &["run"],
        };
        let r = self.reap(args).await;
        (r, self.out.lock().unwrap().join("\n"))
    }

    fn terminated(&self) -> Vec<(String, String)> {
        self.fleet.terminated.lock().unwrap().clone()
    }

    fn calls_to(&self, port: u16) -> Vec<&'static str> {
        self.remote.calls.lock().unwrap().iter().filter(|(h, _)| *h == host(port)).map(|(_, k)| *k).collect()
    }

    fn all_calls(&self) -> Vec<(String, &'static str)> {
        self.remote.calls.lock().unwrap().clone()
    }

    /// The ordered log of remote calls, rsyncs and terminates.
    fn order(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("calls.log")).unwrap_or_default().lines().map(String::from).collect()
    }

    fn notified(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("notified")).unwrap_or_default().lines().map(String::from).collect()
    }

    fn put(&self, file: &str, text: &str) {
        std::fs::write(self.dir.join(file), text).unwrap();
    }

    fn last_row(&self, pod: &Pod) -> Option<Row> {
        self.state().pods.get(&core_reap::key(&pod.provider, &pod.id)).and_then(|p| p.last.as_ref().map(|l| l.row))
    }
}

fn exit_code(r: &Result<()>) -> Option<u8> {
    match r {
        Ok(()) => Some(0),
        Err(e) => e.downcast_ref::<ReapExit>().map(|x| x.code),
    }
}

/// Run `mode` every `every` seconds until `pod` is reaped (or `max` runs).
async fn run_until_reaped(w: &World, mode: &str, every: u64, max: usize) -> usize {
    for i in 0..max {
        if i > 0 {
            w.tick(every);
        }
        let (r, out) = w.run(mode).await;
        assert!(exit_code(&r).is_some_and(|c| c == 0 || c == 3), "{r:?}\n{out}");
        if !w.terminated().is_empty() {
            return i + 1;
        }
    }
    max
}

/// Spec §13 scenario 1 (+ I11, I12): idle for 8 h, observed every 15 min — exactly one
/// terminate, after the git push, both rsync tiers and the verify, in that order, with the
/// identity read on the pod at the deciding probe and again after the backup.
#[tokio::test]
async fn idle_8h_is_reaped_once_after_a_verified_backup() {
    let w = World::new("idle8h", "");
    let a = rp("devtest-alpha", "pa", 22001);
    w.add(a.clone(), Sim::new("pa", "runpod"));
    w.policy(&a, None, Some(8 * 3600));
    let runs = run_until_reaped(&w, "apply", 900, 40).await;
    assert_eq!(w.terminated(), [("runpod".to_string(), "pa".to_string())]);
    assert_eq!(runs, 34, "first look has no baseline; then 33 looks spanning 8h");
    let order = w.order();
    let pos = |needle: &str| order.iter().rposition(|l| l.starts_with(needle)).unwrap_or_else(|| panic!("{needle} missing: {order:#?}"));
    let (git, snap, big, verify, term) = (pos("remote git"), pos("rsync snapshot"), pos("rsync big"), pos("rsync verify"), pos("terminate"));
    assert!(git < snap && snap < big && big < verify && verify < term, "{order:#?}");
    // Two snapshot passes (files, then .git), after the git push.
    assert_eq!(order.iter().filter(|l| l.starts_with("rsync snapshot")).count(), 2);
    // The deciding probe before the git push, the post-backup probe after the verify.
    let probes: Vec<usize> = order.iter().enumerate().filter(|(_, l)| l.starts_with("remote probe")).map(|(i, _)| i).collect();
    assert!(probes.iter().any(|p| *p < git) && probes.iter().any(|p| *p > verify && *p < term), "{order:#?}");
    let s = w.state();
    let tomb = &s.reaped["runpod:pa"];
    assert!(tomb.snapshot_dir.as_deref().unwrap().ends_with("/snap/w0d4/devtest-alpha/") || tomb.snapshot_dir.as_deref().unwrap().contains("/snap/w"), "{tomb:?}");
    assert!(tomb.big_dir.as_deref().unwrap().ends_with("/snap/big/devtest-alpha/"));
    assert!(!s.pods.contains_key("runpod:pa"));
    assert_eq!(s.attempts.len(), 1);
    assert!(w.notified().iter().any(|l| l.contains("reaped devtest-alpha") && l.contains("arena keys revoke devtest-alpha")), "{:?}", w.notified());
    // The log has the would-be and the reap.
    let log = w.store().read_log().unwrap();
    assert!(log.iter().any(|l| l.contains("\"row\":\"reaped\"")));
}

/// Scenario 2: expiry — two confirmed sightings and two quiet looks: reaped. While in use:
/// deferred, notified once per 6 h. While unreachable: nothing, notified.
#[tokio::test]
async fn expiry_is_reaped_only_when_confirmed_and_quiet() {
    let w = World::new("expiry", "");
    let a = rp("devtest-alpha", "pa", 22001);
    w.add(a.clone(), Sim::new("pa", "runpod"));
    w.policy(&a, Some(1200), None);
    // Within the hour before: probed. Not due yet.
    w.step("apply").await;
    assert_eq!(w.last_row(&a), Some(Row::Unknown), "first look: no baseline");
    w.tick(600);
    w.step("apply").await;
    assert_eq!(w.last_row(&a), Some(Row::Scheduled));
    w.tick(600); // expiry now
    w.step("apply").await;
    assert_eq!(w.last_row(&a), Some(Row::ExpiryConfirming));
    assert!(w.terminated().is_empty());
    w.tick(600);
    w.step("apply").await;
    assert_eq!(w.terminated(), [("runpod".to_string(), "pa".to_string())], "confirmed 10 min later, 2 quiet looks");
}

#[tokio::test]
async fn expiry_in_use_or_unreachable_is_deferred_and_notified() {
    let w = World::new("expiry-busy", "");
    let a = rp("devtest-alpha", "pa", 22001);
    let b = rp("devtest-bravo", "pb", 22002);
    let mut busy = Sim::new("pa", "runpod");
    busy.sessions = 1;
    w.add(a.clone(), busy);
    let mut gone = Sim::new("pb", "runpod");
    gone.unreachable = true;
    w.add(b.clone(), gone);
    w.policy(&a, Some(900), None);
    w.policy(&b, Some(900), None);
    for _ in 0..12 {
        w.tick(900);
        let (r, out) = w.run("apply").await;
        assert!(exit_code(&r).is_some_and(|c| c == 0 || c == 3), "{out}");
    }
    assert!(w.terminated().is_empty(), "never while in use or unreachable");
    assert_eq!(w.last_row(&a), Some(Row::Active));
    assert_eq!(w.last_row(&b), Some(Row::ProbeFailed));
    let n = w.notified();
    // 12 runs over 3 h: each pod notified once (the 6 h repeat).
    assert_eq!(n.iter().filter(|l| l.contains("overdue_in_use devtest-alpha")).count(), 1, "{n:?}");
    assert_eq!(n.iter().filter(|l| l.contains("overdue_unreachable devtest-bravo")).count(), 1, "{n:?}");
    w.tick(6 * 3600);
    w.step("apply").await;
    assert_eq!(w.notified().iter().filter(|l| l.contains("overdue_in_use devtest-alpha")).count(), 2);
    let ends = super::ends_column(&w.cfg, &[a.clone(), b.clone()], w.now(), 0).unwrap();
    assert!(ends[0].starts_with("OVERDUE") && ends[0].ends_with("(in use)"), "{ends:?}");
    assert!(ends[1].ends_with("(unreachable)"), "{ends:?}");
}

/// A pod due with a 15-minute idle window: 5 looks 5 minutes apart.
async fn due_world(tag: &str, extra: &str) -> (World, Pod) {
    let w = World::new(tag, extra);
    let a = rp("devtest-alpha", "pa", 22001);
    w.add(a.clone(), Sim::new("pa", "runpod"));
    w.policy(&a, None, Some(900));
    for i in 0..4 {
        if i > 0 {
            w.tick(300);
        }
        let (r, out) = w.run("observe").await;
        assert!(r.is_ok(), "{tag} look {i}: {r:?}\n{out}");
    }
    w.tick(300);
    let s = w.state();
    let n = s.pods.values().next().map_or(0, |p| p.observations.len());
    assert_eq!(n, 4, "{tag}: {:#?}", s.pods.values().next().map(|p| &p.observations));
    (w, a)
}

/// Scenario 3: SSH fails on the deciding probe, or only on the post-backup probe — no terminate.
#[tokio::test]
async fn ssh_failure_before_or_after_the_backup_means_no_terminate() {
    let (w, a) = due_world("ssh-before", "").await;
    w.sim(22001, |s| s.unreachable = true);
    let (r, out) = w.run("apply").await;
    eprintln!("ssh-before: {r:?}\n{out}");
    assert!(w.terminated().is_empty());
    assert_eq!(w.last_row(&a), Some(Row::ProbeFailed));
    let (w, a) = due_world("ssh-after", "").await;
    w.sim(22001, |s| s.unreachable_after_git = true);
    let (_, out) = w.run("apply").await;
    assert!(w.terminated().is_empty(), "{out}");
    assert!(out.contains("not quiet after the backup") || out.contains("post_backup"), "{out}");
    assert!(w.state().pods[&core_reap::key("runpod", &a.id)].terminating.is_none(), "the write-ahead is cleared");
}

/// Scenario 4 (I12): an identity mismatch before the backup, or only after it — no terminate.
#[tokio::test]
async fn identity_is_checked_before_and_after_the_backup() {
    let (w, a) = due_world("id-before", "").await;
    w.sim(22001, |s| s.id = "someone-else".into());
    w.step("apply").await;
    assert!(w.terminated().is_empty());
    assert_eq!(w.last_row(&a), Some(Row::Identity));
    assert!(!w.calls_to(22001).contains(&"git"), "no backup of a stranger");
    let (w, _) = due_world("id-after", "").await;
    w.sim(22001, |s| s.id_after_git = Some("someone-else".into()));
    let (_, out) = w.run("apply").await;
    assert!(w.terminated().is_empty(), "{out}");
    assert!(out.contains("not quiet after the backup"), "{out}");
    let log = w.store().read_log().unwrap();
    assert!(log.iter().any(|l| l.contains("\"row\":\"post_backup_not_quiet\"") && l.contains("\"identity\"")), "{log:#?}");
}

/// Scenario 5: an rsync tier exits 23, 24, 255 or times out — no terminate, retry in 6 h; a
/// verify that finds files pending — no terminate.
#[tokio::test]
async fn rsync_failures_and_pending_verify_mean_no_terminate() {
    for (case, file, text) in [("snap23", "exit_snap", "23"), ("big24", "exit_big", "24"), ("big255", "exit_big", "255"), ("verify-exit", "exit_verify", "23")] {
        let (w, a) = due_world(case, "").await;
        w.put(file, text);
        let (r, out) = w.run("apply").await;
        assert!(w.terminated().is_empty(), "{case}: {out}");
        // A failed tier needs attention; a verify that didn't confirm is "not yet".
        assert_eq!(exit_code(&r), Some(if file == "exit_verify" { 0 } else { 3 }), "{case}");
        let p = &w.state().pods[&core_reap::key("runpod", &a.id)];
        if file != "exit_verify" {
            assert!(p.retry_after.is_some_and(|r| r >= w.now() + core_reap::BACKUP_FAIL_RETRY - 5), "{case}: {p:?}");
            assert!(w.notified().iter().any(|l| l.contains("rsync_failed")), "{case}");
            // And the next run backs off.
            w.tick(300);
            w.step("apply").await;
            assert!(w.terminated().is_empty());
            assert_eq!(w.last_row(&a), Some(Row::BackoffAfterFailure), "{case}");
        }
        assert!(p.terminating.is_none(), "{case}");
    }
    let (w, a) = due_world("timeout", "BACKUP_TIMEOUT_SECS=1").await;
    w.put("hang_big", "");
    w.step("apply").await;
    assert!(w.terminated().is_empty());
    assert!(w.state().pods[&core_reap::key("runpod", &a.id)].retry_after.is_some(), "a timed-out tier is a failure");
    let (w, a) = due_world("pending", "").await;
    w.put("pending", "3");
    w.step("apply").await;
    assert!(w.terminated().is_empty());
    let p = &w.state().pods[&core_reap::key("runpod", &a.id)];
    assert!(p.observations.last().is_some_and(|o| o.verdict == Verdict::Active && o.reasons == [Reason::Files]), "{p:?}");
}

/// Scenario 6: git fails or prints no sentinel — no terminate; `SKIP` on main — reaped.
#[tokio::test]
async fn git_must_end_with_a_sentinel() {
    for (case, git) in [("git-fail", (1, "fatal: remote hung up\n")), ("git-nosentinel", (0, "done\n"))] {
        let (w, a) = due_world(case, "").await;
        w.sim(22001, |s| s.git = (git.0, git.1.into()));
        w.step("apply").await;
        assert!(w.terminated().is_empty(), "{case}");
        assert!(w.state().pods[&core_reap::key("runpod", &a.id)].retry_after.is_some(), "{case}");
        assert!(!w.order().iter().any(|l| l.starts_with("rsync")), "{case}: no rsync after a failed git");
    }
    let (w, _) = due_world("git-skip", "").await;
    w.sim(22001, |s| s.git = (0, "SKIP main\n".into()));
    w.step("apply").await;
    assert_eq!(w.terminated().len(), 1, "SKIP is fine: the tiers carry .git");
    // No repo on the pod: no git step at all, the tiers carry the home.
    let (w, _) = due_world("no-repo", "").await;
    w.sim(22001, |s| s.repo = "absent");
    w.tick(300);
    w.step("observe").await;
    w.tick(300);
    w.step("apply").await;
    assert_eq!(w.terminated().len(), 1);
    assert!(!w.calls_to(22001).contains(&"git"));
}

/// Scenario 7 (I7): Vast's listing fails while RunPod's works — only RunPod is considered;
/// zero SSH to the Vast pod; its policy kept.
#[tokio::test]
async fn a_failed_listing_touches_nothing_of_that_provider() {
    let w = World::new("vast-fails", "");
    let a = rp("devtest-alpha", "pa", 22001);
    w.add(a.clone(), Sim::new("pa", "runpod"));
    let v = Pod { provider: "vast".into(), locked: None, ..rp("devtest-bravo", "77", 22002) };
    w.remote.sims.lock().unwrap().insert(host(22002), Sim::new("77", "runpod"));
    w.fleet.backends.lock().unwrap().push(("vast".into(), Err("vast list HTTP 429".into())));
    w.policy(&a, None, Some(900));
    w.policy(&v, None, Some(900));
    let (_, out) = w.run("apply").await;
    assert!(w.calls_to(22002).is_empty(), "no SSH to a pod whose provider didn't list");
    assert!(!w.calls_to(22001).is_empty());
    assert_eq!(w.last_row(&v), Some(Row::ListingFailed));
    assert!(out.contains("vast: listing failed"), "{out}");
    assert!(w.state().pods.contains_key("vast:77"), "its policy is kept");
}

/// Scenarios 8 + 9 (I2, I6): a locked pod, a delete-protected server, and RunPod on REST v1
/// (lock unknown): zero SSH, never terminated, never unlocked. A lock that raced us (the
/// terminate refused as locked): `TerminateRefusedLocked`, no hold.
#[tokio::test]
async fn locked_protected_and_lock_unknown_pods_are_never_touched() {
    let w = World::new("locked", "");
    let locked = Pod { locked: Some(true), ..rp("devtest-alpha", "pa", 22001) };
    let prot = Pod { delete_protected: Some(true), ..hz("devtest-bravo", "9", 22002) };
    let unknown = Pod { locked: None, ..rp("devtest-charlie", "pc", 22003) };
    for (p, id, prov) in [(&locked, "pa", "runpod"), (&prot, "9", "hetzner"), (&unknown, "pc", "runpod")] {
        w.add(p.clone(), Sim::new(id, prov));
        w.policy(p, Some(1), Some(900));
    }
    for _ in 0..6 {
        w.tick(300);
        w.step("apply").await;
    }
    assert!(w.all_calls().is_empty(), "zero SSH: {:?}", w.all_calls());
    assert!(w.terminated().is_empty());
    assert_eq!((w.last_row(&locked), w.last_row(&prot), w.last_row(&unknown)), (Some(Row::Locked), Some(Row::Locked), Some(Row::LockUnknown)));
    // RunPod on REST v1: unsupported, zero SSH.
    let w = World::new("v1", "RUNPOD_API=v1");
    let a = rp("devtest-alpha", "pa", 22001);
    w.add(a.clone(), Sim::new("pa", "runpod"));
    w.policy(&a, None, Some(900));
    w.step("apply").await;
    assert!(w.all_calls().is_empty());
    assert_eq!(w.last_row(&a), Some(Row::ProviderUnsupported));
    // A lock that raced the reap.
    let (w, a) = due_world("raced", "").await;
    w.fleet.replies.lock().unwrap().push_back(Err(arena_core::Error::Provider { kind: arena_core::ProviderErrorKind::Locked, message: "Pod is locked".into() }));
    let (_, out) = w.run("apply").await;
    assert_eq!(w.terminated().len(), 1);
    assert!(w.state().hold.is_none(), "a lock is a refusal, not a surprise: {out}");
    assert!(out.contains("refused: locked"), "{out}");
    assert!(w.state().pods[&core_reap::key("runpod", &a.id)].terminating.is_none());
}

/// Scenario 10: a pin by name survives a rename to a new id (the pod replaced under its name);
/// a pin added while the backup runs stops the reap at the state re-read.
#[tokio::test]
async fn pins_veto_including_one_added_mid_reap() {
    let w = World::new("pin-name", "");
    let a = rp("devtest-alpha", "new-id", 22001);
    w.add(a.clone(), Sim::new("new-id", "runpod"));
    w.policy(&a, Some(1), Some(900));
    w.store()
        .update(|s| {
            s.pins.push(Pin { name: "devtest-alpha".into(), id: Some("old-id".into()), until: None, note: Some("demo".into()), set_at: 0 });
            Ok(())
        })
        .unwrap();
    for _ in 0..6 {
        w.tick(300);
        w.step("apply").await;
    }
    assert!(w.all_calls().is_empty(), "pinned: not even probed");
    assert!(w.terminated().is_empty());
    assert_eq!(w.last_row(&a), Some(Row::Pinned));
    let (w, a) = due_world("pin-mid", "").await;
    w.sim(22001, |s| s.on_git = OnGit::Pinned);
    let (_, out) = w.run("apply").await;
    assert!(w.terminated().is_empty(), "{out}");
    assert!(w.calls_to(22001).contains(&"git"), "it got as far as the backup");
    assert!(w.state().pods[&core_reap::key("runpod", &a.id)].terminating.is_none());
    assert!(w.store().read_log().unwrap().iter().any(|l| l.contains("\"row\":\"pinned\"") && l.contains("\"kind\":\"act\"")));
    // An expiry extended mid-reap: not due any more.
    let w = World::new("extend-mid", "");
    let b = rp("devtest-bravo", "pb", 22002);
    w.add(b.clone(), Sim::new("pb", "runpod"));
    w.policy(&b, Some(600), None);
    w.sim(22002, |s| s.on_git = OnGit::Extended);
    for _ in 0..6 {
        w.tick(600);
        w.step("apply").await;
    }
    assert!(w.terminated().is_empty());
    assert!(w.calls_to(22002).contains(&"git"));
}

/// Scenario 11 (I6): keep marker, data on /workspace, repo outside the home — never terminated.
#[tokio::test]
async fn keep_and_not_covered_veto() {
    for (case, f) in [
        ("keep", (|s: &mut Sim| s.keep = true) as fn(&mut Sim)),
        ("workspace", |s: &mut Sim| s.workspace = "has_data"),
        ("repo-outside", |s: &mut Sim| s.repo = "outside_home"),
    ] {
        let w = World::new(case, "");
        let a = rp("devtest-alpha", "pa", 22001);
        let mut sim = Sim::new("pa", "runpod");
        f(&mut sim);
        w.add(a.clone(), sim);
        w.policy(&a, Some(1), Some(900));
        for _ in 0..8 {
            w.tick(300);
            w.step("apply").await;
        }
        assert!(w.terminated().is_empty(), "{case}");
        let want = if case == "keep" { Row::Keep } else { Row::NotCovered };
        assert_eq!(w.last_row(&a), Some(want), "{case}");
        let ends = super::ends_column(&w.cfg, &[a.clone()], w.now(), 0).unwrap();
        assert_eq!(ends[0], if case == "keep" { "kept on pod" } else { "not covered" }, "{case}");
    }
}

/// Scenario 12 (I2): pods with no policy, off the list, a staff `@` box, a twin and a shared
/// name get zero remote calls.
#[tokio::test]
async fn inv_scope_zero_remote_calls() {
    let w = World::new("scope", "");
    let pods = [
        rp("devtest-alpha", "nopolicy", 22001),
        rp("devtest-zulu", "offlist", 22002),
        rp("devtest-boss", "staff", 22003),
        rp("devtest-bravo", "b1", 22004),
        rp("devtest-bravo", "b2", 22005),
        rp("devtest-charlie", "c1", 22006),
        rp("devtest-charlie-old", "c0", 22007),
        rp("devtest-delta", "d1", 22008),
    ];
    for p in &pods {
        w.add(p.clone(), Sim::new(&p.id, "runpod"));
    }
    for p in &pods[1..7] {
        w.policy(p, Some(1), Some(900));
    }
    // delta: stopped.
    let stopped = Pod { status: "EXITED".into(), ..pods[7].clone() };
    w.fleet.backends.lock().unwrap()[0].1.as_mut().unwrap()[7] = stopped.clone();
    w.policy(&stopped, Some(1), None);
    for _ in 0..5 {
        w.tick(300);
        w.step("apply").await;
    }
    assert!(w.all_calls().is_empty(), "{:?}", w.all_calls());
    assert!(w.terminated().is_empty());
    let rows: Vec<Option<Row>> = pods[1..].iter().map(|p| w.last_row(p)).collect();
    assert_eq!(
        rows,
        [Some(Row::NotOnList), Some(Row::NotOnList), Some(Row::DuplicateOrTwin), Some(Row::DuplicateOrTwin), Some(Row::DuplicateOrTwin), Some(Row::NotOnList), Some(Row::NotRunning)]
    );
    assert!(w.notified().iter().any(|l| l.contains("overdue_not_running devtest-delta")));
}

/// Scenario 13 (I8): 1 per run, and the daily cap across runs; a killed run's attempt counts.
#[tokio::test]
async fn inv_caps() {
    let w = World::new("caps", "REAP_MAX_PER_DAY=2");
    let names = ["alpha", "bravo", "charlie", "delta", "echo"];
    for (i, n) in names.iter().enumerate() {
        let p = rp(&format!("devtest-{n}"), &format!("p{i}"), 22001 + i as u16);
        w.add(p.clone(), Sim::new(&p.id, "runpod"));
        w.policy(&p, None, Some(900));
    }
    for _ in 0..5 {
        w.tick(300);
        w.step("apply").await;
    }
    assert_eq!(w.terminated().len(), 1, "one per run");
    // A killed run: a write-ahead with no terminate counts against the day.
    w.store()
        .update(|s| {
            s.attempts.push(w.now());
            Ok(())
        })
        .unwrap();
    for _ in 0..4 {
        w.tick(300);
        w.step("apply").await;
    }
    assert_eq!(w.terminated().len(), 1, "the day's 2 attempts are used: {:?}", w.terminated());
    assert!(w.state().pods.values().any(|p| p.last.as_ref().is_some_and(|l| l.row == Row::Capped)));
    // A day later the caps have room again (the streaks restart after the gap).
    w.tick(86_400);
    for _ in 0..5 {
        w.step("apply").await;
        w.tick(300);
    }
    assert_eq!(w.terminated().len(), 2, "a new day");
}

/// Scenario 14 (I7): an unverified clock — observations only, no actions, exit 3.
#[tokio::test]
async fn inv_clock() {
    let (w, a) = due_world("clock", "").await;
    *w.ntp.lock().unwrap() = Some(false);
    let (r, out) = w.run("apply").await;
    assert_eq!(exit_code(&r), Some(3));
    assert!(w.terminated().is_empty(), "{out}");
    assert!(out.contains("clock unverified"), "{out}");
    assert!(w.state().pods[&core_reap::key("runpod", &a.id)].observations.last().is_some_and(|o| !o.clock_ok));
    // The clock going back since the last run: the same.
    let (w, _) = due_world("backstep", "").await;
    w.clock.fetch_sub(1000, Ordering::SeqCst);
    let (r, _) = w.run("apply").await;
    assert_eq!(exit_code(&r), Some(3));
    assert!(w.terminated().is_empty());
}

/// Scenario 15 (G7): a missing marker, too little free space, no ARENA_START_DATE — no
/// actions, notified, exit 3.
#[tokio::test]
async fn an_unhealthy_destination_blocks_and_notifies() {
    let (w, a) = due_world("nomarker", "").await;
    std::fs::remove_file(w.dir.join("snap/big/.arena-backup-tier")).unwrap();
    let (r, out) = w.run("apply").await;
    assert_eq!(exit_code(&r), Some(3));
    assert!(w.terminated().is_empty());
    assert_eq!(w.last_row(&a), Some(Row::DestUnhealthy));
    assert!(out.contains("is the big volume mounted?"), "{out}");
    assert!(w.notified().iter().any(|l| l.contains("dest_unhealthy")));
    let (w, _) = due_world("nospace", "REAP_MIN_FREE_GB=100000").await;
    let (r, out) = w.run("apply").await;
    assert_eq!(exit_code(&r), Some(3));
    assert!(out.contains("under REAP_MIN_FREE_GB"), "{out}");
    assert!(w.terminated().is_empty());
    let (w, a) = due_world("nostart", "ARENA_START_DATE=").await;
    let (r, out) = w.run("apply").await;
    assert_eq!(exit_code(&r), Some(3));
    assert!(w.terminated().is_empty());
    assert!(out.contains("ARENA_START_DATE isn't set"), "{out}");
    assert_eq!(w.last_row(&a), Some(Row::DestUnhealthy));
}

/// Scenario 16 (I13): a dry run writes nothing to pods and terminates nothing (only the probe
/// and the jobs listing, both read-only); `--rehearse` backs up and verifies, never terminates.
#[tokio::test]
async fn dry_run_is_read_only_and_rehearse_never_terminates() {
    let (w, a) = due_world("dry", "").await;
    let (r, out) = w.run("observe").await;
    assert!(r.is_ok(), "{out}");
    assert!(out.contains("WOULD TERMINATE"), "{out}");
    assert!(w.terminated().is_empty());
    assert!(w.all_calls().iter().all(|(_, k)| matches!(*k, "probe" | "jobs")), "{:?}", w.all_calls());
    assert!(!w.order().iter().any(|l| l.starts_with("rsync")));
    assert_eq!(w.state().attempts.len(), 0);
    assert_eq!(w.last_row(&a), Some(Row::WouldReap));
    // (Two looks in the same second don't extend a streak: the next run is a tick later.)
    w.tick(300);
    let (r, out) = w.run("rehearse").await;
    assert!(r.is_ok(), "{out}");
    assert!(w.terminated().is_empty());
    assert!(w.order().iter().any(|l| l.starts_with("rsync verify")), "{:?}", w.order());
    assert!(out.contains("rehearsed"), "{out}");
    assert_eq!(w.state().attempts.len(), 0, "a rehearsal uses no cap");
    assert!(w.store().read_log().unwrap().iter().any(|l| l.contains("\"row\":\"rehearsed\"") && l.contains("/snap/big/devtest-alpha/")));
}

/// Scenario 17: the re-listing shows the pod changed (renamed, stopped, locked) — nothing; or
/// gone — nothing, and a tombstone with its backup paths.
#[tokio::test]
async fn relist_changed_or_gone() {
    for change in [OnGit::Renamed, OnGit::Stopped, OnGit::Locked] {
        let (w, a) = due_world(&format!("relist-{change:?}"), "").await;
        w.sim(22001, |s| s.on_git = change);
        let (_, out) = w.run("apply").await;
        assert!(w.terminated().is_empty(), "{change:?}: {out}");
        assert!(w.state().pods[&core_reap::key("runpod", &a.id)].terminating.is_none(), "{change:?}");
        assert!(out.contains("re-listing didn't confirm"), "{change:?}: {out}");
    }
    let (w, a) = due_world("relist-gone", "").await;
    w.sim(22001, |s| s.on_git = OnGit::Gone);
    let (_, out) = w.run("apply").await;
    assert!(w.terminated().is_empty(), "{out}");
    let s = w.state();
    assert!(!s.pods.contains_key(&core_reap::key("runpod", &a.id)));
    assert!(s.reaped[&core_reap::key("runpod", &a.id)].big_dir.as_deref().is_some_and(|d| d.ends_with("/snap/big/devtest-alpha/")));
}

/// Scenario 18 (I14): a terminate that fails in an unknown way sets a hold; the next --apply
/// runs as observe. A pod still billing two runs after an Ok terminate sets one too (F3).
#[tokio::test]
async fn inv_hold() {
    let (w, _) = due_world("hold500", "").await;
    w.fleet.replies.lock().unwrap().push_back(Err(arena_core::Error::provider("terminate pod HTTP 500")));
    let (r, out) = w.run("apply").await;
    assert_eq!(exit_code(&r), Some(3), "{out}");
    let s = w.state();
    assert!(s.hold.as_ref().is_some_and(|h| h.auto && h.reason.contains("HTTP 500")), "{:?}", s.hold);
    assert!(s.pods.values().all(|p| p.terminating.is_some()), "the write-ahead stays for the next run to reconcile");
    assert!(w.notified().iter().any(|l| l.contains("terminate_failed")));
    w.tick(300);
    let (_, out) = w.run("apply").await;
    assert!(out.contains("OBSERVE (reaping is on hold)"), "{out}");
    assert_eq!(w.terminated().len(), 1, "no retry while held");
    // Released (after a look), the pod — still listed — is a fresh candidate (F1: Interrupted).
    w.reap(&["release"]).await.unwrap();
    assert!(w.state().hold.is_none());
    // F3.
    let (w, _) = due_world("stillbilling", "").await;
    *w.fleet.stays.lock().unwrap() = true;
    w.step("apply").await;
    assert_eq!(w.terminated().len(), 1);
    w.tick(900);
    w.step("observe").await;
    assert!(w.state().hold.is_none());
    w.tick(900);
    let (r, _) = w.run("observe").await;
    assert_eq!(exit_code(&r), Some(3));
    assert!(w.state().hold.is_some_and(|h| h.auto && h.reason.contains("still listed and billing")));
}

/// Scenario 19: another backup (a `pods pull`) holds the pod's backup lock — no observation
/// this run, and within MAX_GAP the streak isn't broken.
#[tokio::test]
async fn a_held_backup_lock_skips_the_look_without_breaking_the_streak() {
    let (w, a) = due_world("lock-held", "").await;
    let dir = w.cfg.get("LOCAL_BACKUP_DIR").unwrap().to_string();
    let held = arena_core::pull::open_pod_lock(&dir, &a.name).unwrap();
    assert!(arena_core::pull::try_lock(&held).unwrap());
    w.tick(0);
    let before = w.calls_to(22001).len();
    w.step("apply").await;
    assert_eq!(w.calls_to(22001).len(), before, "no SSH while the lock is held");
    assert_eq!(w.last_row(&a), Some(Row::BackupBusy));
    drop(held);
    w.tick(300);
    w.step("apply").await;
    assert_eq!(w.terminated().len(), 1, "the streak survived the skipped look");
}

/// I9: a corrupt or unknown-version state file is never treated as empty and never
/// overwritten — exit 1, bytes identical, notified; concurrent writers lose nothing.
#[tokio::test]
async fn state_corrupt_untouched() {
    let (w, _) = due_world("corrupt", "").await;
    let path = w.store().state_path();
    for bad in ["{not json", "{\"version\": 9, \"pods\": {}}"] {
        std::fs::write(&path, bad).unwrap();
        let (r, out) = w.run("apply").await;
        assert_eq!(exit_code(&r), Some(1), "{out}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), bad, "byte-identical");
        assert!(w.terminated().is_empty());
    }
    assert!(w.all_calls().iter().filter(|(_, k)| *k == "probe").count() == 4, "no probes on a corrupt state");
    assert!(w.notified().iter().any(|l| l.contains("state_unreadable")));
    // `reap set`-style writers racing: every one lands.
    let w = World::new("concurrent", "");
    let store = w.store();
    store.save(&ReapState::default()).unwrap();
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let s = store.clone();
            std::thread::spawn(move || {
                for j in 0..10 {
                    s.update(|st| {
                        st.pins.push(Pin { name: format!("p{i}-{j}"), id: None, until: None, note: None, set_at: 0 });
                        Ok(())
                    })
                    .unwrap();
                }
            })
        })
        .collect();
    handles.into_iter().for_each(|h| h.join().unwrap());
    assert_eq!(store.load().unwrap().pins.len(), 80);
}

/// I1 at the CLI: with REAP_ENABLED unset (or 0) in the file, `--apply` runs as observe —
/// nothing terminated, said so.
#[tokio::test]
async fn inv_default_off() {
    for extra in ["REAP_ENABLED=0", "REAP_ENABLED=\"\""] {
        let (w, a) = due_world(&format!("off{}", extra.len()), extra).await;
        let (r, out) = w.run("apply").await;
        assert!(r.is_ok(), "{out}");
        assert!(w.terminated().is_empty());
        assert!(out.contains("reaping disabled in config"), "{out}");
        assert_eq!(w.last_row(&a), Some(Row::WouldReap));
    }
    // And no policy, no probe: a fresh fleet with REAP_ENABLED=1 does nothing at all.
    let w = World::new("nopolicy", "");
    w.add(rp("devtest-alpha", "pa", 22001), Sim::new("pa", "runpod"));
    for _ in 0..3 {
        w.tick(900);
        w.step("apply").await;
    }
    assert!(w.all_calls().is_empty() && w.terminated().is_empty());
}

/// I4 at the source: the reap code reaches `terminate` only through the ticket.
#[test]
fn inv_single_path_in_the_source() {
    let src = include_str!("../reap.rs");
    let code: String = src.lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n");
    assert!(!code.contains(".terminate_pod("), "a direct terminate in reap.rs");
    assert!(!code.contains("terminate_pod_on("), "a direct terminate_pod_on in reap.rs");
    assert_eq!(code.matches("ticket.terminate(").count(), 1);
    assert!(!code.contains("set_locked("), "the reaper never (un)locks");
}

/// `reap set` / `clear` / `pin` / `unpin` / `status` / `hold` / `release` through the CLI parser.
#[tokio::test]
async fn set_clear_pin_unpin_status() {
    let w = World::new("cmds", "");
    *w.fleet.strict.lock().unwrap() = false;
    let a = rp("devtest-alpha", "pa", 22001);
    let staff = rp("devtest-boss", "ps", 22002);
    w.add(a.clone(), Sim::new("pa", "runpod"));
    w.add(staff.clone(), Sim::new("ps", "runpod"));
    w.reap(&["set", "alpha", "--expire-in", "2h", "--idle-after", "8h", "--note", "week 2"]).await.unwrap();
    let p = w.state().pods["runpod:pa"].clone();
    assert_eq!((p.expires_at, p.idle_after_secs, p.note.as_deref()), (Some(w.now() + 7200), Some(28_800), Some("week 2")));
    assert!(w.calls_to(22001).contains(&"notice"), "the notice is written");
    // A staff box named: refused. Under --all: skipped with a note.
    let e = w.reap(&["set", "@devtest-boss", "--expire-in", "2h"]).await.unwrap_err().to_string();
    assert!(e.contains("never reaped"), "{e}");
    w.reap(&["set", "--all", "--extend", "1h"]).await.unwrap();
    assert_eq!(w.state().pods["runpod:pa"].expires_at, Some(w.now() + 3 * 3600));
    assert!(!w.state().pods.contains_key("runpod:ps"));
    // Bounds.
    assert!(w.reap(&["set", "alpha", "--idle-after", "10m"]).await.unwrap_err().to_string().contains("REAP_IDLE_FLOOR_MINS"));
    assert!(w.reap(&["set", "alpha", "--expire-in", "5m"]).await.is_err());
    assert!(w.reap(&["set", "alpha", "--expire-in", "61d"]).await.is_err());
    // Status (state only), pin, unpin, clear.
    w.reap(&["pin", "alpha", "--note", "demo Fri"]).await.unwrap();
    assert!(w.state().pinned("devtest-alpha", "pa", w.now()));
    w.out.lock().unwrap().clear();
    w.reap(&["status"]).await.unwrap();
    let out = w.out.lock().unwrap().join("\n");
    assert!(out.contains("devtest-alpha") && out.contains("pin: devtest-alpha"), "{out}");
    w.reap(&["unpin", "alpha"]).await.unwrap();
    assert!(w.state().pins.is_empty());
    w.reap(&["clear", "alpha", "--idle"]).await.unwrap();
    assert_eq!(w.state().pods["runpod:pa"].idle_after_secs, None);
    w.reap(&["clear", "alpha"]).await.unwrap();
    assert!(w.state().pods.is_empty());
    w.reap(&["hold", "--reason", "cohort demo"]).await.unwrap();
    assert!(w.state().hold.as_ref().is_some_and(|h| !h.auto && h.reason == "cohort demo"));
    w.reap(&["release"]).await.unwrap();
    assert!(w.state().hold.is_none());
    // The log reads back.
    w.reap(&["log", "--tail", "5"]).await.unwrap();
}

/// G1: invalid REAP_* settings or a relative ARENA_STATE_DIR — exit 2, nothing done.
#[tokio::test]
async fn invalid_config_exits_2() {
    for extra in ["REAP_MAX_PER_RUN=9", "ARENA_STATE_DIR=relative/dir"] {
        let w = World::new(&format!("g1-{}", extra.len()), extra);
        let a = rp("devtest-alpha", "pa", 22001);
        w.add(a.clone(), Sim::new("pa", "runpod"));
        let (r, _) = w.run("apply").await;
        assert_eq!(exit_code(&r), Some(2), "{extra}");
        assert!(w.all_calls().is_empty());
    }
}

/// G2: a run already holding the run lock — "already running", exit 0, nothing done.
#[tokio::test]
async fn one_run_at_a_time() {
    let (w, _) = due_world("runlock", "").await;
    let lock = w.store().try_run_lock().unwrap().unwrap();
    let before = w.all_calls().len();
    let (r, out) = w.run("apply").await;
    assert!(r.is_ok() && out.contains("another run is in progress"), "{out}");
    assert_eq!(w.all_calls().len(), before);
    drop(lock);
}

/// Scenario 20: `up`/`create --expire-in` resolve one end time for every pod, recorded the
/// moment each create returns (a `--check` replacement is just another create); refused before
/// any create without an explicit, absolute ARENA_STATE_DIR.
#[test]
fn create_policy_is_resolved_once_and_recorded_per_create() {
    let w = World::new("create", "");
    let now = 1_791_460_800;
    let (policy, store) = resolve_create_policy(&w.cfg, Some("8h"), None, Some("2h"), now, &|_| None).unwrap().unwrap();
    assert_eq!(policy, CreatePolicy { expires_at: Some(now + 8 * 3600), idle_after_secs: Some(7200) });
    for (i, name) in ["devtest-alpha", "devtest-bravo", "devtest-bravo"].iter().enumerate() {
        record_created(&store, &rp(name, &format!("id{i}"), 22001), policy, now + 60 * i as u64, "up");
    }
    let s = store.load().unwrap();
    assert_eq!(s.pods.len(), 3, "the replacement (id2) gets its own record");
    assert!(s.pods.values().all(|p| p.expires_at == Some(now + 8 * 3600) && p.set_by == "up"));
    assert_eq!(resolve_create_policy(&w.cfg, None, None, None, now, &|_| None).unwrap().map(|(p, _)| p), None);
    let rel = Config::parse("ARENA_STATE_DIR=rel\nMACHINE_NAME_PREFIX=devtest");
    let e = resolve_create_policy(&rel, Some("8h"), None, None, now, &|_| None).unwrap_err().to_string();
    assert!(e.contains("absolute"), "{e}");
    assert!(resolve_create_policy(&w.cfg, Some("5m"), None, None, now, &|_| None).is_err());
    assert!(resolve_create_policy(&w.cfg, None, None, Some("5m"), now, &|_| None).unwrap_err().to_string().contains("REAP_IDLE_FLOOR_MINS"));
}

/// Scenario 21: `pods rename` updates the record (and a pin by id); `replace`/`migrate` say the
/// policy stays behind.
#[test]
fn rename_and_replace_hooks() {
    let w = World::new("rename", "");
    let a = rp("devtest-alpha", "pa", 22001);
    w.policy(&a, Some(3600), None);
    w.store()
        .update(|s| {
            s.pins.push(Pin { name: "devtest-alpha".into(), id: Some("pa".into()), until: None, note: None, set_at: 0 });
            Ok(())
        })
        .unwrap();
    renamed(&w.cfg, "runpod", "pa", "devtest-bravo");
    let s = w.state();
    assert_eq!(s.pods["runpod:pa"].name, "devtest-bravo");
    assert_eq!(s.pins[0].name, "devtest-bravo");
    assert_eq!(policy_not_carried_note(&w.cfg, "runpod", "pa", "new1").as_deref(), Some("note: reap policy not carried to new1; re-run 'arena pods reap set'"));
    assert_eq!(policy_not_carried_note(&w.cfg, "runpod", "other", "new1"), None);
}

/// The ENDS column only where the reaper keeps state.
#[test]
fn ends_column_needs_a_state_file() {
    let w = World::new("ends", "");
    let a = rp("devtest-alpha", "pa", 22001);
    assert_eq!(ends_column(&w.cfg, &[a.clone()], T0, 0), None, "no state file: no column");
    w.policy(&a, Some(5 * 3600), None);
    assert_eq!(ends_column(&w.cfg, &[a.clone(), rp("devtest-bravo", "pb", 1)], T0, 0).unwrap(), ["17:00 (in 5h)", "-"]);
    std::fs::write(w.store().state_path(), "junk").unwrap();
    assert_eq!(ends_column(&w.cfg, &[a], T0, 0).unwrap(), ["?"]);
    assert_eq!(ends_column(&Config::parse(""), &[], T0, 0), None);
    assert_eq!(parse_tz("+0200"), Some(7200));
    assert_eq!(parse_tz("-0430"), Some(-16_200));
    assert_eq!(parse_tz("0200"), None);
}

/// `config check`'s reaper rows: what blocks reaping is named.
#[test]
fn readiness_rows_name_the_blockers() {
    let w = World::new("ready", "BACKUP_REMOTE_PATH=ARENA_materials");
    let (lines, blockers) = readiness_rows(&w.cfg, &w.rsync);
    let text = lines.join("\n");
    assert!(text.contains("REAP_ENABLED") && text.contains("marked `snapshot`") && text.contains("marked `big`"), "{text}");
    assert!(blockers.contains(&"BACKUP_REMOTE_PATH unset".to_string()), "{blockers:?}");
    let (_, blockers) = readiness_rows(&Config::parse("ARENA_STATE_DIR=x"), "/nonexistent/rsync");
    assert!(blockers.iter().any(|b| b.contains("ARENA_STATE_DIR")) && blockers.contains(&"rsync".to_string()), "{blockers:?}");
}

/// `cron install --reap observe|apply`: apply refused until REAP_ENABLED=1 is in the file;
/// ARENA_STATE_DIR / ARENA_START_DATE written into the line only when the file doesn't set
/// them; the line is recognised as the reap line (and only that).
#[test]
fn reap_cron_line_rules() {
    let w = World::new("cron", "");
    let path = w.dir.join("config.env");
    let write = |text: &str| std::fs::write(&path, text).unwrap();
    let state = w.dir.join("state").display().to_string();
    write(&format!("ARENA_STATE_DIR={state}\nMACHINE_NAME_PREFIX=devtest\n"));
    let e = super::super::reap_cron_line(true, &path, "/usr/local/bin/arena", "/root/c.env", "/root").unwrap_err().to_string();
    assert!(e.contains("REAP_ENABLED isn't 1") && e.contains("--reap observe"), "{e}");
    let observe = super::super::reap_cron_line(false, &path, "/usr/local/bin/arena", "/root/c.env", "/root").unwrap();
    assert!(observe.ends_with("pods reap run >> /root/arena-reap-cron.log 2>&1") && !observe.contains("ARENA_STATE_DIR="), "{observe}");
    assert_eq!(super::super::CronKind::of(&observe), Some(super::super::CronKind::Reap));
    write(&format!("ARENA_STATE_DIR={state}\nREAP_ENABLED=1\n"));
    let apply = super::super::reap_cron_line(true, &path, "/usr/local/bin/arena", "/root/c.env", "/root").unwrap();
    assert!(apply.contains(" pods reap run --apply --yes >> ") && apply.contains("flock -n /root/.arena-reap-cron.lock"), "{apply}");
    assert_eq!(super::super::CronKind::of(&apply), Some(super::super::CronKind::Reap));
    // No state dir anywhere: refused.
    write("REAP_ENABLED=1\n");
    assert!(super::super::reap_cron_line(false, &path, "/a", "/c", "/root").unwrap_err().to_string().contains("ARENA_STATE_DIR"));
    // Invalid settings: refused.
    write(&format!("ARENA_STATE_DIR={state}\nREAP_MAX_PER_DAY=500\n"));
    assert!(super::super::reap_cron_line(false, &path, "/a", "/c", "/root").unwrap_err().to_string().contains("REAP_MAX_PER_DAY"));
    // The other lines are never taken for it.
    for other in ["*/15 * * * * flock -n -o /root/.arena-backup-cron.lock /a --config /c pods backup --yes >> /root/arena-cron.log 2>&1", "# 7 * * * * arena pods reap run"] {
        assert_ne!(super::super::CronKind::of(other), Some(super::super::CronKind::Reap), "{other}");
    }
}

/// P6: in two-volume mode (`LOCAL_BACKUP_BIG_DIR` set, pulling into LOCAL_BACKUP_DIR) a pull
/// refuses without both tier markers and writes the big tier to its own root with them; a pod
/// whose backup lock another backup holds is skipped — a named failure — and gets no rsync.
#[tokio::test]
async fn pull_two_volume_markers_and_the_per_pod_lock() {
    let w = World::new("pull", "");
    let snap = w.dir.join("snap2");
    let bigvol = w.dir.join("bigvol");
    std::fs::create_dir_all(&snap).unwrap();
    std::fs::create_dir_all(&bigvol).unwrap();
    let cfg = Config::parse(&format!(
        "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=( alpha )\nLOCAL_BACKUP_DIR={}\nLOCAL_BACKUP_BIG_DIR={}\nARENA_START_DATE=2026-10-05\n",
        snap.display(),
        bigvol.display()
    ));
    let pod = rp("devtest-alpha", "pa", 22001);
    let sel = super::super::Selected { pods: vec![pod.clone()], named: true };
    let remote: Arc<dyn Remote> = Arc::new(arena_core::remote::FakeRemote::new());
    let dir = snap.display().to_string();
    let args = || super::super::PullRun { label: Some("w1d1".into()), max_size: None, remote_path: None, no_git: false, no_big: false, dry_run: false, yes: true };
    let pull = |locking| super::super::pull_locked(&remote, &cfg, &dir, args(), &sel, &w.rsync, locking);
    let e = pull(super::super::LockMode::Take(Duration::from_millis(300))).await.unwrap_err().to_string();
    assert!(e.contains("refusing to pull") && e.contains("is the snapshot volume mounted?"), "{e}");
    std::fs::write(snap.join(".arena-backup-tier"), "snapshot\n").unwrap();
    std::fs::write(bigvol.join(".arena-backup-tier"), "big\n").unwrap();
    pull(super::super::LockMode::Take(Duration::from_millis(300))).await.unwrap();
    let calls = w.order();
    assert!(calls.iter().any(|l| l.ends_with(&format!("{}/devtest-alpha/", bigvol.display()))), "{calls:#?}");
    assert!(!std::path::Path::new(&snap.join("big")).exists(), "no big/ under the snapshot root");
    // Busy: another backup holds the pod's lock.
    std::fs::remove_file(w.dir.join("calls.log")).unwrap();
    let held = arena_core::pull::open_pod_lock(&dir, "devtest-alpha").unwrap();
    assert!(arena_core::pull::try_lock(&held).unwrap());
    let e = pull(super::super::LockMode::Take(Duration::from_millis(300))).await.unwrap_err().to_string();
    assert!(e.contains("devtest-alpha (busy)"), "{e}");
    assert!(w.order().is_empty(), "no rsync of a busy pod: {:?}", w.order());
    drop(held);
    // Legacy layout (no LOCAL_BACKUP_BIG_DIR): no markers needed, big/ under the dir.
    let legacy = Config::parse(&format!("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=( alpha )\nLOCAL_BACKUP_DIR={}\n", w.dir.join("legacy").display()));
    let ldir = w.dir.join("legacy").display().to_string();
    super::super::pull_locked(&remote, &legacy, &ldir, args(), &sel, &w.rsync, super::super::LockMode::Take(Duration::from_millis(300))).await.unwrap();
    assert!(w.order().iter().any(|l| l.ends_with(&format!("{ldir}/big/devtest-alpha/"))), "{:?}", w.order());
}

/// R30: two due pods, two reaps allowed this run; the first's backup takes 6 minutes, so the
/// second's deciding look is stale by the time it's acted on — it's looked at again (still
/// idle: reaped). (A look that isn't idle stops it: `inv_ticket_iff_all_gates`' stale and
/// active deciding looks.)
#[tokio::test]
async fn a_stale_deciding_look_is_taken_again() {
    for busy in [false] {
        let w = World::new(&format!("reprobe-{busy}"), "REAP_MAX_PER_RUN=2");
        let a = rp("devtest-alpha", "pa", 22001);
        let b = rp("devtest-bravo", "pb", 22002);
        let mut slow = Sim::new("pa", "runpod");
        slow.on_git = OnGit::Slow;
        w.add(a.clone(), slow);
        w.add(b.clone(), Sim::new("pb", "runpod"));
        w.policy(&a, Some(1), None);
        w.policy(&b, None, Some(900));
        for i in 0..4 {
            if i > 0 {
                w.tick(300);
            }
            w.step("observe").await;
        }
        w.tick(300);
        let probes_before = w.calls_to(22002).iter().filter(|k| **k == "probe").count();
        let (_, out) = w.run("apply").await;
        let probes = w.calls_to(22002).iter().filter(|k| **k == "probe").count() - probes_before;
        // The deciding look, the re-look (stale after alpha's 6 minutes), the post-backup look.
        assert_eq!(probes, 3, "busy={busy}: {out}");
        let ids: Vec<String> = w.terminated().into_iter().map(|(_, id)| id).collect();
        assert_eq!(ids, ["pa", "pb"], "expiry first, then idle: {out}");
    }
}
