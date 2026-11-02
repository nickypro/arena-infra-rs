//! Golden tests of `pods up`'s output (docs/OUTPUT.md): what a terminal shows, at default and
//! at `-v`, for whole runs against fakes — one that goes well, capacity short for a few
//! rounds, a setup that fails, and a pod that never gets an SSH endpoint and is replaced. The
//! clock is paused, so every time in them is exact; the prompt is answered `y` as typed.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use arena_core::remote::{FakeRemote, FakeReply};
use arena_core::Config;

use super::tests::{cfg_text, host, tmp_dir, Fleet, TmpDir};
use crate::cli::PodCmd;
use crate::output_test::rendered;
use crate::{money, Interrupt};

/// A fleet's files in a temp dir: the config (parsed, as if from `config.env` there), its
/// `keys/` with an OpenAI key per name, and a state dir of its own (balance, health cache).
struct FleetDir {
    dir: TmpDir,
    cfg: Config,
}

fn fleet_dir(tag: &str) -> FleetDir {
    let dir = tmp_dir(&format!("golden-{tag}"));
    let keys = dir.0.join("keys");
    std::fs::create_dir_all(&keys).unwrap();
    std::fs::write(keys.join("openai_api_keys.csv"), "host,key\ndevtest-apple,sk-a\ndevtest-bloom,sk-b\ndevtest-cloud,sk-c\n").unwrap();
    let text = format!("{}NEAR=any\nARENA_STATE_DIR={}\n", cfg_text(None), dir.0.join("state").display());
    let cfg = Config::parse(&text).with_source(&dir.0.join("config.env"));
    FleetDir { dir, cfg }
}

/// RunPod's balance, read live: $120.00, burning $0.50/h.
fn balance(fd: &FleetDir) -> money::BalanceCtx {
    use arena_core::balance::{Account, FakeBalanceApi, Live};
    let live = Live { usd: 120.0, provider_burn_per_hr: Some(0.5), spend_limit_per_hr: None, under_balance: Some(false), reduced: false, owed: None };
    let path = fd.dir.0.join("state").join("devtest").join(arena_core::snapshot::BALANCE_FILE);
    money::BalanceCtx { api: Arc::new(FakeBalanceApi::new().live_ok(Account::Runpod, live)), path: Some(Ok(path)) }
}

/// `pods up -a N [--retry-mins M]`: setup, keys, no check.
fn up(add: usize, retry_mins: u64) -> PodCmd {
    PodCmd::Up {
        names: Vec::new(),
        count: None,
        add: Some(add),
        gpu: None,
        gpus: None,
        server_type: None,
        cloud: None,
        max_price: None,
        order: Default::default(),
        disk: None,
        volume: None,
        image: None,
        bootstrap: false,
        api_json: None,
        reap: crate::ReapAtCreate::default(),
        owner: crate::spend::OwnerArgs::default(),
        dry_run: false,
        no_wait: false,
        keep_trying: false,
        retry_mins,
        retry_secs: 60,
        no_setup: false,
        check: false,
        check_attempts: 2,
        retry_bad_hosts: false,
        lock: false,
        timeout: 600,
        endpoint_timeout: None,
        interval: 10,
    }
}

/// `pods create apple --gpu A4000,3090`: two options, tried in order.
fn create_on_two() -> PodCmd {
    PodCmd::Create {
        names: vec!["apple".into()],
        count: None,
        add: None,
        gpu: Some("A4000,3090".into()),
        gpus: None,
        server_type: None,
        cloud: None,
        max_price: None,
        order: arena_core::placement::Order::Listed,
        disk: None,
        volume: None,
        image: None,
        bootstrap: false,
        api_json: None,
        reap: crate::ReapAtCreate::default(),
        owner: crate::spend::OwnerArgs::default(),
        skip_proxy: false,
        dry_run: false,
        keep_trying: false,
        retry_mins: 0,
        retry_secs: 60,
    }
}

/// `cmd` run on a fresh fake (`fleet()`, `remote()`) at default and at `-v`: what each showed.
async fn both(tag: &str, cmd: impl Fn() -> PodCmd, fleet: impl Fn() -> Fleet, remote: impl Fn() -> FakeRemote) -> (String, String) {
    let mut out = Vec::new();
    for verbose in [false, true] {
        let fd = fleet_dir(&format!("{tag}-{verbose}"));
        let bal = balance(&fd);
        let (fleet, remote) = (fleet(), Arc::new(remote()));
        let run = crate::pods::handle_pods_bal(cmd(), &fleet, remote, &fd.cfg, false, Interrupt::manual(), &bal);
        out.push(normalise(&rendered(verbose, &["y"], run).await));
    }
    let loud = out.pop().unwrap();
    (out.pop().unwrap(), loud)
}

/// The balance reading's age (`live 1s`) is the wall clock's, not the paused one.
fn normalise(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let line = match line.split_once(" live ") {
            Some((head, rest)) if rest.split_once(' ').is_some_and(|(age, _)| age.trim_end_matches(['s', 'm', 'h']).bytes().all(|b| b.is_ascii_digit())) => {
                format!("{head} live {}", rest.split_once(' ').map_or("", |(_, r)| r))
            }
            _ => line.to_string(),
        };
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Two pods, each READY on its own: at default, the plan in a line, the balance, the prompt, a
/// line per created pod and per READY one, and the tally.
#[tokio::test(start_paused = true)]
async fn up_that_goes_well() {
    let fleet = || Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0)]), ("bloom", &[("10.0.0.2", 22002, 30)])]);
    let (quiet, loud) = both("well", || up(2, 0), fleet, FakeRemote::new).await;
    assert_eq!(quiet, QUIET_UP_THAT_GOES_WELL.trim_start(), "default:\n{quiet}");
    assert_eq!(loud, LOUD_UP_THAT_GOES_WELL.trim_start(), "-v:\n{loud}");
}

/// No capacity for three rounds, then the pod: one line per round (the dashboard reads them;
/// a terminal shows one, updated in place), never the provider's error again — that's `-v`'s.
#[tokio::test(start_paused = true)]
async fn up_waiting_for_capacity() {
    let fleet = || {
        let f = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0)])]);
        f.dry_creates.store(3, Ordering::SeqCst);
        f
    };
    let (quiet, loud) = both("capacity", || up(1, 10), fleet, FakeRemote::new).await;
    assert_eq!(quiet, QUIET_UP_WAITING_FOR_CAPACITY.trim_start(), "default:\n{quiet}");
    assert_eq!(loud, LOUD_UP_WAITING_FOR_CAPACITY.trim_start(), "-v:\n{loud}");
}

/// One pod's setup fails: its FAILED line says why and what to run; the other is READY; the
/// error says which isn't.
#[tokio::test(start_paused = true)]
async fn up_with_a_failed_setup() {
    let fleet = || Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0)]), ("bloom", &[("10.0.0.2", 22002, 0)])]);
    let remote = || {
        let fake = FakeRemote::new();
        fake.script(&host("10.0.0.2", 22002), [FakeReply::ok(), FakeReply::exit(128, "fatal: could not read from remote repository.")]);
        fake
    };
    let (quiet, loud) = both("setup", || up(2, 0), fleet, remote).await;
    assert_eq!(quiet, QUIET_UP_WITH_A_FAILED_SETUP.trim_start(), "default:\n{quiet}");
    assert_eq!(loud, LOUD_UP_WITH_A_FAILED_SETUP.trim_start(), "-v:\n{loud}");
}

/// `pods create` with two options, the first out of capacity: the options as a compact table
/// (their notes are `-v`'s), the prompt, and the pod — not the option that had no capacity.
#[tokio::test(start_paused = true)]
async fn create_on_the_second_of_two_options() {
    let fleet = || {
        let f = Fleet::new(&[("apple", &[("10.0.0.1", 22001, 0)])]);
        f.dry_creates.store(1, Ordering::SeqCst);
        f
    };
    let (quiet, loud) = both("create", create_on_two, fleet, FakeRemote::new).await;
    assert_eq!(quiet, QUIET_CREATE.trim_start(), "default:\n{quiet}");
    assert_eq!(loud, LOUD_CREATE.trim_start(), "-v:\n{loud}");
}

/// A pod up behind RunPod's SSH proxy only, for the endpoint timeout: one line says it's
/// replaced and why; the new pod is READY.
#[tokio::test(start_paused = true)]
async fn up_replacing_a_pod_with_no_endpoint() {
    let fleet = || Fleet::new(&[("apple", &[("proxy", 0, 5), ("10.0.0.2", 22002, 0)])]);
    let (quiet, loud) = both("endpoint", || up(1, 0), fleet, FakeRemote::new).await;
    assert_eq!(quiet, QUIET_UP_REPLACING_A_POD_WITH_NO_ENDPOINT.trim_start(), "default:\n{quiet}");
    assert_eq!(loud, LOUD_UP_REPLACING_A_POD_WITH_NO_ENDPOINT.trim_start(), "-v:\n{loud}");
}

const QUIET_UP_THAT_GOES_WELL: &str = r#"
1×RTX A4000 · RunPod COMMUNITY
Balance $120.00 → ≈4.3d left
Create devtest-apple, devtest-bloom (≤~$0.64/h), replacing any without SSH? [y/N] y
[created] devtest-apple · 1×RTX A4000 COMMUNITY · $0.17/h
[created] devtest-bloom · 1×RTX A4000 COMMUNITY · $0.17/h
[devtest-apple] READY after 0s · keys ✓
[devtest-bloom] READY after 30s · keys ✓
✓ 2 of 2 READY
"#;

const QUIET_UP_WAITING_FOR_CAPACITY: &str = r#"
1×RTX A4000 · RunPod COMMUNITY
Balance $120.00 → ≈6.0d left
Create devtest-apple (≤~$0.32/h), retrying up to 10m, replacing any without SSH? [y/N] y
round 1: 1 short of target 1 (capacity); retrying in 60s (Ctrl+C to stop)…
round 2: 1 short of target 1 (capacity); retrying in 60s (Ctrl+C to stop)…
round 3: 1 short of target 1 (capacity); retrying in 60s (Ctrl+C to stop)…
[created] devtest-apple · 1×RTX A4000 COMMUNITY · $0.17/h
[devtest-apple] READY after 0s · keys ✓
✓ 1 of 1 READY
"#;

const QUIET_UP_WITH_A_FAILED_SETUP: &str = r#"
1×RTX A4000 · RunPod COMMUNITY
Balance $120.00 → ≈4.3d left
Create devtest-apple, devtest-bloom (≤~$0.64/h), replacing any without SSH? [y/N] y
[created] devtest-apple · 1×RTX A4000 COMMUNITY · $0.17/h
[created] devtest-bloom · 1×RTX A4000 COMMUNITY · $0.17/h
[devtest-apple] READY after 0s · keys ✓
[devtest-bloom] FAILED setup: failed at repo + keys config, exit 128: fatal: could not read from remote repository. — left running (retry: `arena pods setup devtest-bloom`)
Error: 1 of 2 pod(s) not ready: devtest-bloom (FAILED setup)
"#;

const QUIET_UP_REPLACING_A_POD_WITH_NO_ENDPOINT: &str = r#"
1×RTX A4000 · RunPod COMMUNITY
Balance $120.00 → ≈6.0d left
Create devtest-apple (≤~$0.32/h), replacing any without SSH? [y/N] y
[created] devtest-apple · 1×RTX A4000 COMMUNITY · $0.17/h
[devtest-apple] no SSH endpoint — replacing it (attempt 2 of 2)
[created] devtest-apple · 1×RTX A4000 COMMUNITY · $0.17/h
[devtest-apple] READY after 5m10s · keys ✓
✓ 1 of 1 READY
"#;

const LOUD_UP_WITH_A_FAILED_SETUP: &str = r#"
balance: runpod $120.00 live · $0.50/h now + ≤~$0.64/h for 2 new pod(s) (a stand-in that errs high — no live price) → ≈4.3d
Will create 2 pod(s) on runpod (NVIDIA RTX A4000 x1, COMMUNITY, disk 100GB), then per pod, as soon as it can: wait for its SSH endpoint — a pod up without one for 5 min (RunPod: behind its SSH proxy only), or a RunPod pod with no container at --timeout (10 min or more), is TERMINATED and its name recreated (up to 2 placement(s) per name), sync the proxy (if nginx is set up), provision it, copy its API keys — then READY/FAILED per pod:
  devtest-apple, devtest-bloom
Proceed? [y/N] y
Broadcast tokens: Hugging Face ✗ (✓ exported on each pod; ✗ skipped)
API keys: OpenAI (3 hosts) — copied to each pod once it's set up

Each pod is brought up on its own the moment it's created (endpoint wait ≤600s per pod, one up without an endpoint for 5 min is replaced; Ctrl+C stops — nothing is terminated by it)…
[created] devtest-apple id=id1 on 1×RTX A4000 COMMUNITY ($0.17/h)
[created] devtest-bloom id=id2 on 1×RTX A4000 COMMUNITY ($0.17/h)

Created 2 of 2 requested.
[devtest-apple] endpoint 10.0.0.1:22001 (after 0s)
[devtest-apple] setup ✓ (0s)
[devtest-apple] keys ✓
[devtest-apple] READY after 0s — 1×RTX A4000 COMMUNITY
[devtest-bloom] endpoint 10.0.0.2:22002 (after 0s)
[devtest-bloom] FAILED setup: failed at repo + keys config, exit 128: fatal: could not read from remote repository. — left running (retry: `arena pods setup devtest-bloom`)
[proxy] after up: skipped — no proxy configured (SSH_PROXY_HOST unset)

NAME           GPU            $/H  PROXY PORT  HEALTH  STATUS        READY AFTER
devtest-apple  1×RTX A4000  $0.17           -  -       READY                  0s
devtest-bloom  1×RTX A4000  $0.17           -  -       FAILED setup            -

devtest-bloom: FAILED setup — failed at repo + keys config, exit 128: fatal: could not read from remote repository. — left running (retry: `arena pods setup devtest-bloom`)

1 ready, 1 failed (of 2) — READY AFTER runs from the name's first create, replacements included
Error: 1 of 2 pod(s) not ready: devtest-bloom (FAILED setup)
"#;

const LOUD_UP_REPLACING_A_POD_WITH_NO_ENDPOINT: &str = r#"
balance: runpod $120.00 live · $0.50/h now + ≤~$0.32/h for 1 new pod(s) (a stand-in that errs high — no live price) → ≈6.0d
Will create 1 pod(s) on runpod (NVIDIA RTX A4000 x1, COMMUNITY, disk 100GB), then per pod, as soon as it can: wait for its SSH endpoint — a pod up without one for 5 min (RunPod: behind its SSH proxy only), or a RunPod pod with no container at --timeout (10 min or more), is TERMINATED and its name recreated (up to 2 placement(s) per name), sync the proxy (if nginx is set up), provision it, copy its API keys — then READY/FAILED per pod:
  devtest-apple
Proceed? [y/N] y
Broadcast tokens: Hugging Face ✗ (✓ exported on each pod; ✗ skipped)
API keys: OpenAI (3 hosts) — copied to each pod once it's set up

Each pod is brought up on its own the moment it's created (endpoint wait ≤600s per pod, one up without an endpoint for 5 min is replaced; Ctrl+C stops — nothing is terminated by it)…
[created] devtest-apple id=id1 on 1×RTX A4000 COMMUNITY ($0.17/h)

Created 1 of 1 requested.
[devtest-apple] no public endpoint (RunPod ssh proxy only) for 5m00s (its host has no public IP — no scp/rsync/port-forwards) — runpod names no machine or data centre for it: a host failure
[devtest-apple] terminating id1 to recreate devtest-apple (attempt 2 of 2)
[devtest-apple] recreating it — trying 1×RTX A4000 COMMUNITY first
[created] devtest-apple id=id2 on 1×RTX A4000 COMMUNITY (price unknown)
[devtest-apple] endpoint 10.0.0.2:22002 (after 0s)
[devtest-apple] setup ✓ (0s)
[devtest-apple] keys ✓
[devtest-apple] READY after 5m10s — 1×RTX A4000 COMMUNITY
[proxy] after up: skipped — no proxy configured (SSH_PROXY_HOST unset)

NAME           GPU            $/H  PROXY PORT  HEALTH  STATUS  READY AFTER
devtest-apple  1×RTX A4000  $0.17           -  -       READY         5m10s

devtest-apple: #1 1×RTX A4000 COMMUNITY (id1): no public SSH endpoint (RunPod ssh proxy only); terminated → #2 1×RTX A4000 COMMUNITY (id2, 10.0.0.2): ready

1 ready, 0 failed (of 1) — READY AFTER runs from the name's first create, replacements included
"#;

const LOUD_UP_THAT_GOES_WELL: &str = r#"
balance: runpod $120.00 live · $0.50/h now + ≤~$0.64/h for 2 new pod(s) (a stand-in that errs high — no live price) → ≈4.3d
Will create 2 pod(s) on runpod (NVIDIA RTX A4000 x1, COMMUNITY, disk 100GB), then per pod, as soon as it can: wait for its SSH endpoint — a pod up without one for 5 min (RunPod: behind its SSH proxy only), or a RunPod pod with no container at --timeout (10 min or more), is TERMINATED and its name recreated (up to 2 placement(s) per name), sync the proxy (if nginx is set up), provision it, copy its API keys — then READY/FAILED per pod:
  devtest-apple, devtest-bloom
Proceed? [y/N] y
Broadcast tokens: Hugging Face ✗ (✓ exported on each pod; ✗ skipped)
API keys: OpenAI (3 hosts) — copied to each pod once it's set up

Each pod is brought up on its own the moment it's created (endpoint wait ≤600s per pod, one up without an endpoint for 5 min is replaced; Ctrl+C stops — nothing is terminated by it)…
[created] devtest-apple id=id1 on 1×RTX A4000 COMMUNITY ($0.17/h)
[created] devtest-bloom id=id2 on 1×RTX A4000 COMMUNITY ($0.17/h)

Created 2 of 2 requested.
[devtest-apple] endpoint 10.0.0.1:22001 (after 0s)
[devtest-apple] setup ✓ (0s)
[devtest-apple] keys ✓
[devtest-apple] READY after 0s — 1×RTX A4000 COMMUNITY
[devtest-bloom] endpoint 10.0.0.2:22002 (after 30s)
[devtest-bloom] setup ✓ (0s)
[devtest-bloom] keys ✓
[devtest-bloom] READY after 30s — 1×RTX A4000 COMMUNITY
[proxy] after up: skipped — no proxy configured (SSH_PROXY_HOST unset)
[devtest-apple] fleet SSH map updated with the pods that came up after it

NAME           GPU            $/H  PROXY PORT  HEALTH  STATUS  READY AFTER
devtest-apple  1×RTX A4000  $0.17           -  -       READY            0s
devtest-bloom  1×RTX A4000  $0.17           -  -       READY           30s

2 ready, 0 failed (of 2) — READY AFTER runs from the name's first create, replacements included
"#;

const LOUD_UP_WAITING_FOR_CAPACITY: &str = r#"
balance: runpod $120.00 live · $0.50/h now + ≤~$0.32/h for 1 new pod(s) (a stand-in that errs high — no live price) → ≈6.0d
Will create 1 pod(s) on runpod (NVIDIA RTX A4000 x1, COMMUNITY, disk 100GB), retrying up to 10m, then per pod, as soon as it can: wait for its SSH endpoint — a pod up without one for 5 min (RunPod: behind its SSH proxy only), or a RunPod pod with no container at --timeout (10 min or more), is TERMINATED and its name recreated (up to 2 placement(s) per name), sync the proxy (if nginx is set up), provision it, copy its API keys — then READY/FAILED per pod:
  devtest-apple
Proceed? [y/N] y
Broadcast tokens: Hugging Face ✗ (✓ exported on each pod; ✗ skipped)
API keys: OpenAI (3 hosts) — copied to each pod once it's set up

Each pod is brought up on its own the moment it's created (endpoint wait ≤600s per pod, one up without an endpoint for 5 min is replaced; Ctrl+C stops — nothing is terminated by it)…
[stop] no capacity for this GPU right now: provider error: create pod HTTP 400 Bad Request: {"detail":"There are no longer any instances available with the requested specifications. Please refresh and try again.","status":400,"title":"Bad Request"} (created 0/1). Re-run with --retry-mins N (or --keep-trying) to wait for it to free up.

Created 0 of 1 requested.
round 1: 1 short of target 1 (capacity); retrying in 60s (Ctrl+C to stop)…
[stop] no capacity for this GPU right now: provider error: create pod HTTP 400 Bad Request: {"detail":"There are no longer any instances available with the requested specifications. Please refresh and try again.","status":400,"title":"Bad Request"} (created 0/1). Re-run with --retry-mins N (or --keep-trying) to wait for it to free up.

Created 0 of 1 requested.
round 2: 1 short of target 1 (capacity); retrying in 60s (Ctrl+C to stop)…
[stop] no capacity for this GPU right now: provider error: create pod HTTP 400 Bad Request: {"detail":"There are no longer any instances available with the requested specifications. Please refresh and try again.","status":400,"title":"Bad Request"} (created 0/1). Re-run with --retry-mins N (or --keep-trying) to wait for it to free up.

Created 0 of 1 requested.
round 3: 1 short of target 1 (capacity); retrying in 60s (Ctrl+C to stop)…
[created] devtest-apple id=id1 on 1×RTX A4000 COMMUNITY ($0.17/h)

Created 1 of 1 requested.
[devtest-apple] endpoint 10.0.0.1:22001 (after 0s)
[devtest-apple] setup ✓ (0s)
[devtest-apple] keys ✓
[devtest-apple] READY after 0s — 1×RTX A4000 COMMUNITY
[proxy] after up: skipped — no proxy configured (SSH_PROXY_HOST unset)

NAME           GPU            $/H  PROXY PORT  HEALTH  STATUS  READY AFTER
devtest-apple  1×RTX A4000  $0.17           -  -       READY            0s

1 ready, 0 failed (of 1) — READY AFTER runs from the name's first create, replacements included
"#;

const QUIET_CREATE: &str = r#"
warning: couldn't check --gpu `A4000,3090` against RunPod's live GPU catalog (no RUNPOD_API_KEY) — using it unchecked
RunPod, 2 options, tried in order:
#  OPTION       CLOUD         $/H  STOCK
1  1×RTX A4000  COMMUNITY  ~$0.17  -
2  1×RTX 3090   COMMUNITY  ~$0.22  -
Balance $120.00 → ≈5.0d left
Create devtest-apple (≤~$0.50/h)? [y/N] y
[created] devtest-apple · 1×RTX 3090 COMMUNITY · $0.17/h
"#;

const LOUD_CREATE: &str = r#"
warning: couldn't check --gpu `A4000,3090` against RunPod's live GPU catalog (no RUNPOD_API_KEY) — using it unchecked
Placement options on runpod (1 GPU(s) per pod, as listed):
#  OPTION       CLOUD      $/H/POD  PRICE     STOCK
1  1×RTX A4000  COMMUNITY   ~$0.17  estimate  -
2  1×RTX 3090   COMMUNITY   ~$0.22  estimate  -
note: no RUNPOD_API_KEY — prices are preset estimates (~)
note: ~ = preset estimate (no live price for that GPU)
balance: runpod $120.00 live · $0.50/h now + ≤~$0.50/h for 1 new pod(s) (a stand-in that errs high — no live price) → ≈5.0d
Will create up to 1 pod(s) on runpod, each name trying the 2 option(s) above in order, one create at a time (option 1: NVIDIA RTX A4000 x1, COMMUNITY, disk 100GB), then sync the proxy:
  devtest-apple
Proceed? [y/N] y
[no capacity] devtest-apple on 1×RTX A4000 COMMUNITY: provider error: create pod HTTP 400 Bad Request: {"detail":"There are no longer any instances available with the requested specifications. Please refresh and try again.","status":400,"title":"Bad Request"} — trying the next option
[created] devtest-apple id=id1 on 1×RTX 3090 COMMUNITY (~$0.22/h)

NAME           PLACEMENT
devtest-apple  created on 1×RTX 3090 COMMUNITY (~$0.22/h) (after 1×RTX A4000 COMMUNITY: capacity)
Placed 1 of 1.
[proxy] after create: skipped — no proxy configured (SSH_PROXY_HOST unset)
"#;
