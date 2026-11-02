#[test]
fn any_argument_stops_the_dashboard_before_it_reads_a_config() {
    let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(super::args_refusal(&a(&[])), None);
    for help in ["-h", "--help"] {
        let (code, msg) = super::args_refusal(&a(&[help])).unwrap();
        assert_eq!(code, 0);
        assert!(msg.contains("ARENA_CONFIG") && msg.contains("arena [--config <file>]"), "{msg}");
    }
    for argv in [&["--config", "/home/dev/sandbox/config.env"][..], &["--help", "x"], &["tui"], &["-y"]] {
        let (code, msg) = super::args_refusal(&a(argv)).unwrap();
        assert_eq!(code, 2, "{argv:?}");
        assert!(msg.starts_with(&format!("arena-tui: unexpected argument {:?}", argv[0])), "{msg}");
    }
}

use super::*;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use arena_core::balance;
use arena_core::health::PodHealth;
use arena_core::selector::Naming;
use arena_core::snapshot::HealthCache;
use crossterm::event::{KeyCode, KeyModifiers};
use crate::actions::start_cli_action;
use crate::state::{dashboard_snapshot, summarize};
use crate::view::view;
use crate::{ask, backups, lag, newpod, offers, runs};
use arena_core::PodSpec;
use std::collections::HashSet;
use std::sync::atomic::AtomicUsize;

use arena_core::health::{Check, Status};
use arena_core::pod::Maintenance;
use arena_core::remote::FakeRemote;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;

pub(crate) const NOW: u64 = 1_791_460_800; // 2026-10-08T12:00:00Z

/// A healthy 2×A4000 pod as the deep-check script prints it (login-shell chatter first).
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

pub(crate) fn cfg(extra: &str) -> Config {
    Config::parse(&format!(
        "MACHINE_NAME_PREFIX=devtest\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nALLOWED_CUDA_VERSIONS=\"13.0\"\n\
         MACHINE_NAME_LIST=(\n \"alpha\"\n \"bravo\"\n \"charlie\"\n \"delta\"\n)\n{extra}"
    ))
}

/// `devtest-<name>`, RUNNING, at `10.0.0.<n>:<port>` when `port` is given.
pub(crate) fn pod(name: &str, provider: &str, id: &str, n: u8, port: Option<u16>) -> Pod {
    Pod {
        id: id.into(),
        name: format!("devtest-{name}"),
        provider: provider.into(),
        status: "RUNNING".into(),
        ssh_ip: port.map(|_| format!("10.0.0.{n}")),
        ssh_port: port,
        ..Default::default()
    }
}

pub(crate) fn verdict(p: &Pod, status: Status, check: Option<(&str, &str)>) -> PodHealth {
    PodHealth {
        id: p.id.clone(),
        name: p.name.clone(),
        provider: p.provider.clone(),
        status,
        checks: check.map(|(name, detail)| Check { name: name.into(), status, detail: detail.into() }).into_iter().collect(),
        facts: None,
        host: None,
    }
}

pub(crate) fn ui(cfg: Config, remote: Arc<dyn Remote>) -> Ui {
    Ui {
        provider_name: "runpod".into(),
        config_path: "/test/config.env".into(),
        prefix: cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena").to_string(),
        cfg,
        remote,
        short_names: true,
        mode: Mode::List,
        selected: 0,
        marked: HashSet::new(),
        result_scroll: 0,
        result_max: std::cell::Cell::new(0),
        last_table: std::cell::Cell::new((Rect::default(), 0)),
        asker: fake_asker(Arc::default()),
        ask_ai: None,
        deep_all_armed: None,
        trace: lag::Trace::off(),
        cli: Err("no CLI in this test".into()),
        nudge: Arc::new(Notify::new()),
        offers: offers::State::new(true),
        demo: false,
        menu_at: std::cell::Cell::new(None),
        top_at: std::cell::Cell::new(None),
        ai_keys: false,
        ai_set: Vec::new(),
        backups: backups::State::default(),
    }
}

/// Every `?` question a [`fake_asker`] got: the pods' names, the question, the AI and how.
type Asked = Arc<Mutex<Vec<(Vec<String>, String, arena_core::ask::Pick)>>>;

/// An asker that records each question and answers `ANSWER to <question>` at once — never
/// a real AI.
fn fake_asker(log: Asked) -> ask::Asker {
    Arc::new(move |pods: Vec<Pod>, question: String, pick: arena_core::ask::Pick| {
        let log = log.clone();
        Box::pin(async move {
            let title = format!("{} · test", pick.label());
            log.lock().unwrap().push((pods.iter().map(|p| p.name.clone()).collect(), question.clone(), pick));
            ask::Answer { title, text: format!("ANSWER to {question}") }
        })
    })
}

/// Forwards alpha → its endpoint (live) and bravo → an old one (stale).
pub(crate) fn proxy_file() -> String {
    use arena_core::proxy::{render_nginx, Forward};
    let fwd = |name: &str, port: u16, ip: &str, tport: u16| Forward {
        name: format!("devtest-{name}"),
        public_port: port,
        target_ip: ip.into(),
        target_port: tport,
        provider: Some("runpod".into()),
        pod_id: None,
    };
    render_nginx(&[fwd("alpha", 9500, "10.0.0.1", 22001), fwd("bravo", 9501, "10.0.0.9", 22999)])
}

/// The fixed fleet the rendering tests draw:
/// - alpha: RunPod 1×A4000 at $0.17, a host maintenance window, passed a check 12m ago,
///   proxied live on :9500;
/// - bravo: RunPod at $0.25, failed its check (GPU) 2h ago, proxy forward stale;
/// - charlie: a Hetzner cx23 at €0.0056, never checked, no forward;
/// - delta: a stopped RunPod pod (its $0.13 isn't billing), being deep-checked now.
fn fixture_pods() -> Vec<Pod> {
    let mut alpha = pod("alpha", "runpod", "rp1", 1, Some(22001));
    alpha.gpu_type = Some("RTX A4000".into());
    alpha.gpu_count = Some(1);
    alpha.cost_per_hr = Some(0.17);
    alpha.maintenance = Some(Maintenance {
        start: Some("2026-10-09T02:00:00Z".into()),
        end: Some("2026-10-09T06:00:00Z".into()),
        note: Some("host upgrade".into()),
    });
    let mut bravo = pod("bravo", "runpod", "rp2", 2, Some(22002));
    bravo.cost_per_hr = Some(0.25);
    bravo.locked = Some(true);
    let mut charlie = pod("charlie", "hetzner", "88", 3, Some(22));
    charlie.status = "running".into();
    charlie.gpu_type = Some("cx23".into());
    charlie.cost_per_hr = Some(0.0056);
    let mut delta = pod("delta", "runpod", "rp4", 4, None);
    delta.status = "EXITED".into();
    delta.cost_per_hr = Some(0.13);
    vec![delta, charlie, bravo, alpha]
}

pub(crate) fn fixture_shared() -> Shared {
    let cfg = cfg("");
    let pods = fixture_pods();
    let by = |id: &str| pods.iter().find(|p| p.id == id).unwrap().clone();
    let mut cache = HealthCache::new();
    cache.merge(&[verdict(&by("rp1"), Status::Pass, None)], NOW - 720);
    cache.merge(&[verdict(&by("rp2"), Status::Fail, Some(("cuda", "RuntimeError: Error 999: unknown error")))], NOW - 7_300);
    let text = proxy_file();
    let snap = dashboard_snapshot(&pods, &[], Some(&text), &cache, &Naming::from_config(&cfg), NOW);
    let mut shared = Shared::default();
    shared.deep.begin(vec![by("rp4")]);
    shared.summary = summarize(&pods, &HashMap::new());
    shared.publish(snap);
    shared
}

/// The buffer's rows as a terminal shows them: a wide symbol (`💾`) takes two cells, so the
/// cell it hides isn't printed.
pub(crate) fn screen(buf: &Buffer) -> Vec<String> {
    (0..buf.area.height)
        .map(|y| {
            let mut row = String::new();
            let mut x = 0;
            while x < buf.area.width {
                let sym = buf[(x, y)].symbol();
                row.push_str(sym);
                x += (Span::raw(sym).width() as u16).max(1);
            }
            row
        })
        .collect()
}

pub(crate) fn draw(width: u16, height: u16, shared: &Shared, ui: &Ui) -> Vec<String> {
    let mut t = Terminal::new(TestBackend::new(width, height)).unwrap();
    t.draw(|f| view(f, shared, ui, 5)).unwrap();
    screen(t.backend().buffer())
}

/// The row of the pod whose short name is `name`.
pub(crate) fn row<'a>(lines: &'a [String], name: &str) -> &'a str {
    lines
        .iter()
        .find(|l| l.contains(&format!(" {name} ")))
        .unwrap_or_else(|| panic!("no {name} row in\n{}", lines.join("\n")))
}

/// Prepaid accounts read live now at `rate`/h: `(account, USD, rate)`.
pub(crate) fn live_views(accounts: &[(balance::Account, f64, f64)]) -> Vec<AccountView> {
    let fetched: Vec<balance::Fetched> = accounts
        .iter()
        .map(|&(account, usd, rate)| balance::Fetched {
            account,
            key_fp: "fp".into(),
            live: Some(Ok(balance::Live { usd, provider_burn_per_hr: Some(rate), spend_limit_per_hr: None, under_balance: None, reduced: false, owed: None })),
            billed: None,
        })
        .collect();
    balance::views(&balance::BalanceState::default(), &fetched, &balance::Burns::new(), &BalancePolicy::default(), balance::Mode::Inline, NOW)
}

/// A fleet whose every provider fails to list.
pub(crate) struct Down;

#[async_trait::async_trait]
impl Provider for Down {
    fn name(&self) -> &'static str {
        "runpod"
    }
    fn describe(&self, _spec: &PodSpec) -> String {
        String::new()
    }
    async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
        Err(arena_core::Error::provider("list pods HTTP 503"))
    }
    async fn list_by_provider(&self) -> Vec<(String, arena_core::Result<Vec<Pod>>)> {
        vec![("runpod".into(), Err(arena_core::Error::provider("list pods HTTP 503")))]
    }
    async fn create_pod(&self, _spec: &PodSpec) -> arena_core::Result<Pod> {
        panic!("a refresh must never create a pod")
    }
    async fn stop_pod(&self, _id: &str) -> arena_core::Result<()> {
        panic!("a refresh must never stop a pod")
    }
    async fn restart_pod(&self, _id: &str) -> arena_core::Result<()> {
        panic!("a refresh must never restart a pod")
    }
    async fn terminate_pod(&self, _id: &str) -> arena_core::Result<()> {
        panic!("a refresh must never terminate a pod")
    }
}

/// A pod-side fleet for [`refresh`]: lists runpod (two pods) and a vast that 429s,
/// counts list and details calls, and panics on anything that would change the fleet.
pub(crate) struct FakeFleet {
    pub pods: Vec<Pod>,
    pub lists: AtomicUsize,
    pub details: AtomicUsize,
    /// Make the details query fail after filling rp1 (a GraphQL reply with data *and*
    /// `errors`, or one backend of the fleet failing).
    pub details_error: Option<&'static str>,
}

#[async_trait::async_trait]
impl Provider for FakeFleet {
    fn name(&self) -> &'static str {
        "fleet"
    }
    fn describe(&self, _spec: &PodSpec) -> String {
        String::new()
    }
    async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
        panic!("the dashboard lists per provider, so a failed one is known")
    }
    async fn list_by_provider(&self) -> Vec<(String, arena_core::Result<Vec<Pod>>)> {
        self.lists.fetch_add(1, Ordering::SeqCst);
        vec![
            ("runpod".into(), Ok(self.pods.clone())),
            ("vast".into(), Err(arena_core::Error::provider("vast list HTTP 429 Too Many Requests"))),
        ]
    }
    async fn enrich(&self, pods: &mut [Pod]) -> arena_core::Result<()> {
        self.details.fetch_add(1, Ordering::SeqCst);
        for p in pods.iter_mut().filter(|p| p.id == "rp1") {
            p.gpu_type = Some("RTX A4000".into());
            p.gpu_count = Some(1);
            p.cost_per_hr = Some(0.17);
            p.maintenance = Some(Maintenance { note: Some("host upgrade".into()), ..Default::default() });
        }
        match self.details_error {
            Some(e) => Err(arena_core::Error::provider(e)),
            None => Ok(()),
        }
    }
    async fn create_pod(&self, _spec: &PodSpec) -> arena_core::Result<Pod> {
        panic!("a refresh must never create a pod")
    }
    async fn stop_pod(&self, _id: &str) -> arena_core::Result<()> {
        panic!("a refresh must never stop a pod")
    }
    async fn restart_pod(&self, _id: &str) -> arena_core::Result<()> {
        panic!("a refresh must never restart a pod")
    }
    async fn terminate_pod(&self, _id: &str) -> arena_core::Result<()> {
        panic!("a refresh must never terminate a pod")
    }
}

/// A fresh temp directory, removed at the end of the test.
pub(crate) struct Tmp(pub PathBuf);
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
pub(crate) fn tmp(tag: &str) -> Tmp {
    let d = std::env::temp_dir().join(format!("arena-tui-test-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    Tmp(d)
}

/// Review ai-review codex #5: the answer popup's scroll limit counts the lines as the
/// `Paragraph` wraps them — word by word, a wide character two columns — so a long paragraph
/// and a line of CJK scroll to their very end: at the limit the last line sits on the popup's
/// last row, right above its border, at any size. (Counting characters per line came short:
/// the last lines were out of reach.)
#[test]
fn the_answer_scrolls_to_the_end_of_wrapped_text() {
    let shared_arc = Arc::new(Mutex::new(fixture_shared()));
    let shared = fixture_shared();
    let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
    let words: String = (0..400).map(|i| format!("word{i}{} ", "x".repeat(i % 11))).collect();
    let text = format!("{words}\n{}\nTHE-END", "漢字".repeat(300));
    for (w, h) in [(120, 40), (80, 24), (61, 17)] {
        ui.mode = Mode::Answer(ask::Answer { title: "t".into(), text: text.clone() });
        ui.result_scroll = 0;
        draw(w, h, &shared, &ui);
        for _ in 0..200 {
            assert!(ask::key(&mut ui, &shared_arc, KeyCode::PageDown, KeyModifiers::NONE));
        }
        assert_eq!(ui.result_scroll, ui.result_max.get(), "{w}x{h}");
        let rows = draw(w, h, &shared, &ui);
        let at = rows.iter().position(|r| r.contains("THE-END")).unwrap_or_else(|| panic!("{w}x{h}: the end is out of reach:\n{}", rows.join("\n")));
        assert!(rows[at + 1].contains('└'), "{w}x{h}: the last line isn't the last row:\n{}", rows.join("\n"));
    }
}

/// `?` opens the question for the cursor pod, prefilled; typing edits it (Ctrl+U clears),
/// Enter asks in the background — the dashboard stays live, the footer says so — and the
/// answer opens in its own popup when it lands (a modal open meanwhile keeps it waiting),
/// scrolls, and Esc closes it. With pods marked it asks about the marked set; Esc on the
/// input asks nothing; `ASK_AI=off` opens nothing and says how to turn it on. The footer
/// lists the key.
#[tokio::test]
async fn question_mark_asks_in_the_background_and_shows_the_answer() {
    use arena_core::ask::{Ai, Pick};
    let shared = Arc::new(Mutex::new(fixture_shared()));
    let asked: Asked = Arc::default();
    let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
    ui.asker = fake_asker(asked.clone());
    let none = KeyModifiers::NONE;
    let screen = |ui: &Ui, w: u16, h: u16| draw(w, h, &shared.lock().unwrap(), ui);
    for mode in [Mode::List, Mode::Detail] {
        ui.mode = mode;
        let footer = screen(&ui, 240, 12).pop().unwrap();
        assert!(footer.contains("[?] ask"), "{footer}");
    }
    ui.mode = Mode::List;
    ui.selected = 2;
    let cursor = shared.lock().unwrap().pods[2].name.clone();
    let answered = || async {
        for _ in 0..400 {
            if shared.lock().unwrap().ask_answer.is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("no answer landed");
    };

    // `?`: the input, prefilled, saying where the data goes.
    assert!(ask::key(&mut ui, &shared, KeyCode::Char('?'), none));
    let Mode::Ask { value, pick, .. } = &ui.mode else { panic!("{:?}", ui.mode) };
    assert_eq!((value.as_str(), pick), ("What is the current state of this pod? Is anything wrong or needing attention?", &Pick::plain(Ai::Claude)));
    let text = screen(&ui, 140, 40).join("\n");
    assert!(text.contains("ask claude about") && text.contains("keys masked, to Anthropic with no tools"), "{text}");
    assert!(text.contains("[tab] codex instead"), "{text}");
    assert!(screen(&ui, 240, 40).pop().unwrap().contains("[enter] ask"));
    ask::key(&mut ui, &shared, KeyCode::Char('u'), KeyModifiers::CONTROL);
    for c in "is it busy?x".chars() {
        ask::key(&mut ui, &shared, KeyCode::Char(c), none);
    }
    ask::key(&mut ui, &shared, KeyCode::Backspace, none);
    assert!(matches!(&ui.mode, Mode::Ask { value, .. } if value == "is it busy?"), "{:?}", ui.mode);

    // Enter: asked in the background, the footer says so, the list is back at once.
    assert!(ask::key(&mut ui, &shared, KeyCode::Enter, none));
    assert!(matches!(ui.mode, Mode::List));
    let status = format!("asking claude about {cursor}…");
    assert_eq!(shared.lock().unwrap().ask_running.as_deref(), Some(status.as_str()));
    let footer = screen(&ui, 240, 12).pop().unwrap();
    assert!(footer.contains(&status) && footer.contains("[q] quit"), "{footer}");
    answered().await;
    assert!(shared.lock().unwrap().ask_running.is_none());
    assert_eq!(asked.lock().unwrap().clone(), [(vec![cursor.clone()], "is it busy?".to_string(), Pick::plain(Ai::Claude))]);

    // The answer opens; it scrolls; Esc closes it.
    ask::land(&mut ui, &shared);
    assert!(matches!(&ui.mode, Mode::Answer(a) if a.text == "ANSWER to is it busy?"), "{:?}", ui.mode);
    let text = screen(&ui, 140, 40).join("\n");
    assert!(text.contains("claude · test") && text.contains("ANSWER to is it busy?"), "{text}");
    ask::key(&mut ui, &shared, KeyCode::Down, none);
    assert_eq!(ui.result_scroll, 0, "a one-line answer fits: nothing to scroll");
    assert!(ask::key(&mut ui, &shared, KeyCode::Esc, none));
    assert!(matches!(ui.mode, Mode::List) && ui.result_scroll == 0);

    // An answer landing while a modal is open waits for it to close.
    shared.lock().unwrap().ask_answer = Some(ask::Answer { title: "t".into(), text: "later".into() });
    ui.mode = Mode::Select { value: String::new() };
    ask::land(&mut ui, &shared);
    assert!(matches!(ui.mode, Mode::Select { .. }));
    ui.mode = Mode::List;
    ask::land(&mut ui, &shared);
    assert!(matches!(&ui.mode, Mode::Answer(a) if a.text == "later"));
    ui.mode = Mode::List;

    // Pods marked: the question is about them, compared in one ask.
    let marked: Vec<Pod> = shared.lock().unwrap().pods.iter().take(2).cloned().collect();
    ui.marked = marked.iter().map(mark_key).collect();
    ask::key(&mut ui, &shared, KeyCode::Char('?'), none);
    assert!(
        matches!(&ui.mode, Mode::Ask { scope: InputScope::Set(Scope::Marked), value, .. } if value.starts_with("What is the current state of these pods?")),
        "{:?}",
        ui.mode
    );
    ask::key(&mut ui, &shared, KeyCode::Enter, none);
    answered().await;
    let names: Vec<String> = marked.iter().map(|p| p.name.clone()).collect();
    assert_eq!(asked.lock().unwrap().last().map(|(n, _, _)| n.clone()), Some(names));
    ask::land(&mut ui, &shared);
    ask::key(&mut ui, &shared, KeyCode::Esc, none);

    // Esc on the input asks nothing.
    ui.marked.clear();
    ask::key(&mut ui, &shared, KeyCode::Char('?'), none);
    ask::key(&mut ui, &shared, KeyCode::Esc, none);
    assert!(matches!(ui.mode, Mode::List));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(asked.lock().unwrap().len(), 2);

    // ASK_AI=off: nothing opens; the footer says how to turn it on.
    ui.cfg = cfg("ASK_AI=off\n");
    assert!(ask::key(&mut ui, &shared, KeyCode::Char('?'), none));
    assert!(matches!(ui.mode, Mode::List));
    let notice = shared.lock().unwrap().notice.clone().unwrap();
    assert!(notice.error && notice.text.contains("ASK_AI=off") && notice.text.contains("ASK_AI=claude"), "{}", notice.text);
    // Other keys on the list aren't ask's.
    assert!(!ask::key(&mut ui, &shared, KeyCode::Char('r'), none));
}

/// Review ai-review claude #9: `?` on a marked set keeps to this cohort, as `pods ask` does
/// when no pod is named: a non-cohort pod marked along is left out of the question — the
/// input names it, the footer says how many — and a set of only such pods asks nothing; the
/// cursor pod alone is asked about whatever it is.
#[tokio::test]
async fn question_mark_keeps_a_marked_set_to_the_cohort() {
    use arena_core::ask::{Ai, Pick};
    let c = cfg("");
    let mut pods = fixture_pods();
    let mut staff = pod("x", "runpod", "rp9", 9, Some(22009));
    staff.name = "jamesbox".into();
    pods.push(staff);
    let snap = dashboard_snapshot(&pods, &[], None, &HealthCache::new(), &Naming::from_config(&c), NOW);
    let mut fleet = Shared::default();
    fleet.publish(snap);
    let shared = Arc::new(Mutex::new(fleet));
    let asked: Asked = Arc::default();
    let mut ui = ui(c, Arc::new(FakeRemote::new()));
    ui.asker = fake_asker(asked.clone());
    let none = KeyModifiers::NONE;
    let key_of = |name: &str| mark_key(shared.lock().unwrap().pods.iter().find(|p| p.name == name).unwrap());
    let answered = || async {
        for _ in 0..400 {
            if shared.lock().unwrap().ask_answer.take().is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("no answer landed");
    };

    ui.marked = [key_of("jamesbox"), key_of("devtest-alpha")].into_iter().collect();
    assert!(ask::key(&mut ui, &shared, KeyCode::Char('?'), none));
    assert!(matches!(&ui.mode, Mode::Ask { scope: InputScope::Set(Scope::Marked), value, .. } if value.starts_with("What is the current state of this pod?")), "{:?}", ui.mode);
    let text = draw(160, 40, &shared.lock().unwrap(), &ui).join("\n");
    assert!(text.contains("ask claude about alpha — 1 non-cohort pod left out: jamesbox"), "{text}");
    ask::key(&mut ui, &shared, KeyCode::Enter, none);
    assert_eq!(shared.lock().unwrap().ask_running.as_deref(), Some("asking claude about devtest-alpha (1 non-cohort pod left out)…"));
    answered().await;
    assert_eq!(asked.lock().unwrap().last().cloned(), Some((vec!["devtest-alpha".to_string()], "What is the current state of this pod? Is anything wrong or needing attention?".to_string(), Pick::plain(Ai::Claude))));

    // Only non-cohort pods marked: nothing opens, and the footer says why.
    ui.marked = [key_of("jamesbox")].into_iter().collect();
    ask::key(&mut ui, &shared, KeyCode::Char('?'), none);
    assert!(matches!(ui.mode, Mode::List), "{:?}", ui.mode);
    let notice = shared.lock().unwrap().notice.clone().unwrap();
    assert!(notice.error && notice.text.contains("none of the marked pods is this cohort's (jamesbox)"), "{}", notice.text);

    // The cursor on it, nothing marked: asked about, as a named pod.
    ui.marked.clear();
    ui.selected = shared.lock().unwrap().pods.iter().position(|p| p.name == "jamesbox").unwrap();
    ask::key(&mut ui, &shared, KeyCode::Char('?'), none);
    ask::key(&mut ui, &shared, KeyCode::Enter, none);
    answered().await;
    assert_eq!(asked.lock().unwrap().last().map(|(n, _, _)| n.clone()), Some(vec!["jamesbox".to_string()]));
}

/// Review ai-review claude #4: a question whose task panics — here the asker itself, as a
/// poisoned lock would — doesn't leave `?` refusing ("one question at a time") until the
/// dashboard restarts: the footer's `asking …` clears, and `?` opens the question again.
#[tokio::test]
async fn a_question_whose_task_panics_frees_question_mark() {
    let shared = Arc::new(Mutex::new(fixture_shared()));
    let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
    ui.asker = Arc::new(|_pods: Vec<Pod>, _question: String, _pick: arena_core::ask::Pick| -> std::pin::Pin<Box<dyn std::future::Future<Output = ask::Answer> + Send>> {
        Box::pin(async {
            tokio::task::yield_now().await;
            let nothing: Option<ask::Answer> = std::hint::black_box(None);
            nothing.expect("the asker panicked (a test)")
        })
    });
    let none = KeyModifiers::NONE;
    ask::key(&mut ui, &shared, KeyCode::Char('?'), none);
    ask::key(&mut ui, &shared, KeyCode::Enter, none);
    assert!(shared.lock().unwrap().ask_running.is_some(), "asked");
    for _ in 0..400 {
        if shared.lock().unwrap().ask_running.is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(shared.lock().unwrap().ask_running, None, "the panic left `?` refusing");
    assert!(shared.lock().unwrap().ask_answer.is_none());
    assert!(ask::key(&mut ui, &shared, KeyCode::Char('?'), none));
    assert!(matches!(ui.mode, Mode::Ask { .. }), "{:?}", ui.mode);
}

/// Tab in the question switches to the other AI as the config sets it up (its model and
/// effort shown and asked with), and the next `?` opens with it; a malformed setting for the
/// other AI keeps the one there and says why.
#[tokio::test]
async fn tab_switches_the_ai_and_the_next_question_keeps_it() {
    use arena_core::ask::{Ai, Effort, Pick};
    let shared = Arc::new(Mutex::new(fixture_shared()));
    let asked: Asked = Arc::default();
    let mut ui = ui(cfg("ASK_CODEX_MODEL=gpt-6-luna\nASK_CODEX_EFFORT=low\n"), Arc::new(FakeRemote::new()));
    ui.asker = fake_asker(asked.clone());
    let none = KeyModifiers::NONE;
    let luna = Pick { ai: Ai::Codex, model: Some("gpt-6-luna".into()), effort: Some(Effort::Low) };
    ask::key(&mut ui, &shared, KeyCode::Char('?'), none);
    assert!(matches!(&ui.mode, Mode::Ask { pick, .. } if *pick == Pick::plain(Ai::Claude)), "{:?}", ui.mode);
    assert!(ask::key(&mut ui, &shared, KeyCode::Tab, none));
    assert!(matches!(&ui.mode, Mode::Ask { pick, .. } if *pick == luna), "{:?}", ui.mode);
    let text = draw(140, 40, &shared.lock().unwrap(), &ui).join("\n");
    assert!(text.contains("ask codex · gpt-6-luna · low effort about") && text.contains("to OpenAI") && text.contains("[tab] claude instead"), "{text}");
    ask::key(&mut ui, &shared, KeyCode::Enter, none);
    let cursor = shared.lock().unwrap().pods[ui.selected].name.clone();
    assert_eq!(shared.lock().unwrap().ask_running.clone(), Some(format!("asking codex · gpt-6-luna · low effort about {cursor}…")));
    for _ in 0..400 {
        if shared.lock().unwrap().ask_answer.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(asked.lock().unwrap().last().map(|(_, _, p)| p.clone()), Some(luna.clone()));
    ask::land(&mut ui, &shared);
    assert!(draw(140, 40, &shared.lock().unwrap(), &ui).join("\n").contains("codex · gpt-6-luna · low effort · test"));
    ask::key(&mut ui, &shared, KeyCode::Esc, none);

    // The next `?` opens with codex; BackTab goes back.
    ask::key(&mut ui, &shared, KeyCode::Char('?'), none);
    assert!(matches!(&ui.mode, Mode::Ask { pick, .. } if *pick == luna), "{:?}", ui.mode);
    ask::key(&mut ui, &shared, KeyCode::BackTab, none);
    assert!(matches!(&ui.mode, Mode::Ask { pick, .. } if *pick == Pick::plain(Ai::Claude)), "{:?}", ui.mode);

    // The other AI's effort malformed: Tab keeps claude and says why.
    ui.cfg = cfg("ASK_CODEX_EFFORT=fast\n");
    ask::key(&mut ui, &shared, KeyCode::Tab, none);
    assert!(matches!(&ui.mode, Mode::Ask { pick, .. } if pick.ai == Ai::Claude), "{:?}", ui.mode);
    let notice = shared.lock().unwrap().notice.clone().unwrap();
    assert!(notice.error && notice.text.contains("ASK_CODEX_EFFORT must be low, medium, high, xhigh or max (got fast)"), "{}", notice.text);
}

/// The form's `y` runs `arena pods up` — the `arena` beside the dashboard (a stub here),
/// with the dashboard's config, the form's provider and its flags — in the background: its
/// lines reach the runs panel, a created pod shows as a pending row, each pod's verdict and
/// the exit are said, its output is in a file of its own (0600). `s`, `y` in the panel sends
/// a run Ctrl+C — SIGINT to its process group, which `up` takes as "stop where you are".
#[tokio::test]
async fn the_form_runs_pods_up_and_the_panel_follows_it() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("arena-tui-runs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let stub = dir.join("arena");
    std::fs::write(
        &stub,
        "#!/bin/sh\n\
         echo \"args: $*\"\n\
         case \"$*\" in\n\
         *'pods start'*)\n\
           trap 'echo \"[devtest-alpha] STOPPED at setup (Ctrl+C) — left as it is\"; exit 130' INT\n\
           echo waiting\n\
           while :; do sleep 0.05; done ;;\n\
         *'pods logs'*)\n\
           head -c 800000 /dev/zero | tr '\\0' x\n\
           printf '\\n[devtest-alpha] FAILED setup: after a long line\\n'\n\
           exit 1 ;;\n\
         esac\n\
         echo '[created] devtest-echo id=9 on cx33 (€0.0160/h)'\n\
         echo '[devtest-echo] setup ✓ (2m)'\n\
         echo '[devtest-echo] READY after 3m — proxy :9504'\n\
         echo '[devtest-foxtrot] FAILED endpoint: no SSH after 600s' >&2\n\
         exit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    let shared = Arc::new(Mutex::new(fixture_shared()));
    let mut ui = ui(cfg("HETZNER_API_KEY=x\n"), Arc::new(FakeRemote::new()));
    ui.cli = Ok(runs::Cli { exe: stub.clone(), config: "/test/config.env".into(), dir: dir.join("runs") });
    async fn ended(shared: &Mutex<Shared>, id: u64, line: Option<&str>) -> bool {
        for _ in 0..200 {
            {
                let s = shared.lock().unwrap();
                let r = s.runs.iter().find(|r| r.id == id).unwrap();
                if line.map_or(!r.running(), |l| r.lines.iter().any(|x| x == l)) {
                    return true;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    // The form on Hetzner (the only provider with a key), Enter, then `y`.
    newpod::open(&mut ui, &shared);
    assert!(newpod::key(&mut ui, &shared, KeyCode::Enter));
    assert!(matches!(ui.mode, Mode::UpConfirm(_)), "{:?}", ui.mode);
    assert!(newpod::key(&mut ui, &shared, KeyCode::Char('y')));
    let Mode::Runs { id, .. } = ui.mode else { panic!("{:?}", ui.mode) };
    assert!(ended(&shared, id, None).await, "the run never ended");
    {
        let s = shared.lock().unwrap();
        let r = s.runs.iter().find(|r| r.id == id).unwrap();
        assert_eq!(
            r.lines.front().map(String::as_str),
            Some("args: --config /test/config.env --provider hetzner pods up --add 1 --server-type cx33 --volume 0 --yes")
        );
        assert_eq!(r.end.as_ref().map(|(_, e)| e.clone()), Some(runs::End::Code(1)));
        assert_eq!(r.tally(), "1 ready, 1 failed");
        assert_eq!(r.pods[0], ("devtest-echo".to_string(), runs::PodState::Ready("after 3m — proxy :9504".into())));
        assert!(s.pending.iter().any(|p| p.name == "devtest-echo" && p.provider == "hetzner"), "a pending row at once");
        assert_eq!(s.notice.as_ref().map(|n| n.text.as_str()), Some("✗ pods up: exit 1 — 1 ready, 1 failed — [p] shows it"));
        let mode = std::fs::metadata(&r.log).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{}", r.log.display());
        assert!(std::fs::read_to_string(&r.log).unwrap().contains("FAILED endpoint"), "stderr too");
    }

    // A run that waits; `s` asks, `y` sends it Ctrl+C.
    let alpha = shared.lock().unwrap().pods.iter().find(|p| p.name == "devtest-alpha").cloned().unwrap();
    start_cli_action(&mut ui, &shared, Action::Start, &alpha, None);
    let Mode::Runs { id, .. } = ui.mode else { panic!("{:?}", ui.mode) };
    assert!(ended(&shared, id, Some("waiting")).await, "it never started");
    assert!(matches!(runs::key(&mut ui, &shared, KeyCode::Char('s')), runs::Flow::Handled));
    assert!(matches!(ui.mode, Mode::Runs { stop: true, .. }), "{:?}", ui.mode);
    assert!(matches!(runs::key(&mut ui, &shared, KeyCode::Char('y')), runs::Flow::Handled));
    assert!(ended(&shared, id, None).await, "Ctrl+C didn't end it");
    {
        let s = shared.lock().unwrap();
        let r = s.runs.iter().find(|r| r.id == id).unwrap();
        assert_eq!(r.lines.front().map(String::as_str), Some("args: --config /test/config.env --provider runpod pods start devtest-alpha --yes"));
        assert_eq!((r.end.as_ref().map(|(_, e)| e.clone()), r.tally()), (Some(runs::End::Code(130)), "1 stopped".to_string()));
    }

    // Ctrl+C straight after `y` (review ai-review codex #2, round 2 #3: the child was spawned
    // in the background task, so a stop — or a quit leaving it running — could come before
    // it existed): `start` spawns it, so it has its pid when it's listed.
    start_cli_action(&mut ui, &shared, Action::Start, &alpha, None);
    let Mode::Runs { id, .. } = ui.mode else { panic!("{:?}", ui.mode) };
    assert!(shared.lock().unwrap().runs.iter().find(|r| r.id == id).unwrap().pid.is_some(), "spawned before it's listed");
    ui.mode = Mode::Runs { id, scroll: None, stop: true };
    assert!(matches!(runs::key(&mut ui, &shared, KeyCode::Char('y')), runs::Flow::Handled));
    assert_eq!(
        shared.lock().unwrap().notice.as_ref().map(|n| n.text.clone()).as_deref(),
        Some("sent Ctrl+C to the run: it stops where it is, terminates nothing")
    );
    assert!(ended(&shared, id, None).await, "Ctrl+C didn't end it");
    let end = shared.lock().unwrap().runs.iter().find(|r| r.id == id).unwrap().end.clone().map(|(_, e)| e);
    // SIGINT before the stub set its trap kills it; after, the trap exits 130.
    assert!(matches!(end, Some(runs::End::Signal(2) | runs::End::Code(130))), "{end:?}");

    // A line longer than several reads, then a verdict, then the exit: the verdict is read
    // (review ai-review codex round 2 #2: the reading stopped at the first read that
    // completed no line once the child had exited).
    start_cli_action(&mut ui, &shared, Action::Logs, &alpha, None);
    let Mode::Runs { id, .. } = ui.mode else { panic!("{:?}", ui.mode) };
    assert!(ended(&shared, id, None).await, "the logs run never ended");
    {
        let s = shared.lock().unwrap();
        let r = s.runs.iter().find(|r| r.id == id).unwrap();
        assert!(r.lines.iter().any(|l| l == "[devtest-alpha] FAILED setup: after a long line"), "{:?}", r.lines.iter().map(|l| l.len()).collect::<Vec<_>>());
        assert!(r.pods.is_empty(), "a log's lines are output, not progress");
        assert!(r.lines.iter().any(|l| l.len() > 1_000 && l.ends_with('…')), "the long line, cut");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The form's retry field end to end: `up` runs with `--retry-mins 30`, the confirm says until
/// when, the panel says it's waiting for capacity (and how to stop it), and `[s]` `[y]` stop it
/// cleanly between rounds — Ctrl+C to its process group, which `up` takes as "no more rounds"
/// (`interrupted — stopping retries`), the run ending on its own.
#[tokio::test]
async fn a_retrying_up_from_the_form_says_so_and_s_stops_it() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("arena-tui-retry-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let stub = dir.join("arena");
    std::fs::write(
        &stub,
        "#!/bin/sh\n\
         echo \"args: $*\"\n\
         trap 'echo \"interrupted — stopping retries with 0 of 1\"; exit 1' INT\n\
         echo '[stop] no capacity for this GPU right now (created 0/1).'\n\
         echo 'round 1: 1 short of target 1 (capacity); retrying in 60s (Ctrl+C to stop)…'\n\
         while :; do sleep 0.05; done\n",
    )
    .unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    let shared = Arc::new(Mutex::new(fixture_shared()));
    let mut ui = ui(cfg("HETZNER_API_KEY=x\n"), Arc::new(FakeRemote::new()));
    ui.cli = Ok(runs::Cli { exe: stub.clone(), config: "/test/config.env".into(), dir: dir.join("runs") });
    let line_seen = |shared: &Mutex<Shared>, id: u64, f: &dyn Fn(&runs::Run) -> bool| {
        let s = shared.lock().unwrap();
        f(s.runs.iter().find(|r| r.id == id).unwrap())
    };

    // Hetzner's form: provider, server, retry, pods. The retry field → 30 min.
    newpod::open(&mut ui, &shared);
    for k in [KeyCode::Down, KeyCode::Down, KeyCode::Right, KeyCode::Right] {
        assert!(newpod::key(&mut ui, &shared, k));
    }
    let Mode::NewPod(form) = &ui.mode else { panic!("{:?}", ui.mode) };
    assert_eq!(form.retry_mins(), 30);
    let all = draw(140, 40, &shared.lock().unwrap(), &ui).join("\n");
    assert!(all.contains("▶ retry     ‹ 30 min while there's no capacity ›") && all.contains("● 30 min") && all.contains("○ 24 h"), "{all}");
    assert!(newpod::key(&mut ui, &shared, KeyCode::Enter));
    let all = draw(140, 40, &shared.lock().unwrap(), &ui).join("\n");
    assert!(all.contains("--retry-mins 30 --yes") && all.contains("retry: no capacity → tries again every 60s until "), "{all}");
    assert!(newpod::key(&mut ui, &shared, KeyCode::Char('y')));
    let Mode::Runs { id, .. } = ui.mode else { panic!("{:?}", ui.mode) };
    let mut waiting = false;
    for _ in 0..200 {
        if line_seen(&shared, id, &|r| r.waiting.is_some()) {
            waiting = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(waiting, "never said it waits");
    {
        let s = shared.lock().unwrap();
        let r = s.runs.iter().find(|r| r.id == id).unwrap();
        assert_eq!(r.lines.front().map(String::as_str), Some("args: --config /test/config.env --provider hetzner pods up --add 1 --server-type cx33 --volume 0 --retry-mins 30 --yes"));
        assert!(r.retry_until.is_some_and(|u| u > snapshot::unix_now() + 29 * 60), "{:?}", r.retry_until);
        assert!(r.title.ends_with(", retrying up to 30 min"), "{}", r.title);
    }
    let panel = draw(160, 40, &shared.lock().unwrap(), &ui).join("\n");
    assert!(panel.contains("running") && panel.contains("· waiting for capacity"), "{panel}");
    assert!(panel.contains("retry: no capacity → tries again until ") && panel.contains("[s] stops it, keeping what was made"), "{panel}");
    assert!(panel.contains("waiting for capacity until "), "the footer too: {panel}");
    // [s], [y]: Ctrl+C between rounds — it stops retrying and ends by itself.
    assert!(matches!(runs::key(&mut ui, &shared, KeyCode::Char('s')), runs::Flow::Handled));
    assert!(matches!(runs::key(&mut ui, &shared, KeyCode::Char('y')), runs::Flow::Handled));
    let mut ended = false;
    for _ in 0..200 {
        if line_seen(&shared, id, &|r| !r.running()) {
            ended = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ended, "Ctrl+C didn't end it");
    {
        let s = shared.lock().unwrap();
        let r = s.runs.iter().find(|r| r.id == id).unwrap();
        assert!(r.lines.iter().any(|l| l == "interrupted — stopping retries with 0 of 1"), "{:?}", r.lines);
        assert_eq!((r.end.as_ref().map(|(_, e)| e.clone()), r.waiting.clone()), (Some(runs::End::Code(1)), None));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The human (2026-10-29): "i switch between runpod / vast / hetzner and its laggy". The
/// form's provider switch awaited a whole-fleet listing in the key loop. Now `n` opens the
/// form from the rows on screen and each switch is drawn at once — over a provider whose
/// listing takes ten seconds, never asked: the free names are the snapshot's (`up` lists
/// again before it creates), the type field the cached catalog or the presets meanwhile.
#[tokio::test(start_paused = true)]
async fn switching_provider_in_the_form_is_drawn_at_once_and_lists_nothing() {
    struct Slow(AtomicUsize);
    #[async_trait::async_trait]
    impl Provider for Slow {
        fn name(&self) -> &'static str {
            "runpod"
        }
        fn describe(&self, _spec: &PodSpec) -> String {
            String::new()
        }
        async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok(Vec::new())
        }
        async fn create_pod(&self, _spec: &PodSpec) -> arena_core::Result<Pod> {
            unimplemented!()
        }
        async fn stop_pod(&self, _id: &str) -> arena_core::Result<()> {
            unimplemented!()
        }
        async fn restart_pod(&self, _id: &str) -> arena_core::Result<()> {
            unimplemented!()
        }
        async fn terminate_pod(&self, _id: &str) -> arena_core::Result<()> {
            unimplemented!()
        }
    }
    let slow = Arc::new(Slow(AtomicUsize::new(0)));
    let _provider: Arc<dyn Provider> = slow.clone();
    let mut fx = fixture_shared();
    fx.last_refresh = Some(Instant::now());
    let shared = Arc::new(Mutex::new(fx));
    let mut ui = ui(cfg("RUNPOD_API_KEY=x\nVAST_API_KEY=x\nHETZNER_API_KEY=x\n"), Arc::new(FakeRemote::new()));
    let frame = |ui: &Ui| draw(120, 45, &shared.lock().unwrap(), ui).join("\n");
    let t = Instant::now();
    newpod::open(&mut ui, &shared);
    newpod::tick(&mut ui, &shared, Instant::now());
    let screen = frame(&ui);
    assert!(screen.contains("provider  ‹ runpod ›"), "{screen}");
    // alpha..delta are listed (charlie, bravo, alpha, delta): nothing free in a 4-name list.
    assert!(screen.contains("pods      ‹ 1 of 0 free ›"), "{screen}");
    for want in ["vast", "hetzner", "runpod"] {
        assert!(newpod::key(&mut ui, &shared, KeyCode::Right));
        newpod::tick(&mut ui, &shared, Instant::now());
        let screen = frame(&ui);
        assert!(screen.contains(&format!("provider  ‹ {want} ›")), "{want}: {screen}");
        if want == "hetzner" {
            assert!(screen.contains("server    ‹ cx33"), "the server type, cx33 by default: {screen}");
            assert!(!screen.contains("gpus/pod") && !screen.contains("cloud "), "{screen}");
        }
    }
    assert!(t.elapsed() < Duration::from_secs(2), "open and three switches, each drawn: {:?}", t.elapsed());
    assert_eq!(slow.0.load(Ordering::SeqCst), 0, "no listing asked for, let alone awaited");
    assert!(matches!(ui.mode, Mode::NewPod(_)));
}

/// Live ASK4 3c (lane ASK8 item 4), review ai-review claude #1 (lane ASK9 item 1): a hangup or
/// a TERM ends the dashboard from a task of its own — never by way of its event loop, which a
/// hung-up terminal never lets come round again (crossterm's read spins) — stopping a question
/// still being asked with everything it started, and every other child (an action's rsync,
/// which the `exit` left running before), and exiting 128 + the signal. A copy of this test
/// binary plays the dashboard: its loop's thread stuck as that read leaves it, a question out
/// (`arena_core::ask::ai::ask`, as `?` asks it, to a stub claude that — and what it starts —
/// ignores TERM: KILLed after the grace) and a lone child out (`run_local`, as a backup's rsync
/// runs, a stub ignoring HUP and TERM), [`exit_on_signals`] listening. Each signal ends it
/// within the grace and a little, with its status, and nothing of either child is left: no
/// process, no scratch directory. (Lane ASK7's latch, never looked at by a stuck loop, ended
/// nothing.)
#[cfg(target_os = "linux")]
#[test]
fn a_hangup_or_a_term_ends_the_dashboard_and_its_ask_at_once() {
    use arena_core::ask::ai::fixtures::{claude_help, on_path, stub};
    const ROLE: &str = "ARENA_TEST_TUI_EXIT_DIR";
    if let Some(dir) = std::env::var_os(ROLE) {
        // The dashboard's part, in the copy.
        let dir = PathBuf::from(dir);
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async {
            exit_on_signals();
            let lone = dir.join("lone");
            tokio::spawn(async move {
                let env = arena_core::ask::AiEnv { path: Some(on_path(&dir.join("bin"))), home: None, tmp: dir.clone(), vars: Vec::new() };
                let _ = arena_core::ask::ai::ask(&arena_core::ask::Pick::plain(arena_core::ask::Ai::Claude), "is it ok?", Duration::from_secs(120), &env).await;
            });
            tokio::spawn(async move { arena_core::remote::run_local(lone.to_str().unwrap(), &[], "the rsync", Some(Duration::from_secs(120))).await });
            // The loop, on a dead terminal: its thread never comes back.
            std::thread::sleep(Duration::from_secs(60));
        });
        std::process::exit(3);
    }
    let alive = |pid: &str| std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| s.rsplit(')').next().and_then(|r| r.split_whitespace().next()).is_some_and(|st| st != "Z" && st != "X"));
    for (sig, code) in [("-HUP", 129), ("-TERM", 143)] {
        let dir = std::env::temp_dir().join(format!("arena-tui-exit-{}-{code}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (ai, left, bin, lone) = (dir.join("ai.pid"), dir.join("left.pid"), dir.join("bin"), dir.join("lone.pid"));
        std::fs::create_dir_all(&bin).unwrap();
        arena_core::remote::write_test_script(dir.join("lone"), format!("#!/bin/sh\ntrap '' HUP TERM\necho $$ > '{0}.tmp'\nmv '{0}.tmp' '{0}'\nexec sleep 60\n", lone.display())).unwrap();
        stub(
            &bin,
            "claude",
            &format!(
                "if [ \"$1\" = --help ]; then cat <<'EOF'\n{}EOF\nexit 0; fi\n\
                 trap '' TERM\nsleep 60 &\necho $! > '{}'\necho $$ > '{2}.tmp'\nmv '{2}.tmp' '{2}'\nwait",
                claude_help(&[]),
                left.display(),
                ai.display()
            ),
        );
        let mut tui = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::a_hangup_or_a_term_ends_the_dashboard_and_its_ask_at_once", "--test-threads=1"])
            .env(ROLE, &dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !(ai.exists() && lone.exists()) {
            assert!(Instant::now() < deadline, "{sig}: the AI or the rsync never started");
            std::thread::sleep(Duration::from_millis(20));
        }
        let pids: Vec<String> = [&ai, &left, &lone].iter().map(|p| std::fs::read_to_string(p).unwrap().trim().to_string()).collect();
        assert!(pids.iter().all(|p| alive(p)), "{sig}: {pids:?}");
        let scratch = || std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).filter(|n| n.to_string_lossy().starts_with("arena-ask-")).collect::<Vec<_>>();
        assert_eq!(scratch().len(), 1, "{sig}: the ask's scratch directory");
        std::thread::sleep(Duration::from_millis(100));
        assert!(std::process::Command::new("kill").args([sig, &tui.id().to_string()]).status().unwrap().success());
        let sent = Instant::now();
        let status = loop {
            if let Some(status) = tui.try_wait().unwrap() {
                break status;
            }
            if sent.elapsed() > arena_core::remote::TERM_GRACE + Duration::from_secs(4) {
                let _ = tui.kill();
                let _ = tui.wait();
                for p in &pids {
                    let _ = std::process::Command::new("kill").args(["-9", p]).status();
                }
                panic!("{sig}: the dashboard is still running {:?} after it", sent.elapsed());
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(status.code(), Some(code), "{sig}");
        // KILLed before the exit: gone as soon as the kernel has them exit.
        let deadline = Instant::now() + Duration::from_millis(500);
        while pids.iter().any(|p| alive(p)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        for pid in &pids {
            assert!(!alive(pid), "{sig}: the child {pid} outlived the dashboard");
        }
        let left_behind = scratch();
        assert!(left_behind.is_empty(), "{sig}: the ask's scratch directory was left: {left_behind:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// The dashboard's part in a copy of this test binary on a pty ([`OnPty`]): [`dashboard`] for
/// real — its loop on that terminal, its signals and its hangup check — over a fleet of one
/// pod (`devtest-alpha`; nothing refreshes it), `ssh` on PATH a stub when the test put one in
/// `dir/bin`, and, when the test wrote `dir/lone`, a lone child out first (`run_local`, as an
/// action's rsync runs). Its pid in `dir/tui.pid` once that child is up. Exits.
#[cfg(target_os = "linux")]
fn play_dashboard(dir: &std::path::Path) -> ! {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let lone = dir.join("lone");
        if lone.exists() {
            tokio::spawn(async move { arena_core::remote::run_local(lone.to_str().unwrap(), &[], "the rsync", Some(Duration::from_secs(120))).await });
            while !dir.join("lone.pid").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        std::fs::write(dir.join("tui.pid.tmp"), std::process::id().to_string()).unwrap();
        std::fs::rename(dir.join("tui.pid.tmp"), dir.join("tui.pid")).unwrap();
        let shared = Arc::new(Mutex::new(Shared { pods: vec![pod("alpha", "runpod", "rp1", 1, Some(22))], status: RefreshStatus::busy("test"), ..Default::default() }));
        let provider: Arc<dyn Provider> = Arc::new(Down);
        let ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let res = dashboard(&shared, &provider, &AtomicU64::new(3600), &Notify::new(), &AtomicBool::new(false), ui).await;
        eprintln!("the dashboard ended: {res:?}");
    });
    std::process::exit(3)
}

/// Where a copy playing the dashboard on a pty finds its directory.
#[cfg(target_os = "linux")]
const ON_PTY: &str = "ARENA_TEST_TUI_PTY_DIR";

/// crossterm's alternate screen, entered and left.
#[cfg(target_os = "linux")]
const ENTER: &str = "\x1b[?1049h";
#[cfg(target_os = "linux")]
const LEAVE: &str = "\x1b[?1049l";

/// Whether `pid` is a process still running (not a zombie, not gone).
#[cfg(target_os = "linux")]
fn running(pid: &str) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| s.rsplit(')').next().and_then(|r| r.split_whitespace().next()).is_some_and(|st| st != "Z" && st != "X"))
}

/// The CPU `pid` has used, user and system, in clock ticks (100 a second).
#[cfg(target_os = "linux")]
fn cpu_ticks(pid: &str) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let f: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    Some(f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?)
}

/// What a pty showed: its bytes, and where each read of them ended, with when.
#[cfg(target_os = "linux")]
type Drawn = (Vec<u8>, Vec<(usize, Instant)>);

/// A copy of this test binary playing the dashboard ([`play_dashboard`]) on a 40×120 pty of
/// its own — lane ASK8's harness (`tests/hangup.rs`), for the copy: `script` holds the pty's
/// master, its stdin is the terminal's keyboard and `-e` gives the copy's exit status (the
/// copy is the terminal's session leader, exec'd by `script`'s shell). Everything drawn, with
/// when each piece of it came. No network: nothing refreshes, every `ssh` is the test's stub.
#[cfg(target_os = "linux")]
struct OnPty {
    script: std::process::Child,
    /// What's been drawn, and where each read of it ended, with when.
    drawn: Arc<Mutex<Drawn>>,
    reader: Option<std::thread::JoinHandle<()>>,
    dir: PathBuf,
    /// The copy's pid.
    tui: String,
}

#[cfg(target_os = "linux")]
impl OnPty {
    /// `test`'s copy on its pty, its screen up and `dir`'s children started; `None` without
    /// util-linux's `script`.
    fn start(test: &str, dir: &std::path::Path) -> Option<OnPty> {
        if !std::process::Command::new("sh").args(["-c", "command -v script"]).output().is_ok_and(|o| o.status.success()) {
            eprintln!("script (util-linux) missing — skipping");
            return None;
        }
        let exe = std::env::current_exe().unwrap();
        let cmd = format!("stty rows 40 cols 120; exec '{}' --exact 'tests::{test}' --test-threads=1 --nocapture", exe.display());
        let mut script = std::process::Command::new("script")
            .args(["-qfec", &cmd, "/dev/null"])
            .env_clear()
            .envs([("SHELL", "/bin/sh"), ("TERM", "xterm-256color")])
            .env("PATH", format!("{}:/usr/bin:/bin", dir.join("bin").display()))
            .env("HOME", dir)
            .env(ON_PTY, dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let drawn = Arc::new(Mutex::new((Vec::new(), Vec::new())));
        let (mut pipe, sink) = (script.stdout.take().unwrap(), drawn.clone());
        let reader = std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 4096];
            while let Ok(n) = pipe.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let mut d = sink.lock().unwrap();
                d.0.extend_from_slice(&buf[..n]);
                let end = d.0.len();
                d.1.push((end, Instant::now()));
            }
        });
        let mut on = OnPty { script, drawn, reader: Some(reader), dir: dir.to_path_buf(), tui: String::new() };
        let pidfile = dir.join("tui.pid");
        let deadline = Instant::now() + Duration::from_secs(20);
        while !(pidfile.exists() && on.find(ENTER, 0).is_some()) {
            if Instant::now() > deadline || on.script.try_wait().unwrap().is_some() {
                let drawn = on.text(0);
                on.end();
                panic!("{test}: the dashboard never came up: {drawn:?}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        on.tui = std::fs::read_to_string(&pidfile).unwrap();
        // Its first rounds drawn.
        std::thread::sleep(Duration::from_millis(400));
        Some(on)
    }

    /// How much has been drawn.
    fn len(&self) -> usize {
        self.drawn.lock().unwrap().0.len()
    }

    /// What's been drawn from `from` on.
    fn text(&self, from: usize) -> String {
        String::from_utf8_lossy(&self.drawn.lock().unwrap().0[from..]).into_owned()
    }

    /// Where `pat` first is at or after `from`, and when what holds its end came.
    fn find(&self, pat: &str, from: usize) -> Option<(usize, Instant)> {
        let d = self.drawn.lock().unwrap();
        let at = from + d.0.get(from..)?.windows(pat.len()).position(|w| w == pat.as_bytes())?;
        let came = d.1.iter().find(|(end, _)| *end >= at + pat.len())?.1;
        Some((at, came))
    }

    /// [`OnPty::find`], waiting up to `within` for it.
    fn wait_for(&self, pat: &str, from: usize, within: Duration) -> Option<(usize, Instant)> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(found) = self.find(pat, from) {
                return Some(found);
            }
            if Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Keys typed on the terminal.
    fn keys(&mut self, keys: &str) {
        use std::io::Write;
        let stdin = self.script.stdin.as_mut().unwrap();
        stdin.write_all(keys.as_bytes()).unwrap();
        stdin.flush().unwrap();
    }

    /// `sig` (`-HUP`, `-TERM`) to the copy.
    fn signal(&self, sig: &str) -> Instant {
        assert!(std::process::Command::new("kill").args([sig, &self.tui]).status().unwrap().success());
        Instant::now()
    }

    /// The copy's exit status (`script -e`'s), and when it was seen — `None` if it's still
    /// running after `within`.
    fn exited_within(&mut self, within: Duration) -> Option<(std::process::ExitStatus, Instant)> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(status) = self.script.try_wait().unwrap() {
                let seen = Instant::now();
                // All it drew, to its last byte.
                let _ = self.reader.take().map(std::thread::JoinHandle::join);
                return Some((status, seen));
            }
            if Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Whatever is left of it gone: the copy, `script`, every pid the test's stubs wrote.
    fn end(&mut self) {
        let pids = std::fs::read_dir(&self.dir).into_iter().flatten().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".pid")).filter_map(|e| std::fs::read_to_string(e.path()).ok());
        for pid in pids.chain([self.tui.clone()]).filter(|p| !p.trim().is_empty()) {
            let _ = std::process::Command::new("kill").args(["-9", pid.trim()]).stderr(std::process::Stdio::null()).status();
        }
        let _ = self.script.kill();
        let _ = self.script.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A scratch directory for a pty test, with its `bin` (the copy's PATH first) and, when
/// `lone`, a lone child's stub (`lone`: ignores HUP and TERM — only a KILL ends it — its pid
/// in `lone.pid`).
#[cfg(target_os = "linux")]
fn pty_dir(tag: &str, lone: bool) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("arena-tui-pty-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    if lone {
        let pid = dir.join("lone.pid");
        arena_core::remote::write_test_script(dir.join("lone"), format!("#!/bin/sh\ntrap '' HUP TERM\necho $$ > '{0}.tmp'\nmv '{0}.tmp' '{0}'\nexec sleep 60\n", pid.display())).unwrap();
    }
    dir
}

/// Review ai-review claude #4 (lane ASK9 item 1): a TERM on a live terminal puts it back at
/// once — the main screen within a second, not after the grace — and only then stops the
/// children: here an action's rsync that ignores TERM, KILLed after the grace, so the exit
/// (143) comes no sooner than that, the rsync gone with it. Before, the terminal stayed raw on
/// the alternate screen, nothing drawn, for the whole grace. The dashboard for real, in a copy
/// on a pty ([`OnPty`]).
#[cfg(target_os = "linux")]
#[test]
fn a_term_puts_a_live_terminal_back_before_the_grace() {
    if let Some(dir) = std::env::var_os(ON_PTY) {
        play_dashboard(std::path::Path::new(&dir));
    }
    let dir = pty_dir("term", true);
    let Some(mut on) = OnPty::start("a_term_puts_a_live_terminal_back_before_the_grace", &dir) else { return };
    let lone = std::fs::read_to_string(dir.join("lone.pid")).unwrap().trim().to_string();
    let mark = on.len();
    let sent = on.signal("-TERM");
    let back = on.wait_for(LEAVE, mark, Duration::from_secs(5));
    let exited = on.exited_within(arena_core::remote::TERM_GRACE + Duration::from_secs(4));
    let after = on.text(mark);
    let deadline = Instant::now() + Duration::from_millis(500);
    while running(&lone) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let lone_ran_on = running(&lone);
    on.end();
    let (_, back_at) = back.unwrap_or_else(|| panic!("the terminal wasn't put back: {after:?}"));
    let (status, exited_at) = exited.expect("the dashboard is still running after the grace and more");
    assert!(back_at - sent < Duration::from_secs(1), "put back {:?} after the TERM", back_at - sent);
    assert_eq!(status.code(), Some(143));
    assert!(exited_at - sent >= arena_core::remote::TERM_GRACE - Duration::from_millis(300), "exited {:?} after the TERM: the rsync wasn't waited for", exited_at - sent);
    assert!(!lone_ran_on, "the rsync {lone} outlived the dashboard");
    assert!(!after[after.find(LEAVE).unwrap()..].contains(ENTER), "the terminal was taken again: {after:?}");
}

/// Live ASK5 F1, review ai-review claude #1 (lane ASK9 item 1): its terminal hung up (the pty's
/// master closed), the dashboard ends — an action's rsync that ignores HUP and TERM KILLed
/// after the grace, gone with it — and meanwhile its loop no longer spins on the dead terminal
/// (crossterm retries its empty reads for ever: a core, 70–82 ticks in 0.7 s live): stdin
/// parked once the end has begun, the dashboard uses a few ticks over the whole grace. The
/// dashboard for real, in a copy on a pty ([`OnPty`]); it leads the terminal's session, so
/// the kernel's SIGHUP reaches it too.
#[cfg(target_os = "linux")]
#[test]
fn a_hung_up_dashboard_stops_its_children_without_spinning() {
    if let Some(dir) = std::env::var_os(ON_PTY) {
        play_dashboard(std::path::Path::new(&dir));
    }
    let dir = pty_dir("hangup", true);
    let Some(mut on) = OnPty::start("a_hung_up_dashboard_stops_its_children_without_spinning", &dir) else { return };
    let lone = std::fs::read_to_string(dir.join("lone.pid")).unwrap().trim().to_string();
    let tui = on.tui.clone();
    let before = cpu_ticks(&tui).unwrap();
    on.script.kill().unwrap();
    on.script.wait().unwrap();
    let hung = Instant::now();
    let mut used = 0;
    while running(&tui) && hung.elapsed() < arena_core::remote::TERM_GRACE + Duration::from_secs(4) {
        if let Some(now) = cpu_ticks(&tui) {
            used = now - before;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let (took, still) = (hung.elapsed(), running(&tui));
    let deadline = Instant::now() + Duration::from_millis(500);
    while running(&lone) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let lone_ran_on = running(&lone);
    on.end();
    assert!(!still, "the dashboard is still running {took:?} after its terminal hung up");
    assert!(took >= arena_core::remote::TERM_GRACE - Duration::from_millis(300), "gone {took:?} after: the rsync wasn't waited for");
    assert!(!lone_ran_on, "the rsync {lone} outlived the dashboard");
    assert!(used < 50, "the dashboard used {used} ticks of CPU in the {took:?} it took to end");
}

/// Review ai-review claude #6 (lane ASK9 item 3): `c` hands the terminal to ssh and takes it
/// back — and while ssh has it, the terminal isn't the dashboard's. A first `c`, its ssh
/// exiting at once: the dashboard's screen is back (left, then entered again). A second `c`,
/// its ssh still running, then a HUP: that ssh is stopped with the other children, the
/// dashboard exits 129, and writes nothing to the terminal ssh has — no restore into ssh's
/// session, no screen set up again as it goes. The dashboard for real, `ssh` a stub on PATH,
/// in a copy on a pty ([`OnPty`]). The second stub ignores HUP: the copy leads the terminal's
/// session, so its exit would have the kernel HUP its foreground group, ssh in it — which a
/// dashboard under a shell, or ended by a TERM from elsewhere, doesn't get (review claude #1:
/// that ssh was left a stopped background job); only the dashboard's own stop may end it.
#[cfg(target_os = "linux")]
#[test]
fn a_hangup_during_cs_ssh_stops_it_and_leaves_its_terminal_alone() {
    if let Some(dir) = std::env::var_os(ON_PTY) {
        play_dashboard(std::path::Path::new(&dir));
    }
    let dir = pty_dir("ssh", false);
    let ssh = |n: u32| dir.join(format!("ssh{n}.pid"));
    arena_core::remote::write_test_script(
        dir.join("bin/ssh"),
        format!(
            "#!/bin/sh\nn=$(cat '{0}/count' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '{0}/count'\n\
             echo $$ > '{0}'/ssh$n.pid.tmp; mv '{0}'/ssh$n.pid.tmp '{0}'/ssh$n.pid\n[ $n = 1 ] && exit 0\ntrap '' HUP\nexec sleep 60\n",
            dir.display()
        ),
    )
    .unwrap();
    let Some(mut on) = OnPty::start("a_hangup_during_cs_ssh_stops_it_and_leaves_its_terminal_alone", &dir) else { return };
    let wait_file = |p: &std::path::Path| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !p.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        p.exists()
    };
    // `c`, ssh exiting at once: the terminal handed over and taken back.
    let mark = on.len();
    on.keys("c");
    let first = wait_file(&ssh(1));
    let left = on.wait_for(LEAVE, mark, Duration::from_secs(5));
    let back = left.and_then(|(at, _)| on.wait_for(ENTER, at, Duration::from_secs(5)));
    std::thread::sleep(Duration::from_millis(300));
    // `c` again, ssh staying: a HUP while it has the terminal.
    let mark = on.len();
    on.keys("c");
    let second = wait_file(&ssh(2));
    let handed = on.wait_for(LEAVE, mark, Duration::from_secs(5));
    std::thread::sleep(Duration::from_millis(300));
    let mark = on.len();
    let sent = on.signal("-HUP");
    let exited = on.exited_within(arena_core::remote::TERM_GRACE + Duration::from_secs(4));
    let after = on.text(mark);
    let ssh2 = std::fs::read_to_string(ssh(2)).unwrap_or_default().trim().to_string();
    let deadline = Instant::now() + Duration::from_millis(500);
    while running(&ssh2) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let ssh_ran_on = !ssh2.is_empty() && running(&ssh2);
    let all = on.text(0);
    on.end();
    assert!(first && left.is_some() && back.is_some(), "the first `c`: ssh ran {first}, handed over {left:?}, back {back:?}: {all:?}");
    assert!(second && handed.is_some(), "the second `c`: ssh ran {second}, handed over {handed:?}: {all:?}");
    let (status, _) = exited.unwrap_or_else(|| panic!("the dashboard is still running {:?} after the HUP", sent.elapsed()));
    assert_eq!(status.code(), Some(129));
    assert!(!ssh_ran_on, "ssh {ssh2} outlived the dashboard");
    assert!(!after.contains("\x1b[?1049") && !after.contains("\x1b[?1000"), "the dashboard wrote to ssh's terminal: {after:?}");
}
