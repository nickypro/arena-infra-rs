use std::sync::Arc;

use crate::cli::{Cli, Cmd, PodCmd};
use crate::pods::create::{create_with_retry, replacement_rounds, CAPACITY_RETRY_SECS, MAX_CAPACITY_ATTEMPTS, NO_STREAM};
use crate::pods::{handle_pods, handle_pods_bal};
use super::*;

/// Placement never SSHes; an unscripted FakeRemote would fail any call loudly.
fn fake_remote() -> std::sync::Arc<dyn arena_core::remote::Remote> {
    std::sync::Arc::new(arena_core::remote::FakeRemote::new())
}
use arena_core::placement::Order;
use arena_core::{Error, Pod, Result as CoreResult};
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// A RunPod-named provider whose creates succeed except on the GPU types in `dry`
/// (capacity). Records every create as `(name, gpu, cloud)` and counts list calls.
#[derive(Default)]
struct Recorder {
    dry: Vec<&'static str>,
    calls: Mutex<Vec<(String, String, String)>>,
    made: Mutex<Vec<Pod>>,
    lists: AtomicUsize,
    /// A second backend that fails to list (a bogus `VAST_API_KEY`), holding a pod nobody
    /// can see: the aggregate `list_pods` skips it, as the fleet provider's does.
    vast_down: bool,
    /// The $/h each create answers with (`None`: unpriced, as RunPod v1's REST create).
    price: Option<f64>,
    /// Each pod is RUNNING with an SSH endpoint the moment it's made, so `up`'s pipelines
    /// (with `--no-setup`) bring it up at once.
    up_at_once: bool,
    /// How long each listing takes (a slow backend, a RunPod 429 backoff); none by default.
    list_takes: std::time::Duration,
}

impl Recorder {
    fn dry(dry: &[&'static str]) -> Self {
        Recorder { dry: dry.to_vec(), ..Default::default() }
    }
    fn calls(&self) -> Vec<(String, String, String)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl Provider for Recorder {
    fn name(&self) -> &'static str {
        "runpod"
    }
    fn describe(&self, spec: &PodSpec) -> String {
        format!("{} x{}, {}", spec.gpu_type, spec.gpu_count, spec.cloud_type)
    }
    async fn list_pods(&self) -> CoreResult<Vec<Pod>> {
        self.lists.fetch_add(1, Ordering::SeqCst);
        if !self.list_takes.is_zero() {
            tokio::time::sleep(self.list_takes).await;
        }
        Ok(self.made.lock().unwrap().clone())
    }
    async fn list_by_provider(&self) -> Vec<(String, CoreResult<Vec<Pod>>)> {
        let mut out = vec![("runpod".to_string(), self.list_pods().await)];
        if self.vast_down {
            let e = Error::Provider {
                kind: arena_core::ProviderErrorKind::Auth,
                message: r#"list instances HTTP 404 Not Found: {"error":"auth_error","msg":"Invalid user key"}"#.into(),
            };
            out.push(("vast".into(), Err(e)));
        }
        out
    }
    async fn create_pod(&self, spec: &PodSpec) -> CoreResult<Pod> {
        self.calls.lock().unwrap().push((spec.name.clone(), spec.gpu_type.clone(), spec.cloud_type.clone()));
        if self.dry.contains(&spec.gpu_type.as_str()) {
            return Err(Error::capacity("create pod HTTP 500: There are no instances currently available"));
        }
        let mut pod = Pod { id: format!("id-{}", spec.name), name: spec.name.clone(), provider: "runpod".into(), ..Default::default() };
        pod.cost_per_hr = self.price;
        if self.up_at_once {
            let port = 22_000 + self.made.lock().unwrap().len() as u16;
            (pod.status, pod.ssh_ip, pod.ssh_port) = ("RUNNING".into(), Some("203.0.113.9".into()), Some(port));
        }
        self.made.lock().unwrap().push(pod.clone());
        Ok(pod)
    }
    async fn stop_pod(&self, _id: &str) -> CoreResult<()> {
        Ok(())
    }
    async fn restart_pod(&self, _id: &str) -> CoreResult<()> {
        Ok(())
    }
    async fn terminate_pod(&self, _id: &str) -> CoreResult<()> {
        Ok(())
    }
}

/// No RUNPOD_API_KEY (so prices are the preset estimates — no network) and no proxy (so
/// the post-create sync is a skip note).
fn cfg() -> Config {
    Config::parse(
        "MACHINE_NAME_PREFIX=arena8\nGPU_TYPE=\"NVIDIA RTX A4000\"\nCLOUD_TYPE=COMMUNITY\n\
         MACHINE_NAME_LIST=(\n  \"apple\"\n  \"bloom\"\n  \"cider\"\n)\n",
    )
}

fn create(names: &[&str], gpu: Option<&str>, cloud: Option<&str>, max_price: Option<f64>) -> PodCmd {
    PodCmd::Create {
        names: names.iter().map(|s| s.to_string()).collect(),
        count: None,
        add: None,
        gpu: gpu.map(String::from),
        gpus: None,
        server_type: None,
        cloud: cloud.map(String::from),
        max_price,
        order: Order::Cheapest,
        disk: None,
        volume: None,
        image: None,
        bootstrap: false,
        api_json: None,
        reap: crate::ReapAtCreate::default(),
        owner: crate::spend::OwnerArgs::default(),
        skip_proxy: true,
        dry_run: false,
        keep_trying: false,
        retry_mins: 0,
        retry_secs: 0,
    }
}

fn call(name: &str, gpu: &str, cloud: &str) -> (String, String, String) {
    (name.into(), gpu.into(), cloud.into())
}

/// Live finding #9 (fleet-mixed): with Vast's key bogus, `create delta`, `up -n 4` and the
/// `--provider vast` runs all planned names a failed provider might hold — a duplicate pod
/// billed twice. Every name-picking path — explicit names, `-n`, `-a`, dry run or not,
/// `create` and `up`, single spec or placement — now refuses, naming the provider and its
/// key, before any create.
#[tokio::test]
async fn create_and_up_refuse_while_any_provider_fails_to_list() {
    let up = |names: &[&str], count: Option<usize>, gpu: Option<&str>, dry_run: bool| PodCmd::Up {
        names: names.iter().map(|s| s.to_string()).collect(),
        count,
        add: None,
        gpu: gpu.map(String::from),
        gpus: None,
        server_type: None,
        cloud: None,
        max_price: None,
        order: Order::Cheapest,
        disk: None,
        volume: None,
        image: None,
        bootstrap: false,
        api_json: None,
        reap: crate::ReapAtCreate::default(),
        owner: crate::spend::OwnerArgs::default(),
        dry_run,
        no_wait: false,
        keep_trying: false,
        retry_mins: 0,
        retry_secs: 60,
        no_setup: true,
        check: false,
        check_attempts: 2,
        retry_bad_hosts: false,
        lock: false,
        timeout: 60,
        endpoint_timeout: None,
        interval: 10,
    };
    let mut cmds = Vec::new();
    for dry_run in [true, false] {
        for gpu in [None, Some("A4000,3090")] {
            let mut c = create(&["delta"], gpu, None, None);
            if let PodCmd::Create { dry_run: d, .. } = &mut c {
                *d = dry_run;
            }
            cmds.push(c);
            let mut c = create(&[], gpu, None, None);
            if let PodCmd::Create { count, dry_run: d, .. } = &mut c {
                (*count, *d) = (Some(4), dry_run);
            }
            cmds.push(c);
            let mut c = create(&[], gpu, None, None);
            if let PodCmd::Create { add, dry_run: d, .. } = &mut c {
                (*add, *d) = (Some(1), dry_run);
            }
            cmds.push(c);
            cmds.push(up(&["alpha"], None, gpu, dry_run));
            cmds.push(up(&[], Some(4), gpu, dry_run));
        }
    }
    for cmd in cmds {
        let rec = Recorder { vast_down: true, ..Default::default() };
        let e = handle_pods(cmd, &rec, fake_remote(), &cfg(), true).await.unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("vast failed to list") && msg.contains("refusing to pick machine names"), "{msg}");
        assert!(msg.contains("VAST_API_KEY was rejected"), "{msg}");
        assert!(rec.calls().is_empty(), "nothing created: {:?}", rec.calls());
    }
    // The same commands with every provider answering go ahead.
    let rec = Recorder::default();
    handle_pods(create(&["delta"], None, None, None), &rec, fake_remote(), &cfg(), true).await.unwrap();
    assert_eq!(rec.calls().len(), 1);
}

#[test]
fn only_several_options_or_a_cap_leave_the_single_spec_path() {
    let ov = |gpu: Option<&str>, cloud: Option<&str>| SpecOverrides {
        gpu: gpu.map(String::from),
        cloud: cloud.map(String::from),
        ..Default::default()
    };
    let route = |o: &SpecOverrides, cap, kt| {
        placement_request(&cfg(), "runpod", o.clone(), cap, Order::Cheapest, kt).map(|(_, placing)| placing)
    };
    // Today's flags (or none) stay on the single-spec path — `--keep-trying` included.
    // (Community: near the proxy has nothing to pin there.)
    for o in [ov(None, None), ov(Some("3090"), None), ov(Some("A4000"), Some("community"))] {
        assert!(route(&o, None, false).unwrap().is_none(), "{o:?}");
        assert!(route(&o, None, true).unwrap().is_none(), "{o:?}");
    }
    // One SECURE option with NEAR on (the default) is two to try — Europe, then anywhere —
    // unless `--keep-trying`, which waits on one pool: the single path, anywhere.
    let secure = ov(Some("A4000"), Some("secure"));
    let (req, _) = route(&secure, None, false).unwrap().expect("placement");
    assert_eq!(req.near.map(|n| n.region), Some(arena_core::near::Region::Europe));
    assert!(route(&secure, None, true).unwrap().is_none());
    let off = Config::parse("GPU_TYPE=\"NVIDIA RTX A4000\"\nCLOUD_TYPE=COMMUNITY\nNEAR=any\n");
    assert!(placement_request(&off, "runpod", secure.clone(), None, Order::Cheapest, false).unwrap().1.is_none(), "NEAR=any");
    let bad = Config::parse("NEAR=mars\n");
    assert!(placement_request(&bad, "runpod", secure, None, Order::Cheapest, false).is_err(), "refused before anything");
    let (req, base) = route(&ov(Some("A4000,3090"), None), None, false).unwrap().unwrap();
    assert_eq!(req.gpus, ["NVIDIA RTX A4000", "NVIDIA GeForce RTX 3090"]);
    assert_eq!((base.gpu_type.as_str(), base.cloud_type.as_str()), ("NVIDIA RTX A4000", "COMMUNITY"));
    assert!(route(&ov(None, Some("community,secure")), None, false).unwrap().is_some());
    assert!(route(&ov(None, None), Some(0.5), false).unwrap().is_some());
    let e = route(&ov(Some("A4000,3090"), None), None, true).unwrap_err().to_string();
    assert!(e.contains("--retry-mins"), "{e}");
    let e = route(&ov(None, Some("community,spot")), None, false).unwrap_err().to_string();
    assert!(e.contains("`SPOT`"), "{e}");
}

#[test]
fn a_list_that_collapses_to_one_option_creates_that_option_not_the_raw_string() {
    // Review fix: duplicates / stray commas parse to one option, so the single path runs —
    // and it must create the parsed option, not the raw flag text as a GPU id / tier.
    // (NEAR=any: near the proxy would make a SECURE option two — Europe, then anywhere.)
    let cfg = || Config::parse("MACHINE_NAME_PREFIX=arena8\nGPU_TYPE=\"NVIDIA RTX A4000\"\nCLOUD_TYPE=COMMUNITY\nNEAR=any\n");
    let single = |gpu: Option<&str>, cloud: Option<&str>| {
        let o = SpecOverrides { gpu: gpu.map(String::from), cloud: cloud.map(String::from), ..Default::default() };
        let (ov, placing) = placement_request(&cfg(), "runpod", o, None, Order::Cheapest, false).unwrap();
        assert!(placing.is_none(), "{gpu:?} {cloud:?} is one option");
        let spec = spec_with_overrides(&cfg(), &ov);
        (spec.gpu_type, spec.cloud_type)
    };
    let a4000 = "NVIDIA RTX A4000";
    let rtx3090 = "NVIDIA GeForce RTX 3090";
    for (gpu, cloud, want) in [
        (Some("A4000,a4000"), Some("community,COMMUNITY"), (a4000, "COMMUNITY")),
        (Some("NVIDIA RTX A4000,"), None, (a4000, "COMMUNITY")),
        (None, Some("community,"), (a4000, "COMMUNITY")),
        (Some("3090, 3090"), Some("secure,SECURE"), (rtx3090, "SECURE")),
        (Some(" 3090 "), Some(" secure "), (rtx3090, "SECURE")),
        // Unchanged: plain single flags, and absent flags keep config's values.
        (Some("3090"), Some("secure"), (rtx3090, "SECURE")),
        (Some("NVIDIA GeForce RTX 3090"), None, (rtx3090, "COMMUNITY")),
        (None, None, (a4000, "COMMUNITY")),
    ] {
        let got = single(gpu, cloud);
        assert_eq!((got.0.as_str(), got.1.as_str()), want, "{gpu:?} {cloud:?}");
    }
}

#[tokio::test]
async fn a_collapsing_list_on_create_sends_the_one_parsed_option() {
    let fake = Recorder::default();
    handle_pods(create(&["apple"], Some("A4000,a4000"), Some("community,COMMUNITY"), None), &fake, fake_remote(), &cfg(), true)
        .await
        .unwrap();
    assert_eq!(fake.calls(), [call("arena8-apple", "NVIDIA RTX A4000", "COMMUNITY")]);
}

/// One press holds for every phase of the command — including one that only starts
/// waiting after it (a fresh `tokio::signal::ctrl_c()` there would never fire).
#[tokio::test(start_paused = true)]
async fn a_ctrl_c_stays_pressed_for_every_later_phase() {
    let ctrl_c = Interrupt::manual();
    let later = ctrl_c.clone();
    assert!(!later.is_set());
    let waiting = tokio::spawn(ctrl_c.wait());
    ctrl_c.press();
    waiting.await.unwrap();
    assert!(later.is_set());
    tokio::time::timeout(std::time::Duration::from_secs(1), later.wait()).await.expect("seen at once by a later phase");
}

/// An `up --check` replacement waits for capacity the way the first create did.
#[test]
fn replacement_rounds_follow_the_create_flags() {
    use arena_core::placement::{next_round_in_window, Rounds};
    let s = std::time::Duration::from_secs;
    // (keep_trying, retry_mins, retry_secs) → rounds
    let cases = [
        ((false, 0, 60), Rounds { window: s(0), every: s(60) }),   // one try
        ((false, 5, 30), Rounds { window: s(300), every: s(30) }), // --retry-mins
        ((true, 5, 30), Rounds { window: s(300), every: s(30) }),  // --retry-mins wins, as in create_with_retry
        ((true, 0, 60), Rounds { window: s(CAPACITY_RETRY_SECS * u64::from(MAX_CAPACITY_ATTEMPTS)), every: s(CAPACITY_RETRY_SECS) }),
        ((false, 0, 0), Rounds { window: s(0), every: s(1) }),
    ];
    for ((kt, mins, secs), want) in cases {
        let got = replacement_rounds(kt, mins, secs);
        assert_eq!((got.window, got.every), (want.window, want.every), "keep_trying={kt} mins={mins} secs={secs}");
    }
    // --keep-trying: the first try + MAX_CAPACITY_ATTEMPTS retries, as `create_pods` makes.
    let r = replacement_rounds(true, 0, 60);
    let start = tokio::time::Instant::now();
    let (mut now, mut rounds) = (start, 1);
    while next_round_in_window(now, r.every, start + r.window) {
        now += r.every;
        rounds += 1;
    }
    assert_eq!(rounds, 1 + MAX_CAPACITY_ATTEMPTS);
}

/// The single-option retry loop starts no round after `--retry-mins` (the multi-option
/// executor's rule): it used to check the window only *before* sleeping, so with a
/// `--retry-secs` longer than what was left it slept past the window and created anyway.
#[tokio::test(start_paused = true)]
async fn single_option_retry_starts_no_round_after_the_window() {
    // (retry_mins, retry_secs, rounds expected): rounds at t = 0, secs, 2·secs, … ≤ window.
    for (mins, secs, want) in [(1, 600, 1), (1, 45, 2), (2, 60, 3), (1, 60, 2), (0, 30, 1)] {
        let fake = Recorder::dry(&["NVIDIA RTX A4000"]);
        let started = tokio::time::Instant::now();
        let made = create_with_retry(&fake, &cfg(), vec!["arena8-apple".into()], 0, &SpecOverrides::default(), false, mins, secs, &Interrupt::manual(), NO_STREAM)
            .await
            .unwrap();
        assert!(made.is_empty());
        assert_eq!(fake.calls().len(), want, "--retry-mins {mins} --retry-secs {secs}");
        // It gave up right after the last round — no sleep just to give up.
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(secs * (want as u64 - 1)), "{mins}/{secs}");
    }
}

/// The dashboard's add-pod form retries with `--retry-mins` up to a day (the human,
/// 2026-10-30: "an auto-retry option"), and its runs panel's `[s]` sends Ctrl+C: between two
/// rounds that stops the retries at once — no round after it, nothing made, the run going on
/// to its end with what it has — however long the window had left.
#[tokio::test(start_paused = true)]
async fn ctrl_c_between_rounds_ends_a_day_long_retry_window_at_once() {
    let fake = Recorder::dry(&["NVIDIA RTX A4000"]);
    let (config, ov, ctrl_c) = (cfg(), SpecOverrides::default(), Interrupt::manual());
    let started = tokio::time::Instant::now();
    let press = async {
        tokio::time::sleep(std::time::Duration::from_secs(150)).await;
        ctrl_c.press();
    };
    let (made, ()) = tokio::join!(
        create_with_retry(&fake, &config, vec!["arena8-apple".into()], 0, &ov, false, 1440, 60, &ctrl_c, NO_STREAM),
        press
    );
    assert!(made.unwrap().is_empty());
    assert_eq!(fake.calls().len(), 3, "rounds at t = 0, 60, 120; none after the press");
    assert_eq!(started.elapsed(), std::time::Duration::from_secs(150), "stopped at the press, not at the next round");
}

/// A press while the fleet is re-listed before a round (slow: 20 s here) stops there too: it
/// was seen only during the sleep, so the round after a slow listing still created (billed)
/// after the dashboard's `[s]` said "stopping" (pre-merge review of #38, NIT 1).
#[tokio::test(start_paused = true)]
async fn ctrl_c_during_the_relist_creates_nothing_more() {
    let fake = Recorder { list_takes: std::time::Duration::from_secs(20), ..Recorder::dry(&["NVIDIA RTX A4000"]) };
    let (config, ov, ctrl_c) = (cfg(), SpecOverrides::default(), Interrupt::manual());
    let started = tokio::time::Instant::now();
    let press = async {
        // Round 1 at t=0; the wait ends at t=60, the re-list runs to t=80: pressed at t=70,
        // during it.
        tokio::time::sleep(std::time::Duration::from_secs(70)).await;
        ctrl_c.press();
    };
    let (made, ()) = tokio::join!(
        create_with_retry(&fake, &config, vec!["arena8-apple".into()], 0, &ov, false, 1440, 60, &ctrl_c, NO_STREAM),
        press
    );
    assert!(made.unwrap().is_empty());
    assert_eq!(fake.calls().len(), 1, "round 1 only: no round after a press during the re-list");
    assert_eq!(started.elapsed(), std::time::Duration::from_secs(70), "stopped at the press, not after the listing");
}

/// The link the test above stands in for: a real SIGINT — what the dashboard's `[s]` sends
/// the run's process group (`kill -INT -- -pgid`) — presses [`Interrupt::ctrl_c`] once a
/// retry wait is listening, and the process goes on (it isn't killed by it). In a child
/// process (this test binary again, running only [`interrupt_child`]), so no other test's
/// latch can see the signal (review ai-review codex r2: Ctrl+C's path unverified).
#[test]
fn a_real_sigint_presses_the_retry_waits_ctrl_c() {
    use std::io::{BufRead, BufReader};
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "pods::placement::placement_cli_tests::interrupt_child", "--ignored", "--nocapture", "--test-threads=1"])
        .env("ARENA_INTERRUPT_CHILD", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut seen = Vec::new();
    for line in out.by_ref() {
        let line = line.unwrap();
        seen.push(line.clone());
        // (libtest's `test … ... ` has no newline: the line may start with it.)
        if line.ends_with("interrupt-child: listening") {
            break;
        }
    }
    assert!(seen.last().is_some_and(|l| l.ends_with("interrupt-child: listening")), "{seen:?}");
    let sent = std::process::Command::new("kill").args(["-INT", &child.id().to_string()]).status().unwrap();
    assert!(sent.success());
    seen.extend(out.map_while(Result::ok));
    let status = child.wait().unwrap();
    assert!(seen.iter().any(|l| l == "interrupt-child: pressed"), "{seen:?}");
    assert!(status.success(), "the SIGINT was taken, not left to end the process: {status:?} {seen:?}");
}

/// Not a test on its own: [`a_real_sigint_presses_the_retry_waits_ctrl_c`]'s child. Listens
/// as a retry wait does, says so, and reports the press (or gives up after 20 s).
#[tokio::test]
#[ignore = "a child process of a_real_sigint_presses_the_retry_waits_ctrl_c"]
async fn interrupt_child() {
    if std::env::var("ARENA_INTERRUPT_CHILD").is_err() {
        return;
    }
    let ctrl_c = Interrupt::ctrl_c();
    let pressed = ctrl_c.wait(); // arms: the SIGINT listener is registered here
    println!("interrupt-child: listening");
    match tokio::time::timeout(std::time::Duration::from_secs(20), pressed).await {
        Ok(()) => println!("interrupt-child: pressed"),
        Err(_) => println!("interrupt-child: timed out"),
    }
    assert!(ctrl_c.is_set());
}

/// RunPod with every create dry until `wet_at`, next to a Vast that's throttled for the
/// next `vast_throttled` listings (a 429, retryable) — or, `vast_refused`, rejects its key.
struct Flaky {
    wet_at: tokio::time::Instant,
    made: Mutex<Vec<Pod>>,
    calls: Mutex<Vec<String>>,
    vast_throttled: AtomicUsize,
    vast_refused: bool,
}

impl Flaky {
    fn new(dry_for: u64) -> Self {
        Flaky {
            wet_at: tokio::time::Instant::now() + std::time::Duration::from_secs(dry_for),
            made: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            vast_throttled: AtomicUsize::new(0),
            vast_refused: false,
        }
    }
}

#[async_trait]
impl Provider for Flaky {
    fn name(&self) -> &'static str {
        "runpod"
    }
    fn describe(&self, _spec: &PodSpec) -> String {
        String::new()
    }
    async fn list_pods(&self) -> CoreResult<Vec<Pod>> {
        Ok(self.made.lock().unwrap().clone())
    }
    async fn list_by_provider(&self) -> Vec<(String, CoreResult<Vec<Pod>>)> {
        let vast = if self.vast_refused {
            Err(Error::Provider { kind: arena_core::ProviderErrorKind::Auth, message: "vast list HTTP 401".into() })
        } else if self.vast_throttled.load(Ordering::SeqCst) > 0 {
            self.vast_throttled.fetch_sub(1, Ordering::SeqCst);
            Err(Error::Provider { kind: arena_core::ProviderErrorKind::RateLimited, message: "vast list HTTP 429".into() })
        } else {
            Ok(Vec::new())
        };
        vec![("runpod".to_string(), self.list_pods().await), ("vast".to_string(), vast)]
    }
    async fn create_pod(&self, spec: &PodSpec) -> CoreResult<Pod> {
        self.calls.lock().unwrap().push(spec.name.clone());
        if tokio::time::Instant::now() < self.wet_at {
            return Err(Error::capacity("create pod HTTP 500: There are no instances currently available"));
        }
        let pod = Pod { id: format!("id-{}", spec.name), name: spec.name.clone(), provider: "runpod".into(), ..Default::default() };
        self.made.lock().unwrap().push(pod.clone());
        Ok(pod)
    }
    async fn stop_pod(&self, _id: &str) -> CoreResult<()> {
        Ok(())
    }
    async fn restart_pod(&self, _id: &str) -> CoreResult<()> {
        Ok(())
    }
    async fn terminate_pod(&self, _id: &str) -> CoreResult<()> {
        Ok(())
    }
}

/// Review fix: the single-option retry loop (`create`/`up -n` without a `--gpu` list)
/// re-lists like placement's rounds do — a round retries only the confirmed names not made
/// yet (one that exists now is skipped), a listing missing a backend that may answer again
/// skips the round instead of ending the run, and one that won't (a rejected key) ends it.
#[tokio::test(start_paused = true)]
async fn single_option_retry_rounds_relist_and_ride_out_a_throttled_backend() {
    let s = std::time::Duration::from_secs;
    let names = || vec!["arena8-apple".to_string(), "arena8-bloom".to_string()];
    // Dry until t=100. Round 2 (t=60): Vast is throttled through the re-list's retries
    // (15s) — skipped. Meanwhile someone else made arena8-bloom. Round 3 (t=135): apple only.
    let fake = Flaky::new(100);
    fake.vast_throttled.store(5, Ordering::SeqCst);
    let started = tokio::time::Instant::now();
    let other = async {
        tokio::time::sleep(s(30)).await;
        let bloom = Pod { id: "theirs".into(), name: "arena8-bloom".into(), provider: "runpod".into(), ..Default::default() };
        fake.made.lock().unwrap().push(bloom);
    };
    let (config, ov, ctrl_c) = (cfg(), SpecOverrides::default(), Interrupt::manual());
    let (made, ()) =
        tokio::join!(create_with_retry(&fake, &config, names(), 2, &ov, false, 10, 60, &ctrl_c, NO_STREAM), other);
    let made: Vec<String> = made.unwrap().into_iter().map(|m| m.pod.name).collect();
    assert_eq!(made, ["arena8-apple"]);
    assert_eq!(*fake.calls.lock().unwrap(), ["arena8-apple", "arena8-apple"], "t=0 (dry, the round stops), then t=135");
    assert_eq!(started.elapsed(), s(60 + 15 + 60));

    // Vast throttled for good: the window ends the run, with no create after round 1.
    let fake = Flaky::new(0);
    fake.vast_throttled.store(usize::MAX, Ordering::SeqCst);
    let dry = Flaky { wet_at: tokio::time::Instant::now() + s(3600), ..fake };
    let made = create_with_retry(&dry, &cfg(), names(), 2, &SpecOverrides::default(), false, 3, 60, &Interrupt::manual(), NO_STREAM)
        .await
        .unwrap();
    assert!(made.is_empty());
    assert_eq!(dry.calls.lock().unwrap().len(), 1, "only round 1 created");

    // Vast's key rejected: no round runs on a listing that won't become whole.
    let refused = Flaky { vast_refused: true, ..Flaky::new(3600) };
    let err = create_with_retry(&refused, &cfg(), names(), 0, &SpecOverrides::default(), false, 3, 60, &Interrupt::manual(), NO_STREAM)
        .await
        .unwrap_err();
    assert!(err.error.to_string().contains("refusing to create") && err.error.to_string().contains("vast failed to list"), "{}", err.error);
    assert_eq!(refused.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn create_with_a_gpu_list_falls_back_and_later_names_skip_the_dry_pool() {
    let fake = Recorder::dry(&["NVIDIA RTX A4000"]);
    handle_pods(create(&["apple", "bloom"], Some("A4000,3090"), None, None), &fake, fake_remote(), &cfg(), true).await.unwrap();
    assert_eq!(
        fake.calls(),
        [
            call("arena8-apple", "NVIDIA RTX A4000", "COMMUNITY"), // cheapest (~$0.17) first: dry
            call("arena8-apple", "NVIDIA GeForce RTX 3090", "COMMUNITY"),
            call("arena8-bloom", "NVIDIA GeForce RTX 3090", "COMMUNITY"), // A4000 blocked this round
        ]
    );
    assert_eq!(fake.made.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_single_gpu_and_cloud_create_is_unchanged() {
    // One option: the single-spec path — `--keep-trying` still accepted, one create per name.
    let fake = Recorder::default();
    let mut cmd = create(&["apple"], Some("3090"), Some("secure"), None);
    if let PodCmd::Create { keep_trying, order, .. } = &mut cmd {
        *keep_trying = true;
        *order = Order::Listed; // meaningless with one option
    }
    handle_pods(cmd, &fake, fake_remote(), &cfg(), true).await.unwrap();
    assert_eq!(fake.calls(), [call("arena8-apple", "NVIDIA GeForce RTX 3090", "SECURE")]);
}

#[tokio::test]
async fn refusals_happen_before_anything_is_listed_or_created() {
    let fake = Recorder::default();
    let mut kt = create(&["apple"], Some("A4000,3090"), None, None);
    if let PodCmd::Create { keep_trying, .. } = &mut kt {
        *keep_trying = true;
    }
    assert!(handle_pods(kt, &fake, fake_remote(), &cfg(), true).await.is_err());
    let bad_tier = create(&["apple"], Some("A4000"), Some("community,all"), None);
    let e = handle_pods(bad_tier, &fake, fake_remote(), &cfg(), true).await.unwrap_err().to_string();
    assert!(e.contains("`ALL`"), "{e}");
    // A cap nothing fits under (A4000 ~$0.17, 3090 ~$0.22): refused, nothing created.
    let e = handle_pods(create(&["apple"], Some("A4000,3090"), None, Some(0.10)), &fake, fake_remote(), &cfg(), true)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("no option left to try"), "{e}");
    assert!(fake.calls().is_empty());
    assert_eq!(fake.lists.load(Ordering::SeqCst), 1, "only the cap case got as far as listing names");
}

#[tokio::test]
async fn a_price_cap_filters_and_a_dry_run_creates_nothing() {
    // 2 GPUs per pod: A4000 ~$0.34 fits $0.40, 3090 ~$0.44 doesn't.
    let fake = Recorder::default();
    let mut cmd = create(&["apple"], Some("3090,A4000"), None, Some(0.40));
    if let PodCmd::Create { gpus, .. } = &mut cmd {
        *gpus = Some(2);
    }
    handle_pods(cmd, &fake, fake_remote(), &cfg(), true).await.unwrap();
    assert_eq!(fake.calls(), [call("arena8-apple", "NVIDIA RTX A4000", "COMMUNITY")]);

    let fake = Recorder::default();
    let up = PodCmd::Up {
        names: vec![],
        count: Some(2),
        add: None,
        gpu: Some("A4000,3090".into()),
        gpus: None,
        server_type: None,
        cloud: Some("community,secure".into()),
        max_price: Some(0.30),
        order: Order::Listed,
        disk: None,
        volume: None,
        image: None,
        bootstrap: false,
        api_json: None,
        reap: crate::ReapAtCreate::default(),
        owner: crate::spend::OwnerArgs::default(),
        dry_run: true,
        no_wait: false,
        keep_trying: false,
        retry_mins: 0,
        retry_secs: 60,
        no_setup: false,
        check: true,
        check_attempts: 2,
        retry_bad_hosts: false,
        lock: false,
        timeout: 600,
        endpoint_timeout: None,
        interval: 12,
    };
    handle_pods(up, &fake, fake_remote(), &cfg(), true).await.unwrap();
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn up_places_through_the_options_then_carries_on() {
    let fake = Recorder::dry(&["NVIDIA GeForce RTX 3090"]);
    let up = PodCmd::Up {
        names: vec!["apple".into()],
        count: None,
        add: None,
        gpu: Some("3090,A4000".into()),
        gpus: None,
        server_type: None,
        cloud: None,
        max_price: None,
        order: Order::Listed, // 3090 first despite the price
        disk: None,
        volume: None,
        image: None,
        bootstrap: false,
        api_json: None,
        reap: crate::ReapAtCreate::default(),
        owner: crate::spend::OwnerArgs::default(),
        dry_run: false,
        no_wait: true,
        keep_trying: false,
        retry_mins: 0,
        retry_secs: 60,
        no_setup: true,
        check: false,
        check_attempts: 2,
        retry_bad_hosts: false,
        lock: false,
        timeout: 600,
        endpoint_timeout: None,
        interval: 12,
    };
    handle_pods(up, &fake, fake_remote(), &cfg(), true).await.unwrap();
    assert_eq!(
        fake.calls(),
        [call("arena8-apple", "NVIDIA GeForce RTX 3090", "COMMUNITY"), call("arena8-apple", "NVIDIA RTX A4000", "COMMUNITY")]
    );
}

/// The `up` in [`up_places_through_the_options_then_carries_on`]: 3090 then A4000.
fn up_apple() -> PodCmd {
    PodCmd::Up {
        names: vec!["apple".into()],
        count: None,
        add: None,
        gpu: Some("3090,A4000".into()),
        gpus: None,
        server_type: None,
        cloud: None,
        max_price: None,
        order: Order::Listed,
        disk: None,
        volume: None,
        image: None,
        bootstrap: false,
        api_json: None,
        reap: crate::ReapAtCreate::default(),
        owner: crate::spend::OwnerArgs::default(),
        dry_run: false,
        no_wait: true,
        keep_trying: false,
        retry_mins: 0,
        retry_secs: 60,
        no_setup: true,
        check: false,
        check_attempts: 2,
        retry_bad_hosts: false,
        lock: false,
        timeout: 600,
        endpoint_timeout: None,
        interval: 12,
    }
}

/// A balance reading `live`, with a state file of its own (removed on drop): inline
/// surfaces read live only when the state can be saved.
struct TestBal(money::BalanceCtx, std::path::PathBuf);
impl std::ops::Deref for TestBal {
    type Target = money::BalanceCtx;
    fn deref(&self) -> &money::BalanceCtx {
        &self.0
    }
}
impl Drop for TestBal {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.1);
    }
}
fn balance_of(live: arena_core::balance::Live) -> TestBal {
    use arena_core::balance::{Account, FakeBalanceApi};
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!("arena-preflight-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
    let path = Some(Ok(dir.join("devtest").join(arena_core::snapshot::BALANCE_FILE)));
    TestBal(money::BalanceCtx { api: Arc::new(FakeBalanceApi::new().live_ok(Account::Runpod, live)), path }, dir)
}

/// Invariant 2 for `up`/`create`: a critical balance (under RunPod's minimum, under an
/// hour's credit for the new pods) is said next to the plan — and changes nothing: with
/// `--yes` the very same creates run as with no balance at all, dry runs create nothing.
#[tokio::test]
async fn up_preflight_warns_and_still_creates() {
    use arena_core::balance::Live;
    let poor = Live { usd: 0.10, provider_burn_per_hr: Some(0.47), spend_limit_per_hr: Some(80.0), under_balance: Some(true), reduced: false, owed: None };
    let bal = balance_of(poor);
    let plain = Recorder::dry(&["NVIDIA GeForce RTX 3090"]);
    handle_pods(up_apple(), &plain, fake_remote(), &cfg(), true).await.unwrap();
    let fake = Recorder::dry(&["NVIDIA GeForce RTX 3090"]);
    handle_pods_bal(up_apple(), &fake, fake_remote(), &cfg(), true, Interrupt::manual(), &bal).await.unwrap();
    assert_eq!(fake.calls(), plain.calls());
    assert_eq!(fake.calls().len(), 2);
    // `create` likewise; a dry run creates nothing either way.
    let plain = Recorder::default();
    handle_pods(create(&["apple"], None, None, None), &plain, fake_remote(), &cfg(), true).await.unwrap();
    let fake = Recorder::default();
    handle_pods_bal(create(&["apple"], None, None, None), &fake, fake_remote(), &cfg(), true, Interrupt::manual(), &bal).await.unwrap();
    assert_eq!((fake.calls(), fake.calls().len()), (plain.calls(), 1));
    let dry = Recorder::default();
    let mut cmd = create(&["apple"], None, None, None);
    if let PodCmd::Create { dry_run, .. } = &mut cmd {
        *dry_run = true;
    }
    handle_pods_bal(cmd, &dry, fake_remote(), &cfg(), true, Interrupt::manual(), &bal).await.unwrap();
    assert!(dry.calls().is_empty());
    // What it said: the runway after, an hour's credit, underBalance.
    let base = spec_with_overrides(&cfg(), &SpecOverrides::default());
    let req = arena_core::placement::Request::from_flags(Some("3090,A4000"), None, &base, None, Order::Listed).unwrap();
    let plan = plan_placement(&cfg(), "runpod", &req, &base).await.unwrap();
    let lines = money::balance_preflight(&fake, &cfg(), &bal, 1, Some(&plan), &base, None).await;
    // The options' prices are preset estimates (no key): raised to the known GPU's
    // stand-in (RunPod's recorded catalog), marked `~`.
    // The reading's age shows once it's a second old (`live 1s`): a loaded machine gets there
    // between `balance_of` and here, so it's taken out before comparing.
    let first = match lines[0].split_once("live ") {
        Some((head, rest)) => match rest.split_once("s ·") {
            Some((n, tail)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => format!("{head}live ·{tail}"),
            _ => lines[0].clone(),
        },
        None => lines[0].clone(),
    };
    assert!(first.starts_with("balance: runpod $0.10 live · $0.47/h now + ≤~$") && first.ends_with("‼ under 24h"), "{lines:?}");
    assert!(lines.iter().any(|l| l.starts_with("⚠ RunPod needs ≥1h of credit for the new pods (≈~$")), "{lines:?}");
    assert_eq!(lines.last().unwrap(), "‼ RunPod reports the account under its minimum balance — creates will likely fail");
}

#[tokio::test]
async fn create_preflight_credit_and_spend_limit_lines() {
    use arena_core::balance::{BalancePolicy, Live};
    let base = spec_with_overrides(&cfg(), &SpecOverrides::default());
    let req = arena_core::placement::Request::from_flags(Some("A4000,3090"), None, &base, None, Order::Cheapest).unwrap();
    let plan = plan_placement(&cfg(), "runpod", &req, &base).await.unwrap();
    // No key: the options carry preset estimates, so the bound is each option at the most
    // its GPU is known to cost (A4000 $0.32, RTX 3090 $0.50 recorded) — here $0.50.
    let policy = BalancePolicy::default();
    let dearest = plan
        .options
        .iter()
        .map(|o| o.price_per_pod.unwrap_or(0.0).max(arena_core::balance::stand_in_price(Some(&o.gpu), Some(o.gpu_count), &policy).unwrap()))
        .fold(0.0, f64::max);
    assert!((dearest - 0.5).abs() < 1e-9, "{plan:?}");
    // Plenty of money, but 400 new pods at the dearest option would pass the $80/h limit.
    let near_limit = Live { usd: 5_000.0, provider_burn_per_hr: Some(10.0), spend_limit_per_hr: Some(80.0), under_balance: Some(false), reduced: false, owed: None };
    let lines = money::balance_preflight(&Recorder::default(), &cfg(), &balance_of(near_limit), 400, Some(&plan), &base, None).await;
    let want = format!(
        "⚠ planned ~${:.2}/h exceeds RunPod's spend limit $80.00/h — creates past it will be refused",
        10.0 + 400.0 * dearest
    );
    assert!(lines.contains(&want), "{lines:?}");
    assert!(!lines.iter().any(|l| l.contains("1h of credit")), "{lines:?}");
    // $1 for 10 new pods: under an hour's credit.
    let thin = Live { usd: 1.0, provider_burn_per_hr: Some(0.0), spend_limit_per_hr: Some(80.0), under_balance: Some(false), reduced: false, owed: None };
    let lines = money::balance_preflight(&Recorder::default(), &cfg(), &balance_of(thin.clone()), 10, Some(&plan), &base, None).await;
    let credit = format!(
        "⚠ RunPod needs ≥1h of credit for the new pods (≈~${:.2}); the account has $1.00 — creates will likely fail (402)",
        10.0 * dearest
    );
    assert!(lines.contains(&credit), "{lines:?}");
    // The single-spec path: nothing priced the A4000 — it stands in at the most it is
    // known to cost ($0.32/h), never at $0: the runway and the credit check use it, `~`.
    let lines = money::balance_preflight(&Recorder::default(), &cfg(), &balance_of(thin.clone()), 10, None, &base, None).await;
    assert_eq!(
        lines,
        [
            "balance: runpod $1.00 live · $0.00/h now + ≤~$3.20/h for 10 new pod(s) (a stand-in that errs high — no live price) → <1h ‼ under 24h",
            "⚠ RunPod needs ≥1h of credit for the new pods (≈~$3.20); the account has $1.00 — creates will likely fail (402)",
        ]
    );
    // An option nobody priced counts at its stand-in in the `≤` bound (it may be the
    // dearest): an H100 SXM at RunPod's recorded $3.49, never the June preset's $2.79.
    let mut unpriced = plan.clone();
    unpriced.options[0].price_per_pod = None;
    unpriced.options[0].gpu = "NVIDIA H100 80GB HBM3".into();
    let lines = money::balance_preflight(&Recorder::default(), &cfg(), &balance_of(thin.clone()), 10, Some(&unpriced), &base, None).await;
    assert!(lines[0].contains("+ ≤~$34.90/h for 10 new pod(s)"), "{lines:?}");
    // A live catalog price is taken as it is (it's what RunPod charges right now).
    let mut live_priced = plan.clone();
    for o in live_priced.options.iter_mut() {
        (o.price_per_pod, o.price_source) = (Some(0.2), Some(arena_core::placement::PriceSource::Live));
    }
    let lines = money::balance_preflight(&Recorder::default(), &cfg(), &balance_of(thin), 10, Some(&live_priced), &base, None).await;
    assert!(lines[0].contains("+ ≤$2.00/h for 10 new pod(s) →"), "{lines:?}");
    // No account for this provider configured: nothing.
    assert!(money::balance_preflight(&Recorder::default(), &cfg(), &money::BalanceCtx::from_config(&cfg()), 10, Some(&plan), &base, None).await.is_empty());
}

/// Merge of wave 7 balance onto lane G's `up`: every path that makes a pod remembers the
/// price its create answered with, for the balance (a later listing with no price —
/// RunPod v1's REST list — still prices the pod): `create`, `up --no-wait`, and the
/// streaming `up` whose pipelines start as each create returns — single spec and
/// placement alike. A create with no price remembers nothing.
#[tokio::test]
async fn every_create_path_remembers_its_price_for_the_balance() {
    use arena_core::balance::{self, Account, Live};
    let live = Live { usd: 100.0, provider_burn_per_hr: Some(0.0), spend_limit_per_hr: None, under_balance: None, reduced: false, owed: None };
    let bal = balance_of(live);
    let remembered = |id: &str| {
        let (state, _) = balance::load(bal.state_path().unwrap());
        state.accounts.get(&Account::Runpod).and_then(|a| a.prices.get(id)).map(|(usd, _)| *usd)
    };
    let streaming = |gpu: Option<&str>| {
        let mut up = up_apple();
        if let PodCmd::Up { gpu: g, no_wait, timeout, interval, .. } = &mut up {
            (*g, *no_wait, *timeout, *interval) = (gpu.map(String::from), false, 60, 1);
        }
        up
    };
    let cases: [(&str, PodCmd, f64); 5] = [
        ("create, single spec", create(&["apple"], None, None, None), 0.21),
        ("create, placement", create(&["apple"], Some("3090,A4000"), None, None), 0.22),
        ("up --no-wait, placement", up_apple(), 0.23),
        ("up, placement (streaming)", streaming(Some("3090,A4000")), 0.24),
        ("up, single spec (streaming)", streaming(None), 0.25),
    ];
    for (case, cmd, price) in cases {
        let rec = Recorder { price: Some(price), up_at_once: true, ..Default::default() };
        handle_pods_bal(cmd, &rec, fake_remote(), &cfg(), true, Interrupt::manual(), &bal).await.unwrap_or_else(|e| panic!("{case}: {e:#}"));
        let made = rec.made.lock().unwrap().clone();
        assert_eq!(made.len(), 1, "{case}");
        assert_eq!(remembered(&made[0].id), Some(price), "{case}");
    }
    // Unpriced: nothing new remembered (the last case's price stays as it was).
    let rec = Recorder { up_at_once: true, ..Default::default() };
    handle_pods_bal(streaming(None), &rec, fake_remote(), &cfg(), true, Interrupt::manual(), &bal).await.unwrap();
    assert_eq!(remembered("id-arena8-apple"), Some(0.25));
}

#[tokio::test]
async fn placement_plans_without_a_fleet_or_a_key() {
    // `offers` itself (every provider, `--all`, `gpus`): `offers::tests`.
    let base = spec_with_overrides(&cfg(), &SpecOverrides { gpus: Some(2), ..Default::default() });
    let req = arena_core::placement::Request::from_flags(Some("3090,A4000"), None, &base, Some(0.40), Order::Cheapest).unwrap();
    let plan = plan_placement(&cfg(), "runpod", &req, &base).await.unwrap();
    assert_eq!(plan.options.len(), 1);
    assert_eq!(plan.notes[0], "no RUNPOD_API_KEY — prices are preset estimates (~)");
    assert_eq!(
        plan_header(&plan),
        "Placement options on runpod (2 GPU(s) per pod, cheapest first, max $0.40/h per pod):"
    );
    // Vast without a key can't search: unpriced (so a cap leaves nothing), and it says why.
    let vast = plan_placement(&cfg(), "vast", &req, &base).await.unwrap();
    assert!(vast.options.is_empty() && vast.dropped.len() == 2, "{vast:?}");
    assert_eq!(vast.notes[0], "no VAST_API_KEY — vast options are unpriced (--max-price can't check them)");
}

/// The shared planner applies NEAR itself, so `offers` (whatever shape it takes) shows the
/// order create/up run: SECURE options in Europe first; a community-only plan says why
/// nothing is near; `NEAR=any` plans as before.
#[tokio::test]
async fn the_shared_planner_puts_near_options_first() {
    use arena_core::near::Region;
    let base = spec_with_overrides(&cfg(), &SpecOverrides::default());
    let req = |cloud: &str| arena_core::placement::Request::from_flags(Some("A4000"), Some(cloud), &base, None, Order::Cheapest).unwrap();
    let plan = plan_placement(&cfg(), "runpod", &req("secure"), &base).await.unwrap();
    let near: Vec<Option<Region>> = plan.options.iter().map(|o| o.near).collect();
    assert_eq!(near, [Some(Region::Europe), None], "{plan:?}");
    assert!(arena_core::placement::render_plan(&plan).contains("WHERE"));
    let community = plan_placement(&cfg(), "runpod", &req("community"), &base).await.unwrap();
    assert_eq!(community.options.len(), 1);
    assert!(community.notes.iter().any(|n| n.starts_with("near (Europe): RunPod's COMMUNITY cloud can't be placed by data centre")), "{:?}", community.notes);
    let off = Config::parse("MACHINE_NAME_PREFIX=arena8\nGPU_TYPE=\"NVIDIA RTX A4000\"\nNEAR=any\n");
    let plain = plan_placement(&off, "runpod", &req("secure"), &base).await.unwrap();
    assert!(plain.near.is_none() && plain.options.len() == 1 && !plain.notes.iter().any(|n| n.contains("near")), "{plain:?}");
    // The operator's own data-centre pin: no near option, and a note says so.
    let pinned = PodSpec { api_extra: serde_json::json!({"dataCenterIds": ["US-TX-3"]}).as_object().cloned(), ..base.clone() };
    let own = plan_placement(&cfg(), "runpod", &req("secure"), &pinned).await.unwrap();
    assert_eq!(own.options.len(), 1);
    assert!(own.notes.iter().any(|n| n.starts_with("near (Europe): off — CREATE_EXTRA_JSON / --api-json already says where")), "{:?}", own.notes);
}

#[test]
fn placement_flags_parse() {
    use clap::Parser;
    let parse = |args: &[&str]| Cli::try_parse_from(args).map(|c| c.cmd);
    match parse(&["arena", "offers", "--gpu", "A4000,3090", "--max-price", "$0.5", "--gpus", "2", "--order", "listed", "--json"]).unwrap() {
        Cmd::Offers { gpu, max_price, gpus, order, all, json, .. } => {
            assert_eq!((gpu.as_deref(), max_price, gpus, order, all, json), (Some("A4000,3090"), Some(0.5), Some(2), Order::Listed, false, true));
        }
        _ => panic!("not offers"),
    }
    match parse(&["arena", "offers", "--provider", "hetzner", "--server-type", "cx33"]).unwrap() {
        Cmd::Offers { server_type, all, .. } => assert_eq!((server_type.as_deref(), all), (Some("cx33"), false)),
        _ => panic!("not offers"),
    }
    // A view is a provider (or `all`), and takes the filters; `--all` is `all`'s old
    // spelling, never beside a view; `-v` the notes.
    for view in offers::VIEWS {
        match parse(&["arena", "offers", view, "--gpu", "A4000", "--cloud", "secure", "--max-price", "1", "-v"]).unwrap() {
            Cmd::Offers { view: v, gpu, cloud, max_price, verbose, all, .. } => {
                assert_eq!((v.as_deref(), gpu.as_deref(), cloud.as_deref(), max_price, verbose, all), (Some(view), Some("A4000"), Some("secure"), Some(1.0), true, false));
            }
            _ => panic!("not offers"),
        }
    }
    assert!(matches!(parse(&["arena", "offers", "--all", "--json", "--gpu", "B300 MIG"]).unwrap(), Cmd::Offers { all: true, json: true, view: None, .. }));
    assert!(parse(&["arena", "offers", "lambda"]).is_err());
    assert!(parse(&["arena", "offers", "runpod", "--all"]).is_err());
    match parse(&["arena", "pods", "create", "-n", "3", "--gpu", "A4000,3090", "--cloud", "community,secure"]).unwrap() {
        Cmd::Pods(PodCmd::Create { order, max_price, .. }) => assert_eq!((order, max_price), (Order::Cheapest, None)),
        _ => panic!("not create"),
    }
    assert!(matches!(parse(&["arena", "pods", "up", "-n", "1", "--max-price", "0.3"]).unwrap(), Cmd::Pods(PodCmd::Up { max_price: Some(_), .. })));
    for bad in [&["arena", "offers", "--max-price", "0"][..], &["arena", "offers", "--max-price", "cheap"], &["arena", "offers", "--order", "fastest"]] {
        assert!(parse(bad).is_err(), "{bad:?}");
    }
}
