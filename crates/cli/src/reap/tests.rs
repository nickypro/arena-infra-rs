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
    /// REVIEW: a file in the home changed at this time (the pod clock is the control clock here):
    /// a probe sees it when it runs after it and its scan window starts at or before it.
    file_at: Option<u64>,
    /// REVIEW: probing this pod takes this long — the control clock moves on (a slow probe round).
    probe_secs: u64,
    /// RS2: the probe's sweep-folder scan (`none`/`hit`/…) and other writable mounts.
    uncovered: &'static str,
    mounts: &'static str,
    /// RS6: the home as resolved (`ok`/`link`/`err`).
    home: &'static str,
    /// RS7: processes beyond the image's boot-time three — `(seconds after PID 1, cmdline
    /// cksum, interactive shell, name)` — and whether the list is unreadable.
    procs: Vec<(u64, u32, bool, &'static str)>,
    procs_unreadable: bool,
    /// RS7: a process that starts after the backup's git step (a job launched mid-reap).
    proc_after_git: Option<(u64, u32, bool, &'static str)>,
    /// R2-2: links out of the home, data outside it; RS7: scheduled work, open terminals.
    links: &'static str,
    roots: &'static str,
    sched: &'static str,
    ttys: &'static str,
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
    /// REVIEW: an operator starts `pods replace`/`migrate` of it: `<name>-new` appears listed.
    Twin,
    /// RS8: an operator sets `REAP_ENABLED=0` in the config file.
    Disable,
    /// RS9: the big volume is unmounted (its tier marker gone).
    Unmount,
    /// REVIEW-2: a `migrate copy` (placed on Hetzner) creates `<name>-new` there, and
    /// Hetzner's listing fails at the reap's final re-listing.
    TwinOnFailedProvider,
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
            file_at: None,
            probe_secs: 0,
            uncovered: "none",
            mounts: "clean",
            home: "ok",
            procs: Vec::new(),
            procs_unreadable: false,
            proc_after_git: None,
            links: "none",
            roots: "none",
            sched: "none",
            ttys: "0",
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
    /// The world's directory (its config file, the backup roots).
    dir: PathBuf,
    /// Each probe's host and the (pod-clock) time its file scan started.
    scans: Mutex<Vec<(String, u64)>>,
    /// Each probe's host and the pod creation time it was handed (its last argument).
    created: Mutex<Vec<(String, String)>>,
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
                self.created.lock().unwrap().push((host.to_string(), cmd.rsplit('\'').nth(1).unwrap_or_default().to_string()));
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
                    files: {
                        let scan_from = match since {
                            Since::At(t) => t,
                            Since::Back(b) => (now - 22).saturating_sub(b),
                        };
                        self.scans.lock().unwrap().push((host.to_string(), scan_from));
                        let late_file = sim.file_at.is_some_and(|f| f >= scan_from && f <= now);
                        if sim.files_hit || late_file { "hit" } else { "none" }
                    },
                    sshd,
                    keep: sim.keep,
                    workspace: sim.workspace,
                    repo: sim.repo,
                    uncovered: sim.uncovered,
                    mounts: sim.mounts,
                    home: sim.home,
                    links: sim.links,
                    roots: sim.roots,
                    sched: sim.sched,
                    ttys: sim.ttys,
                    procs: (!sim.procs_unreadable).then(|| {
                        let pid1: u64 = sim.pid1.parse().unwrap_or(0);
                        let after_git = sim.proc_after_git.filter(|_| sim.git_done);
                        [(0, 11, false, "start.sh"), (1, 22, false, "sshd"), (4, 33, false, "jupyter-lab")]
                            .into_iter()
                            .chain(sim.procs.iter().copied())
                            .chain(after_git)
                            .map(|(secs, hash, shell, name)| (pid1 + secs * 100, hash, shell, name.to_string()))
                            .collect()
                    }),
                    ..Default::default()
                };
                if sim.probe_secs > 0 {
                    self.clock.fetch_add(sim.probe_secs, Ordering::SeqCst);
                }
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
                if sim.on_git == OnGit::Twin {
                    for (_, r) in listing.iter_mut() {
                        if let Ok(pods) = r {
                            if let Some(p) = pods.iter().find(|p| p.id == id).cloned() {
                                pods.push(Pod { id: format!("{}-twin", p.id), name: format!("{}-new", p.name), ..p });
                            }
                        }
                    }
                }
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
                    (OnGit::Disable, _) => {
                        let cfg = self.dir.join("config.env");
                        let text = std::fs::read_to_string(&cfg).unwrap();
                        std::fs::write(&cfg, text.replace("REAP_ENABLED=1", "REAP_ENABLED=0")).unwrap();
                    }
                    (OnGit::TwinOnFailedProvider, _) => {
                        for (n, r) in listing.iter_mut() {
                            if n == "hetzner" {
                                *r = Err("hetzner: list timed out".into());
                            }
                        }
                    }
                    (OnGit::Unmount, _) => {
                        std::fs::remove_file(self.dir.join("snap/big/.arena-backup-tier")).unwrap();
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
# What the pod holds now ($d/content), written as rsync does: a temp file renamed over the old.
put() { mkdir -p "$1" && cat "$d/content" > "$1/.work.tmp" 2>/dev/null || echo v1 > "$1/.work.tmp"; mv "$1/.work.tmp" "$1/work.txt"; }
case "$last" in
  */snap/big/*) echo "rsync big $last" >> "$d/calls.log"; printf '%s\n' "$@" > "$d/big_args"; [ -f "$d/hang_big" ] && sleep 5; put "$last"; exit "$(cat "$d/exit_big" 2>/dev/null || echo 0)";;
esac
echo "rsync snapshot $last" >> "$d/calls.log"
echo "Number of regular files transferred: 1"
put "$last"
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
        std::fs::write(dir.join("config.env"), &text).unwrap();
        let cfg = Config::parse(&text).with_source(&dir.join("config.env"));
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
            remote: Arc::new(Remote2 {
                sims: Mutex::new(HashMap::new()),
                listing,
                store,
                clock: clock.clone(),
                log,
                calls: Mutex::new(Vec::new()),
                dir: dir.clone(),
                scans: Mutex::new(Vec::new()),
                created: Mutex::new(Vec::new()),
            }),
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
            config_file: self.cfg.source().map(Path::to_path_buf),
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
    // RS1: the tombstone names the FROZEN copies, never the live per-name dirs.
    let (snap, big) = (tomb.snapshot_dir.as_deref().unwrap(), tomb.big_dir.as_deref().unwrap());
    assert!(snap.contains("/snap/reaped/devtest-alpha/") && snap.ends_with("-runpod-pa/snapshot"), "{tomb:?}");
    assert!(big.contains("/snap/big/reaped/devtest-alpha/") && big.ends_with("-runpod-pa/big"), "{tomb:?}");
    assert_eq!(std::fs::read_to_string(Path::new(big).join("work.txt")).unwrap(), "v1\n");
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
    assert!(
        w.store().read_log().unwrap().iter().any(|l| l.contains("\"row\":\"rehearsed\"") && l.contains("/snap/big/reaped/devtest-alpha/") && l.contains("-rehearsal/big")),
        "a rehearsal freezes its copy too, marked as one"
    );
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
    assert!(s.reaped[&core_reap::key("runpod", &a.id)].big_dir.as_deref().is_some_and(|d| d.contains("/snap/big/reaped/devtest-alpha/")));
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
    let (_, blockers) = super::readiness_rows_for(&Config::parse("ARENA_STATE_DIR=x"), "/nonexistent/rsync", true);
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
    assert!(apply.contains(" pods reap run --apply --yes >> ") && apply.contains("flock -n -o /root/.arena-reap-cron.lock"), "{apply}");
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

// ---------------------------------------------------------------------------------------
// REVIEW (adversarial safety review of the reaper lane, branch `review`). Each test states the
// property that SHOULD hold; those marked "fails today" demonstrate a finding.
// ---------------------------------------------------------------------------------------

/// REVIEW RS1 (fails today): a reap's verified backup lands in the pod NAME's shared, mutable
/// dirs — `<big root>/<name>/` (all files, rsync without `--delete`) and today's
/// `<label>/<name>/` — the very dirs every later pod of that name is pulled into. The participant
/// whose idle pod was reaped comes back; `pods up` fills the free list name with a fresh
/// `devtest-alpha` (new id); its next `pods pull` / `pods backup --pull` overwrites every
/// same-path file (their edited ARENA_materials files, `.git/HEAD`/refs/logs …) with the image's
/// pristine copies — possibly before anyone ran `pods restore --from big`. The tombstone and the
/// "reaped" notification still point there. (The same sharing happens when R5 can't see a
/// same-named twin because that provider's listing failed this run.)
#[tokio::test]
async fn review_rs1_a_reaped_pods_backup_is_not_its_successors_pull_dir() {
    let w = World::new("rv-rs1", "");
    let a = rp("devtest-alpha", "pa", 22001);
    w.add(a.clone(), Sim::new("pa", "runpod"));
    w.policy(&a, None, Some(900));
    run_until_reaped(&w, "apply", 300, 12).await;
    assert_eq!(w.terminated(), [("runpod".to_string(), "pa".to_string())]);
    let tomb = w.state().reaped["runpod:pa"].clone();
    // Where `pods pull` writes a (new) devtest-alpha under the same config.
    let dir = crate::local_backup_dir(&w.cfg);
    let (big_root, _) = crate::pull_roots(&w.cfg, &dir);
    let (wk, d) = crate::resolve_week_day(&w.cfg, None, None).unwrap();
    let successor_big = arena_core::pull::big_dest_in(&big_root, "devtest-alpha");
    let successor_snap = arena_core::pull::local_dest(&dir, &format!("w{wk}d{d}"), "devtest-alpha");
    assert!(
        tomb.big_dir.as_deref() != Some(successor_big.as_str()) && tomb.snapshot_dir.as_deref() != Some(successor_snap.as_str()),
        "the reaped pod's only verified copies are the dirs a same-named successor's pulls overwrite in place: {tomb:?}"
    );
}

/// REVIEW RS2 (fails today): the reap's "verified" copy is the big tier, whose filter is the
/// backup default — it drops every dot-directory but `.git`/`.claude`, `TEMP_FOLDER_FOR_SWEEPS/`
/// (sweep/RLVR run outputs, "deliberately NOT backed up"), `venv/`, `huggingface/`, `hf_cache/`,
/// `models--*/`, `datasets--*/`, … The verify runs with the same filter, so it can't notice, and
/// the probe has no NotCovered fact for any of them (only `/workspace` and a repo outside home).
/// Terminating destroys whatever is only there. (Spec §1 accepts "the existing excludes" — but
/// a backup that drops data on purpose was safe only while nothing terminated pods behind it.)
///
/// Fixed (RS2) as the review allowed: the reap's backup is its own tier
/// ([`PullConfig::reap_tier`]) — dot-directories kept; what it still leaves out is either a
/// documented cache/secret (`REAP_NOT_KEPT`) or vetoed (`REAP_VETOED`: a pod with anything
/// written there since its container started is never reaped — `uncovered_and_other_mounts_veto`).
#[test]
fn review_rs2_the_reaps_verified_copy_keeps_participant_data() {
    use arena_core::pull::{PullConfig, REAP_NOT_KEPT, REAP_VETOED};
    let t = SshTarget::for_host("root", "10.0.0.1", 22001, None);
    let args = arena_core::pull::rsync_args(&t, &PullConfig::reap_tier(), "/big/devtest-alpha/");
    assert!(!args.iter().any(|a| a == "**/.*/"), "dot-directories are kept: {args:?}");
    for d in ["TEMP_FOLDER_FOR_SWEEPS/", "venv/"] {
        assert!(REAP_NOT_KEPT.contains(&d) || REAP_VETOED.contains(&d), "{d} dropped without being documented or vetoed");
    }
    // R2-1: a `huggingface/`/`hf_cache/` folder is copied (only the hub cache is left out).
    for d in ["huggingface/", "hf_cache/"] {
        assert!(!args.iter().any(|a| a == d), "{d} is excluded: {args:?}");
    }
    assert!(REAP_VETOED.contains(&"TEMP_FOLDER_FOR_SWEEPS/"));
    // The verify reads with the same filter as the copy.
    let verify = arena_core::pull::verify_args(&t, &PullConfig::reap_tier(), "/big/devtest-alpha/");
    for a in args.iter().filter(|a| a.starts_with("--exclude") || a.starts_with("--include") || a.starts_with("--filter")) {
        assert!(verify.contains(a), "{a} missing from the verify");
    }
}

/// REVIEW RS4 (fails today): nothing watches the pod between the deciding look and the
/// backup. The deciding look may be up to OBS_FRESH (5 min) old when the backup starts (the
/// probe round of a fleet takes minutes), and the post-backup look only scans files changed
/// since `backup start − 60 s`. A participant who comes back after the idle window, saves a
/// file and leaves inside that gap is not seen: the pod is terminated ~3 min after they used it.
/// (Their file is in the backup — the environment, their tmux/kernels, is gone.)
#[tokio::test]
async fn review_rs4_activity_between_the_deciding_look_and_the_backup_is_unwatched() {
    let w = World::new("rv-rs4", "");
    let a = rp("devtest-alpha", "pa", 22001);
    let b = rp("devtest-bravo", "pb", 22002);
    w.add(a.clone(), Sim::new("pa", "runpod"));
    w.add(b.clone(), Sim::new("pb", "runpod"));
    w.policy(&a, None, Some(900));
    w.policy(&b, None, Some(8 * 3600));
    for i in 0..4 {
        if i > 0 {
            w.tick(300);
        }
        w.step("observe").await;
    }
    w.tick(300);
    let t = w.now();
    // This run's probe round takes a while (bravo's look, after alpha's): alpha's deciding look
    // at t is still "fresh" (< OBS_FRESH) when its backup starts at t + 240.
    w.sim(22002, |s| s.probe_secs = 240);
    // Meanwhile alpha's participant logs in, saves a file at t + 60, and logs out.
    w.sim(22001, |s| s.file_at = Some(t + 60));
    let (_, out) = w.run("apply").await;
    assert!(w.terminated().is_empty(), "reaped ~3 min after its participant saved a file (unwatched gap): {out}");
}

/// REVIEW RS5 (fails today): R5 (a `-old`/`-new` twin listed ⇒ a replace/migrate is running ⇒
/// nothing) is checked before the probe only. The re-listing just before the terminate (R37)
/// re-checks id, name, status and lock — not twins. An operator who starts `pods replace` /
/// `migrate` of the pod during the reap's (possibly hour-long) backup has `<name>-new` listed and
/// is copying the home from this pod: the reaper terminates the source under it.
#[tokio::test]
async fn review_rs5_a_twin_listed_at_the_relist_stops_the_terminate() {
    let (w, _a) = due_world("rv-rs5", "").await;
    w.sim(22001, |s| s.on_git = OnGit::Twin);
    let (_, out) = w.run("apply").await;
    assert!(w.terminated().is_empty(), "terminated while `devtest-alpha-new` (a replace/migrate of it) was listed: {out}");
}

// ---------------------------------------------------------------------------------------
// Safety review fixes (RS1–RS9).
// ---------------------------------------------------------------------------------------

/// RS1: a reaped pod's backup is frozen — a same-named successor, reaped (or pulled) later into
/// the same per-name dirs with different files, leaves the first one's frozen copy
/// byte-identical, and the two reaps' copies are apart.
#[tokio::test]
async fn rs1_a_reaped_pods_frozen_copy_outlives_its_successor() {
    let (w, a) = due_world("rs1-succ", "").await;
    w.put("content", "the first devtest-alpha's work\n");
    w.step("apply").await;
    assert_eq!(w.terminated().len(), 1);
    let first = w.state().reaped[&core_reap::key("runpod", &a.id)].clone();
    let frozen_big = Path::new(first.big_dir.as_deref().unwrap()).join("work.txt");
    let frozen_snap = Path::new(first.snapshot_dir.as_deref().unwrap()).join("work.txt");
    let before = (std::fs::read(&frozen_big).unwrap(), std::fs::read(&frozen_snap).unwrap());
    assert_eq!(before.0, b"the first devtest-alpha's work\n");
    // A new devtest-alpha (another id), with other files, idle and reaped in its turn.
    let b = rp("devtest-alpha", "pa2", 22002);
    w.add(b.clone(), Sim::new("pa2", "runpod"));
    w.policy(&b, None, Some(900));
    w.put("content", "the successor's own\n");
    w.tick(60);
    let runs = run_until_reaped_n(&w, 2, 300, 12).await;
    assert!(runs < 12, "the successor was never reaped");
    let second = w.state().reaped[&core_reap::key("runpod", &b.id)].clone();
    assert_ne!(second.big_dir, first.big_dir);
    assert_eq!(std::fs::read(Path::new(second.big_dir.as_deref().unwrap()).join("work.txt")).unwrap(), b"the successor's own\n");
    // The live per-name dir now holds the successor's — the first's frozen copy didn't move.
    assert_eq!(std::fs::read(w.dir.join("snap/big/devtest-alpha/work.txt")).unwrap(), b"the successor's own\n");
    assert_eq!((std::fs::read(&frozen_big).unwrap(), std::fs::read(&frozen_snap).unwrap()), before);
    // The notification and `reap status` point at the frozen copies.
    assert!(w.notified().iter().any(|l| l.contains(first.big_dir.as_deref().unwrap())), "{:?}", w.notified());
    w.out.lock().unwrap().clear();
    w.reap(&["status"]).await.unwrap();
    let out = w.out.lock().unwrap().join("\n");
    assert!(out.contains("/snap/big/reaped/devtest-alpha/"), "{out}");
}

/// [`run_until_reaped`] until `n` terminates.
async fn run_until_reaped_n(w: &World, n: usize, every: u64, max: usize) -> usize {
    for i in 0..max {
        if i > 0 {
            w.tick(every);
        }
        let (r, out) = w.run("apply").await;
        assert!(exit_code(&r).is_some_and(|c| c == 0 || c == 3), "{r:?}\n{out}");
        if w.terminated().len() >= n {
            return i + 1;
        }
    }
    max
}

/// RS1: a frozen copy that can't be made (its folder already there) or doesn't count equal is
/// no backup — nothing terminated, the operator told.
#[tokio::test]
async fn rs1_no_terminate_without_a_frozen_copy() {
    let (w, _a) = due_world("rs1-freeze", "").await;
    // `reaped/` can't be created: a FILE where its folder goes.
    std::fs::write(w.dir.join("snap/big/reaped"), "in the way").unwrap();
    let (r, out) = w.run("apply").await;
    assert!(w.terminated().is_empty(), "{out}");
    assert_eq!(exit_code(&r), Some(3), "needs attention: {out}");
    assert!(w.notified().iter().any(|l| l.contains("freeze_failed devtest-alpha")), "{:?}", w.notified());
    assert!(w.store().read_log().unwrap().iter().any(|l| l.contains("\"row\":\"freeze_failed\"")));
}

/// RS2: a pod with files written in a sweep folder (which the reap's backup never copies), or
/// data on another writable mount, is never reaped — and a recent notebook in a dot-directory is
/// simply backed up (the reap tier keeps it).
#[tokio::test]
async fn rs2_uncovered_and_other_mounts_veto() {
    for (tag, set) in [("sweep", (|s: &mut Sim| s.uncovered = "hit") as fn(&mut Sim)), ("mount", |s: &mut Sim| s.mounts = "has_data"), ("ws", |s: &mut Sim| s.workspace = "has_data")] {
        let w = World::new(&format!("rs2-{tag}"), "");
        let a = rp("devtest-alpha", "pa", 22001);
        let mut sim = Sim::new("pa", "runpod");
        set(&mut sim);
        w.add(a.clone(), sim);
        w.policy(&a, Some(600), Some(900));
        for _ in 0..10 {
            w.tick(300);
            w.step("apply").await;
        }
        assert!(w.terminated().is_empty(), "{tag}");
        assert_eq!(w.last_row(&a), Some(Row::NotCovered), "{tag}");
        let ends = super::ends_column(&w.cfg, &[a.clone()], w.now(), 0).unwrap();
        assert_eq!(ends[0], "not covered", "{tag}");
    }
}

/// RS4: the deciding look is retaken right before the backup when it's over 60 s old (here:
/// another pod's slow probe in the same round), and the post-backup look scans from exactly
/// where the deciding look's scan began — nothing between the two goes unwatched.
#[tokio::test]
async fn rs4_the_deciding_look_is_fresh_and_the_post_backup_scan_starts_there() {
    for (secs, looks) in [(59, 1), (61, 2)] {
        let w = World::new(&format!("rs4-{secs}"), "");
        let a = rp("devtest-alpha", "pa", 22001);
        let b = rp("devtest-bravo", "pb", 22002);
        w.add(a.clone(), Sim::new("pa", "runpod"));
        w.add(b.clone(), Sim::new("pb", "runpod"));
        w.policy(&a, None, Some(900));
        w.policy(&b, None, Some(8 * 3600));
        for i in 0..4 {
            if i > 0 {
                w.tick(300);
            }
            w.step("observe").await;
        }
        w.tick(300);
        w.sim(22002, |s| s.probe_secs = secs);
        let first = w.calls_to(22001).len();
        let scans_before = w.remote.scans.lock().unwrap().len();
        let (_, out) = w.run("apply").await;
        assert_eq!(w.terminated(), [("runpod".to_string(), "pa".to_string())], "{secs}: {out}");
        let calls: Vec<&str> = w.calls_to(22001)[first..].to_vec();
        let git = calls.iter().position(|k| *k == "git").unwrap();
        assert_eq!(calls[..git].iter().filter(|k| **k == "probe").count(), looks, "{secs} s: {calls:?}");
        let scans: Vec<u64> = w.remote.scans.lock().unwrap()[scans_before..].iter().filter(|(h, _)| *h == host(22001)).map(|(_, t)| *t).collect();
        let (deciding, post) = (scans[looks - 1], *scans.last().unwrap());
        assert_eq!(post, deciding, "{secs} s: the post-backup scan starts where the deciding look's did ({scans:?})");
    }
}

/// RS5: a `-new`/`-old` twin listed at the final relist stops the terminate (the review's test
/// above); and `pods replace` / `migrate` refuse a pod whose reap is in progress (its
/// write-ahead record), whatever this shell's ARENA_STATE_DIR.
#[test]
fn rs5_replace_refuses_a_pod_being_reaped() {
    let w = World::new("rs5-repl", "");
    let a = rp("devtest-alpha", "pa", 22001);
    w.policy(&a, None, Some(900));
    assert_eq!(super::reap_in_progress(&w.cfg, &a, "replace"), None);
    store::write_ahead(&w.store(), "runpod:pa", "run-1", Trigger::Idle, T0).unwrap();
    let why = super::reap_in_progress(&w.cfg, &a, "replace").unwrap();
    assert!(why.starts_with("refusing to replace: the auto-reaper is reaping devtest-alpha") && why.contains("reap hold"), "{why}");
    // Another pod, or no state at all: nothing.
    assert_eq!(super::reap_in_progress(&w.cfg, &rp("devtest-bravo", "pb", 22002), "replace"), None);
    assert_eq!(super::reap_in_progress(&Config::parse("MACHINE_NAME_PREFIX=devtest"), &a, "migrate"), None);
}

/// RS7: a dormant `nohup python train.py` (no CPU, no files, no session) is a process of its
/// own — never reaped; a boot-time service restarted later, a login shell or tmux's server
/// isn't; an unreadable process list is "can't tell".
#[tokio::test]
async fn rs7_a_dormant_process_vetoes_and_boot_services_dont() {
    let cases: [(&str, Vec<(u64, u32, bool, &'static str)>, bool, Option<Row>); 5] = [
        ("dormant job", vec![(3600, 901, false, "python")], false, Some(Row::Active)),
        ("restarted jupyter", vec![(3600, 33, false, "jupyter-lab")], false, None),
        ("login shell", vec![(3600, 77, true, "bash")], false, None),
        ("tmux server", vec![(3600, 78, false, "tmux: server")], false, None),
        ("unreadable", vec![], true, Some(Row::Unknown)),
    ];
    for (case, procs, unreadable, want) in cases {
        let w = World::new(&format!("rs7-{}", case.replace(' ', "-")), "");
        let a = rp("devtest-alpha", "pa", 22001);
        let mut sim = Sim::new("pa", "runpod");
        sim.procs = procs;
        sim.procs_unreadable = unreadable;
        w.add(a.clone(), sim);
        w.policy(&a, None, Some(900));
        let runs = run_until_reaped(&w, "apply", 300, 10).await;
        match want {
            Some(row) => {
                assert!(w.terminated().is_empty(), "{case}");
                assert_eq!(w.last_row(&a), Some(row), "{case}");
            }
            None => assert!(runs < 10 && w.terminated().len() == 1, "{case}: never reaped"),
        }
    }
    // A process started during the backup: the post-backup look stops the terminate.
    let (w, _a) = due_world("rs7-late", "").await;
    w.sim(22001, |s| s.proc_after_git = Some((7200, 902, false, "python")));
    w.step("apply").await;
    assert!(w.terminated().is_empty());
    assert!(w.store().read_log().unwrap().iter().any(|l| l.contains("post_backup_not_quiet") && l.contains("process")));
}

/// RS8: `REAP_ENABLED=0` written to the config file during the backup stops the terminate
/// (`disabled_mid_run`); the write-ahead is cleared and no cap is lost beyond the attempt.
#[tokio::test]
async fn rs8_reap_enabled_off_mid_run_stops_the_terminate() {
    let (w, a) = due_world("rs8-off", "").await;
    w.sim(22001, |s| s.on_git = OnGit::Disable);
    let (_, out) = w.run("apply").await;
    assert!(w.terminated().is_empty(), "{out}");
    assert!(w.store().read_log().unwrap().iter().any(|l| l.contains("\"row\":\"disabled_mid_run\"")));
    assert!(w.state().pods[&core_reap::key("runpod", &a.id)].terminating.is_none());
    // An unreadable config file at that point is "off" too.
    let (w, _a) = due_world("rs8-gone", "").await;
    let mut env = w.env();
    env.config_file = Some(w.dir.join("no-such-config.env"));
    let argv = ["arena", "pods", "reap", "run", "--apply"].map(String::from);
    let super::super::Cmd::Pods(super::super::PodCmd::Reap(cmd)) = super::super::Cli::try_parse_from(argv).unwrap().cmd else { panic!() };
    let _ = handle_reap(cmd, &env).await;
    assert!(w.terminated().is_empty());
    assert!(w.store().read_log().unwrap().iter().any(|l| l.contains("\"row\":\"disabled_mid_run\"")));
}

/// RS9: the backup volume unmounted after the run's check (here: during the git push) — the
/// re-check right before the first write stops the reap: no rsync, nothing terminated.
#[tokio::test]
async fn rs9_a_volume_unmounted_before_the_writes_stops_the_reap() {
    let (w, _a) = due_world("rs9-unmount", "").await;
    w.sim(22001, |s| s.on_git = OnGit::Unmount);
    let (r, out) = w.run("apply").await;
    assert!(w.terminated().is_empty(), "{out}");
    assert_eq!(exit_code(&r), Some(3), "{out}");
    let order = w.order();
    let git = order.iter().position(|l| l.starts_with("remote git")).unwrap();
    assert!(!order[git..].iter().any(|l| l.starts_with("rsync")), "{order:#?}");
    assert!(w.store().read_log().unwrap().iter().any(|l| l.contains("\"row\":\"dest_unhealthy\"")));
}

/// RS3: every state-changing reap command says which state file it wrote.
#[tokio::test]
async fn rs3_state_changes_name_the_state_file() {
    let w = World::new("rs3-path", "");
    let a = rp("devtest-alpha", "pa", 22001);
    w.add(a.clone(), Sim::new("pa", "runpod"));
    *w.fleet.strict.lock().unwrap() = false;
    let (state, hold) = (w.store().state_path().display().to_string(), w.store().hold_path().display().to_string());
    for (args, path) in [
        (vec!["set", "devtest-alpha", "--idle-after", "8h"], &state),
        (vec!["pin", "devtest-alpha"], &state),
        (vec!["unpin", "devtest-alpha"], &state),
        (vec!["hold", "--reason", "drill"], &hold),
        (vec!["release"], &hold),
        (vec!["clear", "devtest-alpha"], &state),
    ] {
        w.out.lock().unwrap().clear();
        w.reap(&args).await.unwrap();
        let out = w.out.lock().unwrap().join("\n");
        assert!(out.contains(&format!("(state: {path})")), "{args:?}: {out}");
    }
}

/// RS2: the reap's big tier is its own (`reap_tier`): the home's dot-directories are copied, the
/// sweep folder isn't (and vetoes instead).
#[tokio::test]
async fn rs2_the_reaps_big_tier_keeps_dot_dirs() {
    let (w, _a) = due_world("rs2-tier", "").await;
    w.step("apply").await;
    assert_eq!(w.terminated().len(), 1);
    let args = std::fs::read_to_string(w.dir.join("big_args")).unwrap();
    assert!(!args.contains("**/.*/"), "the reap's copy drops dot-directories:\n{args}");
    assert!(args.contains("TEMP_FOLDER_FOR_SWEEPS/") && args.contains(".cache/"), "{args}");
}

// ---------------------------------------------------------------------------------------
// Review RR (robustness / regressions) — each test states the behaviour that should hold and
// fails on 0e1a148.
// ---------------------------------------------------------------------------------------

/// RR: `up/create --expire-in` on a pod the reaper never acts on — Vast, or RunPod on REST v1
/// (the default `RUNPOD_API`, which lists `locked: None`) — is recorded without a word (`reap
/// set` refuses the very same pod), and `pods list`'s ENDS column then counts down to the end
/// time and shows `OVERDUE …` past it, as if the pod were going to be reaped. It never is (R3 /
/// R8 every run): the operator replaced `at` + destroy with `up --expire-in` and the pod bills on.
#[test]
fn review_rr_create_flags_on_a_never_reaped_provider_are_silent_and_look_live() {
    let w = World::new("rr-unsup", "");
    let vast = Pod { provider: "vast".into(), locked: None, ..rp("devtest-alpha", "77", 22001) };
    let v1 = Pod { locked: None, ..rp("devtest-bravo", "pv1", 22002) };
    let naming = Naming::from_config(&w.cfg);
    assert!(never_reaped(&vast, &[vast.clone()], &naming, true).is_some(), "reap set refuses Vast");
    assert!(never_reaped(&v1, &[v1.clone()], &naming, false).is_some(), "reap set refuses RunPod v1");
    // `up/create --expire-in 8h` accepts both and records them (nothing in resolve looks at the
    // provider; the create path prints nothing about it).
    let (policy, store) = resolve_create_policy(&w.cfg, Some("8h"), None, None, T0, &|_| None).unwrap().unwrap();
    record_created(&store, &vast, policy, T0, "up");
    record_created(&store, &v1, policy, T0, "up");
    let ends = ends_column(&w.cfg, &[vast.clone(), v1.clone()], T0 + 9 * 3600, 0).unwrap();
    for (pod, label) in [&vast, &v1].iter().zip(&ends) {
        assert!(
            !label.starts_with("OVERDUE") && !label.contains("(in "),
            "{} ({}): ENDS says `{label}` for a pod the reaper will never terminate",
            pod.name,
            pod.provider
        );
    }
}

/// A provider whose first `list_pods` waits until released — to interleave two `reap set`s.
struct Gated<'a> {
    inner: &'a Fleet,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait]
impl Provider for Gated<'_> {
    fn name(&self) -> &'static str {
        "runpod"
    }
    fn describe(&self, _spec: &PodSpec) -> String {
        String::new()
    }
    async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
        self.entered.notify_one();
        self.release.notified().await;
        self.inner.list_pods().await
    }
    async fn create_pod(&self, _spec: &PodSpec) -> arena_core::Result<Pod> {
        panic!("create")
    }
    async fn stop_pod(&self, _id: &str) -> arena_core::Result<()> {
        panic!("stop")
    }
    async fn restart_pod(&self, _id: &str) -> arena_core::Result<()> {
        panic!("restart")
    }
    async fn terminate_pod(&self, _id: &str) -> arena_core::Result<()> {
        panic!("terminate")
    }
}

/// RR: two `reap set`s from two terminals. `handle_set` loads the state WITHOUT the state lock,
/// computes every field from that copy (`set_values(old)`), lists the fleet, and only then
/// writes inside `store.update` — assigning `expires_at`/`idle_after_secs` from the stale copy.
/// So `reap set A --idle-after 8h` started just before `reap set A --extend 2h` finishes puts the
/// OLD end time back: the operator's extension is silently lost (and `first_due_seen` reset).
#[tokio::test]
async fn review_rr_concurrent_reap_set_keeps_both_changes() {
    let w = World::new("rr-race", "");
    *w.fleet.strict.lock().unwrap() = false;
    let a = rp("devtest-alpha", "pa", 22001);
    w.add(a.clone(), Sim::new("pa", "runpod"));
    w.policy(&a, Some(3 * 3600), None);
    let gated = Gated { inner: &w.fleet, entered: tokio::sync::Notify::new(), release: tokio::sync::Notify::new() };
    let mut slow_env = w.env();
    slow_env.provider = &gated;
    let argv = ["arena", "pods", "reap", "set", "devtest-alpha", "--idle-after", "8h"].map(String::from);
    let super::super::Cmd::Pods(super::super::PodCmd::Reap(slow_cmd)) = super::super::Cli::try_parse_from(argv).unwrap().cmd else { panic!() };
    let slow = handle_reap(slow_cmd, &slow_env);
    let fast = async {
        gated.entered.notified().await; // the slow `set` has loaded the state and is listing
        w.reap(&["set", "devtest-alpha", "--extend", "2h"]).await.unwrap();
        gated.release.notify_one();
    };
    let (r, ()) = tokio::join!(slow, fast);
    r.unwrap();
    let p = &w.state().pods["runpod:pa"];
    assert_eq!(p.idle_after_secs, Some(8 * 3600));
    assert_eq!(p.expires_at, Some(T0 + 5 * 3600), "the --extend 2h made meanwhile was overwritten with the stale end time");
}

/// RR: `pods pull` (and `pods backup`, which takes them the same way) holds EVERY selected pod's
/// backup lock until the whole fleet run ends — not until that pod's own rsyncs end. One slow
/// pod (up to BACKUP_TIMEOUT_SECS) therefore keeps every other pod locked: each reaper probe in
/// that time is `BackupBusy` (no observation — idle streaks break past MAX_GAP), and an
/// operator's `pods pull <other pod>` waits 120 s and FAILS as "busy", where before it ran.
#[tokio::test]
async fn review_rr_a_pull_releases_each_pods_lock_when_that_pod_is_done() {
    use std::os::unix::fs::PermissionsExt;
    let w = World::new("rr-hold", "");
    let snap = w.dir.join("bk");
    std::fs::create_dir_all(&snap).unwrap();
    let mark = w.dir.join("alpha.done");
    let rsync = w.dir.join("rsync-slow-bravo");
    std::fs::write(
        &rsync,
        format!(
            "#!/bin/sh\ncase \"$1\" in --version) echo rsync; exit 0;; esac\nfor a in \"$@\"; do last=\"$a\"; done\n\
             case \"$last\" in *devtest-bravo*) sleep 4;; *big/devtest-alpha*) touch '{}';; esac\n\
             echo 'Number of regular files transferred: 1'\nexit 0\n",
            mark.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&rsync, std::fs::Permissions::from_mode(0o755)).unwrap();
    let cfg = Config::parse(&format!("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=( alpha bravo )\nLOCAL_BACKUP_DIR={}\n", snap.display()));
    let sel = super::super::Selected { pods: vec![rp("devtest-alpha", "pa", 22001), rp("devtest-bravo", "pb", 22002)], named: true };
    let remote: Arc<dyn Remote> = Arc::new(arena_core::remote::FakeRemote::new());
    let dir = snap.display().to_string();
    let args = super::super::PullRun { label: Some("w1d1".into()), max_size: None, remote_path: None, no_git: false, no_big: false, dry_run: false, yes: true };
    let rs = rsync.display().to_string();
    let pull = super::super::pull_locked(&remote, &cfg, &dir, args, &sel, &rs, super::super::LockMode::Take(Duration::from_millis(300)));
    let check = async {
        for _ in 0..100 {
            if mark.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(mark.exists(), "alpha's big tier never ran");
        tokio::time::sleep(Duration::from_millis(1000)).await; // alpha's jobs are all done; bravo still sleeping
        let f = arena_core::pull::open_pod_lock(&dir, "devtest-alpha").unwrap();
        arena_core::pull::try_lock(&f).unwrap()
    };
    let (r, alpha_free) = tokio::join!(pull, check);
    r.unwrap();
    assert!(alpha_free, "alpha's backup was finished, yet its lock stayed held while bravo's rsync ran");
}

/// RR: the per-pod lock is opened under `<LOCAL_BACKUP_DIR>/.arena-locks` with a RECURSIVE
/// 0700 create — so on a box where LOCAL_BACKUP_DIR doesn't exist yet, the first `pods pull`/
/// `pods backup` (even the git-only `--no-pull` cron, which never touched that directory before)
/// creates LOCAL_BACKUP_DIR and every missing parent as 0700 (root's cron: root-only), where the
/// pull used to create it with the umask's mode — another user can't then read or restore the
/// backups.
#[tokio::test]
async fn review_rr_taking_the_locks_doesnt_make_the_backup_dir_private() {
    use std::os::unix::fs::PermissionsExt;
    let w = World::new("rr-mode", "");
    let base = w.dir.join("fresh").join("backup");
    let reference = w.dir.join("reference");
    std::fs::create_dir_all(&reference).unwrap(); // what the pull's own create_dir_all gives
    let locks = super::super::take_pod_locks(&base.display().to_string(), &["devtest-alpha".to_string()], Duration::from_millis(100)).await;
    drop(locks);
    // Fixed (RR5) as the coordinator set it: the locks never create LOCAL_BACKUP_DIR or a parent
    // of it at all — a run with it missing goes as before (no locks), the pull creating its
    // destination with the umask's mode as it always did.
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert!(!base.exists() && !w.dir.join("fresh").exists(), "the lock created LOCAL_BACKUP_DIR (or its parent)");
    std::fs::create_dir_all(&base).unwrap();
    assert_eq!(mode(&base), mode(&reference));
}

/// RR: `config check` for someone who never enables the reaper (REAP_ENABLED unset) now prints
/// a block of red ✗ rows (state dir, RUNPOD_API, tier markers, …) that aren't failures for them.
#[test]
fn review_rr_config_check_has_no_red_rows_when_the_reaper_is_off() {
    let cfg = Config::parse("MACHINE_NAME_PREFIX=devtest\nLOCAL_BACKUP_DIR=/srv/arena-backup\nARENA_START_DATE=2026-10-05\n");
    let (lines, blockers) = readiness_rows(&cfg, "rsync");
    let crosses: Vec<&String> = lines.iter().filter(|l| l.contains('✗')).collect();
    assert!(!blockers.is_empty(), "there are gaps (they only count when enabled)");
    assert!(crosses.is_empty(), "REAP_ENABLED unset, yet config check shows ✗ rows: {crosses:#?}");
}

/// RR (regression for users who never enable the reaper): two listed pods that share a name —
/// a duplicate `up`, a stray stopped twin — lock each other out. The lock is per NAME, and
/// `take_pod_locks` opens it once per selected pod: the run's second open of the same file is
/// refused by its own first (flock is per open file description), so after the full 120 s wait
/// the name is "busy", and `pods backup` / `pods pull` then drop EVERY pod of that name — no
/// git push, no rsync, a failed run — on every */15 tick (and every other pod's backup waits
/// those 120 s first). Before the locks, both pods were backed up.
#[tokio::test]
async fn review_rr_two_pods_sharing_a_name_are_still_backed_up() {
    let w = World::new("rr-dup", "");
    let snap = w.dir.join("bk");
    std::fs::create_dir_all(&snap).unwrap();
    let cfg = Config::parse(&format!("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=( alpha )\nLOCAL_BACKUP_DIR={}\n", snap.display()));
    let sel = super::super::Selected { pods: vec![rp("devtest-alpha", "pa", 22001), rp("devtest-alpha", "pa2", 22003)], named: false };
    let remote: Arc<dyn Remote> = Arc::new(arena_core::remote::FakeRemote::new());
    let dir = snap.display().to_string();
    let args = super::super::PullRun { label: Some("w1d1".into()), max_size: None, remote_path: None, no_git: false, no_big: false, dry_run: false, yes: true };
    let r = super::super::pull_locked(&remote, &cfg, &dir, args, &sel, &w.rsync, super::super::LockMode::Take(Duration::from_millis(300))).await;
    let rsyncs = w.order().iter().filter(|l| l.starts_with("rsync")).count();
    assert!(r.is_ok() && rsyncs > 0, "nothing of devtest-alpha was backed up ({rsyncs} rsyncs): {r:?}");
}

/// RR: the reap cron line is `flock -n LOCK arena …` WITHOUT `-o`, so the locked fd that
/// flock(1) hands to arena is inherited by everything arena spawns (ssh, the reap's rsyncs,
/// REAP_NOTIFY_CMD and whatever it backgrounds). If arena dies (killed, OOM) while one of them
/// lives on, that orphan keeps `.arena-reap-cron.lock` and every later tick exits at once,
/// silently (`flock -n` prints nothing) — the reaper just stops. The backup line uses
/// `flock -n -o` for exactly this reason ("a stray rsync outliving an arena that was itself
/// killed would otherwise keep every later tick skipping").
#[test]
fn review_rr_reap_cron_lock_is_not_inherited_by_arenas_children() {
    let line = arena_core::reap::render::cron_line(true, "/usr/local/bin/arena", "/root/c.env", "/root", &[]);
    assert!(line.contains("flock -n -o "), "{line}");
}

/// RR: `pods reap run --json` isn't always JSON on stdout: a run that finds another in progress
/// (the cron's, four times an hour) prints a plain sentence instead and exits 0 — and after a
/// reap the proxy sync's report line (`report_sync`, println) lands on stdout before the JSON.
#[tokio::test]
async fn review_rr_reap_run_json_is_always_json() {
    let w = World::new("rr-json", "");
    let lock = w.store().try_run_lock().unwrap().unwrap();
    w.out.lock().unwrap().clear();
    let r = w.reap(&["run", "--json"]).await;
    let out = w.out.lock().unwrap().join("\n");
    drop(lock);
    assert!(serde_json::from_str::<serde_json::Value>(&out).is_ok(), "`reap run --json` printed non-JSON: {out:?} ({r:?})");
}

// ---------------------------------------------------------------------------------------
// Robustness review fixes (RR1–RR15), beyond the review's own tests above.
// ---------------------------------------------------------------------------------------

/// RR1/RR3: a run's locks — once per name, released per piece of work, idle ones at once.
#[tokio::test]
async fn rr_pod_locks_once_per_name_released_per_pod() {
    let w = World::new("rr-locks", "");
    let dir = w.dir.join("bk");
    std::fs::create_dir_all(&dir).unwrap();
    let d = dir.display().to_string();
    let names = ["devtest-alpha", "devtest-alpha", "devtest-bravo", "devtest-charlie"].map(String::from);
    let mut locks = super::super::take_pod_locks(&d, &names, Duration::from_millis(200)).await;
    assert!(locks.busy.is_empty(), "a run never waits on its own lock: {:?}", locks.busy);
    for n in ["devtest-alpha", "devtest-alpha", "devtest-bravo"] {
        locks.expect(n);
    }
    locks.release_idle();
    let free = |n: &str| arena_core::pull::try_lock(&arena_core::pull::open_pod_lock(&d, n).unwrap()).unwrap();
    assert!(free("devtest-charlie"), "no work under it: released at once");
    assert!(!free("devtest-alpha") && !free("devtest-bravo"));
    locks.done("devtest-alpha");
    assert!(locks.holds("devtest-alpha") && !free("devtest-alpha"), "the other alpha pod still works under it");
    locks.done("devtest-alpha");
    assert!(free("devtest-alpha"), "the last alpha job ended");
    assert!(!free("devtest-bravo"));
    drop(locks);
    assert!(free("devtest-bravo"));
    // Another process's hold (here: an open lock of our own, outside the run) is "busy".
    let other = arena_core::pull::open_pod_lock(&d, "devtest-bravo").unwrap();
    assert!(arena_core::pull::try_lock(&other).unwrap());
    let locks = super::super::take_pod_locks(&d, &names, Duration::from_millis(200)).await;
    assert_eq!(locks.busy, ["devtest-bravo"]);
}

/// RR5: the locks never create LOCAL_BACKUP_DIR (a run with it missing goes as before, no
/// locks, no warning); inside an existing one only `.arena-locks`, 0700.
#[tokio::test]
async fn rr_locks_make_only_their_own_folder() {
    use std::os::unix::fs::PermissionsExt;
    let w = World::new("rr-lockdir", "");
    let missing = w.dir.join("not-yet").join("backup");
    let locks = super::super::take_pod_locks(&missing.display().to_string(), &["devtest-alpha".to_string()], Duration::from_millis(50)).await;
    assert!(locks.held.is_empty() && locks.busy.is_empty());
    assert!(!w.dir.join("not-yet").exists(), "the lock created the backup dir's parent");
    assert!(arena_core::pull::open_pod_lock(&missing.display().to_string(), "devtest-alpha").is_err());
    let base = w.dir.join("bk2");
    std::fs::create_dir_all(&base).unwrap();
    let before = std::fs::metadata(&base).unwrap().permissions().mode();
    drop(arena_core::pull::open_pod_lock(&base.display().to_string(), "devtest-alpha").unwrap());
    assert_eq!(std::fs::metadata(&base).unwrap().permissions().mode(), before, "the backup dir's mode is its own");
    assert_eq!(std::fs::metadata(base.join(".arena-locks")).unwrap().permissions().mode() & 0o777, 0o700);
}

/// RR2: `up/create --expire-*/--idle-after` refuse a provider the reaper never acts on — before
/// anything is created — with `reap set`'s words; RunPod v2 and Hetzner are fine.
#[test]
fn rr_create_flags_refuse_never_reaped_providers() {
    let v1 = Config::parse("RUNPOD_API=v1");
    let v2 = Config::parse("RUNPOD_API=v2");
    let r = super::create_flags_refusal(&v1, "runpod").unwrap();
    assert!(r.contains("RunPod REST v1") && r.contains("only acts on RunPod (RUNPOD_API=v2) and Hetzner") && r.contains("nothing was created"), "{r}");
    assert!(super::create_flags_refusal(&v2, "vast").unwrap().contains("pods on vast"));
    assert_eq!(super::create_flags_refusal(&v2, "runpod"), None);
    assert_eq!(super::create_flags_refusal(&v1, "hetzner"), None);
    // ENDS: a record made inert by a rename outside arena, or a shared name, says so too.
    let w = World::new("rr-inert", "");
    let a = rp("devtest-alpha", "pa", 22001);
    w.policy(&a, Some(3600), None);
    let renamed = Pod { name: "devtest-bravo".into(), ..a.clone() };
    let ends = super::ends_column(&w.cfg, &[renamed], T0 + 7200, 0).unwrap();
    assert_eq!(ends[0], "not reapable (renamed outside arena)");
    let twin = rp("devtest-alpha", "pz", 22009);
    let ends = super::ends_column(&w.cfg, &[a.clone(), twin], T0 + 7200, 0).unwrap();
    assert_eq!(ends[0], "not reapable (name shared)");
    let ends = super::ends_column(&w.cfg, &[a], T0 + 7200, 0).unwrap();
    assert!(ends[0].starts_with("OVERDUE"), "{ends:?}");
}

/// RR8: the reaper off — one neutral row, no outside command, a ✗ only for a REAP_* value set
/// and invalid; full rows once it's on or its cron line is in. Merge of wave 7 ("✗ means fail"
/// in `config check`): a gap is ✗ only while REAP_ENABLED=1 (then it fails the check), ⚠ while
/// the reaper only observes.
#[test]
fn rr_config_check_reaper_rows_off_and_on() {
    let off = Config::parse("MACHINE_NAME_PREFIX=devtest\n");
    let (lines, blockers) = readiness_rows(&off, "/nonexistent/rsync");
    assert_eq!(lines.len(), 1, "{lines:#?}");
    assert!(lines[0].contains("· reaper") && lines[0].contains("off (opt-in)") && lines[0].contains("thing(s) to set up"), "{lines:?}");
    assert!(!blockers.contains(&"rsync".to_string()), "no outside command when off: {blockers:?}");
    let typo = Config::parse("MACHINE_NAME_PREFIX=devtest\nREAP_MAX_PER_DAY=500\n");
    let (lines, _) = readiness_rows(&typo, "/nonexistent/rsync");
    assert!(lines.iter().any(|l| l.contains('✗') && l.contains("REAP_* settings")), "{lines:#?}");
    assert_eq!(lines.iter().filter(|l| l.contains('✗')).count(), 1, "{lines:#?}");
    let (lines, blockers) = super::readiness_rows_for(&off, "/nonexistent/rsync", true);
    assert!(lines.len() > 5 && blockers.contains(&"rsync".to_string()), "{lines:#?}");
    assert!(lines.iter().any(|l| l.contains('⚠')) && !lines.iter().any(|l| l.contains('✗')), "observe only: ⚠, never ✗: {lines:#?}");
    let on = Config::parse("MACHINE_NAME_PREFIX=devtest\nREAP_ENABLED=1\n");
    let (lines, blockers) = super::readiness_rows_for(&on, "/nonexistent/rsync", true);
    assert!(lines.iter().any(|l| l.contains('✗') && l.contains("rsync")) && blockers.contains(&"rsync".to_string()), "{lines:#?}");
}

/// RR9: `--json` errors are one JSON document too.
#[tokio::test]
async fn rr_json_errors_are_json() {
    let w = World::new("rr-json-err", "");
    std::fs::create_dir_all(w.store().dir.clone()).unwrap();
    std::fs::write(w.store().state_path(), "{not json").unwrap();
    for args in [vec!["run", "--json"], vec!["status", "--json"]] {
        w.out.lock().unwrap().clear();
        let r = w.reap(&args).await;
        let out = w.out.lock().unwrap().join("\n");
        assert!(r.is_err(), "{args:?}");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{args:?}: {e}: {out}"));
        assert!(v["error"].as_str().is_some_and(|e| e.contains("reap.json")), "{args:?}: {v}");
    }
    // The log, as one array.
    let w = World::new("rr-json-log", "");
    let a = rp("devtest-alpha", "pa", 22001);
    w.add(a.clone(), Sim::new("pa", "runpod"));
    w.policy(&a, None, Some(900));
    w.step("observe").await;
    w.out.lock().unwrap().clear();
    w.reap(&["log", "--json"]).await.unwrap();
    let out = w.out.lock().unwrap().join("\n");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert!(v.as_array().is_some_and(|a| !a.is_empty()), "{v}");
}

/// RR11: the reap notice is (re)written on a pod with a policy — what `up` (after setup),
/// `pods setup` and a restart's re-provision call; a pod with none gets nothing.
#[tokio::test]
async fn rr_notice_reasserted_where_a_policy_is() {
    let w = World::new("rr-notice", "");
    let a = rp("devtest-alpha", "pa", 22001);
    let b = rp("devtest-bravo", "pb", 22002);
    w.add(a.clone(), Sim::new("pa", "runpod"));
    w.add(b.clone(), Sim::new("pb", "runpod"));
    w.policy(&a, Some(3600), None);
    super::reassert_notices(w.remote.as_ref(), &w.cfg, &[a.clone(), b.clone()]).await;
    assert_eq!(w.calls_to(22001), ["notice"]);
    assert!(w.calls_to(22002).is_empty());
    let t = SshTarget::from_pod(&b, &w.cfg).unwrap();
    assert_eq!(super::reassert_notice(w.remote.as_ref(), &w.cfg, &b, &t).await, Ok(false));
}

/// RR13: a `reap.json` that can't be read doesn't stop `reap hold` or `release` (the hold has
/// its own file); a run still refuses to act; the error names the recovery.
#[tokio::test]
async fn rr_hold_and_release_work_with_a_corrupt_state() {
    let (w, _a) = due_world("rr-corrupt", "").await;
    let bad = "{\"version\": 99}";
    std::fs::write(w.store().state_path(), bad).unwrap();
    w.reap(&["hold", "--reason", "corrupt state drill"]).await.unwrap();
    assert_eq!(w.store().read_hold().unwrap().map(|h| h.reason), Some("corrupt state drill".to_string()));
    let (r, out) = w.run("apply").await;
    assert!(w.terminated().is_empty(), "{out}");
    let e = format!("{:#}", r.unwrap_err());
    assert!(e.contains(".bad-") && e.contains("hold`/`release` still work"), "{e}");
    // `up/create --expire-in` refuses before creating anything, naming the same recovery.
    let e = format!("{:#}", resolve_create_policy(&w.cfg, Some("8h"), None, None, T0, &|_| None).unwrap_err());
    assert!(e.contains("before creating pods with a policy") && e.contains(".bad-"), "{e}");
    w.reap(&["release"]).await.unwrap();
    assert_eq!(w.store().read_hold().unwrap(), None);
    assert_eq!(std::fs::read_to_string(w.store().state_path()).unwrap(), bad, "left untouched");
    // A hold file that can't be read: a run doesn't act; `release` offers to remove it.
    std::fs::write(w.store().state_path(), w.store().load().map(|s| s.to_json()).unwrap_or_else(|_| ReapState::default().to_json())).unwrap();
    std::fs::write(w.store().hold_path(), "garbage").unwrap();
    assert!(w.store().load().is_err(), "an unreadable hold is never \"no hold\"");
    w.reap(&["release"]).await.unwrap();
    assert!(!w.store().hold_path().exists());
}

// ---------------------------------------------------------------------------------------
// Second review (on 8464ea3) — each test fails on 8464ea3 unless it says otherwise.
// ---------------------------------------------------------------------------------------

/// REVIEW-2 R2-3: a freeze that fails (RS1's `freeze_failed`) has no back-off, unlike a failed
/// git push or rsync (6 h): every tick takes a new write-ahead (spending `REAP_MAX_PER_DAY`),
/// runs the pod's whole backup again (both tiers + the verify, up to BACKUP_TIMEOUT_SECS of
/// transfer) and tries another freeze — each partial one left under `reaped/`, which nothing
/// ever cleans. Here: `reaped/` blocked, three ticks 15 min apart.
#[tokio::test]
async fn review2_r2_3_a_failed_freeze_is_retried_every_tick() {
    let (w, a) = due_world("rv2-freeze-retry", "").await;
    std::fs::write(w.dir.join("snap/big/reaped"), "in the way").unwrap();
    for i in 0..3 {
        if i > 0 {
            w.tick(900);
        }
        w.step("apply").await;
    }
    assert!(w.terminated().is_empty());
    let bigs = w.order().iter().filter(|l| l.starts_with("rsync big")).count();
    let attempts = w.state().attempts.len();
    let retry = w.state().pods[&core_reap::key("runpod", &a.id)].retry_after;
    assert!(
        bigs <= 1 && attempts <= 1,
        "a persistent freeze failure re-ran the full reap backup {bigs}x and spent {attempts} of the daily cap in 30 min (retry_after {retry:?})"
    );
}

/// REVIEW-2 R2-4: `pods replace` / `migrate copy` / `cutover` refuse a pod whose reap is in
/// progress only when they can READ the reaper's state: an operator whose user can't (root's
/// cron owns `$ARENA_STATE_DIR`, 0700/0600; a non-root operator shell) gets no refusal —
/// `reap_in_progress` reads "can't read" as "no reap", although the reaper (root) can act.
#[test]
fn review2_r2_4_reap_in_progress_fails_open_on_an_unreadable_state() {
    use std::os::unix::fs::PermissionsExt;
    let uid = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("Uid:")).and_then(|l| l.split_whitespace().nth(1)).map(String::from));
    if uid.as_deref() == Some("0") {
        eprintln!("running as root: permissions don't stop reads — skipping");
        return;
    }
    let w = World::new("rv2-inprog", "");
    let a = rp("devtest-alpha", "pa", 22001);
    w.policy(&a, None, Some(900));
    store::write_ahead(&w.store(), "runpod:pa", "run-1", Trigger::Idle, T0).unwrap();
    assert!(super::reap_in_progress(&w.cfg, &a, "replace").is_some(), "readable: refused");
    // The state as another user sees root's 0700 state dir.
    let state_dir = w.dir.join("state");
    std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o000)).unwrap();
    let seen = super::reap_in_progress(&w.cfg, &a, "replace");
    std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(seen.is_some(), "a reap in progress the operator's user can't read about: replace goes ahead");
}

/// REVIEW-2 R2-5: the final re-listing's R5 check (RS5) looks only at the providers that
/// listed fine. A `migrate copy` placed on another provider creates `<name>-new` THERE; when
/// that provider's listing fails at the relist, the twin can't be seen — "can't tell" — yet
/// the terminate goes ahead (only the pod's own provider must have listed).
#[tokio::test]
async fn review2_r2_5_a_failed_listing_elsewhere_at_the_relist_is_not_a_clean_r5() {
    let (w, _a) = due_world("rv2-relist-other", "").await;
    w.sim(22001, |s| s.on_git = OnGit::TwinOnFailedProvider);
    let (_, out) = w.run("apply").await;
    assert!(w.terminated().is_empty(), "terminated although a provider that may hold its -new twin didn't list: {out}");
}

// ---------------------------------------------------------------------------------------
// Third review fixes (R2-1 … R2-5, RS7 a/b/c).
// ---------------------------------------------------------------------------------------

/// R2-1/R2-2: data the reap's backup never reads — a link out of the home, a file written
/// outside it since the pod's creation, a sweep folder holding anything — vetoes (`not
/// covered`), for expiry and idle alike; each look hands the probe the pod's listed creation.
#[tokio::test]
async fn r2_outside_data_links_and_sweeps_veto() {
    for (tag, set) in [
        ("links", (|s: &mut Sim| s.links = "hit") as fn(&mut Sim)),
        ("roots", |s: &mut Sim| s.roots = "hit"),
        ("sweep", |s: &mut Sim| s.uncovered = "hit"),
    ] {
        let w = World::new(&format!("r2-{tag}"), "");
        let a = Pod { created_at: Some(T0 - 86_400), ..rp("devtest-alpha", "pa", 22001) };
        let mut sim = Sim::new("pa", "runpod");
        set(&mut sim);
        w.add(a.clone(), sim);
        w.policy(&a, Some(600), Some(900));
        for _ in 0..8 {
            w.tick(300);
            w.step("apply").await;
        }
        assert!(w.terminated().is_empty(), "{tag}");
        assert_eq!(w.last_row(&a), Some(Row::NotCovered), "{tag}");
        let created: Vec<String> = w.remote.created.lock().unwrap().iter().map(|(_, c)| c.clone()).collect();
        assert!(!created.is_empty() && created.iter().all(|c| *c == (T0 - 86_400).to_string()), "{tag}: {created:?}");
    }
}

/// RS7a/b: scheduled work (a crontab line, an `at` job, a timer) or an open terminal (RunPod's
/// proxy shell among them) is use — never reaped; can't read them: can't tell.
#[tokio::test]
async fn rs7_scheduled_work_and_terminals_are_use() {
    for (tag, set, row) in [
        ("sched", (|s: &mut Sim| s.sched = "hit") as fn(&mut Sim), Row::Active),
        ("tty", |s: &mut Sim| s.ttys = "1", Row::Active),
        ("sched-unknown", |s: &mut Sim| s.sched = "err", Row::Unknown),
    ] {
        let w = World::new(&format!("rs7-{tag}"), "");
        let a = rp("devtest-alpha", "pa", 22001);
        let mut sim = Sim::new("pa", "runpod");
        set(&mut sim);
        w.add(a.clone(), sim);
        w.policy(&a, None, Some(900));
        let runs = run_until_reaped(&w, "apply", 300, 8).await;
        assert!(runs == 8 && w.terminated().is_empty(), "{tag}");
        assert_eq!(w.last_row(&a), Some(row), "{tag}");
    }
}

/// R2-3: a failed freeze leaves nothing of its own under `reaped/` (the leaves it made, exactly
/// those, are removed) and backs off 6 h like a failed git push or rsync.
#[tokio::test]
async fn r2_3_a_failed_freeze_cleans_up_after_itself_and_backs_off() {
    let (w, a) = due_world("r2-3-clean", "").await;
    // Something of an earlier reap there: never touched.
    let earlier = w.dir.join("snap/reaped/devtest-alpha/20260101T000000Z-runpod-old/snapshot");
    std::fs::create_dir_all(&earlier).unwrap();
    std::fs::write(earlier.join("work.txt"), "an earlier reap's").unwrap();
    std::fs::write(w.dir.join("snap/big/reaped"), "in the way").unwrap();
    w.step("apply").await;
    assert!(w.terminated().is_empty());
    let leaves: Vec<String> = std::fs::read_dir(w.dir.join("snap/reaped/devtest-alpha")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(leaves, ["20260101T000000Z-runpod-old"], "only this attempt's own leaves go");
    assert_eq!(std::fs::read_to_string(earlier.join("work.txt")).unwrap(), "an earlier reap's");
    let p = &w.state().pods[&core_reap::key("runpod", &a.id)];
    assert!(p.retry_after.is_some_and(|t| t >= w.now() + 6 * 3600 - 60), "{:?}", p.retry_after);
    assert!(w.notified().iter().any(|l| l.contains("freeze_failed devtest-alpha") && l.contains("retried in 6h")), "{:?}", w.notified());
}

/// R2-4: where the reaper is configured, a state that can't be read refuses (corrupt here —
/// the review's own test covers a permission error); not configured, no check at all.
#[test]
fn r2_4_reap_in_progress_fails_closed_only_where_the_reaper_is_configured() {
    let w = World::new("r2-4-closed", "");
    let a = rp("devtest-alpha", "pa", 22001);
    assert_eq!(super::reap_in_progress(&w.cfg, &a, "replace"), None, "no state file: no reap");
    std::fs::create_dir_all(w.store().dir.clone()).unwrap();
    std::fs::write(w.store().state_path(), "{garbage").unwrap();
    let why = super::reap_in_progress(&w.cfg, &a, "migrate").unwrap();
    assert!(why.starts_with("refusing to migrate: can't tell whether the auto-reaper is reaping devtest-alpha") && why.contains("reap.json"), "{why}");
    // REAP_ENABLED off in the file (and no reap cron line on this box): nothing is read.
    let off = Config::parse(&format!("MACHINE_NAME_PREFIX=devtest\nARENA_STATE_DIR={}\nREAP_ENABLED=0\n", w.dir.join("state").display()));
    if !super::super::reap_cron_installed() {
        assert_eq!(super::reap_in_progress(&off, &a, "replace"), None);
    }
}
