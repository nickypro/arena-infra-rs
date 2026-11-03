//! `d` and `D`: deep checks in the background, which pods `D` checks, and their verdicts in the health cache.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arena_core::health::{deep_check_command_for, judge_deep_call, CheckArgs, CourseBook, HealthPolicy, PodHealth, DEEP_CHECK_TIMEOUT};
use arena_core::metrics::PodMetrics;
use arena_core::remote::{describe_error, Remote};
use arena_core::ssh::SshTarget;
use arena_core::{snapshot, Config, Pod};

use crate::refresh::{health_cache_path, spend_path};
use crate::state::mark_key;
use crate::{Shared, Ui};

/// How long `D`'s first press waits for the second (within [`NOTICE_FOR`], so the prompt is
/// still on screen).
pub(crate) const DEEP_ALL_CONFIRM: Duration = Duration::from_secs(5);

/// GPU memory in use, in MiB, above which `D` takes a GPU as someone's: a model held between
/// notebook cells sits at 0% utilization. An idle GPU shows a few MiB, a few hundred at most.
const BUSY_GPU_MEM_MB: u32 = 1024;

/// What `D` deep-checks: every running pod whose GPUs it has read and found no one using. A deep
/// check loads the GPUs for about a minute, and on a busy one it reads as a failure, so it leaves
/// out:
/// - **busy** pods: any GPU with a compute process, over the reaper's idle line
///   ([`arena_core::idle::IDLE_GPU_PCT`]) or holding more than [`BUSY_GPU_MEM_MB`] — any GPU, not
///   the mean: one busy GPU of four is a busy pod;
/// - **unread** ones: no metrics yet (the first refresh's early paint);
/// - **unreadable** ones: the read failed, or it found no GPUs or a GPU without its utilization
///   and memory (a stalled `nvidia-smi`) — a pod too loaded to answer must not look idle. `d`
///   checks one of those on purpose;
/// - **CPU-only** boxes ([`Pod::cpu_only`]): a deep check there always fails.
///
/// The ones left out come back to be said. Pure.
pub(crate) fn deep_all_plan(pods: &[Pod], metrics: &HashMap<String, PodMetrics>) -> (Vec<Pod>, Skipped) {
    let mut check = Vec::new();
    let mut skipped = Skipped::default();
    for p in pods.iter().filter(|p| p.status.eq_ignore_ascii_case("RUNNING")) {
        match metrics.get(&p.name) {
            _ if p.cpu_only() => skipped.no_gpu += 1,
            None => skipped.unread += 1,
            Some(m) if !gpus_read(p, m) => skipped.unreadable += 1,
            Some(m) if gpu_busy(m) => skipped.busy.push(p.name.clone()),
            Some(_) => check.push(p.clone()),
        }
    }
    (check, skipped)
}

/// Whether a read of a pod's GPUs worked: no error, at least one GPU and no fewer than the
/// listing says (a lost GPU drops out of `nvidia-smi`'s rows, busy or not), and every GPU's
/// utilization and memory read.
fn gpus_read(p: &Pod, m: &PodMetrics) -> bool {
    m.error.is_none()
        && !m.gpus.is_empty()
        && p.gpu_count.is_none_or(|n| m.gpus.len() >= n as usize)
        && m.gpus.iter().all(|g| g.util_pct.is_some() && g.mem_used_mb.is_some())
}

/// Whether any of a pod's GPUs is in use: a compute process, utilization over
/// [`arena_core::idle::IDLE_GPU_PCT`] (the reaper's line), or more than [`BUSY_GPU_MEM_MB`] of
/// memory taken.
fn gpu_busy(m: &PodMetrics) -> bool {
    !m.gpu_procs.is_empty()
        || m.gpus.iter().any(|g| {
            g.util_pct.is_some_and(|u| u > arena_core::idle::IDLE_GPU_PCT) || g.mem_used_mb.is_some_and(|u| u > BUSY_GPU_MEM_MB)
        })
}

/// What `D`'s second press checks: the pods `plan` still qualifies, kept to those the first press
/// named (`armed`, by [`mark_key`]) — a pod a refresh made eligible in between was never shown to
/// the operator. Unarmed (the first press, or the window lapsed): the plan as it is. Pure.
pub(crate) fn deep_all_confirm(plan: Vec<Pod>, armed: Option<&std::collections::HashSet<String>>) -> Vec<Pod> {
    match armed {
        Some(named) => plan.into_iter().filter(|p| named.contains(&mark_key(p))).collect(),
        None => plan,
    }
}

/// The pods `D` leaves out: the busy ones by name, and how many weren't read yet, couldn't be
/// read or have no GPU.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Skipped {
    busy: Vec<String>,
    unread: usize,
    unreadable: usize,
    no_gpu: usize,
}

/// ` — skipping 2 busy: apple, bloom; 3 not read yet; 1 unreadable; 1 without a GPU` (at most
/// five named), or nothing.
pub(crate) fn skipped_words(s: &Skipped) -> String {
    let mut parts = Vec::new();
    if !s.busy.is_empty() {
        let mut names = s.busy.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
        if s.busy.len() > 5 {
            names.push_str(", …");
        }
        parts.push(format!("{} busy: {names}", s.busy.len()));
    }
    if s.unread > 0 {
        parts.push(format!("{} not read yet", s.unread));
    }
    if s.unreadable > 0 {
        parts.push(format!("{} whose GPUs couldn't be read", s.unreadable));
    }
    if s.no_gpu > 0 {
        parts.push(format!("{} without a GPU", s.no_gpu));
    }
    if parts.is_empty() { String::new() } else { format!(" — skipping {}", parts.join("; ")) }
}

/// Deep-check `pods` (the cursor pod or the marked set) **in the background** — no modal:
/// the dashboard stays live, the rows say `checking`, and the footer says when it's done.
/// Pods already being checked are skipped. The verdicts are written to the health cache
/// (where `arena snapshot` and the next refresh read them) and shown at once.
pub(crate) fn start_deep_check(shared: &Arc<Mutex<Shared>>, ui: &Ui, pods: Vec<Pod>) {
    // Config first: a malformed MIN_DRIVER_VERSION is said before any pod is touched.
    let policy = match HealthPolicy::for_fleet(&ui.cfg, spend_path(&ui.cfg).as_deref()) {
        Ok(p) => p,
        Err(e) => {
            shared.lock().unwrap().notify(format!("✗ deep check: {e}"), true);
            return;
        }
    };
    let claimed = {
        let mut s = shared.lock().unwrap();
        let claimed = s.deep.begin(pods);
        match claimed.len() {
            0 => s.notify("already being deep-checked", false),
            n => s.notify(
                format!(
                    "deep-checking {n} pod{} in the background (up to {}s)…",
                    if n == 1 { "" } else { "s" },
                    DEEP_CHECK_TIMEOUT.as_secs()
                ),
                false,
            ),
        }
        claimed
    };
    if claimed.is_empty() {
        return;
    }
    let (shared, remote, cfg) = (shared.clone(), ui.remote.clone(), ui.cfg.clone());
    tokio::spawn(async move {
        let results = deep_check_pods(&remote, &cfg, &policy, &claimed).await;
        finish_deep_check(&shared, health_cache_path(&cfg), &claimed, results, snapshot::unix_now()).await;
    });
}

/// Run the deep check on each pod concurrently over `remote` — the script `pods test
/// --deep` runs (its course facts too), one exec within [`DEEP_CHECK_TIMEOUT`], judged by the
/// same core function (`health::judge_deep_call`) with the same course book (`CourseBook::gather`:
/// the proxy ports tried from here alongside). A pod with no SSH endpoint is a FAIL: it was asked
/// for. A pod whose check task died is a FAIL too, never dropped. Results in `pods` order.
async fn deep_check_pods(remote: &Arc<dyn Remote>, cfg: &Config, policy: &HealthPolicy, pods: &[Pod]) -> Vec<PodHealth> {
    // `CONDA_ENV=""` disables activation, as for the CLI.
    let conda = cfg.get("CONDA_ENV").unwrap_or("arena-env").to_string();
    let course = CourseBook::gather(remote, cfg, pods, !cfg!(test));
    let jobs: Vec<_> = pods
        .iter()
        .map(|pod| {
            SshTarget::from_pod(pod, cfg).ok().map(|target| {
                let (remote, cmd) = (remote.clone(), deep_check_command_for(Some(&conda), &CheckArgs::for_pod(cfg, pod)));
                tokio::spawn(async move {
                    remote.exec(&target, &cmd, Some(DEEP_CHECK_TIMEOUT)).await.map_err(|e| describe_error(&e))
                })
            })
        })
        .collect();
    let policy = HealthPolicy { course: Some(course.await), ..policy.clone() };
    let mut results = Vec::with_capacity(pods.len());
    for (pod, job) in pods.iter().zip(jobs) {
        results.push(match job {
            None => PodHealth::unreachable(pod, format!("no SSH endpoint yet (status {})", pod.status)),
            Some(handle) => match handle.await {
                Ok(call) => judge_deep_call(pod, call, &policy),
                Err(e) => PodHealth::unreachable(pod, format!("the check task failed: {e}")),
            },
        });
    }
    results
}

/// The I/O tail of a background deep check: record the verdicts in the health cache file
/// (off the async workers — it takes a lock), then fold them into what's on screen and say
/// so in the footer. A cache that can't be written costs only the persistence: the
/// session keeps the verdicts (see `state::DeepChecks`) and the footer says why.
async fn finish_deep_check(
    shared: &Arc<Mutex<Shared>>,
    cache: Option<std::result::Result<PathBuf, String>>,
    claimed: &[Pod],
    results: Vec<PodHealth>,
    now: u64,
) {
    let (results, warning) = match cache {
        None => (results, None),
        Some(Err(e)) => (results, Some(format!("not cached: {e}"))),
        Some(Ok(path)) => {
            let written = tokio::task::spawn_blocking(move || {
                let w = match snapshot::record_health(&path, &results, None, now) {
                    Ok(w) => w,
                    Err(e) => Some(format!("couldn't write the health cache {}: {e}", path.display())),
                };
                (results, w)
            })
            .await;
            match written {
                Ok(done) => done,
                // The write task itself died: the verdicts went with it — still release the pods.
                Err(e) => (Vec::new(), Some(format!("recording the results failed: {e}"))),
            }
        }
    };
    let mut guard = shared.lock().unwrap();
    let s = &mut *guard;
    let mut line = s.deep.finish(claimed, &results, now, &mut s.snap);
    if let Some(w) = warning {
        line.push_str(&format!(" ({})", w.strip_prefix("warning: ").unwrap_or(&w)));
    }
    let failed = results.iter().any(|h| h.status == arena_core::health::Status::Fail);
    s.notify(line, failed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena_core::health::Status;
    use arena_core::remote::{FakeRemote, FakeReply, RemoteCall};
    use arena_core::selector::Naming;
    use arena_core::snapshot::HealthCache;
    use crate::state::{dashboard_snapshot, health_cell};
    use crate::tests::{cfg, pod, tmp, ui, verdict, DEEP_HEALTHY, NOW};

    /// `D` checks every running pod whose GPUs it has read and found idle: busy ones (a compute
    /// process, any GPU over 5% or holding > 1 GiB) are named; unread, unreadable and CPU-only
    /// ones counted; a stopped pod isn't checked or named.
    #[test]
    fn deep_all_checks_running_pods_no_one_is_using() {
        use arena_core::metrics::{GpuProc, GpuStat};
        let gpu = |util| GpuStat { util_pct: Some(util), mem_used_mb: Some(3), ..Default::default() };
        let held = GpuStat { util_pct: Some(0), mem_used_mb: Some(9_000), ..Default::default() };
        let m = |gpus: Vec<GpuStat>, procs: usize| PodMetrics {
            gpus,
            gpu_procs: (0..procs).map(|i| GpuProc { pid: i as u32, ..Default::default() }).collect(),
            ..Default::default()
        };
        let mut stopped = pod("delta", "runpod", "d", 4, Some(22));
        stopped.status = "EXITED".into();
        let mut zero = pod("golf", "vast", "g", 7, Some(22));
        zero.gpu_count = Some(0);
        let pods = [
            pod("alpha", "runpod", "a", 1, Some(22)),
            pod("bravo", "runpod", "b", 2, Some(22)),
            pod("charlie", "runpod", "c", 3, Some(22)),
            stopped,
            pod("echo", "hetzner", "e", 5, Some(22)),
            pod("foxtrot", "runpod", "f", 6, Some(22)),
            zero,
            pod("hotel", "runpod", "h", 8, Some(22)),
            pod("india", "runpod", "i", 9, Some(22)),
            pod("juliet", "runpod", "j", 10, Some(22)),
        ];
        let failed = PodMetrics { error: Some("ssh: timed out".into()), ..Default::default() };
        let metrics: HashMap<String, PodMetrics> = [
            ("devtest-alpha", m(vec![gpu(3), gpu(0)], 0)),
            ("devtest-bravo", m(vec![gpu(0)], 1)),
            // One GPU of four at 30%: the mean is 7%, but the pod is busy.
            ("devtest-charlie", m(vec![gpu(30), gpu(0), gpu(0), gpu(0)], 0)),
            ("devtest-echo", PodMetrics { cpu_pct: Some(3), ..Default::default() }),
            // A model held between notebook cells: 0% but 9 GB in use.
            ("devtest-hotel", m(vec![gpu(0), held], 0)),
            ("devtest-india", failed),
            // Read, but `nvidia-smi` gave nothing (stalled, or a driver mismatch under a running
            // job): no evidence it's idle.
            ("devtest-juliet", PodMetrics { cpu_pct: Some(5), ..Default::default() }),
            // A GPU whose utilization didn't come back.
            ("devtest-kilo", m(vec![GpuStat { util_pct: None, mem_used_mb: Some(3), ..Default::default() }], 0)),
            // Exactly the reaper's idle line is idle.
            ("devtest-lima", m(vec![gpu(5)], 0)),
            // Listed with two GPUs, one row back: the missing one may be the busy one.
            ("devtest-mike", m(vec![gpu(0)], 0)),
        ]
        .into_iter()
        .map(|(n, m)| (n.to_string(), m))
        .collect();
        let mut pods = pods.to_vec();
        let mut mike = pod("mike", "runpod", "m", 13, Some(22));
        mike.gpu_count = Some(2);
        let mut lima = pod("lima", "runpod", "l", 12, Some(22));
        lima.gpu_count = Some(1);
        pods.extend([pod("kilo", "runpod", "k", 11, Some(22)), lima, mike]);
        let (check, skipped) = deep_all_plan(&pods, &metrics);
        assert_eq!(check.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["devtest-alpha", "devtest-lima"]);
        let busy = ["devtest-bravo", "devtest-charlie", "devtest-hotel"].map(String::from).to_vec();
        // foxtrot has no metrics yet; india's read failed, juliet's, kilo's and mike's GPUs weren't
        // all read; echo and golf have no GPU.
        assert_eq!(skipped, Skipped { busy, unread: 1, unreadable: 4, no_gpu: 2 });
        assert_eq!(
            skipped_words(&skipped),
            " — skipping 3 busy: devtest-bravo, devtest-charlie, devtest-hotel; 1 not read yet; 4 whose GPUs couldn't be read; 2 without a GPU"
        );
        assert_eq!(skipped_words(&Skipped::default()), "");
        let many = Skipped { busy: (0..7).map(|i| format!("p{i}")).collect(), ..Default::default() };
        assert_eq!(skipped_words(&many), " — skipping 7 busy: p0, p1, p2, p3, p4, …");
        // The early paint: listed, nothing read — `D` has nothing to check.
        let (check, skipped) = deep_all_plan(&pods, &HashMap::new());
        assert!(check.is_empty());
        assert_eq!(skipped_words(&skipped), " — skipping 10 not read yet; 2 without a GPU");
    }

    /// `D`'s second press checks what the first named and still qualifies: a pod a refresh made
    /// eligible in between is left out, one that turned busy (gone from the plan) too.
    #[test]
    fn the_second_d_checks_only_what_the_first_named() {
        let [alpha, bravo, charlie] = [("alpha", "a"), ("bravo", "b"), ("charlie", "c")].map(|(n, id)| pod(n, "runpod", id, 1, Some(22)));
        let named: std::collections::HashSet<String> = [&alpha, &bravo].map(mark_key).into_iter().collect();
        // Since the first press: bravo turned busy (out of the plan), charlie became idle.
        let plan = vec![alpha.clone(), charlie.clone()];
        let names = |v: Vec<Pod>| v.into_iter().map(|p| p.name).collect::<Vec<_>>();
        assert_eq!(names(deep_all_confirm(plan.clone(), Some(&named))), ["devtest-alpha"]);
        assert_eq!(names(deep_all_confirm(plan, None)), ["devtest-alpha", "devtest-charlie"], "unarmed: the plan");
    }

    /// The dashboard's deep check is `pods test --deep`'s: the same script in one exec per
    /// pod within DEEP_CHECK_TIMEOUT, judged by core — a hung pod is a FAIL at the budget, a
    /// pod without an endpoint a FAIL without any SSH.
    #[tokio::test(start_paused = true)]
    async fn deep_check_pods_runs_the_clis_check_over_remote_within_its_budget() {
        let fake = Arc::new(FakeRemote::new());
        fake.script("10.0.0.1:22001", [FakeReply::stdout(DEEP_HEALTHY).after(Duration::from_secs(40))]);
        fake.script("10.0.0.2:22002", [FakeReply::hang()]);
        let remote: Arc<dyn Remote> = fake.clone();
        let cfg = cfg("");
        let policy = HealthPolicy::from_config(&cfg).unwrap();
        let pods = [
            pod("alpha", "runpod", "rp1", 1, Some(22001)),
            pod("bravo", "runpod", "rp2", 2, Some(22002)),
            pod("charlie", "runpod", "rp3", 3, None),
        ];
        let start = tokio::time::Instant::now();
        let results = deep_check_pods(&remote, &cfg, &policy, &pods).await;
        assert_eq!(start.elapsed(), DEEP_CHECK_TIMEOUT, "ends at the hung pod's budget");

        let got: Vec<(&str, Status, String)> = results
            .iter()
            .map(|h| (h.name.as_str(), h.status, h.checks.iter().find(|c| c.status == Status::Fail).map(|c| c.detail.clone()).unwrap_or_default()))
            .collect();
        assert_eq!(
            got,
            [
                ("devtest-alpha", Status::Pass, String::new()),
                ("devtest-bravo", Status::Fail, "timed out after 150s".to_string()),
                ("devtest-charlie", Status::Fail, "no SSH endpoint yet (status RUNNING)".to_string()),
            ]
        );
        for (host, pod) in [("10.0.0.1:22001", &pods[0]), ("10.0.0.2:22002", &pods[1])] {
            let want = RemoteCall::Exec {
                host: String::new(),
                cmd: arena_core::health::deep_check_command_for(Some("arena-env"), &CheckArgs::for_pod(&cfg, pod)),
                timeout: Some(DEEP_CHECK_TIMEOUT),
            };
            let calls = fake.calls_to(host);
            assert_eq!(calls.len(), 1, "{host}");
            assert!(matches!((&calls[0], &want), (RemoteCall::Exec { cmd, timeout, .. }, RemoteCall::Exec { cmd: w, timeout: wt, .. }) if cmd == w && timeout == wt));
        }
        assert_eq!(fake.calls().len(), 2, "no SSH for the pod without an endpoint");
    }

    /// `d` never blocks: the call returns at once with the rows saying `checking`, a second
    /// `d` on a pod being checked does nothing, and the verdicts land on the rows (and in
    /// the footer) when the background task finishes.
    #[tokio::test(start_paused = true)]
    async fn d_checks_in_the_background_and_the_verdicts_land_on_the_rows() {
        let fake = Arc::new(FakeRemote::new());
        fake.script("10.0.0.1:22001", [FakeReply::stdout(DEEP_HEALTHY).after(Duration::from_secs(30))]);
        fake.script("10.0.0.2:22002", [FakeReply::exit(255, "ssh: connect to host 10.0.0.2 port 22002: Connection refused").after(Duration::from_secs(5))]);
        let ui = ui(cfg(""), fake.clone());
        let pods = vec![pod("alpha", "runpod", "rp1", 1, Some(22001)), pod("bravo", "runpod", "rp2", 2, Some(22002))];
        let shared = Arc::new(Mutex::new(Shared::default()));
        {
            let mut s = shared.lock().unwrap();
            let snap = dashboard_snapshot(&pods, &[], None, &HealthCache::new(), &Naming::from_config(&ui.cfg), NOW);
            s.publish(snap);
        }

        start_deep_check(&shared, &ui, pods.clone());
        {
            let s = shared.lock().unwrap();
            assert_eq!(s.deep.running(), 2);
            assert!(s.pods.iter().all(|p| health_cell(None, s.deep.is_running(p), NOW).0 == "…"));
            assert_eq!(s.notice.as_ref().unwrap().text, "deep-checking 2 pods in the background (up to 150s)…");
        }
        start_deep_check(&shared, &ui, vec![pods[0].clone()]);
        assert_eq!(shared.lock().unwrap().notice.as_ref().unwrap().text, "already being deep-checked");

        // Let the background task run to completion (the paused clock jumps ahead).
        for _ in 0..100 {
            if shared.lock().unwrap().deep.running() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let s = shared.lock().unwrap();
        assert_eq!(s.deep.running(), 0);
        let status = |id: &str| s.snap.pods.iter().find(|p| p.pod.id == id).and_then(|p| p.health.as_ref()).map(|h| h.status);
        assert_eq!((status("rp1"), status("rp2")), (Some(Status::Pass), Some(Status::Fail)));
        let notice = s.notice.as_ref().unwrap();
        assert_eq!(notice.text, "deep check: 1 pass, 0 warn, 1 fail — failed: devtest-bravo");
        assert!(notice.error);
        assert_eq!(fake.calls().len(), 2, "one check per pod, not two");
    }

    /// The verdicts go to the same health cache `pods test --deep` writes (owner-only), where
    /// `arena snapshot` and the next refresh read them; a cache that can't be written still
    /// leaves them on screen and says why.
    #[tokio::test]
    async fn finished_checks_are_recorded_in_the_shared_health_cache() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("deepcache");
        let cfg = cfg(&format!("ARENA_STATE_DIR={}\n", dir.0.display()));
        let alpha = pod("alpha", "runpod", "rp1", 1, Some(22001));
        let shared = Arc::new(Mutex::new(Shared::default()));
        let claimed = shared.lock().unwrap().deep.begin(vec![alpha.clone()]);
        let results = vec![verdict(&alpha, Status::Warn, Some(("network", "0.4 MB/s from huggingface.co")))];
        finish_deep_check(&shared, health_cache_path(&cfg), &claimed, results.clone(), NOW).await;
        let path = dir.0.join("devtest").join(snapshot::HEALTH_CACHE_FILE);
        let (cache, warning) = HealthCache::load(&path);
        assert_eq!(warning, None);
        let rec = cache.get(&alpha).unwrap();
        assert_eq!((rec.status, rec.checked_at), (Status::Warn, NOW));
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        {
            let s = shared.lock().unwrap();
            assert_eq!(s.deep.running(), 0);
            assert_eq!(s.notice.as_ref().unwrap().text, "deep check: 0 pass, 1 warn, 0 fail");
        }

        // No usable state dir: still shown (session), and the footer says it wasn't cached.
        let claimed = shared.lock().unwrap().deep.begin(vec![alpha.clone()]);
        let unusable = Some(Err("ARENA_STATE_DIR must be an absolute path (got state)".to_string()));
        finish_deep_check(&shared, unusable, &claimed, results, NOW + 60).await;
        let s = shared.lock().unwrap();
        assert!(s.notice.as_ref().unwrap().text.ends_with("(not cached: ARENA_STATE_DIR must be an absolute path (got state))"));
        assert_eq!(s.deep.layer(HealthCache::new()).get(&alpha).unwrap().checked_at, NOW + 60);
    }
}
