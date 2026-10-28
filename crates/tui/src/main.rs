//! `arena-tui` — the interactive dashboard.
//!
//! It lists pods from the configured provider and, for each pod with an SSH endpoint,
//! fetches GPU stats (`nvidia-smi`) and an optional operator-defined progress signal
//! (`PROGRESS_CMD`) over SSH. Fetching runs in a **background task** so the UI never
//! blocks: the loop redraws and handles keys continuously while fresh data lands as
//! soon as each sweep finishes. Move the cursor with `↑/↓`/`j/k`, open a per-pod detail
//! pane (per-GPU breakdown + util/temp sparklines) with `Enter`, act on the selected
//! pod with `a` (restart / stop / terminate / backup / setup / test / run / set-branch),
//! ask claude or codex about it (or the marked set) with `?` (`ask.rs`, in the background),
//! `f` cycles the refresh cadence, `r` refreshes now.
//!
//! Each refresh is one `arena_core::snapshot` [`FleetSnapshot`] — the pods as listed, the
//! local proxy config and the health cache, joined by the same builder `arena snapshot`
//! uses — so $/h and the fleet total, the maintenance badge, the proxy port + live/stale
//! state and the last `pods test --deep` verdict read exactly as the CLI prints them. `d`
//! deep-checks the cursor pod (or the marked set) in the background and records the
//! verdicts in that cache; `/` marks pods by the CLI's selector syntax (`apple..delta`).
//! Every pod SSH call goes through `arena_core::remote::Remote` with a time budget, so a
//! wedged pod can't hang an action. The summary bar also carries the prepaid accounts'
//! balance and runway (`arena balance`'s views, observed inline in the refresh's slower
//! details round — at most one live read per account per 5 min — and recorded in the same
//! state file the CLI keeps; a low or critical one leads the bar).
//!
//! Safety against live prod is built into the *interaction*, not bolted on: a mutating
//! action always pops a confirmation modal. Lifecycle actions (restart/stop/terminate)
//! make you type the pod's exact name back before they apply; backup/setup say what they
//! will do in plain words (`v` shows the generated command) and take a single `y`. There is
//! no way to mutate a pod without going through that modal — the dashboard's reads stay reads.
//!
//! Provider is chosen by `ARENA_PROVIDER` (default `runpod`); config path by
//! `ARENA_CONFIG`; initial cadence by `ARENA_REFRESH_SECS` (default 5).

mod ask;
mod prefs;
mod state;

use std::collections::HashMap;
use std::io::{stdout, Stdout};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Sparkline, Table, TableState, Wrap},
};
use tokio::sync::Notify;
use tokio::task::JoinSet;

use arena_core::balance::{self, AccountView, BalanceApi, BalancePolicy, HttpBalanceApi};
use arena_core::openrouter::{CohortUsage, KeyUsage, OpenRouter, UsageView};
use arena_core::fleet;
use arena_core::health::{deep_check_command, judge_deep_call, HealthPolicy, PodHealth, DEEP_CHECK_TIMEOUT};
use arena_core::metrics::{self, PodMetrics, ProbeOpts};
use arena_core::provider::{ContainerReset, Provider};
use arena_core::proxy::Listing;
use arena_core::remote::{describe_error, Remote, SshRemote};
use arena_core::selector::Naming;
use arena_core::snapshot::{self, FleetSnapshot, HealthCache};
use arena_core::ssh::SshTarget;
use arena_core::naming;
use arena_core::{Config, Pod, PodSpec};

use prefs::Prefs;
use arena_core::status::pod_status;
use state::{
    action_summary, capture_details, dashboard_snapshot, details_due, display_name, health_cell, mark_key, marked_pods,
    marked_set_token, merge_details, overlay_details, partial_notice, proxy_cell, proxy_detail, rtt_cell,
    select_marks, short_branch, spark, summarize, summary_text, Action, Confirm, DeepChecks, Endpoint, FleetSummary,
    History, NewPodForm, NpField, PodDetails, ProviderOpt, Rtts, Tone, RTT_EVERY, RTT_WAIT,
};

const DEFAULT_CONFIG: &str = "/home/dev/prod-ro/config.env";
/// Shorter than backup/setup's 10s: a down pod shouldn't stall a whole metrics sweep.
const METRICS_CONNECT_TIMEOUT: u32 = 4;
/// The cadences `f` cycles through (seconds).
const REFRESH_STEPS: &[u64] = &[2, 5, 10, 20, 60];

// Per-call SSH budgets for the actions (every pod call goes through `Remote` with one, so
// a wedged pod ends its action with `timed out after Ns` instead of a `working…` modal
// that never closes). The metrics probe uses core's `PROBE_TIMEOUT`, setup core's
// `SetupTimeouts` and the deep check core's `DEEP_CHECK_TIMEOUT`; the rest mirror the
// CLI's budgets for the same commands, which live in its binary, not in core.

/// `test` (import torch): a cold import takes 10–30s, so 90s means wedged — as `pods test`.
const TEST_TIMEOUT: Duration = Duration::from_secs(90);

/// `run`: an arbitrary command can legitimately take a while (a download, a test suite),
/// but never forever — the CLI's `pods run` default, 30 min.
const RUN_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// `backup`: git add + commit + push — seconds normally, a first push of notebooks can take
/// minutes. 5 min, as `pods backup`.
const BACKUP_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// `set-branch`: fetch + checkout + fast-forward against GitHub — 2 min means stuck, as `pods
/// set-branch`.
const BRANCH_TIMEOUT: Duration = Duration::from_secs(2 * 60);

// set-branch's switch and init-branches bound themselves on the pod (`timeout`, then a KILL) inside
// that budget, so their pod side has ended before arena reports a timeout and releases the pod's
// lock (review ai-review claude #13).
const _: () = assert!(arena_core::backup::SWITCH_BOUND_SECS + arena_core::backup::SWITCH_KILL_AFTER_SECS < BRANCH_TIMEOUT.as_secs());

/// How long one details query (`Provider::enrich`) may take before the refresh carries on
/// without it — the HTTP client has no timeout of its own (`pods list` uses the same 20s).
const ENRICH_TIMEOUT: Duration = Duration::from_secs(20);

/// How long a footer notice (a selector result, a finished deep check) stays up.
const NOTICE_FOR: Duration = Duration::from_secs(12);

/// What the UI is currently showing. `List`/`Detail` are the normal views; the rest
/// are modal (the background fetch keeps running, but a modal can't be acted on by a
/// stray refresh — it only reads shared data).
#[derive(Debug, Clone)]
enum Mode {
    List,
    Detail,
    /// Action chooser for the selected pod (`sel` = highlighted row, navigable ↑/↓).
    Menu { sel: usize },
    /// A pending action awaiting confirmation.
    Confirm(Confirm),
    /// Multi-pod action chooser (safe ops). `scope` is the whole fleet (`A`) or just the
    /// marked set (`a` with marks); `sel` = highlighted row.
    FleetMenu { scope: Scope, sel: usize },
    /// A pending multi-pod action; requires typing the confirm token (`ALL` for the whole
    /// fleet, the pod count for a marked set — `ALL` again for terminate/restart/stop on a big
    /// one or one that is every listed pod: [`fleet_confirm_token`]). `note`: what the confirm
    /// says besides (a stop's pods it will skip, and why — [`StopPlan::skip_lines`]).
    FleetConfirm { action: Action, typed: String, scope: Scope, note: String },
    /// Interactive add-pod form (↑↓ field, ←→ value).
    NewPod(NewPodForm),
    /// Collect a free-text argument (a command, or a branch) for `Run`/`SetBranch`,
    /// against one pod or a multi-pod scope, then execute on Enter.
    Input { action: Action, scope: InputScope, value: String },
    /// `/`: type a selection in the CLI's selector syntax; Enter marks what it resolves to
    /// (a typo marks nothing and says why in the footer).
    Select { value: String },
    /// A background action is in flight (the SSH work runs off the UI thread so the
    /// dashboard stays live); replaced by `Result` when it finishes. Keys are ignored.
    Working(String),
    /// The outcome of the last action; any key dismisses (then we nudge a refresh).
    Result(String),
    /// `?`: the question about the cursor pod or the marked set, prefilled; Enter asks `pick`'s AI
    /// in the background, Tab switches to the other (`ask.rs`).
    Ask { scope: InputScope, value: String, pick: arena_core::ask::Pick },
    /// An answer to a `?` question: scrollable; Esc closes.
    Answer(ask::Answer),
}

/// Which pods a multi-pod action targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// Every pod the provider reports.
    All,
    /// The marked (space-selected) set.
    Marked,
}

/// Who an [`Mode::Input`] action targets.
#[derive(Debug, Clone)]
enum InputScope {
    Pod { name: String, id: String },
    Set(Scope),
}

/// Live fleet data, written by the background fetch task and read by the UI thread.
/// Guarded by a std `Mutex` held only for brief, synchronous critical sections — never
/// across an `.await`, so the fetcher and the UI never deadlock.
#[derive(Default)]
struct Shared {
    /// The rows, in display order: always `snap.pods`' pods, set together by
    /// [`Shared::publish`] — so a row index means the same pod in both.
    pods: Vec<Pod>,
    /// This refresh's core snapshot: per pod its proxy port + state and last deep check,
    /// plus the fleet cost and which providers failed to list.
    snap: FleetSnapshot,
    /// Deep checks started from the dashboard (running ones, and this session's verdicts).
    deep: DeepChecks,
    /// The footer's one-line message, if any (see [`Notice`]).
    notice: Option<Notice>,
    metrics: HashMap<String, PodMetrics>,
    history: HashMap<String, History>,
    summary: FleetSummary,
    status: String,
    last_refresh: Option<Instant>,
    /// True while a sweep is in flight (drives the footer's ⟳ indicator).
    refreshing: bool,
    /// Optimistic placeholders for pods we just created but the provider's list API
    /// hasn't reported yet — shown as "pending" so a new card appears immediately. The
    /// fetcher drops each one as soon as the real pod shows up.
    pending: Vec<Pod>,
    /// Set by a background action task when it finishes; the UI loop swaps it into a
    /// `Result` modal. Lets SSH actions run off the UI thread so the dashboard never
    /// freezes mid-`setup`.
    action_result: Option<String>,
    /// True while a background action is in flight (so new triggers are ignored).
    action_running: bool,
    /// The prepaid accounts as last observed in a details round (empty before the first,
    /// or with no prepaid key configured).
    balance: Vec<AccountView>,
    /// OpenRouter's keys and credit as last read (`None`: no provisioning key configured —
    /// no `AI$` column, nothing in the summary bar).
    ai: Option<UsageView>,
    /// The cohort's share of it, for the summary bar.
    ai_cohort: Option<CohortUsage>,
    /// `n`'s fresh listing, set by its background task ([`start_new_pod_listing`]); the UI
    /// loop turns it into the add-pod form ([`new_pod_mode`]).
    new_pod_listing: Option<std::result::Result<Vec<Pod>, String>>,
    /// What a restart does to each row's container disk, by [`mark_key`]: its backend's answer
    /// (`Provider::restart_resets_container`, pure), set with the rows each refresh — what the
    /// action menus' restart label says ([`Shared::restart_reset`]).
    resets: HashMap<String, ContainerReset>,
    /// A `?` question out (`asking claude about …`, for the footer); `None` when none is.
    ask_running: Option<String>,
    /// When it was asked: the footer's spinner and seconds while it's out.
    ask_since: Option<Instant>,
    /// Its answer, set by the background task; the loop opens it ([`ask::land`]).
    ask_answer: Option<ask::Answer>,
    /// The RTT from this box to each pod's SSH endpoint, as last timed (the RTT column).
    rtts: Rtts,
}

impl Shared {
    /// Show `snap`: its pods become the rows, in its order.
    fn publish(&mut self, snap: FleetSnapshot) {
        self.pods = snap.pods.iter().map(|p| p.pod.clone()).collect();
        self.snap = snap;
    }

    /// What a restart does to these pods' disks, for the menu's label: the worst of their
    /// backends' answers (`state::worst_reset`); a pod not answered for yet is `Unknown`.
    fn restart_reset<'a>(&self, pods: impl IntoIterator<Item = &'a Pod>) -> ContainerReset {
        state::worst_reset(pods.into_iter().map(|p| self.resets.get(&mark_key(p)).copied().unwrap_or(ContainerReset::Unknown)))
    }

    fn notify(&mut self, text: impl Into<String>, error: bool) {
        self.notice = Some(Notice { text: text.into(), error, at: Instant::now() });
    }
}

/// A one-line message in the footer — a selector's result or typo, a deep check starting
/// or finishing — shown for [`NOTICE_FOR`] (or until the next one) instead of the refresh
/// status. Not a modal: nothing waits on the operator to dismiss it.
#[derive(Debug, Clone)]
struct Notice {
    text: String,
    error: bool,
    at: Instant,
}

/// UI-thread-only state: what the operator is looking at / interacting with. Kept
/// separate from `Shared` so key handling never contends with the fetcher.
struct Ui {
    provider_name: String,
    config_path: String,
    cfg: Config,
    /// How the dashboard reaches pods: [`SshRemote`] for real (a fake in tests).
    remote: Arc<dyn Remote>,
    /// `MACHINE_NAME_PREFIX`, for shortening names/branches.
    prefix: String,
    /// Show short pod names (`apple`) instead of full (`arena8-apple`). Persisted.
    short_names: bool,
    mode: Mode,
    /// Cursor into `Shared::pods` (clamped to a valid row each frame).
    selected: usize,
    /// Pods marked for a bulk action (space, a click, or `/`), by `state::mark_key`
    /// (`provider:id` — a bare id can belong to two providers' pods).
    marked: std::collections::HashSet<String>,
    /// Scroll offset for the result modal (so long fleet outputs are readable) and the `?`
    /// answer popup.
    result_scroll: u16,
    /// How far the result or answer popup last drawn can scroll — its text's height less its
    /// view's — set as it's drawn, so a key never scrolls past the end ([`scroll_by`]).
    result_max: std::cell::Cell<u16>,
    /// The pods table's last-rendered `(area, scroll_offset)`, set by `pods_table` each
    /// frame so mouse clicks can be mapped back to a pod row. Cell because `view` borrows
    /// `&Ui` (the TUI is single-threaded, so interior mutability is safe here).
    last_table: std::cell::Cell<(Rect, usize)>,
    /// What answers `?` ([`ask::real_asker`]; a fake in tests).
    asker: ask::Asker,
    /// The AI last picked in `?` (Tab, or a question asked): the next `?` opens with it.
    /// `None`: `ASK_AI`'s.
    ask_ai: Option<arena_core::ask::Ai>,
    /// When `D` was pressed, and the pods it named ([`mark_key`]): a second `D` within
    /// [`DEEP_ALL_CONFIRM`] deep-checks those of them that still qualify.
    deep_all_armed: Option<(Instant, std::collections::HashSet<String>)>,
}

impl Ui {
    /// The pod's name as currently displayed (short or full).
    fn shown_name(&self, full: &str) -> String {
        display_name(full, &self.prefix, self.short_names)
    }
}

/// What `arena-tui` does with its command-line arguments: it takes none (`arena tui` passes
/// the config and provider in `ARENA_CONFIG`/`ARENA_PROVIDER`), so any argument stops it
/// before the config is read — `Some((exit code, message))`; `-h`/`--help` is usage, exit 0.
/// It used to ignore them: `arena-tui --config <sandbox config>` loaded the default (prod)
/// config and refreshed the prod fleet (live, 2026-10-18).
fn args_refusal(args: &[String]) -> Option<(i32, String)> {
    let usage = format!(
        "arena-tui takes no arguments: the config comes from ARENA_CONFIG (default {DEFAULT_CONFIG}), \
         the provider from ARENA_PROVIDER. Run `arena [--config <file>] [--provider <name>] tui`."
    );
    match args {
        [] => None,
        [a] if a == "-h" || a == "--help" => Some((0, usage)),
        _ => Some((2, format!("arena-tui: unexpected argument {:?} — {usage}", args[0]))),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some((code, message)) = args_refusal(&args) {
        if code == 0 {
            println!("{message}");
        } else {
            eprintln!("{message}");
        }
        std::process::exit(code);
    }
    // No terminal, no dashboard — and nothing read or refreshed first (the refresh starts
    // before the screen is set up, so a run without one used to poll the fleet, then fail).
    {
        use std::io::IsTerminal;
        if !std::io::stdout().is_terminal() {
            eprintln!("arena-tui: needs a terminal (stdout isn't one) — nothing was read or refreshed");
            std::process::exit(2);
        }
    }
    let config_path =
        std::env::var("ARENA_CONFIG").unwrap_or_else(|_| DEFAULT_CONFIG.to_string());
    let cfg = Config::load(&PathBuf::from(&config_path))
        .with_context(|| format!("loading config {config_path}"))?;
    let provider_name = std::env::var("ARENA_PROVIDER").unwrap_or_else(|_| "runpod".to_string());
    // Fleet-spanning provider (like the CLI): the dashboard lists pods across ALL configured
    // backends — runpod + vast + hetzner — not just the primary. `warn_on_partial=false` so a
    // provider hiccup (e.g. Vast 429) doesn't print onto the alternate screen. Creates still
    // go to the chosen primary (ARENA_PROVIDER).
    let provider: Arc<dyn Provider> =
        Arc::from(arena_core::provider::build_fleet(&provider_name, &cfg, false)?);
    let progress_cmd = cfg.get("PROGRESS_CMD").map(String::from);
    let initial_secs = std::env::var("ARENA_REFRESH_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);

    let shared = Arc::new(Mutex::new(Shared {
        status: "loading…".into(),
        ..Default::default()
    }));
    let interval = Arc::new(AtomicU64::new(initial_secs));
    let nudge = Arc::new(Notify::new());
    let details_wanted = Arc::new(AtomicBool::new(false));
    let remote: Arc<dyn Remote> = Arc::new(SshRemote);

    // The prepaid balances for the summary bar: observed in the refresh's details round
    // (bounded, quiet), recorded in the CLI's state file. A bad BALANCE_WARN_HOURS falls
    // back to the default (`arena balance` is the place that says so).
    let (policy, _) = BalancePolicy::from_config(&cfg);
    let balance = BalanceCtx::new(Arc::new(HttpBalanceApi::from_config(&cfg)), balance_state_path(&cfg).and_then(Result::ok), policy);
    // OpenRouter's keys and credit, on the same cadence, when a provisioning key is set.
    let ai = OpenRouter::from_config(&cfg).map(AiCtx::new);

    // Background fetcher: keeps `shared` fresh without blocking the UI.
    tokio::spawn(fetch_loop(
        provider.clone(),
        remote.clone(),
        cfg.clone(),
        progress_cmd,
        shared.clone(),
        Cadence { interval: interval.clone(), nudge: nudge.clone(), details_wanted: details_wanted.clone() },
        DetailsState { balance: Some(balance), ai, rtt: Some(Rtts::default()), ..Default::default() },
    ));

    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena").to_string();
    let ui = Ui {
        provider_name,
        config_path,
        asker: ask::real_asker(remote.clone(), cfg.clone()),
        remote,
        prefix,
        short_names: Prefs::load().short_names,
        cfg,
        mode: Mode::List,
        selected: 0,
        marked: std::collections::HashSet::new(),
        result_scroll: 0,
        result_max: std::cell::Cell::new(0),
        last_table: std::cell::Cell::new((Rect::default(), 0)),
        ask_ai: None,
        deep_all_armed: None,
    };

    dashboard(&shared, &provider, &interval, &nudge, &details_wanted, ui).await
}

/// The dashboard on this terminal until it ends — `main`'s, and a test's copy's. However it ends,
/// the latch comes first (`arena_core::remote::end_children`: no child starts after it), then the
/// terminal is put back, then every child still running is stopped (TERM, then KILL after the
/// grace: `arena_core::remote::stop_all_children`) — the operator's terminal back at once, never
/// held for the grace (review ai-review claude #4). `q` and Ctrl-C end the loop, and that's done
/// here, the runtime then dropping the rest; a hangup, a TERM, a dead terminal or a failed read or
/// draw end it from outside the loop ([`end_now`], which exits). A cause after the first leaves
/// the ending to that one.
async fn dashboard(
    shared: &Arc<Mutex<Shared>>,
    provider: &Arc<dyn Provider>,
    interval: &AtomicU64,
    nudge: &Notify,
    details_wanted: &AtomicBool,
    ui: Ui,
) -> Result<()> {
    exit_on_signals();
    let mut terminal = setup_terminal()?;
    exit_on_hangup();
    match run(&mut terminal, shared, provider, interval, nudge, details_wanted, ui).await {
        // `q` or Ctrl-C — or the loop finding the terminal put back by an end already under way,
        // which exits.
        Ok(()) => {
            if !arena_core::remote::end_children() {
                return std::future::pending().await;
            }
            let restored = restore_terminal(&mut terminal);
            arena_core::remote::stop_all_children().await;
            restored.map(drop)
        }
        // A terminal that fails a read or a draw is taken as gone: no retry.
        Err(e) => {
            end_now(1, Some(format!("{e:#}"))).await;
            Ok(())
        }
    }
}

/// When the background refresh runs: every `interval` seconds, or at once on `nudge`;
/// `details_wanted` (the `r` key) asks for the provider's details query too.
struct Cadence {
    interval: Arc<AtomicU64>,
    nudge: Arc<Notify>,
    details_wanted: Arc<AtomicBool>,
}

/// The background refresh loop: one [`refresh`] per cadence tick, waiting for the
/// cadence to elapse *or* a manual nudge (`r`/`f`/action), whichever comes first. `details`
/// starts with what the slower round reads besides the provider: balances, OpenRouter.
async fn fetch_loop(
    provider: Arc<dyn Provider>,
    remote: Arc<dyn Remote>,
    cfg: Config,
    progress_cmd: Option<String>,
    shared: Arc<Mutex<Shared>>,
    cadence: Cadence,
    mut details: DetailsState,
) {
    // The per-pod probe gathers GPU stats + branch + setup health in one SSH call.
    let repo_path = cfg.get("BACKUP_REPO_PATH").map(String::from).unwrap_or_else(|| {
        format!("/root/{}", cfg.get("ARENA_REPO_NAME").unwrap_or("ARENA_materials"))
    });
    let opts = ProbeOpts {
        progress_cmd,
        repo_path: Some(repo_path),
        key_remote: Some(cfg.get("GIT_SSH_KEY_REMOTE").unwrap_or("/root/.ssh/id_ed25519").to_string()),
        nvidia_smi_timeout: None,
        repo_timeout: None,
    };
    loop {
        shared.lock().unwrap().refreshing = true;
        let forced = cadence.details_wanted.swap(false, Ordering::Relaxed);
        refresh(provider.as_ref(), &remote, &cfg, &opts, &shared, &mut details, forced).await;

        let secs = cadence.interval.load(Ordering::Relaxed).max(1);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
            _ = cadence.nudge.notified() => {}
        }
    }
}

/// What the refresh remembers between rounds about the provider's details query.
#[derive(Default)]
struct DetailsState {
    /// The last good answer, re-applied to every listing until the next one.
    details: HashMap<String, PodDetails>,
    /// When the query last ran (tokio's clock, so tests can step it).
    last: Option<tokio::time::Instant>,
    /// Why the last query's answer is incomplete, if it is.
    warning: Option<String>,
    /// The prepaid balances, observed on the details cadence (`None`: not shown — tests).
    balance: Option<BalanceCtx>,
    /// OpenRouter's keys and credit, read on the details cadence (`None`: no provisioning key).
    ai: Option<AiCtx>,
    /// The RTT timings kept between rounds (`None`: none taken — tests, whose fixture
    /// endpoints must never be connected to).
    rtt: Option<Rtts>,
}

/// What the `AI$` column and the summary bar's OpenRouter part read with: the provisioning
/// API (scoped to `OPENROUTER_WORKSPACE`), when a read last started, the view the reads
/// update, and the read in flight. Reads run in the background: a slow listing (a workspace
/// of many pages) never holds a refresh, and what it read shows from the next one.
struct AiCtx {
    api: Arc<OpenRouter>,
    last: Option<tokio::time::Instant>,
    view: Arc<Mutex<UsageView>>,
    reading: Option<tokio::task::JoinHandle<()>>,
}

impl AiCtx {
    fn new(api: OpenRouter) -> Self {
        AiCtx { api: Arc::new(api), last: None, view: Arc::default(), reading: None }
    }
}

/// How long the key listing may take (every page, the workspace lookup first) before the
/// last one is kept.
const AI_KEYS_TIMEOUT: Duration = Duration::from_secs(20);
/// How long the credit read may take: as long as an inline balance read.
const AI_CREDITS_TIMEOUT: Duration = balance::INLINE_TIMEOUT;

/// Read OpenRouter's keys and credit, each within its own bound, and apply them to `view`
/// apart ([`UsageView::apply`]): one failing never holds back the other's fresh answer.
async fn read_ai(api: Arc<OpenRouter>, view: Arc<Mutex<UsageView>>) {
    async fn bounded<T>(d: Duration, f: impl std::future::Future<Output = arena_core::Result<T>>) -> std::result::Result<T, String> {
        match tokio::time::timeout(d, f).await {
            Ok(r) => r.map_err(|e| e.to_string()),
            Err(_) => Err(format!("timed out after {}s", d.as_secs())),
        }
    }
    let (keys, credits) = tokio::join!(bounded(AI_KEYS_TIMEOUT, api.list_keys()), bounded(AI_CREDITS_TIMEOUT, api.credits()));
    view.lock().unwrap_or_else(std::sync::PoisonError::into_inner).apply(keys, credits);
}

/// What the summary bar's balance reads with: the reads, the state file the CLI keeps too
/// (`None`: live readings only) and the threshold — plus the state as last applied here,
/// so a state file that can't be saved never makes every round read live again.
struct BalanceCtx {
    api: Arc<dyn BalanceApi>,
    path: Option<PathBuf>,
    policy: BalancePolicy,
    /// The state as this dashboard last applied it.
    memory: Option<balance::BalanceState>,
    /// When the balance was last observed (tokio's clock).
    last: Option<tokio::time::Instant>,
}

impl BalanceCtx {
    fn new(api: Arc<dyn BalanceApi>, path: Option<PathBuf>, policy: BalancePolicy) -> Self {
        BalanceCtx { api, path, policy, memory: None, last: None }
    }

    /// The state to observe against: the file's, unless what this dashboard last applied
    /// is newer (the file couldn't be written, or another writer lost a race).
    fn state(&self) -> balance::BalanceState {
        let (file, _) = balance::load_opt(self.path.as_deref());
        let newest = |s: &balance::BalanceState| s.accounts.values().filter_map(|a| a.last_live_attempt()).max();
        match &self.memory {
            Some(m) if newest(m) > newest(&file) => m.clone(),
            _ => file,
        }
    }
}

/// Observe the prepaid balances (inline: ≤ INLINE_TIMEOUT, a live read only when the last
/// attempt is FRESH old), quiet whatever happens to them.
async fn observe_balance(b: &BalanceCtx, now: u64) -> (balance::BalanceState, Vec<balance::Fetched>) {
    let state = b.state();
    let fetched = balance::observe(b.api.as_ref(), &state, balance::Mode::Inline, balance::INLINE_TIMEOUT, now).await;
    (state, fetched)
}

/// Record what was observed against this listing's burns (unknown for a provider that
/// failed to list — every one, when none answered) and judge it.
fn balance_views(
    b: &mut BalanceCtx,
    (state, fetched): (balance::BalanceState, Vec<balance::Fetched>),
    listing: &Listing,
    pods: &[Pod],
    now: u64,
) -> Vec<AccountView> {
    let burns = balance::burns_with(listing, pods, &state, &b.policy);
    let (state, _) = balance::commit(b.path.as_deref(), state, &fetched, &burns, balance::Mode::Inline, now);
    let views = balance::views(&state, &fetched, &burns, &b.policy, balance::Mode::Inline, now);
    b.memory = Some(state);
    b.last = Some(tokio::time::Instant::now());
    views
}

/// One refresh: list pods (per provider, so a provider that failed is known — not just
/// missing), probe them concurrently, build the core snapshot and publish it to `shared`.
///
/// API calls per refresh are what they always were — one list per provider. The details
/// query (`Provider::enrich`: RunPod's GraphQL for GPU, $/h and the host maintenance
/// window) runs alongside the probes only when `state::details_due` says so (every
/// [`state::DETAILS_EVERY`], or on `r` = `forced`), and its answer is re-applied to the
/// listings in between. The proxy config and the health cache are local file reads.
async fn refresh(
    provider: &dyn Provider,
    remote: &Arc<dyn Remote>,
    cfg: &Config,
    opts: &ProbeOpts,
    shared: &Mutex<Shared>,
    details: &mut DetailsState,
    forced: bool,
) {
    let listing = Listing::from_results(provider.list_by_provider().await);
    let now = snapshot::unix_now();
    // The prepaid balances on the details cadence (every DETAILS_EVERY, or `r` after the
    // minimum gap) — and FRESH keeps live reads to one per account per 5 min however often.
    let balance_due = details.balance.as_ref().is_some_and(|b| details_due(b.last.map(|t| t.elapsed()), forced));
    if !listing.any_ok() {
        // Keep the last known pods on screen; just report the error. The balance is still
        // observed and aged — with every burn unknown, from the provider's rate or the last
        // sample, else `?` — never frozen at its last value.
        let views = match details.balance.as_mut().filter(|_| balance_due) {
            Some(b) => {
                let seen = observe_balance(b, now).await;
                Some(balance_views(b, seen, &listing, &[], now))
            }
            None => None,
        };
        let errs: Vec<String> = listing.errors().iter().map(|(p, e)| format!("{p}: {e}")).collect();
        let mut s = shared.lock().unwrap();
        s.status = format!("list error: {}", errs.join("; "));
        if let Some(v) = views {
            s.balance = v;
        }
        s.refreshing = false;
        return;
    }
    let partial: Vec<String> = listing.errors().iter().map(|(p, _)| p.to_string()).collect();
    let mut pods = listing.pods();
    // The first refresh shows the rows as soon as they're listed: the probes, the details
    // query and the balance reads below can take their full budgets (seconds, up to
    // ENRICH_TIMEOUT), and until now the screen said `loading…` for all of it.
    if shared.lock().unwrap().last_refresh.is_none() {
        publish_listing(cfg, shared, &pods, &partial, now);
    }
    // OpenRouter on the details cadence, in the background (never two reads at once).
    if let Some(a) = details.ai.as_mut() {
        let idle = a.reading.as_ref().is_none_or(|h| h.is_finished());
        if idle && details_due(a.last.map(|t| t.elapsed()), forced) {
            a.reading = Some(tokio::spawn(read_ai(a.api.clone(), a.view.clone())));
            a.last = Some(tokio::time::Instant::now());
        }
    }
    // `SSH_CONFIG_FILE` kept current, as `pods list` keeps it: from a complete listing only (a
    // provider that failed would drop its pods' blocks), written only when an endpoint moved; a
    // failure is a status-line warning, never more.
    let ssh_warning = if partial.is_empty() {
        // No home under test: a `~/` value can't reach the developer's own ~/.ssh.
        let home = if cfg!(test) { None } else { std::env::var("HOME").ok() };
        match arena_core::sshconfig::sync_for(cfg, &pods, home.as_deref()) {
            Some(Err(e)) => Some(format!("ssh config not rewritten: {e}")),
            _ => None,
        }
    } else {
        None
    };
    let observing = async {
        match details.balance.as_ref().filter(|_| balance_due) {
            Some(b) => Some(observe_balance(b, now).await),
            None => None,
        }
    };
    // RTT: the endpoints due a timing ([`RTT_EVERY`]), timed alongside the SSH probes — each
    // bounded by [`RTT_WAIT`], well inside the probes' own budget, so the round isn't longer.
    let rtt_due = details.rtt.as_ref().map(|r| r.due(&pods, Instant::now())).unwrap_or_default();
    let (metrics, observed, timed) = if details_due(details.last.map(|t| t.elapsed()), forced) {
        let mut enriched = pods.clone();
        let (metrics, failed, seen, timed) = tokio::join!(
            fetch_metrics(remote, &pods, cfg, opts),
            enrich_bounded(provider, &mut enriched),
            observing,
            time_endpoints(rtt_due)
        );
        // A query that failed part-way still shows what it filled (as `pods list` does),
        // over the last good details rather than instead of them (`state::merge_details`).
        merge_details(&mut details.details, capture_details(&enriched), failed.is_none());
        details.warning = failed;
        details.last = Some(tokio::time::Instant::now());
        (metrics, seen, timed)
    } else {
        tokio::join!(fetch_metrics(remote, &pods, cfg, opts), observing, time_endpoints(rtt_due))
    };
    if let Some(r) = details.rtt.as_mut() {
        r.record(timed, &pods, Instant::now());
    }
    overlay_details(&mut pods, &details.details);
    // The balance's runway is judged against what is billing as listed (not the pending
    // placeholders, and after the details filled the prices), a provider that failed to
    // list counting as unknown — never as $0/h.
    let balance_views = match (observed, details.balance.as_mut()) {
        (Some(seen), Some(b)) => Some(balance_views(b, seen, &listing, &pods, now)),
        _ => None,
    };
    let (proxy, proxy_warning) = snapshot::local_proxy_text(cfg);
    let (file_health, health_warning) = load_health(cfg);

    let mut guard = shared.lock().unwrap();
    let s = &mut *guard;
    // Drop optimistic placeholders the provider now reports, then show the
    // real fleet plus any still-pending placeholders.
    s.pending.retain(|pp| !pods.iter().any(|r| r.name == pp.name));
    let mut display = pods;
    display.extend(s.pending.iter().cloned());
    let health = s.deep.layer(file_health);
    let snap = dashboard_snapshot(&display, &partial, proxy.as_deref(), &health, &Naming::from_config(cfg), now);

    for pod in &display {
        let m = metrics.get(&pod.name);
        let pct = |(u, t): (u32, u32)| if t > 0 { (u as u64 * 100 / t as u64) as u32 } else { 0 };
        // A CPU-only box's graphs are its CPU and RAM (there's no GPU to draw).
        let (util, mem_pct) = if pod.cpu_only() {
            (m.and_then(|m| m.cpu_pct), m.and_then(|m| m.host_mem_summary()).map(pct))
        } else {
            (m.and_then(|m| m.mean_util()), m.and_then(|m| m.mem_summary()).map(pct))
        };
        s.history.entry(pod.name.clone()).or_default().push(util, m.and_then(|m| m.max_temp()), mem_pct);
    }
    s.summary = summarize(&display, &metrics);
    if let Some(a) = &details.ai {
        let view = a.view.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
        let names: Vec<String> = display.iter().map(|p| p.name.clone()).collect();
        s.ai_cohort = Some(view.cohort(prefix, &names));
        s.ai = Some(view);
    }
    let warnings: Vec<&str> =
        [&details.warning, &proxy_warning, &health_warning, &ssh_warning].into_iter().flatten().map(String::as_str).collect();
    s.status = status_line(&s.summary, &warnings);
    s.metrics = metrics;
    if let Some(r) = &details.rtt {
        s.rtts = r.clone();
    }
    if let Some(v) = balance_views {
        s.balance = v;
    }
    s.resets = backend_resets(provider, &display);
    s.publish(snap);
    s.last_refresh = Some(Instant::now());
    s.refreshing = false;
}

/// Show `pods` as listed, before anything about them is read: the rows, their proxy and last
/// deep check (local files) and the counts — no metrics yet. The first refresh's early paint.
fn publish_listing(cfg: &Config, shared: &Mutex<Shared>, pods: &[Pod], partial: &[String], now: u64) {
    let (proxy, _) = snapshot::local_proxy_text(cfg);
    let (file_health, _) = load_health(cfg);
    let mut s = shared.lock().unwrap();
    let health = s.deep.layer(file_health);
    let snap = dashboard_snapshot(pods, partial, proxy.as_deref(), &health, &Naming::from_config(cfg), now);
    s.summary = summarize(pods, &HashMap::new());
    s.status = format!("listed {} — reading them…", count_label(pods.len()));
    s.publish(snap);
}

/// `1 pod`, `3 pods`.
fn count_label(n: usize) -> String {
    format!("{n} pod{}", if n == 1 { "" } else { "s" })
}

/// What a restart does to each of `pods`' container disks, by [`mark_key`]: each one's backend's
/// answer (`Provider::restart_resets_container` — pure, no request), for the menus' restart label.
fn backend_resets(provider: &dyn Provider, pods: &[Pod]) -> HashMap<String, ContainerReset> {
    pods.iter().map(|p| (mark_key(p), provider.restart_resets_container(p))).collect()
}

/// The provider's best-effort details query, bounded by [`ENRICH_TIMEOUT`]. `Some(why)`
/// when the details are incomplete (the pods keep whatever was filled before a failure).
async fn enrich_bounded(provider: &dyn Provider, pods: &mut [Pod]) -> Option<String> {
    match tokio::time::timeout(ENRICH_TIMEOUT, provider.enrich(pods)).await {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(format!("pod details incomplete: {e}")),
        Err(_) => Some(format!("pod details timed out after {}s", ENRICH_TIMEOUT.as_secs())),
    }
}

/// Where the dashboard reads — and its deep checks write — the health cache: the CLI's
/// path ([`snapshot::health_cache_path_for`]), so `pods test --deep`'s verdicts show here
/// and the dashboard's show in `arena snapshot`. `None` in this crate's unit tests unless
/// the test's config sets `ARENA_STATE_DIR`: a test must never touch the developer's real
/// `~/.local/state`.
fn health_cache_path(cfg: &Config) -> Option<std::result::Result<PathBuf, String>> {
    if cfg!(test) && cfg.get("ARENA_STATE_DIR").is_none() {
        return None;
    }
    Some(snapshot::health_cache_path_for(cfg))
}

/// Where the balance state lives — the CLI's file ([`snapshot::BALANCE_FILE`] beside the
/// health cache), under the same test guard as [`health_cache_path`].
fn balance_state_path(cfg: &Config) -> Option<std::result::Result<PathBuf, String>> {
    if cfg!(test) && cfg.get("ARENA_STATE_DIR").is_none() {
        return None;
    }
    Some(snapshot::state_file_path_for(cfg, snapshot::BALANCE_FILE))
}

/// The health cache as it stands (an empty one when there is none or it can't be read),
/// plus the warning to show when it was unusable. Never fails: health is a convenience.
fn load_health(cfg: &Config) -> (HealthCache, Option<String>) {
    match health_cache_path(cfg) {
        None => (HealthCache::new(), None),
        Some(Err(e)) => (HealthCache::new(), Some(format!("no health cache: {e}"))),
        Some(Ok(path)) => HealthCache::load(&path),
    }
}

/// Time each endpoint's RTT ([`arena_core::rtt::connect_rtt`]: one TCP connect, within
/// [`RTT_WAIT`]), all at once. `None` for one that didn't answer in time.
async fn time_endpoints(due: Vec<Endpoint>) -> Vec<(Endpoint, Option<Duration>)> {
    let mut set = JoinSet::new();
    for ep in due {
        set.spawn(async move {
            let d = arena_core::rtt::connect_rtt((ep.0.as_str(), ep.1), 1, RTT_WAIT).await;
            (ep, d)
        });
    }
    let mut out = Vec::new();
    while let Some(joined) = set.join_next().await {
        out.extend(joined.ok());
    }
    out
}

/// Fan metric fetches out across the fleet concurrently over `remote` — each probe bounded
/// by core's `PROBE_TIMEOUT` and a short connect timeout, so a single unreachable pod
/// can't hold up the sweep.
async fn fetch_metrics(
    remote: &Arc<dyn Remote>,
    pods: &[Pod],
    cfg: &Config,
    opts: &ProbeOpts,
) -> HashMap<String, PodMetrics> {
    let mut set = JoinSet::new();
    for pod in pods {
        if let Ok(mut target) = SshTarget::from_pod(pod, cfg) {
            target.connect_timeout_secs = METRICS_CONNECT_TIMEOUT;
            let name = pod.name.clone();
            let opts = opts.clone();
            let remote = remote.clone();
            set.spawn(async move { (name, metrics::fetch_with(remote.as_ref(), &target, &opts).await) });
        }
    }
    let mut fresh = HashMap::new();
    while let Some(joined) = set.join_next().await {
        if let Ok((name, m)) = joined {
            fresh.insert(name, m);
        }
    }
    fresh
}

/// The footer's refresh status: liveness (the pod count lives in the summary bar), then
/// any warning from this refresh's reads (details query, proxy file, health cache).
fn status_line(s: &FleetSummary, warnings: &[&str]) -> String {
    let mut out = format!("{} reporting", s.reporting);
    if s.unreachable > 0 {
        out.push_str(&format!(" · {} unreachable", s.unreachable));
    }
    for w in warnings {
        out.push_str(&format!(" · ⚠ {}", w.strip_prefix("warning: ").unwrap_or(w)));
    }
    out
}

/// Whether the terminal is the dashboard's now — raw mode, the alternate screen, the mouse — so
/// an end from outside the loop puts it back ([`restore_best_effort`]); not while `c`'s ssh has
/// it ([`hand_over`]). Held while the terminal changes hands ([`setup_terminal`],
/// [`restore_terminal`]), while a frame is drawn ([`while_ours`]) and while that end puts it back:
/// so none of them interleaves with another, no frame is drawn once it's back (on the main
/// screen), and once the dashboard is ending (`arena_core::remote::ending`) it isn't taken again —
/// `c`'s ssh stopped by the end, the dashboard doesn't set up its screen again under the exit.
static TERMINAL: Mutex<bool> = Mutex::new(false);

fn terminal_ours() -> std::sync::MutexGuard<'static, bool> {
    TERMINAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Take the terminal: raw mode, the alternate screen, the mouse. Refused once the dashboard is
/// ending.
fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    let mut ours = terminal_ours();
    if arena_core::remote::ending() {
        anyhow::bail!("the dashboard is ending");
    }
    *ours = true;
    enable_raw_mode()?;
    let mut out = stdout();
    // EnableMouseCapture: wheel-scroll the list + click to select/multi-select rows.
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

/// Give the terminal back. `false`: it wasn't the dashboard's — an end from outside the loop has
/// put it back already — and nothing is written.
fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<bool> {
    let mut ours = terminal_ours();
    if !std::mem::replace(&mut *ours, false) {
        return Ok(false);
    }
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    Ok(true)
}

/// `f` on the terminal — a frame, a clear — while it is the dashboard's ([`TERMINAL`], held
/// meanwhile). `false`, `f` not called: it isn't — an end from outside the loop has put it back.
fn while_ours(terminal: &mut Terminal<CrosstermBackend<Stdout>>, f: impl FnOnce(&mut Terminal<CrosstermBackend<Stdout>>) -> std::io::Result<()>) -> Result<bool> {
    let ours = terminal_ours();
    if !*ours {
        return Ok(false);
    }
    f(terminal)?;
    Ok(true)
}

/// How long [`restore_best_effort`] waits for the terminal to take its last writes.
const RESTORE_WAIT: Duration = Duration::from_millis(300);

/// The terminal put back as [`restore_terminal`] does — raw mode off, the main screen, no mouse
/// capture, the cursor shown — when the dashboard ends from outside its loop: best effort (on a
/// hung-up terminal every step fails, and that's fine), on a thread of its own waited for no more
/// than [`RESTORE_WAIT`] (a terminal that has stopped reading would hold a write for ever, or a
/// frame's draw the lock). Nothing when the terminal isn't the dashboard's ([`TERMINAL`]): `c`'s
/// ssh has it, and what ssh did to it is ssh's to undo.
fn restore_best_effort() {
    let (done, wait) = std::sync::mpsc::channel();
    let restoring = std::thread::Builder::new().name("restore".into()).spawn(move || {
        let mut ours = terminal_ours();
        if std::mem::replace(&mut *ours, false) {
            let _ = disable_raw_mode();
            let _ = execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture, crossterm::cursor::Show);
        }
        drop(ours);
        let _ = done.send(());
    });
    if restoring.is_ok() {
        let _ = wait.recv_timeout(RESTORE_WAIT);
    }
}

/// The signals that end the dashboard ([`exit_on_signals`]), by number: the exit status is 128 +
/// it, as a shell reports a process the signal killed (the same on Linux and macOS).
const SIGHUP: i32 = 1;
const SIGTERM: i32 = 15;

/// How often [`exit_on_hangup`] looks at the terminal.
const HANGUP_POLL: Duration = Duration::from_millis(200);

/// End the dashboard now, from outside its event loop — which may never come round again: on a
/// hung-up terminal crossterm's read spins for ever (live ASK4 3c) — so nothing here waits on it.
/// In order:
/// 1. the latch (`arena_core::remote::end_children`), before anything else: no child starts after
///    it (review ai-review codex #3), and a later cause — the hangup's SIGHUP and the dead terminal
///    both seen, a TERM after a HUP, the loop failing on the dead terminal — waits for this one's
///    exit;
/// 2. the terminal put back as far as it can be ([`restore_best_effort`]), at once, never after
///    the grace (review ai-review claude #4), and `why` said there;
/// 3. stdin parked (`arena_core::tty::park_stdin`): the loop's next read waits, no longer spinning
///    a core on a dead terminal meanwhile (live ASK5 F1);
/// 4. every child still running stopped — the AI's whole group, and every ssh, scp and rsync of a
///    refresh, an action or a backup, and `c`'s ssh, which an `exit` left running before (review
///    ai-review claude #1) — TERM, then KILL after the grace (`arena_core::remote::stop_all_children`);
/// 5. the asks' scratch directories removed (`exit` drops nothing:
///    `arena_core::ask::ai::remove_scratch`);
/// 6. `exit(code)`.
async fn end_now(code: i32, why: Option<String>) {
    if !arena_core::remote::end_children() {
        return std::future::pending().await;
    }
    restore_best_effort();
    if let Some(why) = why {
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), "arena-tui: {why}");
    }
    arena_core::tty::park_stdin();
    arena_core::remote::stop_all_children().await;
    arena_core::ask::ai::remove_scratch();
    std::process::exit(code)
}

/// Mouse handling: wheel-scroll the pod list (or the result modal), and in the list
/// left-click a row to select it. Clicking the far-left of a row — or shift-clicking
/// anywhere on it — toggles that pod's multi-select mark (same `marked` set as `space`).
fn handle_mouse(m: MouseEvent, ui: &mut Ui, shared: &Arc<Mutex<Shared>>) {
    let in_list = matches!(ui.mode, Mode::List);
    let in_result = matches!(ui.mode, Mode::Result(_));
    match m.kind {
        MouseEventKind::ScrollUp => {
            if in_result {
                ui.result_scroll = scroll_by(ui.result_scroll, ui.result_max.get(), -1);
            } else if in_list {
                ui.selected = ui.selected.saturating_sub(1);
            }
        }
        MouseEventKind::ScrollDown => {
            if in_result {
                ui.result_scroll = scroll_by(ui.result_scroll, ui.result_max.get(), 1);
            } else if in_list {
                let len = shared.lock().unwrap().pods.len();
                if len > 0 {
                    ui.selected = (ui.selected + 1).min(len - 1);
                }
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if !in_list {
                return;
            }
            let (area, offset) = ui.last_table.get();
            // Data rows sit below the top border (1) + header (1), above the bottom border.
            let rows_top = area.y.saturating_add(2);
            let rows_bot = area.y.saturating_add(area.height).saturating_sub(1);
            let (mx, my) = (m.column, m.row);
            if my < rows_top || my >= rows_bot || mx < area.x || mx >= area.x.saturating_add(area.width)
            {
                return; // click outside the table rows
            }
            let idx = offset + (my - rows_top) as usize;
            let key = {
                let s = shared.lock().unwrap();
                if idx >= s.pods.len() {
                    return;
                }
                mark_key(&s.pods[idx])
            };
            ui.selected = idx;
            // Far-left of the row, or shift-click, toggles the multi-select mark.
            let left_zone = mx < area.x.saturating_add(5);
            let shift = m.modifiers.contains(KeyModifiers::SHIFT);
            if (left_zone || shift) && !ui.marked.remove(&key) {
                ui.marked.insert(key);
            }
        }
        _ => {}
    }
}

/// Read the currently selected pod (a clone, so we don't hold the lock).
fn selected_pod(shared: &Arc<Mutex<Shared>>, idx: usize) -> Option<Pod> {
    shared.lock().unwrap().pods.get(idx).cloned()
}

/// Open an interactive SSH shell to the selected pod: suspend the dashboard (leave the
/// alternate screen + raw mode), hand the terminal to `ssh`, then restore on exit ([`hand_over`]).
/// The one SSH call that doesn't go through `Remote`, on purpose: it's the operator's own shell —
/// it needs the terminal (a PTY, stdin) and has no time budget.
fn connect_ssh(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    shared: &Arc<Mutex<Shared>>,
    ui: &mut Ui,
) -> Result<()> {
    let Some(pod) = selected_pod(shared, ui.selected) else { return Ok(()) };
    let target = match SshTarget::from_pod(&pod, &ui.cfg) {
        Ok(t) => t,
        Err(e) => {
            ui.mode = Mode::Result(format!("✗ {}: {e}", pod.name));
            return Ok(());
        }
    };
    let banner = format!("Connecting to {} ({}@{}:{}) — exit the shell to return to the dashboard…\n", pod.name, target.user, target.host, target.port);
    // ssh_args carries the port/keys.
    let mut ssh = std::process::Command::new("ssh");
    ssh.args(target.ssh_args());
    if let Err(e) = hand_over(terminal, &banner, &mut ssh)? {
        ui.mode = Mode::Result(format!("✗ ssh {}: {e}", pod.name));
    }
    Ok(())
}

/// Hand the terminal to `cmd` until it exits — `c`'s ssh: blocking, it inherits this terminal
/// (an interactive PTY) — `banner` said first on the main screen, then take the terminal back.
/// While `cmd` has it the terminal isn't the dashboard's ([`TERMINAL`]): a hangup or a TERM then
/// stops `cmd` with every other child (it's tracked: `arena_core::remote::run_foreground`) and puts
/// nothing back — the terminal is `cmd`'s to restore — and, the dashboard ending, the terminal
/// isn't taken back: `Err`, which ends the loop, the exit [`end_now`]'s. Never started once the
/// dashboard is ending. `Ok(Err)`: `cmd` couldn't be run.
fn hand_over(terminal: &mut Terminal<CrosstermBackend<Stdout>>, banner: &str, cmd: &mut std::process::Command) -> Result<arena_core::Result<std::process::ExitStatus>> {
    if !restore_terminal(terminal)? {
        anyhow::bail!("the dashboard is ending");
    }
    println!("{banner}");
    let status = arena_core::remote::run_foreground(cmd, "ssh");
    *terminal = setup_terminal()?;
    while_ours(terminal, |t| t.clear())?;
    Ok(status)
}

/// Advance the cadence to the next step in `REFRESH_STEPS` (wrapping).
fn cycle_interval(interval: &AtomicU64) {
    let cur = interval.load(Ordering::Relaxed);
    let idx = REFRESH_STEPS.iter().position(|&s| s == cur).unwrap_or(usize::MAX);
    let next = REFRESH_STEPS[idx.wrapping_add(1) % REFRESH_STEPS.len()];
    interval.store(next, Ordering::Relaxed);
}

/// Run a pod/fleet action off the UI thread: mark busy, spawn it, and stash the result
/// in `Shared` for the loop to pick up. This keeps the dashboard live during slow SSH
/// work (a `setup`/`backup` across the fleet takes many seconds) instead of freezing.
fn start_action<F>(shared: &Arc<Mutex<Shared>>, task: F)
where
    F: std::future::Future<Output = String> + Send + 'static,
{
    {
        let mut s = shared.lock().unwrap();
        s.action_running = true;
        s.action_result = None;
    }
    let shared = shared.clone();
    tokio::spawn(async move {
        let msg = task.await;
        let mut s = shared.lock().unwrap();
        s.action_result = Some(msg);
        s.action_running = false;
    });
}

/// Resolve a [`Scope`] to the pods it targets, from the live list + the marked set.
fn scope_pods(
    shared: &Arc<Mutex<Shared>>,
    marked: &std::collections::HashSet<String>,
    scope: Scope,
) -> Vec<Pod> {
    let s = shared.lock().unwrap();
    match scope {
        Scope::All => s.pods.clone(),
        Scope::Marked => marked_pods(&s.pods, marked).into_iter().cloned().collect(),
    }
}

/// The token the operator must type to confirm a multi-pod action: `ALL` for the whole
/// fleet (a deliberate high bar), and for terminate/restart on a marked set that is big or
/// is every listed pod (`state::marked_set_token` — `/first..last` marks a cohort in one
/// line); otherwise the pod count of the marked set (which they chose).
fn fleet_confirm_token(
    shared: &Arc<Mutex<Shared>>,
    marked: &std::collections::HashSet<String>,
    scope: Scope,
    action: Action,
) -> String {
    fleet_confirm_token_str(&shared.lock().unwrap(), marked, scope, action)
}

/// Enter in the `/` input: mark what `input` resolves to (`state::select_marks`), replacing
/// the current marks — not adding to them, so the marked set is exactly what was typed —
/// and say so in the footer. A typo marks nothing and leaves the existing marks as they
/// were (the error in red). Either way back to the list.
fn apply_select(ui: &mut Ui, shared: &mut Shared, input: &str) {
    match select_marks(input, &Naming::from_config(&ui.cfg), &shared.pods) {
        Ok((keys, msg)) => {
            ui.marked = keys;
            shared.notify(msg, false);
        }
        Err(e) => shared.notify(format!("✗ {e}"), true),
    }
    ui.mode = Mode::List;
}

/// What a key on the list or detail view leaves for the loop to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListKey {
    /// `q`: leave the dashboard.
    Quit,
    /// Handled: the cursor, the marks, or which view or popup is open.
    Done,
    /// Not one of [`list_key`]'s: the loop's own (it needs the provider, the terminal or the
    /// shared state).
    Other,
}

/// The list and detail views' keys that need nothing but `ui` and the row count `len`. Only
/// `q` quits (and Ctrl+C, from anywhere); Esc steps back and never quits — from the detail view
/// to the list, and on the list it clears the marks if there are any, else does nothing. Live:
/// Esc on the list quit, so the Esc-Esc that backs out of a popup closed the dashboard.
fn list_key(ui: &mut Ui, code: KeyCode, len: usize) -> ListKey {
    match code {
        KeyCode::Char('q') => return ListKey::Quit,
        KeyCode::Esc if matches!(ui.mode, Mode::Detail) => ui.mode = Mode::List,
        KeyCode::Esc | KeyCode::Char('x') => ui.marked.clear(),
        KeyCode::Up | KeyCode::Char('k') => ui.selected = ui.selected.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => {
            if ui.selected + 1 < len {
                ui.selected += 1;
            }
        }
        KeyCode::Enter => {
            if len > 0 {
                ui.mode = Mode::Detail;
            }
        }
        KeyCode::Char('/') => ui.mode = Mode::Select { value: String::new() },
        // Act on the marked set if any are marked, else the cursor pod.
        KeyCode::Char('a') if !ui.marked.is_empty() => ui.mode = Mode::FleetMenu { scope: Scope::Marked, sel: 0 },
        KeyCode::Char('a') if len > 0 => ui.mode = Mode::Menu { sel: 0 },
        KeyCode::Char('A') if len > 0 => ui.mode = Mode::FleetMenu { scope: Scope::All, sel: 0 },
        KeyCode::Char('a' | 'A') => {}
        _ => return ListKey::Other,
    }
    ListKey::Done
}

/// `n`: list the whole fleet **in the background** — fresh, not the cached snapshot, so the
/// form never offers a name that already exists — and say so meanwhile; the loop opens the
/// form when the listing lands ([`new_pod_mode`]). Awaiting it in the key loop froze the
/// dashboard (no redraw, no keys) for as long as the slowest provider took to answer.
fn start_new_pod_listing(shared: &Arc<Mutex<Shared>>, provider: &Arc<dyn Provider>, ui: &mut Ui) {
    ui.mode = Mode::Working("listing the fleet for free names…".into());
    let (shared, provider) = (shared.clone(), provider.clone());
    tokio::spawn(async move {
        let listed = provider.list_pods().await.map_err(|e| e.to_string());
        shared.lock().unwrap().new_pod_listing = Some(listed);
    });
}

/// The add-pod form for `n`'s listing — or why there is none.
fn new_pod_mode(ui: &Ui, listed: std::result::Result<Vec<Pod>, String>) -> Mode {
    match listed {
        Ok(existing) => Mode::NewPod(build_new_pod_form(&ui.cfg, &ui.provider_name, &existing)),
        Err(e) => Mode::Result(format!("✗ list failed: {e}")),
    }
}

/// A hangup or a TERM ends the dashboard at once ([`end_now`], exit status 129 or 143), from a
/// task of its own: a question still being asked (`?`) stopped with everything it started
/// (review ai-review claude #2: the AI, in a session of its own, ran on when either signal killed
/// the dashboard outright), the terminal put back. Never through the event loop (lane ASK7 H
/// latched a flag there): after a hangup crossterm's read never returns, so the flag was never
/// seen — the dashboard spun a core for ever, SIGTERM no longer stopping it, and its `?` ran on
/// (live ASK4 3c). Listened for from this call on.
fn exit_on_signals() {
    use tokio::signal::unix::{signal, SignalKind};
    for (kind, signo) in [(SignalKind::hangup(), SIGHUP), (SignalKind::terminate(), SIGTERM)] {
        if let Ok(mut listener) = signal(kind) {
            tokio::spawn(async move {
                if listener.recv().await.is_some() {
                    end_now(128 + signo, None).await;
                }
            });
        }
    }
}

/// A terminal that has hung up (`arena_core::tty::hung_up`: stdout's, and stdin's when it is one)
/// ends the dashboard as a hangup does ([`end_now`], 129), looked at every [`HANGUP_POLL`] from a
/// task of its own: without the SIGHUP — ignored, or never sent to this process — the event loop
/// would spin on the dead terminal for ever.
fn exit_on_hangup() {
    use std::io::IsTerminal;
    let fds: Vec<i32> = [(0, std::io::stdin().is_terminal()), (1, std::io::stdout().is_terminal())].into_iter().filter(|(_, tty)| *tty).map(|(fd, _)| fd).collect();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(HANGUP_POLL).await;
            if fds.iter().any(|&fd| arena_core::tty::hung_up(fd)) {
                end_now(128 + SIGHUP, None).await;
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
async fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    shared: &Arc<Mutex<Shared>>,
    provider: &Arc<dyn Provider>,
    interval: &AtomicU64,
    nudge: &Notify,
    details_wanted: &AtomicBool,
    mut ui: Ui,
) -> Result<()> {
    loop {
        // A finished background action surfaces as a Result modal (then nudge a refresh).
        {
            let msg = shared.lock().unwrap().action_result.take();
            if let Some(msg) = msg {
                ui.result_scroll = 0;
                ui.mode = Mode::Result(msg);
                nudge.notify_one();
            }
        }
        // A `?` answer landed: its popup (from the list/detail view; otherwise it waits).
        ask::land(&mut ui, shared);
        // `n`'s listing landed: the add-pod form (or why not).
        {
            let listed = shared.lock().unwrap().new_pod_listing.take();
            if let Some(listed) = listed {
                ui.mode = new_pod_mode(&ui, listed);
            }
        }
        // Clamp the cursor in case the fleet shrank under us.
        let len = shared.lock().unwrap().pods.len();
        ui.selected = if len == 0 { 0 } else { ui.selected.min(len - 1) };

        let secs = interval.load(Ordering::Relaxed);
        {
            let s = shared.lock().unwrap();
            // Put back by an end from outside the loop ([`end_now`]): nothing more to draw — the
            // loop ends, and that end exits.
            if !while_ours(terminal, |t| t.draw(|f| view(f, &s, &ui, secs)).map(drop))? {
                return Ok(());
            }
        }

        // Short poll: the loop keeps redrawing (~7 fps) so live data and the
        // "updated Ns ago" line stay current without any key press.
        if !event::poll(Duration::from_millis(150))? {
            continue;
        }
        let ev = event::read()?;
        if let Event::Mouse(m) = ev {
            handle_mouse(m, &mut ui, shared);
            continue; // redraw at the top of the loop
        }
        let Event::Key(k) = ev else { continue };
        if k.kind != KeyEventKind::Press {
            continue; // ignore key-release/repeat on terminals that emit them
        }
        // Ctrl+C always quits, from any mode. Raw mode swallows the usual SIGINT, so
        // we handle the keystroke ourselves and let `restore_terminal` clean up.
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            break;
        }
        let code = k.code;
        // `?` and its two popups (ask.rs).
        if ask::key(&mut ui, shared, code, k.modifiers) {
            continue;
        }

        match ui.mode.clone() {
            Mode::List | Mode::Detail => match list_key(&mut ui, code, len) {
                ListKey::Quit => break,
                ListKey::Done => {}
                ListKey::Other => match code {
                    KeyCode::Char('r') => {
                        // A manual refresh also re-reads the provider's details (maintenance
                        // windows, $/h) — rate-limited by `state::details_due`.
                        details_wanted.store(true, Ordering::Relaxed);
                        nudge.notify_one();
                    }
                    KeyCode::Char('d') => {
                        // Deep-check the marked set if any are marked, else the cursor pod —
                        // in the background; the rows say `checking` until it lands.
                        let pods = if ui.marked.is_empty() {
                            selected_pod(shared, ui.selected).into_iter().collect()
                        } else {
                            scope_pods(shared, &ui.marked, Scope::Marked)
                        };
                        if !pods.is_empty() {
                            start_deep_check(shared, &ui, pods);
                        }
                    }
                    KeyCode::Char('D') => {
                        // Every running pod no one is using, after a second `D`: the first
                        // says how many, and which it leaves out. The second checks only pods
                        // the first named that still qualify — never one a refresh added since.
                        let (mut pods, skipped) = {
                            let s = shared.lock().unwrap();
                            deep_all_plan(&s.pods, &s.metrics)
                        };
                        let armed = ui.deep_all_armed.take().filter(|(t, _)| t.elapsed() < DEEP_ALL_CONFIRM);
                        let skipped = skipped_words(&skipped);
                        pods = deep_all_confirm(pods, armed.as_ref().map(|(_, named)| named));
                        if pods.is_empty() {
                            shared.lock().unwrap().notify(format!("nothing to deep-check{skipped}"), false);
                        } else if armed.is_some() {
                            start_deep_check(shared, &ui, pods);
                        } else {
                            ui.deep_all_armed = Some((Instant::now(), pods.iter().map(mark_key).collect()));
                            let n = pods.len();
                            shared.lock().unwrap().notify(
                                format!(
                                    "D again to deep-check {n} running pod{} (about a minute of GPU load each){skipped}",
                                    if n == 1 { "" } else { "s" }
                                ),
                                false,
                            );
                        }
                    }
                    KeyCode::Char('s') => {
                        // Toggle short/full names and persist the choice.
                        ui.short_names = !ui.short_names;
                        Prefs { short_names: ui.short_names }.save();
                    }
                    KeyCode::Char('f') => {
                        cycle_interval(interval);
                        nudge.notify_one(); // apply a shorter cadence immediately
                    }
                    KeyCode::Char(' ') => {
                        // Toggle multi-select mark on the cursor pod.
                        if let Some(pod) = selected_pod(shared, ui.selected) {
                            let key = mark_key(&pod);
                            if !ui.marked.remove(&key) {
                                ui.marked.insert(key);
                            }
                        }
                    }
                    KeyCode::Char('c') => {
                        connect_ssh(terminal, shared, &mut ui)?;
                        nudge.notify_one();
                    }
                    KeyCode::Char('n') => start_new_pod_listing(shared, provider, &mut ui),
                    _ => {}
                },
            },
            Mode::Menu { mut sel } => {
                let n = Action::MENU.len();
                match code {
                    KeyCode::Esc => ui.mode = Mode::List,
                    KeyCode::Up | KeyCode::Char('k') => {
                        sel = sel.saturating_sub(1);
                        ui.mode = Mode::Menu { sel };
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        sel = (sel + 1).min(n - 1);
                        ui.mode = Mode::Menu { sel };
                    }
                    KeyCode::Enter => choose_pod_action(shared, &mut ui, provider, Action::MENU[sel.min(n - 1)].1),
                    KeyCode::Char(ch) => match Action::from_key(ch) {
                        Some(action) => choose_pod_action(shared, &mut ui, provider, action),
                        None => ui.mode = Mode::Menu { sel },
                    },
                    _ => ui.mode = Mode::Menu { sel },
                }
            }
            Mode::Confirm(mut c) => {
                let typed = c.action.requires_typed_name();
                match code {
                    KeyCode::Esc => ui.mode = Mode::List,
                    KeyCode::Enter if c.is_satisfied() => apply_action(shared, &mut ui, provider, &c),
                    KeyCode::Char('y') if !typed => apply_action(shared, &mut ui, provider, &c),
                    KeyCode::Char('n') if !typed => ui.mode = Mode::List,
                    KeyCode::Char('v') if !typed && c.preview.is_some() => {
                        c.full = !c.full;
                        ui.mode = Mode::Confirm(c);
                    }
                    KeyCode::Backspace if typed => {
                        c.typed.pop();
                        ui.mode = Mode::Confirm(c);
                    }
                    KeyCode::Char(ch) if typed => {
                        c.typed.push(ch);
                        ui.mode = Mode::Confirm(c);
                    }
                    _ => ui.mode = Mode::Confirm(c),
                }
            }
            Mode::FleetMenu { scope, mut sel } => {
                let actions = Action::fleet_menu(scope == Scope::Marked);
                let n = actions.len();
                match code {
                    KeyCode::Esc => ui.mode = Mode::List,
                    KeyCode::Up | KeyCode::Char('k') => {
                        sel = sel.saturating_sub(1);
                        ui.mode = Mode::FleetMenu { scope, sel };
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        sel = (sel + 1).min(n - 1);
                        ui.mode = Mode::FleetMenu { scope, sel };
                    }
                    KeyCode::Enter => {
                        choose_fleet_action(shared, &mut ui, provider, actions[sel.min(n - 1)].1, scope);
                    }
                    KeyCode::Char(ch) => match actions.iter().find(|(k, _)| *k == ch) {
                        Some((_, action)) => choose_fleet_action(shared, &mut ui, provider, *action, scope),
                        None => ui.mode = Mode::FleetMenu { scope, sel },
                    },
                    _ => ui.mode = Mode::FleetMenu { scope, sel },
                }
            }
            Mode::FleetConfirm { action, mut typed, scope, note } => {
                let token = fleet_confirm_token(shared, &ui.marked, scope, action);
                match code {
                    KeyCode::Esc => ui.mode = Mode::List,
                    KeyCode::Enter if typed == token => {
                        let pods = scope_pods(shared, &ui.marked, scope);
                        let (provider, remote, cfg) = (provider.clone(), ui.remote.clone(), ui.cfg.clone());
                        ui.mode = Mode::Working(format!("running {} on {} pod(s)…", action.label(), pods.len()));
                        start_action(shared, async move {
                            execute_fleet(&provider, &remote, &cfg, action, pods, None).await
                        });
                    }
                    KeyCode::Backspace => {
                        typed.pop();
                        ui.mode = Mode::FleetConfirm { action, typed, scope, note };
                    }
                    KeyCode::Char(c) => {
                        typed.push(c);
                        ui.mode = Mode::FleetConfirm { action, typed, scope, note };
                    }
                    _ => ui.mode = Mode::FleetConfirm { action, typed, scope, note },
                }
            }
            Mode::NewPod(mut form) => match code {
                KeyCode::Esc => ui.mode = Mode::List,
                KeyCode::Up | KeyCode::Char('k') => {
                    form.move_field(-1);
                    ui.mode = Mode::NewPod(form);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    form.move_field(1);
                    ui.mode = Mode::NewPod(form);
                }
                KeyCode::Left | KeyCode::Char('h') | KeyCode::Right | KeyCode::Char('l') => {
                    let delta = if matches!(code, KeyCode::Left | KeyCode::Char('h')) { -1 } else { 1 };
                    if form.change(delta) {
                        // Provider changed — re-list its pods for accurate free names.
                        if let Some(free) = relist_free(&ui.cfg, form.provider_name()).await {
                            form.count = form.count.min(free.len().max(1));
                            form.free = free;
                        }
                    }
                    ui.mode = Mode::NewPod(form);
                }
                KeyCode::Enter => {
                    let pname = form.provider_name().to_string();
                    match build_provider(&ui.cfg, &pname) {
                        Err(e) => ui.mode = Mode::Result(format!("✗ {pname}: {e}")),
                        Ok(prov) => match list_whole_fleet(&ui.cfg, &pname).await {
                            Err(e) => ui.mode = Mode::Result(format!("✗ listing the fleet: {e}")),
                            Ok(existing) => {
                                let prefix = ui.cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
                                let free = naming::next_free_names(
                                    prefix,
                                    &ui.cfg.machine_names,
                                    &existing,
                                    form.count,
                                );
                                if free.is_empty() {
                                    ui.mode = Mode::Result("✗ no free machine names".into());
                                } else {
                                    let cloud = form.cloud_type().map(String::from);
                                    let gpu_type = form.gpu_type().to_string();
                                    let disk = form.disk_gb;
                                    let volume = form.volume_gb;
                                    ui.mode = Mode::Result(format!(
                                        "creating {} pod(s) on {pname}…",
                                        free.len()
                                    ));
                                    {
                                        let s = shared.lock().unwrap();
                                        while_ours(terminal, |t| t.draw(|f| view(f, &s, &ui, secs)).map(drop))?;
                                    }
                                    let (msg, created) = create_pods(
                                        &prov,
                                        &ui.cfg,
                                        &free,
                                        cloud.as_deref(),
                                        &gpu_type,
                                        form.gpu_count,
                                        Some(disk),
                                        Some(volume),
                                    )
                                    .await;
                                    // Show the new pods immediately as "pending" until
                                    // the provider's list API reports them.
                                    {
                                        let mut s = shared.lock().unwrap();
                                        for mut p in created {
                                            p.status = "pending".into();
                                            p.ssh_ip = None;
                                            p.ssh_port = None;
                                            if !s.pending.iter().any(|x| x.name == p.name) {
                                                s.pending.push(p);
                                            }
                                        }
                                    }
                                    ui.mode = Mode::Result(msg);
                                    nudge.notify_one();
                                }
                            }
                        },
                    }
                }
                _ => ui.mode = Mode::NewPod(form),
            },
            Mode::Input { action, scope, mut value } => match code {
                KeyCode::Esc => ui.mode = Mode::List,
                KeyCode::Enter if !value.trim().is_empty() => {
                    let (provider, remote, cfg) = (provider.clone(), ui.remote.clone(), ui.cfg.clone());
                    match scope {
                        InputScope::Pod { name, id } => {
                            let pod = {
                                let s = shared.lock().unwrap();
                                s.pods.iter().find(|p| p.id == id).cloned()
                            };
                            match pod {
                                None => ui.mode = Mode::Result(format!("{name} is gone — refresh")),
                                Some(pod) => {
                                    ui.mode = Mode::Working(format!("{} on {name}…", action.label()));
                                    start_action(shared, async move {
                                        execute(provider.as_ref(), remote.as_ref(), &cfg, action, &pod, Some(&value)).await
                                    });
                                }
                            }
                        }
                        InputScope::Set(sc) => {
                            let pods = scope_pods(shared, &ui.marked, sc);
                            ui.mode = Mode::Working(format!("{} on {} pod(s)…", action.label(), pods.len()));
                            start_action(shared, async move {
                                execute_fleet(&provider, &remote, &cfg, action, pods, Some(value)).await
                            });
                        }
                    }
                }
                KeyCode::Backspace => {
                    value.pop();
                    ui.mode = Mode::Input { action, scope, value };
                }
                KeyCode::Char(ch) => {
                    value.push(ch);
                    ui.mode = Mode::Input { action, scope, value };
                }
                _ => ui.mode = Mode::Input { action, scope, value },
            },
            Mode::Select { mut value } => match code {
                KeyCode::Esc => ui.mode = Mode::List,
                KeyCode::Enter => apply_select(&mut ui, &mut shared.lock().unwrap(), &value),
                KeyCode::Backspace => {
                    value.pop();
                    ui.mode = Mode::Select { value };
                }
                KeyCode::Char(ch) => {
                    value.push(ch);
                    ui.mode = Mode::Select { value };
                }
                _ => ui.mode = Mode::Select { value },
            },
            // A background action is running — ignore keys (Ctrl+C still quits, above).
            Mode::Working(_) => {}
            // Handled by `ask::key` above.
            Mode::Ask { .. } | Mode::Answer(_) => {}
            Mode::Result(_) => {
                if !result_key(&mut ui, code) {
                    ui.result_scroll = 0;
                    ui.mode = Mode::List;
                    nudge.notify_one();
                }
            }
        }
    }
    Ok(())
}

/// A popup's scroll offset moved by `delta` lines, kept within `0..=max` (`max`: how far the
/// popup last drawn can scroll, [`Ui::result_max`]). An offset already past `max` — the window
/// grew since — counts as `max`, so the first key back moves the text at once.
pub(crate) fn scroll_by(scroll: u16, max: u16, delta: i32) -> u16 {
    let s = (i32::from(scroll.min(max)) + delta).clamp(0, i32::from(max));
    u16::try_from(s).unwrap_or(max)
}

/// The result modal's keys: ↑↓/jk a line, PgUp/PgDn ten, never past the end of the text.
/// `false`: any other key, which dismisses it.
fn result_key(ui: &mut Ui, code: KeyCode) -> bool {
    let delta = match code {
        KeyCode::Up | KeyCode::Char('k') => -1,
        KeyCode::Down | KeyCode::Char('j') => 1,
        KeyCode::PageUp => -10,
        KeyCode::PageDown => 10,
        _ => return false,
    };
    ui.result_scroll = scroll_by(ui.result_scroll, ui.result_max.get(), delta);
    true
}

/// Run an action against every pod concurrently, returning a summary line, then each pod's
/// outcome. Used by the fleet menu (backup / setup / test / run / set-branch, and restart /
/// stop / terminate on a marked set). A stop acts only on the pods [`stop_plan`] stops, then
/// names the ones it skipped — and ends in a ✗ while one of them still bills, as `pods stop`
/// exits non-zero then.
async fn execute_fleet(
    provider: &Arc<dyn Provider>,
    remote: &Arc<dyn Remote>,
    cfg: &Config,
    action: Action,
    pods: Vec<Pod>,
    arg: Option<String>,
) -> String {
    let (pods, skipped) = match action {
        Action::Stop => {
            let plan = stop_plan(provider.as_ref(), pods);
            let mut skipped = plan.skip_lines();
            skipped.extend(plan.still_billing());
            (plan.stop, skipped)
        }
        _ => (pods, Vec::new()),
    };
    let total = pods.len();
    let mut set = JoinSet::new();
    for pod in pods {
        let provider = provider.clone();
        let remote = remote.clone();
        let cfg = cfg.clone();
        let arg = arg.clone();
        set.spawn(async move { execute(provider.as_ref(), remote.as_ref(), &cfg, action, &pod, arg.as_deref()).await });
    }
    let mut ok = 0usize;
    let mut lines: Vec<String> = Vec::new();
    while let Some(joined) = set.join_next().await {
        if let Ok(msg) = joined {
            if msg.starts_with('✓') {
                ok += 1;
            }
            lines.push(msg);
        }
    }
    // Show EVERY pod's line (sorted), not just failures — so e.g. `test` shows each
    // pod's torch version. The result modal scrolls if it's taller than the screen.
    lines.sort();
    let header = format!("{} — {ok}/{total} ok", action.label());
    std::iter::once(header).chain(lines).chain(skipped).collect::<Vec<_>>().join("\n")
}

/// What a multi-pod `stop` does with each pod — `pods stop`'s rules, through the same core
/// helpers: only a pod that bills ([`arena_core::status::is_billing`]: running or starting —
/// an allow-list, so a state nobody knows never gets a pod stopped) is stopped; the rest are
/// said and left alone. A billing pod whose backend has no stop at all
/// ([`Provider::stop_support`] — Shadeform: it bills until it's deleted) is skipped and named,
/// and the others still stop (review SF3: one overflow VM must not veto the rest). A locked
/// pod is refused by [`execute`], as for one pod.
struct StopPlan {
    stop: Vec<Pod>,
    /// Billing pods whose backend can't stop them, each with why: they still bill.
    no_stop: Vec<(Pod, String)>,
    /// Pods that aren't billing (already stopped, a state nobody knows).
    idle: Vec<Pod>,
}

/// Sort `pods` for a stop ([`StopPlan`]). Pure: no request.
fn stop_plan(provider: &dyn Provider, pods: Vec<Pod>) -> StopPlan {
    let mut plan = StopPlan { stop: Vec::new(), no_stop: Vec::new(), idle: Vec::new() };
    for p in pods {
        if !arena_core::status::is_billing(&p.status) {
            plan.idle.push(p);
        } else if let Err(e) = provider.stop_support(&p) {
            let why = no_stop_why(e);
            plan.no_stop.push((p, why));
        } else {
            plan.stop.push(p);
        }
    }
    plan
}

/// A [`Provider::stop_support`] refusal in the operator's words (the backend's own sentence).
fn no_stop_why(e: arena_core::Error) -> String {
    match e {
        arena_core::Error::NotImplemented(m) => m,
        other => other.to_string(),
    }
}

impl StopPlan {
    /// Why there is nothing to confirm: every billing pod has no stop — refused, as `pods stop`
    /// refuses such a selection — or nothing bills at all.
    fn refusal(&self) -> Option<String> {
        if !self.stop.is_empty() {
            return None;
        }
        Some(match self.no_stop.first() {
            Some((_, why)) => {
                let names: Vec<&str> = self.no_stop.iter().map(|(p, _)| p.name.as_str()).collect();
                format!("✗ can't stop {} — {why}", names.join(", "))
            }
            None => "⊘ nothing to stop — no running or starting pod among them".into(),
        })
    }

    /// One line per pod left alone, and why — for the confirm and the result.
    fn skip_lines(&self) -> Vec<String> {
        let idle = self.idle.iter().map(|p| format!("⊘ {} — skipped: not running or starting ({})", p.name, p.status));
        let no_stop = self.no_stop.iter().map(|(p, why)| format!("⊘ {} — skipped, still billing: {why}", p.name));
        idle.chain(no_stop).collect()
    }

    /// The closing ✗ while a skipped pod still bills (`pods stop` exits non-zero then): what
    /// was asked isn't done while one of them bills.
    fn still_billing(&self) -> Option<String> {
        (!self.no_stop.is_empty()).then(|| {
            let names: Vec<&str> = self.no_stop.iter().map(|(p, _)| p.name.as_str()).collect();
            format!(
                "✗ {} pod(s) still billing — their provider has no stop: {} (back up, then `arena pods terminate`)",
                names.len(),
                names.join(", ")
            )
        })
    }
}

/// Build the GPU-type choices for the add-pod form: the config default first (so the
/// default create matches the CLI), then the shared core presets, de-duplicated.
fn gpu_type_choices(cfg: &Config) -> Vec<String> {
    let mut out = Vec::new();
    let default = PodSpec::from_config(cfg).gpu_type;
    if !default.is_empty() {
        out.push(default);
    }
    for api in arena_core::gpu::preset_apis() {
        if !out.contains(&api) {
            out.push(api);
        }
    }
    if out.is_empty() {
        out.push("NVIDIA RTX A4000".to_string());
    }
    out
}

/// Build a provider by name into an `Arc` (the TUI shares providers across tasks).
fn build_provider(cfg: &Config, name: &str) -> Result<Arc<dyn Provider>> {
    Ok(Arc::from(arena_core::provider::build(name, cfg)?))
}

/// Every configured provider's pods — the whole fleet, as the CLI's create lists it — or why
/// not: a name is free only if no provider holds it, and a provider that didn't answer might
/// ([`arena_core::provider::list_complete`]). Listing just the chosen provider reused a name
/// a pod on another provider held (finding #9's class of duplicate). `provider_name` is the
/// fleet's primary, as on the CLI.
async fn list_whole_fleet(cfg: &Config, provider_name: &str) -> arena_core::Result<Vec<Pod>> {
    let fleet = arena_core::provider::build_fleet(provider_name, cfg, false)?;
    arena_core::provider::list_complete(fleet.as_ref()).await
}

/// The free machine names across the whole fleet ([`list_whole_fleet`]). None on any failure.
async fn relist_free(cfg: &Config, provider_name: &str) -> Option<Vec<String>> {
    let existing = list_whole_fleet(cfg, provider_name).await.ok()?;
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    Some(naming::next_free_names(prefix, &cfg.machine_names, &existing, usize::MAX))
}

/// Seed the add-pod form: provider availability from configured API keys (default to
/// the launch provider if it has a key), GPU choices, and the free names for `existing`.
fn build_new_pod_form(cfg: &Config, launch_provider: &str, existing: &[Pod]) -> NewPodForm {
    let key_for = |p: &str| match p {
        "runpod" => "RUNPOD_API_KEY",
        "vast" => "VAST_API_KEY",
        "hetzner" => "HETZNER_API_KEY",
        "shadeform" => "SHADEFORM_API_KEY",
        _ => "",
    };
    let providers: Vec<ProviderOpt> = ["runpod", "vast", "hetzner", "shadeform"]
        .iter()
        .map(|p| ProviderOpt {
            name: p.to_string(),
            available: cfg.get(key_for(p)).map(|v| !v.is_empty()).unwrap_or(false),
        })
        .collect();
    let provider_idx = providers
        .iter()
        .position(|o| o.name == launch_provider && o.available)
        .or_else(|| providers.iter().position(|o| o.available))
        .unwrap_or(0);
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    let free = naming::next_free_names(prefix, &cfg.machine_names, existing, usize::MAX);
    let spec = PodSpec::from_config(cfg);
    NewPodForm {
        providers,
        provider_idx,
        cloud_types: vec!["COMMUNITY".into(), "SECURE".into()],
        cloud_idx: usize::from(spec.cloud_type.eq_ignore_ascii_case("SECURE")),
        gpu_types: gpu_type_choices(cfg),
        gpu_idx: 0,
        gpu_count: spec.gpu_count.max(1),
        disk_gb: spec.disk_gb.max(20),
        volume_gb: spec.volume_gb,
        count: 1,
        free,
        field: 0,
    }
}

/// Config `CREATE_EXTRA_JSON` for a create on `provider`, as `arena pods create` takes it:
/// parsed and checked against that backend's own fields before anything is created.
fn create_extra(provider: &dyn Provider, cfg: &Config) -> arena_core::Result<Option<arena_core::apiextra::Extra>> {
    let extra = arena_core::apiextra::combine(cfg.get("CREATE_EXTRA_JSON"), None)?;
    if let Some(x) = &extra {
        provider.check_create_extra(x)?;
    }
    Ok(extra)
}

/// Create the named pods (sequentially, no capacity-wait), returning a summary line.
/// Each create is gated by the add-pod form's Enter, mirroring `arena pods create` —
/// config `CREATE_EXTRA_JSON` included (a bad one creates nothing and says why), and the
/// prices the creates answered with remembered for the balance.
#[allow(clippy::too_many_arguments)] // the add-pod form's fields, one argument each (as `arena pods create`'s flags)
async fn create_pods(
    provider: &Arc<dyn Provider>,
    cfg: &Config,
    names: &[String],
    cloud_type: Option<&str>,
    gpu_type: &str,
    gpu_count: u32,
    disk_gb: Option<u32>,
    volume_gb: Option<u32>,
) -> (String, Vec<Pod>) {
    let mut base = PodSpec::from_config(cfg);
    base.api_extra = match create_extra(provider.as_ref(), cfg) {
        Ok(extra) => extra,
        Err(e) => return (format!("created 0/{}\n✗ {e}", names.len()), Vec::new()),
    };
    base.gpu_type = gpu_type.to_string();
    base.gpu_count = gpu_count;
    if let Some(c) = cloud_type {
        base.cloud_type = c.to_string();
    }
    if let Some(d) = disk_gb {
        base.disk_gb = d;
    }
    if let Some(v) = volume_gb {
        base.volume_gb = v;
    }
    let mut created: Vec<Pod> = Vec::new();
    let mut fails: Vec<String> = Vec::new();
    for name in names {
        let mut spec = base.clone();
        spec.name = name.clone();
        spec.env.push(("MACHINE_NAME".into(), name.clone()));
        match provider.create_pod(&spec).await {
            Ok(pod) => created.push(pod),
            Err(e) => fails.push(format!("✗ {name}: {e}")),
        }
    }
    remember_prices(cfg, &created);
    let mut s = format!("created {}/{}", created.len(), names.len());
    for f in fails.iter().take(8) {
        s.push('\n');
        s.push_str(f);
    }
    (s, created)
}

/// Remember the prices `created` answered with in the balance state the CLI keeps
/// ([`balance_state_path`]), for the configured prepaid accounts — as every `arena` create
/// path does — so a later listing with no price (RunPod v1's) still prices these pods. Best
/// effort and quiet; nothing without a state directory.
fn remember_prices(cfg: &Config, created: &[Pod]) {
    if let Some(Ok(path)) = balance_state_path(cfg) {
        let _ = balance::remember_prices(&path, &balance::configured(cfg), created, snapshot::unix_now());
    }
}

/// A menu pick for the cursor pod: run/set-branch collect an argument first; everything
/// else goes through a confirm modal. Shared by the arrow-Enter and letter-key paths. A stop
/// of a pod whose backend has no stop (Shadeform) is refused here, before the confirm — as
/// `pods stop` refuses it alone: the confirm's "keeps no data" warning would only mislead.
fn choose_pod_action(shared: &Arc<Mutex<Shared>>, ui: &mut Ui, provider: &Arc<dyn Provider>, action: Action) {
    let Some(pod) = selected_pod(shared, ui.selected) else {
        ui.mode = Mode::List;
        return;
    };
    let shown = ui.shown_name(&pod.name);
    if action == Action::Stop {
        if let Err(e) = provider.stop_support(&pod) {
            ui.mode = Mode::Result(format!("✗ can't stop {} — {}", pod.name, no_stop_why(e)));
            return;
        }
    }
    ui.mode = if action.needs_input() {
        Mode::Input { action, scope: InputScope::Pod { name: shown, id: pod.id }, value: String::new() }
    } else {
        let preview = build_preview(&ui.cfg, action, &pod);
        let wipes_disk = provider.restart_wipes_container_disk(&pod);
        let reset = provider.restart_resets_container(&pod);
        Mode::Confirm(Confirm { wipes_disk, reset, ..Confirm::new(action, shown, pod.id, preview) })
    };
}

/// A menu pick for a multi-pod scope: `test` runs right away (read-only); run/set-branch
/// collect an argument; the rest go through the typed-token confirm. A stop first sorts its
/// pods as `pods stop` does ([`stop_plan`]): one with nothing to stop is refused here, and the
/// confirm names what it will skip.
fn choose_fleet_action(
    shared: &Arc<Mutex<Shared>>,
    ui: &mut Ui,
    provider: &Arc<dyn Provider>,
    action: Action,
    scope: Scope,
) {
    match action {
        Action::Stop => {
            let plan = stop_plan(provider.as_ref(), scope_pods(shared, &ui.marked, scope));
            ui.mode = match plan.refusal() {
                Some(r) => Mode::Result(r),
                None => Mode::FleetConfirm { action, typed: String::new(), scope, note: plan.skip_lines().join("\n") },
            };
        }
        Action::Test => {
            let pods = scope_pods(shared, &ui.marked, scope);
            let (provider, remote, cfg) = (provider.clone(), ui.remote.clone(), ui.cfg.clone());
            ui.mode = Mode::Working(format!("testing torch on {} pod(s)…", pods.len()));
            start_action(shared, async move {
                execute_fleet(&provider, &remote, &cfg, Action::Test, pods, None).await
            });
        }
        Action::Run | Action::SetBranch => {
            ui.mode = Mode::Input { action, scope: InputScope::Set(scope), value: String::new() };
        }
        _ => ui.mode = Mode::FleetConfirm { action, typed: String::new(), scope, note: String::new() },
    }
}

/// Start a confirmed single-pod action **in the background** (so a slow SSH op doesn't
/// freeze the dashboard): show a busy modal and spawn the work, whose outcome the run
/// loop later swaps into a result modal. The pod is looked up fresh by id.
fn apply_action(shared: &Arc<Mutex<Shared>>, ui: &mut Ui, provider: &Arc<dyn Provider>, c: &Confirm) {
    let Some(pod) = shared.lock().unwrap().pods.iter().find(|p| p.id == c.pod_id).cloned() else {
        ui.mode = Mode::Result(format!("{} is gone — refresh", c.pod_name));
        return;
    };
    ui.mode = Mode::Working(format!("{}ing {}…", c.action.label(), pod.name));
    let (provider, remote, cfg, action) = (provider.clone(), ui.remote.clone(), ui.cfg.clone(), c.action);
    start_action(shared, async move {
        execute(provider.as_ref(), remote.as_ref(), &cfg, action, &pod, None).await
    });
}

/// How long `D`'s first press waits for the second (within [`NOTICE_FOR`], so the prompt is
/// still on screen).
const DEEP_ALL_CONFIRM: Duration = Duration::from_secs(5);

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
fn deep_all_plan(pods: &[Pod], metrics: &HashMap<String, PodMetrics>) -> (Vec<Pod>, Skipped) {
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
fn deep_all_confirm(plan: Vec<Pod>, armed: Option<&std::collections::HashSet<String>>) -> Vec<Pod> {
    match armed {
        Some(named) => plan.into_iter().filter(|p| named.contains(&mark_key(p))).collect(),
        None => plan,
    }
}

/// The pods `D` leaves out: the busy ones by name, and how many weren't read yet, couldn't be
/// read or have no GPU.
#[derive(Debug, Default, PartialEq)]
struct Skipped {
    busy: Vec<String>,
    unread: usize,
    unreadable: usize,
    no_gpu: usize,
}

/// ` — skipping 2 busy: apple, bloom; 3 not read yet; 1 unreadable; 1 without a GPU` (at most
/// five named), or nothing.
fn skipped_words(s: &Skipped) -> String {
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
fn start_deep_check(shared: &Arc<Mutex<Shared>>, ui: &Ui, pods: Vec<Pod>) {
    // Config first: a malformed MIN_DRIVER_VERSION is said before any pod is touched.
    let policy = match HealthPolicy::from_config(&ui.cfg) {
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
/// --deep` runs, one exec within [`DEEP_CHECK_TIMEOUT`], judged by the same core function
/// (`health::judge_deep_call`). A pod with no SSH endpoint is a FAIL: it was asked for. A
/// pod whose check task died is a FAIL too, never dropped. Results in `pods` order.
async fn deep_check_pods(remote: &Arc<dyn Remote>, cfg: &Config, policy: &HealthPolicy, pods: &[Pod]) -> Vec<PodHealth> {
    // `CONDA_ENV=""` disables activation, as for the CLI.
    let cmd = deep_check_command(Some(cfg.get("CONDA_ENV").unwrap_or("arena-env")));
    let jobs: Vec<_> = pods
        .iter()
        .map(|pod| {
            SshTarget::from_pod(pod, cfg).ok().map(|target| {
                let (remote, cmd) = (remote.clone(), cmd.clone());
                tokio::spawn(async move {
                    remote.exec(&target, &cmd, Some(DEEP_CHECK_TIMEOUT)).await.map_err(|e| describe_error(&e))
                })
            })
        })
        .collect();
    let mut results = Vec::with_capacity(pods.len());
    for (pod, job) in pods.iter().zip(jobs) {
        results.push(match job {
            None => PodHealth::unreachable(pod, format!("no SSH endpoint yet (status {})", pod.status)),
            Some(handle) => match handle.await {
                Ok(call) => judge_deep_call(pod, call, policy),
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

/// Perform one action against a pod, returning a one-line human-readable outcome.
/// This is the *only* place the dashboard mutates anything. `arg` carries the typed
/// command (`Run`) or branch (`SetBranch`); it's `None` for the other actions.
///
/// A pod the listing reports locked ([`arena_core::lock`]) is refused restart, stop and
/// terminate here, before any call — as RunPod itself would refuse them, and as the CLI does;
/// a refusal the listing didn't predict reads the same way ([`arena_core::lock::explain`]).
/// The dashboard never unlocks: that's `arena pods unlock`, on purpose.
async fn execute(
    provider: &dyn Provider,
    remote: &dyn Remote,
    cfg: &Config,
    action: Action,
    pod: &Pod,
    arg: Option<&str>,
) -> String {
    use arena_core::lock;
    let name = &pod.name;
    if matches!(action, Action::Restart | Action::Stop | Action::Terminate) && lock::is_locked(pod) {
        return format!("✗ {} {name} refused: {}", action.label(), lock::unlock_hint(&[name]));
    }
    match action {
        // Only a backend known to reset on restart (RunPod v2) is said to have; one that may
        // not have (RunPod v1, live 2026-10-09) gets the same advice, without the claim.
        Action::Restart => match provider.restart_pod(&pod.id).await {
            Ok(()) => match provider.restart_resets_container(pod) {
                ContainerReset::Yes => format!("✓ restarted {name} — reset to the image: run setup (p) once it's up"),
                ContainerReset::Unknown => {
                    format!("✓ restarted {name} — it may have been reset to the image: run setup (p) once it's up")
                }
                ContainerReset::No => format!("✓ restarted {name}"),
            },
            Err(e) => format!("✗ restart {name} failed: {}", lock::explain(name, &e)),
        },
        Action::Stop => match provider.stop_pod_outcome(&pod.id).await {
            Ok(outcome) => match outcome.note() {
                Some(note) => format!("✓ stopped {name} — ⚠ {note}"),
                None => format!("✓ stopped {name}"),
            },
            Err(e) => format!("✗ stop {name} failed: {}", lock::explain(name, &e)),
        },
        Action::Terminate => match provider.terminate_pod(&pod.id).await {
            Ok(()) => format!("✓ terminated {name}"),
            Err(e) => format!("✗ terminate {name} failed: {}", lock::explain(name, &e)),
        },
        Action::Backup => run_backup(remote, cfg, pod).await,
        Action::Setup => run_setup(remote, cfg, pod).await,
        Action::Test => run_ssh_oneline(remote, cfg, pod, TORCH_TEST_CMD, "torch", TEST_TIMEOUT).await,
        Action::Run => match arg {
            Some(cmd) if !cmd.trim().is_empty() => run_ssh_output(remote, cfg, pod, cmd).await,
            _ => format!("✗ {name}: no command given"),
        },
        Action::SetBranch => match arg {
            Some(branch) if !branch.trim().is_empty() => run_set_branch(remote, cfg, pod, branch.trim()).await,
            _ => format!("✗ {name}: no branch given"),
        },
    }
}

/// The torch health check (mirrors `arena pods test`).
const TORCH_TEST_CMD: &str =
    "python -c 'import torch; print(torch.__version__)' 2>&1 || python3 -c 'import torch; print(torch.__version__)'";

/// Run a command over SSH (within `timeout`) and report its last output line — `test`'s
/// torch version (`run` shows the whole output: [`run_ssh_output`]).
async fn run_ssh_oneline(
    remote: &dyn Remote,
    cfg: &Config,
    pod: &Pod,
    cmd: &str,
    what: &str,
    timeout: Duration,
) -> String {
    let target = match SshTarget::from_pod(pod, cfg) {
        Ok(t) => t,
        Err(e) => return format!("✗ {}: {e}", pod.name),
    };
    match remote.exec(&target, cmd, Some(timeout)).await {
        Ok(out) if out.success => {
            let line = out.stdout.lines().last().unwrap_or("").trim();
            format!("✓ {} {what}: {line}", pod.name)
        }
        Ok(out) => format!("✗ {} {what} ({}): {}", pod.name, out.exit_label(), out.stderr.trim()),
        Err(e) => format!("✗ {} {what} failed: {}", pod.name, describe_error(&e)),
    }
}

/// `run`: the operator's command over SSH, within [`RUN_TIMEOUT`], reported with its output
/// ([`run_report`]).
async fn run_ssh_output(remote: &dyn Remote, cfg: &Config, pod: &Pod, cmd: &str) -> String {
    let target = match SshTarget::from_pod(pod, cfg) {
        Ok(t) => t,
        Err(e) => return format!("✗ {}: {e}", pod.name),
    };
    match remote.exec(&target, cmd, Some(RUN_TIMEOUT)).await {
        Ok(out) => run_report(&pod.name, &out),
        Err(e) => format!("✗ {} run failed: {}", pod.name, describe_error(&e)),
    }
}

/// The most output lines `run` shows per pod: the last ones — where a command's result and its
/// errors end up — with the rest counted. The result popup scrolls; a fleet's worth of
/// unbounded output would not.
const RUN_OUTPUT_LINES: usize = 40;

/// The most characters of one output line `run` shows (a minified file, a progress log).
const RUN_LINE_CHARS: usize = 300;

/// `run`'s outcome for one pod: the command's output — stdout, plus stderr when it failed —
/// each line as a terminal would show it (`jobs::display_line`: a progress bar's last state, no
/// escape sequences), on the heading's line when it's one line, else indented under it, the
/// last [`RUN_OUTPUT_LINES`] of it with how many came before. Live: it showed only the last
/// line — `nvidia-smi -L; hostname` read as just the hostname. Pure.
fn run_report(name: &str, out: &arena_core::ssh::SshOutput) -> String {
    let head = if out.success { format!("✓ {name} run") } else { format!("✗ {name} run ({})", out.exit_label()) };
    let mut text: Vec<String> = out.stdout.lines().map(|l| arena_core::jobs::display_line(l.as_bytes())).collect();
    if !out.success {
        text.extend(out.stderr.lines().map(|l| arena_core::jobs::display_line(l.as_bytes())));
    }
    while text.last().is_some_and(|l| l.trim().is_empty()) {
        text.pop();
    }
    let shown: Vec<String> = text.iter().map(|l| truncate(&l.replace('\t', "    "), RUN_LINE_CHARS)).collect();
    match shown.as_slice() {
        [] => format!("{head}: (no output)"),
        [one] => format!("{head}: {one}"),
        _ => {
            let skip = shown.len().saturating_sub(RUN_OUTPUT_LINES);
            let mut lines = vec![format!("{head}:")];
            if skip > 0 {
                lines.push(format!("  … {skip} earlier line(s) not shown"));
            }
            lines.extend(shown[skip..].iter().map(|l| format!("  {l}")));
            lines.join("\n")
        }
    }
}

/// Gently switch a pod's ARENA checkout to `branch` (mirrors `arena pods set-branch`),
/// within [`BRANCH_TIMEOUT`] — after the branch it's on is backed up ([`run_backup`]), as the
/// CLI does: after a switch, commits that couldn't be pushed live only in a local branch on the
/// pod. A branch that can't be pushed isn't switched away from here, since the TUI can't copy
/// the pod's files first; the line names the CLI command that does. A pod with a merge in
/// progress is never switched by a gentle switch, the CLI's included: its line says what is in
/// progress and how to end it ([`busy_not_switched`]).
async fn run_set_branch(remote: &dyn Remote, cfg: &Config, pod: &Pod, branch: &str) -> String {
    let target = match SshTarget::from_pod(pod, cfg) {
        Ok(t) => t,
        Err(e) => return format!("✗ {}: {e}", pod.name),
    };
    let (backed, kept) = backup_and_report(remote, cfg, pod).await;
    match kept {
        // A pod whose `origin` isn't the configured repo: the CLI doesn't switch it either.
        Some(Kept::WrongOrigin(why)) => return format!("✗ {} not switched to {branch}: {why}", pod.name),
        Some(Kept::Busy { what, on, unmerged }) => return busy_not_switched(&pod.name, branch, &what, &on, unmerged),
        None => {}
    }
    if backed.starts_with('✗') || backed.contains(" — busy:") {
        return format!(
            "✗ {} not switched to {branch}: the branch it's on wasn't pushed first ({backed}) — `arena pods set-branch {branch} {}` \
             copies its files, then switches",
            pod.name, pod.name
        );
    }
    let repo_path = cfg.get("BACKUP_REPO_PATH").map(String::from).unwrap_or_else(|| {
        format!("/root/{}", cfg.get("ARENA_REPO_NAME").unwrap_or("ARENA_materials"))
    });
    // The TUI set-branch is gentle (ff-only); the destructive --hard reset is CLI-only.
    let cmd = arena_core::backup::checkout_command(&repo_path, branch, cfg.get("GIT_SSH_KEY_REMOTE"), false);
    match remote.exec(&target, &cmd, Some(BRANCH_TIMEOUT)).await {
        Ok(out) if out.success => format!("✓ {} → {branch}", pod.name),
        Ok(out) => format!("✗ {} set-branch ({}): {}", pod.name, out.exit_label(), out.stderr.trim()),
        Err(e) => format!("✗ {} set-branch failed: {}", pod.name, describe_error(&e)),
    }
}

/// Commit + push the pod's ARENA tree over SSH **on its current branch** (mirrors
/// `arena backup` for a single pod): never switches/creates a branch, skips main/master, and
/// refuses a pod whose `origin` isn't the configured repo. Within [`BACKUP_TIMEOUT`].
async fn run_backup(remote: &dyn Remote, cfg: &Config, pod: &Pod) -> String {
    backup_and_report(remote, cfg, pod).await.0
}

/// Why a pod's backup leaves it where it is for the TUI's set-branch — said in words of its own,
/// not as a push that failed ([`backup_and_report`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Kept {
    /// Its `origin` isn't the configured repo: the refusal in its own words (`origin is …, not the
    /// configured … — not backed up to git; run …`).
    WrongOrigin(String),
    /// An operation in progress (`BUSY`): what it is (`merge`, …: `arena_core::backup::BUSY_OP_SH`),
    /// on which branch, and whether paths are still unmerged.
    Busy { what: String, on: String, unmerged: bool },
}

/// The TUI set-branch's line for a pod with an operation in progress (review F5): what it is and
/// how to end it, as the CLI's gentle switch would refuse it (copying the pod first only to
/// refuse, the CLI's advice before, would gain nothing) — and `--hard` named as the destructive
/// way, which copies the files first and then discards the uncommitted changes to tracked files,
/// the operation with them. Pure.
fn busy_not_switched(pod: &str, branch: &str, what: &str, on: &str, unmerged: bool) -> String {
    use arena_core::backup::{busy_advice, busy_noun, busy_situation, busy_switch_why};
    let noun = busy_noun(what);
    let why = busy_switch_why(what).map_or_else(String::new, |w| format!(" ({w})"));
    format!(
        "✗ {pod} not switched to {branch}: {} — not committed or switched{why}; {} on the pod, then switch again. Or, \
         destructively: `arena pods set-branch {branch} {pod} --hard` copies its files first, then discards its uncommitted \
         changes to tracked files ({noun} with them)",
        busy_situation(what, on),
        busy_advice(what, unmerged)
    )
}

/// [`run_backup`]'s line, and why the pod must stay where it is beyond a failed push, when it
/// must ([`Kept`]).
async fn backup_and_report(remote: &dyn Remote, cfg: &Config, pod: &Pod) -> (String, Option<Kept>) {
    use arena_core::backup::{self, parse_backup_report};
    let target = match SshTarget::from_pod(pod, cfg) {
        Ok(t) => t,
        Err(e) => return (format!("✗ {}: {e}", pod.name), None),
    };
    // The pod's backup lock, like every other writer of its backup (RR10): never alongside a
    // pull, the cron's push or the reaper's own backup of it (whose git step would then fail
    // on `.git/index.lock`). Busy = say so and do nothing.
    let dir = cfg.get("LOCAL_BACKUP_DIR").map(str::trim).filter(|d| !d.is_empty()).unwrap_or("./backup");
    let _lock = match arena_core::pull::try_pod_lock_now(dir, &pod.name) {
        Ok(lock) => lock,
        Err(()) => return (format!("⊘ {} — busy: another backup of it (a pull, the cron or the reaper) is running; try again shortly", pod.name), None),
    };
    let repo = backup_repo_path(cfg);
    let origin = backup::configured_repo(cfg);
    let cmd = backup::backup_command(&repo, cfg.get("GIT_SSH_KEY_REMOTE"), &format!("arena-tui backup {}", pod.name), origin.as_deref());
    match remote.exec(&target, &cmd, Some(BACKUP_TIMEOUT)).await {
        Ok(out) if out.success => {
            let report = parse_backup_report(&out.stdout);
            let notes = report.notes();
            let also = if notes.is_empty() { String::new() } else { format!(" — {}", notes.join("; ")) };
            let line = match &report.outcome {
                Some((backup::BACKUP_PUSHED, branch)) => format!("✓ backed up {} → {branch}{also}", pod.name),
                Some((backup::BACKUP_NO_CHANGES, branch)) => format!("✓ {} — no changes (on {branch}){also}", pod.name),
                Some((backup::BACKUP_SKIPPED, branch)) => format!("⊘ {} — skipped (on protected branch {branch})", pod.name),
                _ => format!("✓ backed up {}{also}", pod.name),
            };
            (line, None)
        }
        Ok(out) => {
            let report = parse_backup_report(&out.stdout);
            let fix = report.failure(&repo, &pod.name, out.code);
            let line = match &fix {
                Some(fix) => format!("✗ backup {}: {fix}", pod.name),
                None => format!("✗ backup {} ({}): {}", pod.name, out.exit_label(), out.stderr.trim()),
            };
            let kept = match (report.wrong_origin_why(out.code), report.in_progress(out.code)) {
                (Some(_), _) => fix.map(Kept::WrongOrigin),
                (None, Some(what)) => Some(Kept::Busy {
                    what: what.to_string(),
                    on: report.branch.clone().unwrap_or_else(|| "its branch".to_string()),
                    unmerged: report.unmerged,
                }),
                (None, None) => None,
            };
            (line, kept)
        }
        Err(e) => (format!("✗ backup {} failed: {}", pod.name, describe_error(&e)), None),
    }
}

/// The ARENA checkout path on a pod (config `BACKUP_REPO_PATH`, else /root/<repo name>).
fn backup_repo_path(cfg: &Config) -> String {
    cfg.get("BACKUP_REPO_PATH").map(String::from).unwrap_or_else(|| {
        format!("/root/{}", cfg.get("ARENA_REPO_NAME").unwrap_or("ARENA_materials"))
    })
}

/// Provision the pod over SSH — the image-based steps of `arena pods setup` (non-force,
/// matching the CLI default): copy the deploy key, move the repo onto the pod's volume when it
/// has one, run the config. Through core's runner, so each step keeps its own budget (core
/// `SetupTimeouts`: `SETUP_TIMEOUT_SECS` honoured) and a step's `arena-warning:` lines — the
/// only sign that the repo did NOT go onto the volume, and why — reach the result line
/// (`✓ set up <pod> (warning: …)`) instead of being dropped with its stdout.
async fn run_setup(remote: &dyn Remote, cfg: &Config, pod: &Pod) -> String {
    use arena_core::setup::{provision, BootRetry, ProvisionOutcome};
    let target = match SshTarget::from_pod(pod, cfg) {
        Ok(t) => t,
        Err(e) => return format!("✗ {}: {e}", pod.name),
    };
    let steps = match setup_steps(cfg, pod) {
        Ok(s) => s,
        Err(e) => return format!("✗ {}: {e}", pod.name),
    };
    // No boot-race retries: the operator presses Setup on a pod that is already up.
    let no_wait = BootRetry { window: Duration::ZERO, every: Duration::ZERO };
    match provision(remote, &target, &steps, no_wait).await {
        ProvisionOutcome::Done { warnings, .. } if warnings.is_empty() => format!("✓ set up {}", pod.name),
        ProvisionOutcome::Done { warnings, .. } => format!("✓ set up {} (warning: {})", pod.name, warnings.join("; ")),
        failed => format!("✗ setup {}: {}", pod.name, failed.describe()),
    }
}

/// The TUI's setup steps: core's image-based list (as before: no hetzner script, and no VS
/// Code warm-up — that's `arena pods setup`'s).
fn setup_steps(cfg: &Config, pod: &Pod) -> arena_core::Result<Vec<arena_core::setup::ProvisionStep>> {
    use arena_core::setup::{provisioning_steps, SetupConfig, SetupTimeouts};
    let mut scfg = SetupConfig::from_config(cfg)?;
    scfg.vscode = None;
    let budget = SetupTimeouts::from_config(cfg, None)?;
    Ok(provisioning_steps("image", &scfg, &pod.name, false, "", &budget))
}

/// The exact command(s) a safe (non-typed) action will run, for the confirm modal's `v` (it
/// says what the action does otherwise: [`confirm_body`]). Lifecycle actions return `None`.
fn build_preview(cfg: &Config, action: Action, pod: &Pod) -> Option<String> {
    let target = SshTarget::from_pod(pod, cfg);
    match action {
        Action::Backup => Some(match &target {
            Ok(t) => {
                // The script's arguments, not its ~170 lines (`arena pods backup --dry-run` prints it,
                // as the here-doc that writes the file this line reads).
                let msg = format!("arena-tui backup {}", pod.name);
                let origin = arena_core::backup::configured_repo(cfg);
                arena_core::backup::backup_dry_run_line(t, &backup_repo_path(cfg), cfg.get("GIT_SSH_KEY_REMOTE"), &msg, origin.as_deref())
            }
            Err(e) => format!("⚠ {e}"),
        }),
        Action::Setup => Some(match (&target, setup_steps(cfg, pod)) {
            (Ok(t), Ok(steps)) => steps
                .iter()
                .map(|step| {
                    use arena_core::setup::ProvisionStep;
                    match step {
                        ProvisionStep::Scp { local, remote, .. } => t.display_scp(local, remote),
                        ProvisionStep::Run { cmd, .. } => t.display_command(cmd),
                        ProvisionStep::Optional { label, summary, .. } => format!("# {label} (best-effort): {summary}"),
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
            (Err(e), _) => format!("⚠ {e}"),
            (_, Err(e)) => format!("⚠ {e}"),
        }),
        Action::Test => Some(match &target {
            Ok(t) => t.display_command(TORCH_TEST_CMD),
            Err(e) => format!("⚠ {e}"),
        }),
        _ => None,
    }
}

fn temp_style(t: Option<u32>) -> Style {
    match t {
        Some(t) if t >= 85 => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        Some(t) if t >= 75 => Style::default().fg(Color::Yellow),
        Some(_) => Style::default().fg(Color::Green),
        None => Style::default().fg(Color::DarkGray),
    }
}

fn util_style(u: Option<u32>, err: bool) -> Style {
    if err {
        return Style::default().fg(Color::Red);
    }
    match u {
        Some(0) => Style::default().fg(Color::DarkGray), // idle GPU — worth noticing
        Some(_) => Style::default().fg(Color::Green),
        None => Style::default().fg(Color::DarkGray),
    }
}

/// Style a fullness percentage (RAM / disk): plain until ~90%, yellow when nearly full,
/// red when critically full — so a pod about to run out of space/RAM stands out.
fn capacity_style(pct: Option<u32>) -> Style {
    match pct {
        Some(p) if p >= 95 => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        Some(p) if p >= 90 => Style::default().fg(Color::Yellow),
        None => Style::default().fg(Color::DarkGray),
        _ => Style::default(),
    }
}

/// Truncate a string to `max` display columns, adding an ellipsis if cut.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// A one-letter, color-coded provider badge: R=runpod, V=vast, H=hetzner, S=shadeform.
fn provider_cell(provider: &str) -> Cell<'static> {
    let (letter, color) = match provider {
        "runpod" => ("R", Color::Cyan),
        "vast" => ("V", Color::Magenta),
        "hetzner" => ("H", Color::Yellow),
        "shadeform" => ("S", Color::Green),
        other => (other.get(0..1).unwrap_or("?"), Color::Gray),
    };
    Cell::from(Span::styled(
        letter.to_uppercase(),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    ))
}

/// The compact setup cluster (SET): four glyphs for `~/.name`, the deploy key, the git
/// origin→GitHub, and an API key — `.name`/key/origin/api. ✓ green / ✗ red / · gray.
/// (Detail pane spells them out. Not to be confused with HEALTH, the deep check.)
fn setup_cell(m: Option<&PodMetrics>) -> Cell<'static> {
    let glyph = |ok: Option<bool>| match ok {
        Some(true) => Span::styled("✓", Style::default().fg(Color::Green)),
        Some(false) => Span::styled("✗", Style::default().fg(Color::Red)),
        None => Span::styled("·", Style::default().fg(Color::DarkGray)),
    };
    match m {
        Some(m) if m.error.is_none() => {
            let origin_ok = m.origin.as_deref().map(|o| o.contains("github.com"));
            Cell::from(Line::from(vec![
                glyph(m.has_name),
                glyph(m.has_key),
                glyph(origin_ok),
                glyph(m.has_api_key),
            ]))
        }
        _ => Cell::from(Span::styled("····", Style::default().fg(Color::DarkGray))),
    }
}

/// A glyph + style marking how the local branch diverges from its upstream:
/// `↑` ahead (committed but unpushed — e.g. a blocked push), `↓` behind, `⇕` diverged,
/// or none when in sync / no upstream / unreachable.
fn sync_marker(m: Option<&PodMetrics>) -> (&'static str, Style) {
    match m {
        Some(m) if m.error.is_none() => match (m.ahead.unwrap_or(0), m.behind.unwrap_or(0)) {
            (a, b) if a > 0 && b > 0 => ("⇕", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            (a, _) if a > 0 => ("↑", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            (_, b) if b > 0 => ("↓", Style::default().fg(Color::Cyan)),
            _ => ("", Style::default()),
        },
        _ => ("", Style::default()),
    }
}

/// A human description of branch-vs-origin sync, for the detail pane.
fn sync_summary(m: Option<&PodMetrics>) -> String {
    let Some(m) = m else { return "-".into() };
    if m.error.is_some() {
        return "-".into();
    }
    match (m.ahead, m.behind) {
        (Some(a), Some(b)) if a > 0 && b > 0 => {
            format!("⇕ diverged — {a} ahead, {b} behind origin (push blocked?)")
        }
        (Some(a), _) if a > 0 => format!("↑ {a} commit(s) NOT on origin — push blocked/failed?"),
        (_, Some(b)) if b > 0 => format!("↓ {b} commit(s) behind origin"),
        (Some(_), Some(_)) => "✓ in sync with origin".into(),
        _ => "· no upstream (branch never pushed)".into(),
    }
}

/// The right-hand detail cell: progress if we have it, else the (truncated) fetch
/// error, else "-".
fn detail_cell(m: Option<&PodMetrics>) -> (String, Style) {
    match m {
        Some(m) if m.progress.is_some() => (m.progress.clone().unwrap(), Style::default()),
        Some(m) if m.error.is_some() => {
            let mut e = m.error.clone().unwrap();
            if e.len() > 48 {
                e.truncate(47);
                e.push('…');
            }
            (e, Style::default().fg(Color::Red).add_modifier(Modifier::DIM))
        }
        _ => ("-".into(), Style::default().fg(Color::DarkGray)),
    }
}

fn view(f: &mut Frame, shared: &Shared, ui: &Ui, secs: u64) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // title
            Constraint::Length(1), // fleet summary
            Constraint::Min(0),    // body
            Constraint::Length(1), // footer / hints
        ])
        .split(f.area());

    let title = Paragraph::new(format!(
        "arena-infra-rs dashboard · provider: {} · {}",
        ui.provider_name, ui.config_path
    ))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(title, chunks[0]);

    f.render_widget(summary_line(shared), chunks[1]);

    if matches!(ui.mode, Mode::Detail) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(chunks[2]);
        // No room for the history sparklines in the split view.
        pods_table(f, shared, ui, cols[0], false);
        detail_pane(f, shared, ui, cols[1]);
    } else {
        pods_table(f, shared, ui, chunks[2], true);
    }

    f.render_widget(Paragraph::new(footer_hint(shared, ui, secs)), chunks[3]);

    match &ui.mode {
        Mode::Menu { sel } => render_menu(f, shared, ui, *sel),
        Mode::Confirm(c) => render_confirm(f, c),
        Mode::FleetMenu { scope, sel } => render_fleet_menu(f, shared, ui, *scope, *sel),
        Mode::FleetConfirm { action, typed, scope, note } => render_fleet_confirm(f, shared, ui, *action, typed, *scope, note),
        Mode::NewPod(form) => render_new_pod(f, form),
        Mode::Input { action, scope, value } => render_input(f, shared, ui, *action, scope, value),
        Mode::Select { value } => render_select(f, value),
        Mode::Working(msg) => render_result(f, msg, 0, &ui.result_max),
        Mode::Result(msg) => render_result(f, msg, ui.result_scroll, &ui.result_max),
        Mode::Ask { scope, value, pick } => ask::render_input(f, shared, ui, scope, value, pick),
        Mode::Answer(answer) => ask::render_answer(f, answer, ui.result_scroll, &ui.result_max),
        _ => {}
    }
}

/// The free-text input modal for `run` / `set-branch` (one pod or a multi-pod scope) — also
/// their confirm: Enter runs it. On top, what Enter will do as typed so far ([`action_summary`]:
/// `alpha: switch to branch w3d2 (fast-forward only)`, the command itself for `run`; several
/// pods named once), wrapped by the widget.
fn render_input(f: &mut Frame, shared: &Shared, ui: &Ui, action: Action, scope: &InputScope, value: &str) {
    let names = match scope {
        InputScope::Pod { name, .. } => vec![name.clone()],
        InputScope::Set(sc) => scope_names(shared, ui, *sc),
    };
    let text = format!(
        "{}\n\n{}\n\n  > {value}\n\n[enter] run  [esc] cancel",
        action_summary(action, &names, Some(value), CONFIRM_NAMES_MAX).join("\n"),
        action.input_prompt(),
    );
    let area = centered_rect(70, 40, f.area());
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(format!(" {} ", action.label())),
            ),
        area,
    );
}

/// The `/` modal: type a selection in the CLI's selector syntax.
fn render_select(f: &mut Frame, value: &str) {
    let text = format!(
        "mark pods by selector — the CLI's syntax:\n\n  names or ids     apple bloom   (or apple,bloom)\n  a range          apple..delta   (MACHINE_NAME_LIST order)\n  leave out        -x cloud   or   !cloud\n  narrow           --on runpod   --gpus 2\n\nReplaces the current marks; a typo marks nothing.\n\n  > {value}\n\n[enter] mark  [esc] cancel"
    );
    let area = centered_rect(64, 50, f.area());
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)).title(" select "),
        ),
        area,
    );
}

/// The summary bar (`state::summary_text`): counts, then the fleet's burn from the
/// snapshot's core `FleetCost`, worded as `pods list`'s footer, then the prepaid balances —
/// led, in yellow, by which provider failed to list when one did (the totals after it are
/// then a floor), and by a low (yellow) or critical (red) balance.
fn summary_line(s: &Shared) -> Paragraph<'static> {
    let text = summary_text(&s.summary, &s.snap.cost, &s.snap.partial, &s.balance, s.ai_cohort.as_ref());
    let (lead, _, _) = balance::tui_parts(&s.balance);
    let ai_lead = s.ai_cohort.as_ref().and_then(|a| a.tui_parts().0);
    let mut rest = text.as_str();
    let mut spans = Vec::new();
    for (notice, tone) in [
        (partial_notice(&s.snap.partial), Some(Tone::Warn)),
        (lead, state::balance_tone(&s.balance)),
        (ai_lead, state::ai_tone(s.ai_cohort.as_ref())),
    ] {
        if let (Some(n), Some(tone)) = (notice.filter(|n| rest.starts_with(n.as_str())), tone) {
            rest = &rest[n.len()..];
            spans.push(Span::styled(n, tone_style(tone)));
        }
    }
    spans.push(Span::raw(rest.to_string()));
    Paragraph::new(Line::from(spans)).style(Style::default().add_modifier(Modifier::BOLD))
}

/// A [`Tone`] as a colour.
fn tone_style(t: Tone) -> Style {
    match t {
        Tone::Good => Style::default().fg(Color::Green),
        Tone::Warn => Style::default().fg(Color::Yellow),
        Tone::Bad => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        Tone::Busy => Style::default().fg(Color::Cyan),
        Tone::Dim => Style::default().fg(Color::DarkGray),
    }
}

/// The pods table's width for columns of these widths: their sum, one space between each,
/// and 4 for the block's two borders and the `▶ ` highlight column.
fn table_width(cols: &[usize]) -> usize {
    cols.iter().sum::<usize>() + cols.len().saturating_sub(1) + 4
}

/// How many of `optional` (column widths, most wanted first) fit in a table `w` wide beside
/// the `core` columns. Always a prefix, so widening the terminal only ever adds columns.
fn columns_that_fit(w: usize, core: &[usize], optional: &[usize]) -> usize {
    let mut cols = core.to_vec();
    optional
        .iter()
        .take_while(|&&c| {
            cols.push(c);
            table_width(&cols) <= w
        })
        .count()
}

/// A pod's `AI$` cell: its OpenRouter key's spend against the cap (`-`: no key named after
/// it), yellow from 80% of the cap, red at the cap or disabled — grey when the last listing
/// failed (`stale`: these are older numbers, the summary bar says `(n/a)`).
fn ai_cell(k: Option<&KeyUsage>, stale: bool, read: bool) -> Cell<'static> {
    match k {
        // Before the first read lands: not known yet, rather than "no key".
        None if !read => Cell::from("…").style(tone_style(Tone::Dim)),
        None => Cell::from("-").style(tone_style(Tone::Dim)),
        Some(k) if stale => Cell::from(k.cell()).style(tone_style(Tone::Dim)),
        Some(k) => {
            let style = state::fill_tone(k.fill()).map(tone_style).unwrap_or_default();
            Cell::from(k.cell()).style(style)
        }
    }
}

fn pods_table(f: &mut Frame, shared: &Shared, ui: &Ui, area: Rect, with_spark: bool) {
    // Responsive. The essential columns — mark, P, NAME, STATUS, M, SET, GPU, GPU%, $/H,
    // HEALTH, PROXY — always show and fit in 73 columns. Next come the mid columns, kept
    // while they fit (`MID_W`, all of them from 111 columns), so a narrow terminal or the
    // detail view's 55% split drops TEMP, RTT, MEM, BRANCH, DISK in turn instead of having
    // ratatui shrink every column (NAME, the one that says which pod a row is, worst of
    // all). Above that the nice-to-haves (SAVED, AI$, host CPU/RAM, then the UTIL%/MEM%
    // graphs, then PROGRESS when PROGRESS_CMD is set) come in as there's room. Below 73
    // columns the table is crushed as before.
    let w = area.width as usize;
    // The fleet columns from the core snapshot — M (maintenance badge), HEALTH (last deep
    // check) and PROXY — are essential and always shown, so every threshold below sits
    // `FLEET_W` further out than it did before they existed. PROXY is just the port
    // (coloured live/stale) unless there's room for the full `:9500 stale` label.
    const HEALTH_W: usize = 8; // "fail 23h", "checking"
    let full_proxy = w >= 160;
    let proxy_w = if full_proxy { 11 } else { 6 };
    const FLEET_W: usize = 1 + HEALTH_W + 6 + 3; // + their inter-column spacing
    let show_saved = w >= 96 + FLEET_W;
    // AI$: each pod's OpenRouter key, spent against its cap — only with a provisioning key.
    const AI_W: usize = 9; // "1.23/5", "12.3/50", "0.02/0.02"
    let show_ai = shared.ai.is_some() && w >= 104 + FLEET_W;
    let show_host = w >= 134 + FLEET_W; // host CPU% + RAM (extra, only when there's room)
    // PROGRESS is what PROGRESS_CMD reports, so only with one set — and it comes after the
    // graphs, which say more at a glance (an SSH error is in STATUS and the detail pane).
    let want_progress = ui.cfg.get("PROGRESS_CMD").is_some_and(|c| !c.trim().is_empty());
    // When cramped, names compress (arena8-apple→apple) and the GPU drops the "RTX "
    // noise. Names compact a bit earlier (so you see "jack", not a truncated
    // "arena8-ja"); GPU always carries count + VRAM ("2×A4000 16G"), truncated if tight.
    let compact_names = w < 116 + FLEET_W;
    let narrow = w < 100 + FLEET_W;
    let name_w = if compact_names { 10 } else { 16 };
    let gpu_w = if narrow { 12 } else { 16 };
    let now = shared.snap.generated_at;

    // mark, P, NAME, STATUS, M, SET, GPU, GPU%, $/H, HEALTH, PROXY.
    let core = [1, 1, name_w, 4, 1, 4, gpu_w, 5, 7, HEALTH_W, proxy_w];
    // DISK, BRANCH, MEM, RTT, TEMP — kept in this order: a disk filling up (coloured) breaks
    // a participant's work, the branch says which iteration they're on, MEM/TEMP are in the
    // detail pane's per-GPU table anyway; RTT (how far the pod is from the proxy every
    // keystroke goes through) is in the detail pane too.
    const MID_W: [usize; 5] = [9, 6, 9, 5, 4];
    let mid = columns_that_fit(w, &core, &MID_W);
    let (show_disk, show_branch, show_mem, show_rtt, show_temp) = (mid > 0, mid > 1, mid > 2, mid > 3, mid > 4);

    // Sparklines (GPU%/MEM% history) flex to fill whatever horizontal space is left after
    // the other columns — so they grow on a wide screen and simply vanish when there's no
    // room, rather than living behind a fixed threshold. PROGRESS gets a fixed budget when
    // sparks are present so the leftover math is stable.
    const PROGRESS_W: usize = 16;
    let mut fixed: Vec<usize> = core.to_vec();
    fixed.extend(&MID_W[..mid]);
    if show_saved {
        fixed.push(6);
    }
    if show_ai {
        fixed.push(AI_W);
    }
    if show_host {
        fixed.extend([4, 4]);
    }
    let leftover = w.saturating_sub(table_width(&fixed));
    let show_spark = with_spark && leftover >= 20; // ~2×9 + spacing
    // PROGRESS takes its fixed budget only from what the graphs leave at their narrowest.
    let show_progress = want_progress && leftover >= PROGRESS_W + 1 + if show_spark { 20 } else { 0 };
    let spark_room = if show_progress { leftover - PROGRESS_W - 1 } else { leftover };
    let spark_w = if show_spark { (spark_room.saturating_sub(3) / 2).clamp(9, 30) } else { 0 };

    let mut header_cells = vec!["", "P", "NAME", "STATUS", "M", "SET", "GPU", "GPU%"];
    for (show, title) in [(show_mem, "MEM"), (show_temp, "TEMP"), (show_disk, "DISK")] {
        if show {
            header_cells.push(title);
        }
    }
    header_cells.extend(["$/H", "HEALTH", "PROXY"]);
    if show_rtt {
        header_cells.push("RTT");
    }
    if show_branch {
        header_cells.push("BRANCH");
    }
    if show_saved {
        header_cells.push("SAVED");
    }
    if show_ai {
        header_cells.push("AI$");
    }
    if show_host {
        header_cells.push("CPU%");
        header_cells.push("RAM%");
    }
    if show_progress {
        header_cells.push("PROGRESS");
    }
    if show_spark {
        // GPU and its memory (green, cyan); a CPU box's CPU and RAM (magenta, blue).
        header_cells.push("UTIL%~");
        header_cells.push("MEM%~");
    }
    let header = Row::new(header_cells).style(Style::default().add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = shared
        .pods
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let m = shared.metrics.get(&p.name);
            // This row's snapshot entry (same index: `Shared::publish`).
            let sp = shared.snap.pods.get(i);
            let util = m.and_then(|m| m.mean_util());
            let err = m.map(|m| m.error.is_some()).unwrap_or(false);
            let util_str = match (util, err) {
                (Some(u), _) => format!("{u}%"),
                (None, true) => "err".into(),
                (None, false) => "-".into(),
            };
            let mem = match m.and_then(|m| m.mem_summary()) {
                Some((u, t)) => format!("{:.0}/{:.0}G", u as f64 / 1024.0, t as f64 / 1024.0),
                None => "-".into(),
            };
            let disk_pct = m.and_then(|m| m.disk_summary()).filter(|(_, t)| *t > 0)
                .map(|(u, t)| (u as u64 * 100 / t as u64) as u32);
            let disk = match m.and_then(|m| m.disk_summary()) {
                Some((u, t)) => format!("{:.0}/{:.0}G", u as f64 / 1024.0, t as f64 / 1024.0),
                None => "-".into(),
            };
            let temp = m.and_then(|m| m.max_temp());
            let temp_str = temp.map(|t| format!("{t}C")).unwrap_or_else(|| "-".into());
            // As `pods list` shows it: the provider's currency, `-` unless billing.
            let cost = fleet::price_label(p);
            // The one-letter badge: M = host maintenance on record (the more urgent, so it
            // wins), else L = locked (stop/restart/terminate refused; the detail pane says so).
            let maint = if state::has_maintenance(p) {
                Cell::from("M").style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
            } else if arena_core::lock::is_locked(p) {
                Cell::from("L").style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
            } else {
                Cell::from(" ")
            };
            let (health, health_tone) =
                health_cell(sp.and_then(|sp| sp.health.as_ref()), shared.deep.is_running(p), now);
            let (proxy, proxy_tone) = sp.map(|sp| proxy_cell(sp, !full_proxy)).unwrap_or(("?".into(), Tone::Dim));
            // GPU now comes from the live nvidia-smi readout (provider list omits it).
            // Append VRAM when wide; drop the "RTX " noise when the table is cramped.
            let mut gpu = m
                .and_then(|m| m.gpu_summary())
                .or_else(|| p.gpu_type.clone())
                .unwrap_or_else(|| "-".into());
            if narrow {
                gpu = gpu.replace("RTX ", "");
            }
            // Always append VRAM; the column truncates the tail if space is tight.
            if gpu != "-" {
                if let Some(g) = m.and_then(|m| m.vram_gb()) {
                    gpu = format!("{gpu} {g}G");
                }
            }
            let branch = match m.and_then(|m| m.branch.clone()) {
                Some(b) => truncate(&short_branch(&b, &ui.prefix), 6),
                None => "-".into(),
            };
            // RUNNING-but-unreachable reads as "init" (still coming up), in yellow.
            let status_label = pod_status(p, m.map(|m| m.error.is_none()));
            let status_cell = if status_label == "no-ssh" {
                Cell::from(status_label).style(Style::default().fg(Color::Red))
            } else if status_label == "init" {
                Cell::from(status_label).style(Style::default().fg(Color::Yellow))
            } else {
                Cell::from(status_label)
            };
            let mark = if ui.marked.contains(&mark_key(p)) {
                Cell::from("•").style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
            } else {
                Cell::from(" ")
            };
            let mut cells = vec![
                mark,
                provider_cell(&p.provider),
                // Force the short name when cramped, regardless of the persisted pref.
                Cell::from(if compact_names {
                    display_name(&p.name, &ui.prefix, true)
                } else {
                    ui.shown_name(&p.name)
                }),
                status_cell,
                maint,
                setup_cell(m),
                Cell::from(gpu),
                Cell::from(util_str).style(util_style(util, err)),
            ];
            if show_mem {
                cells.push(Cell::from(mem));
            }
            if show_temp {
                cells.push(Cell::from(temp_str).style(temp_style(temp)));
            }
            if show_disk {
                cells.push(Cell::from(disk).style(capacity_style(disk_pct)));
            }
            cells.extend([
                Cell::from(cost),
                Cell::from(health).style(tone_style(health_tone)),
                Cell::from(proxy).style(tone_style(proxy_tone)),
            ]);
            if show_rtt {
                let (rtt, tone) = rtt_cell(shared.rtts.of(p));
                cells.push(Cell::from(rtt).style(tone_style(tone)));
            }
            if show_branch {
                cells.push(Cell::from(branch));
            }
            if show_saved {
                // Time style: yellow when there's uncommitted work; grey when unknown OR
                // when it's a clean tree that's been quiet for a day+ (nothing to do); plain
                // otherwise.
                let stale = m
                    .and_then(|m| m.last_commit)
                    .map(|ts| now_unix().saturating_sub(ts) > 86_400)
                    .unwrap_or(false);
                let style = match m.and_then(|m| m.dirty_files) {
                    Some(n) if n > 0 => Style::default().fg(Color::Yellow),
                    Some(0) if stale => Style::default().fg(Color::DarkGray),
                    None => Style::default().fg(Color::DarkGray),
                    _ => Style::default(),
                };
                // A sync glyph (↑ unpushed / ↓ behind / ⇕ diverged) sits right before the
                // time, so a blocked/failed push is obvious next to "when it was saved".
                let (sync, sync_style) = sync_marker(m);
                cells.push(Cell::from(Line::from(vec![
                    Span::styled(sync, sync_style),
                    Span::styled(rel_time_short(m), style),
                ])));
            }
            if show_ai {
                let ai = shared.ai.as_ref();
                cells.push(ai_cell(
                    ai.and_then(|a| a.keys.get(&p.name)),
                    ai.is_some_and(|a| a.keys_error.is_some()),
                    ai.is_some_and(|a| a.read),
                ));
            }
            if show_host {
                // Host CPU% and RAM% (both coloured by load); exact RAM GB is in detail.
                let cpu = m.and_then(|m| m.cpu_pct);
                cells.push(Cell::from(cpu.map(|c| format!("{c}%")).unwrap_or_else(|| "-".into()))
                    .style(util_style(cpu, err)));
                let ram_pct = m.and_then(|m| m.host_mem_summary())
                    .filter(|(_, t)| *t > 0)
                    .map(|(u, t)| (u as u64 * 100 / t as u64) as u32);
                cells.push(Cell::from(ram_pct.map(|p| format!("{p}%")).unwrap_or_else(|| "-".into()))
                    .style(capacity_style(ram_pct)));
            }
            if show_progress {
                let (detail, detail_style) = detail_cell(m);
                cells.push(Cell::from(detail).style(detail_style));
            }
            if show_spark {
                let h = shared.history.get(&p.name);
                let gpu_hist = h.map(|h| h.util_data()).unwrap_or_default();
                let mem_hist = h.map(|h| h.mem_data()).unwrap_or_default();
                // Grey the sparklines out when the pod isn't reachable — the history is
                // just zeros then, so colour would imply live data that isn't there.
                let cpu = p.cpu_only();
                let connected =
                    m.is_some_and(|m| m.error.is_none() && if cpu { m.cpu_pct.is_some() } else { !m.gpus.is_empty() });
                let (gpu_c, mem_c) = match (connected, cpu) {
                    (false, _) => (Color::DarkGray, Color::DarkGray),
                    (true, false) => (Color::Green, Color::Cyan),
                    (true, true) => (Color::Magenta, Color::Blue),
                };
                cells.push(Cell::from(spark(&gpu_hist, spark_w)).style(Style::default().fg(gpu_c)));
                cells.push(Cell::from(spark(&mem_hist, spark_w)).style(Style::default().fg(mem_c)));
            }
            Row::new(cells)
        })
        .collect();

    let mut widths = vec![
        Constraint::Length(1),  // mark (•)
        Constraint::Length(1),  // P (provider glyph)
        Constraint::Length(name_w as u16), // NAME (auto-short when cramped)
        Constraint::Length(4),  // STATUS (abbreviated: run/exit/stop…)
        Constraint::Length(1),  // M (host maintenance badge; L = locked)
        Constraint::Length(4),  // SET (.name/key/origin/api)
        Constraint::Length(gpu_w as u16), // GPU (+VRAM when wide)
        Constraint::Length(5),  // GPU%
    ];
    if show_mem {
        widths.push(Constraint::Length(MID_W[2] as u16)); // MEM (e.g. "120/240G")
    }
    if show_temp {
        widths.push(Constraint::Length(MID_W[4] as u16)); // TEMP (e.g. "85C")
    }
    if show_disk {
        widths.push(Constraint::Length(MID_W[0] as u16)); // DISK (e.g. "12/100G")
    }
    widths.extend([
        Constraint::Length(7),               // $/H (e.g. "$0.17", "€0.006")
        Constraint::Length(HEALTH_W as u16), // HEALTH (e.g. "pass 12m")
        Constraint::Length(proxy_w as u16),  // PROXY (":9500", or ":9500 stale" when wide)
    ]);
    if show_rtt {
        widths.push(Constraint::Length(MID_W[3] as u16)); // RTT (e.g. "23ms", "180ms")
    }
    if show_branch {
        widths.push(Constraint::Length(MID_W[1] as u16)); // BRANCH (e.g. "w1d2")
    }
    if show_saved {
        widths.push(Constraint::Length(6)); // SAVED (e.g. "3h", "↑2d")
    }
    if show_ai {
        widths.push(Constraint::Length(AI_W as u16)); // AI$ (e.g. "1.23/5")
    }
    if show_host {
        widths.push(Constraint::Length(4)); // CPU%
        widths.push(Constraint::Length(4)); // RAM%
    }
    if show_progress {
        // Fixed budget when sparks are filling the rest; otherwise flex to fill.
        widths.push(if show_spark { Constraint::Length(PROGRESS_W as u16) } else { Constraint::Min(10) });
    }
    if show_spark {
        widths.push(Constraint::Length(spark_w as u16)); // GPU% history
        widths.push(Constraint::Length(spark_w as u16)); // MEM% history
    }
    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(Style::default().bg(Color::Indexed(237)).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ")
        .block(Block::default().borders(Borders::ALL).title("Pods"));

    let mut ts = TableState::default();
    if !shared.pods.is_empty() {
        ts.select(Some(ui.selected.min(shared.pods.len() - 1)));
    }
    f.render_stateful_widget(table, area, &mut ts);
    // Record where the rows landed (area + the scroll offset ratatui chose) so a mouse
    // click can be mapped back to a pod index.
    ui.last_table.set((area, ts.offset()));
}

/// When the pod's repo was last committed (= last backup, since `pods backup` commits +
/// pushes), plus whether there's uncommitted work since. `-` if not reported yet.
fn backup_summary(m: Option<&PodMetrics>) -> String {
    let Some(m) = m else { return "-".into() };
    match m.last_commit {
        Some(ts) => {
            let state = match m.dirty_files {
                Some(0) => "clean".to_string(),
                Some(n) => format!("⚠ {n} uncommitted"),
                None => "?".to_string(),
            };
            format!("{}  ({state})", rel_time(ts))
        }
        None => "-".into(),
    }
}

/// Current unix time in seconds.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A short relative time like "3h ago" / "2d ago" from a unix timestamp.
fn rel_time(unix_secs: i64) -> String {
    let d = now_unix() - unix_secs;
    match d {
        d if d < 60 => "just now".into(),
        d if d < 3600 => format!("{}m ago", d / 60),
        d if d < 86_400 => format!("{}h ago", d / 3600),
        d => format!("{}d ago", d / 86_400),
    }
}

/// A compact relative time for the table cell: "3h", "2d", "12m", "now", or "-".
fn rel_time_short(m: Option<&PodMetrics>) -> String {
    match m.and_then(|m| m.last_commit) {
        None => "-".into(),
        Some(ts) => {
            let d = now_unix() - ts;
            match d {
                d if d < 60 => "now".into(),
                d if d < 3600 => format!("{}m", d / 60),
                d if d < 86_400 => format!("{}h", d / 3600),
                d => format!("{}d", d / 86_400),
            }
        }
    }
}

/// Lines in the detail pane's facts block (see `detail_pane`'s `facts`).
const FACT_LINES: u16 = 18;

/// The per-pod detail pane (shown in Detail mode): identity + endpoint, proxy forward,
/// cost, the host maintenance window + note, the last deep check (and why), a per-GPU
/// table, full progress text, and util/temp sparklines from the rolling history.
fn detail_pane(f: &mut Frame, shared: &Shared, ui: &Ui, area: Rect) {
    let Some(pod) = shared.pods.get(ui.selected) else { return };
    let m = shared.metrics.get(&pod.name);

    let block =
        Block::default().borders(Borders::ALL).title(format!(" {} ", ui.shown_name(&pod.name)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Optional extra sections, shown only when there's data: GPU compute processes and
    // the most-recent ARENA_3.0 commits. Each costs one row for its TOP border/title.
    let n_procs = m.map(|m| m.gpu_procs.len()).unwrap_or(0).min(4);
    let n_commits = m.map(|m| m.recent_commits.len()).unwrap_or(0).min(4);
    let procs_h = if n_procs > 0 { n_procs as u16 + 1 } else { 0 };
    let commits_h = if n_commits > 0 { n_commits as u16 + 1 } else { 0 };
    // History graphs are a nice-to-have; show them only if the pane is still tall enough
    // once the facts, per-GPU table, and the extra sections have taken their space.
    // The OpenRouter key's line, under `cost:`, when a provisioning key is configured. "No
    // key" only from a listing that worked: before the first read, or after a failed one,
    // absence isn't known — and numbers kept from an older read say so.
    let ai = shared.ai.as_ref().map(|a| match (a.keys.get(&pod.name), a.read, a.keys_error.as_deref()) {
        (None, false, _) => "… (reading OpenRouter's keys)".to_string(),
        (None, _, Some(e)) => format!("? (OpenRouter's keys couldn't be read: {e})"),
        (None, _, None) => "- (no OpenRouter key named after this pod — `arena keys gen` makes one)".to_string(),
        (Some(k), _, None) => k.detail(),
        (Some(k), _, Some(_)) => format!("{} (as last read; the latest read failed)", k.detail()),
    });
    let fact_lines = FACT_LINES + u16::from(ai.is_some());
    let show_graphs = inner.height >= fact_lines + 3 + procs_h + commits_h + 6;

    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(fact_lines), // header facts
        Constraint::Min(3),             // per-GPU table
    ];
    let mut idx = 2;
    let procs_idx = (procs_h > 0).then(|| {
        constraints.push(Constraint::Length(procs_h));
        let x = idx;
        idx += 1;
        x
    });
    let commits_idx = (commits_h > 0).then(|| {
        constraints.push(Constraint::Length(commits_h));
        let x = idx;
        idx += 1;
        x
    });
    if show_graphs {
        constraints.push(Constraint::Length(3)); // util sparkline
        constraints.push(Constraint::Length(3)); // temp sparkline
    }
    let rows = Layout::default().direction(Direction::Vertical).constraints(constraints).split(inner);

    let endpoint = match (&pod.ssh_ip, pod.ssh_port) {
        (Some(ip), Some(port)) => format!("{ip}:{port}"),
        _ => "(no SSH endpoint yet)".into(),
    };
    // Where it runs (the provider's name for the place) and how far that is from here — the
    // proxy box, on prod — as the RTT column times it.
    let place = pod.location.clone().unwrap_or_else(|| "-".into());
    let rtt = match (shared.rtts.of(pod), &pod.ssh_ip) {
        (Some(d), _) => format!("rtt {}ms from this box", d.as_millis()),
        (None, Some(_)) => format!("rtt - (a TCP connect to the SSH port, every {}s)", RTT_EVERY.as_secs()),
        (None, None) => "rtt -".into(),
    };
    let progress = m
        .and_then(|m| m.progress.clone())
        .or_else(|| m.and_then(|m| m.error.clone()))
        .unwrap_or_else(|| "-".into());
    let gpu = m
        .and_then(|m| m.gpu_summary())
        .or_else(|| pod.gpu_type.clone())
        .unwrap_or_else(|| "-".into());
    // `pods list`'s $/H label (the provider's currency; `-` unless billing), plus per day.
    let cost = match fleet::price_label(pod).as_str() {
        "-" => "-".to_string(),
        label => {
            let day = pod.cost_per_hr.map(|c| fleet::fmt_money(fleet::currency_symbol(&pod.provider), c * 24.0));
            format!("{label}/h  ({}/day)", day.unwrap_or_default())
        }
    };
    let sp = shared.snap.pods.get(ui.selected);
    // The snapshot lines are clipped to one row each (after the 10-column label), so a long
    // reason or note can't wrap and push the rest of the facts out of their block.
    let fit = |s: String| truncate(&s, (inner.width as usize).saturating_sub(10).max(8));
    let proxy = fit(sp.map(proxy_detail).unwrap_or_else(|| "?".into()));
    // The last deep check: verdict + age + worst issue as `arena snapshot` words it, and
    // the first failing/warning check's own words (the operator's view, so the raw text).
    let health = match sp.and_then(|sp| sp.health.as_ref()) {
        _ if shared.deep.is_running(pod) => "checking… (deep check running in the background)".to_string(),
        None => "- (never deep-checked — d runs one)".to_string(),
        Some(h) => format!(
            "{}  (checked {})",
            snapshot::health_label(Some(h), shared.snap.generated_at),
            snapshot::rfc3339(h.checked_at)
        ),
    };
    let health = fit(health);
    let why = fit(
        sp.and_then(|sp| sp.health.as_ref()).and_then(|h| h.reasons.first().cloned()).unwrap_or_else(|| "-".into()),
    );
    let maint = fit(fleet::maintenance_label(pod.maintenance.as_ref()));
    let disk = match m.and_then(|m| m.disk_summary()) {
        Some((u, t)) => {
            let pct = if t > 0 { u as f64 / t as f64 * 100.0 } else { 0.0 };
            format!("{:.1}/{:.1}G ({pct:.0}%)", u as f64 / 1024.0, t as f64 / 1024.0)
        }
        None => "-".into(),
    };
    let ok = |b: Option<bool>| match b {
        Some(true) => "✓",
        Some(false) => "✗",
        None => "·",
    };
    let origin = m.and_then(|m| m.origin.clone()).unwrap_or_else(|| "-".into());
    let origin_ok = m.and_then(|m| m.origin.as_deref().map(|o| o.contains("github.com")));
    // Host: CPU% and RAM (exact GB + %).
    let host = {
        let cpu = m.and_then(|m| m.cpu_pct).map(|c| format!("CPU {c}%")).unwrap_or_else(|| "CPU -".into());
        let ram = match m.and_then(|m| m.host_mem_summary()) {
            Some((u, t)) => {
                let pct = if t > 0 { u as f64 / t as f64 * 100.0 } else { 0.0 };
                format!("RAM {:.1}/{:.1}G ({pct:.0}%)", u as f64 / 1024.0, t as f64 / 1024.0)
            }
            None => "RAM -".into(),
        };
        format!("{cpu}   {ram}")
    };
    let facts = format!(
        "status:   {}\ngpu:      {}\nendpoint: {}\nwhere:    {}\nproxy:    {}\ncost:     {}\n{}maint:    {}\nhealth:   {}\nreason:   {}\ndisk:     {}\nhost:     {}\nbranch:   {}\nbackup:   {}\nsync:     {}\norigin:   {} {}\nsetup:    .name {}   deploy-key {}   origin→gh {}   api-key {}\ntokens:   HF {}   Claude-Code {}\nprogress: {}",
        {
            let status = pod_status(pod, m.map(|m| m.error.is_none()));
            if arena_core::lock::is_locked(pod) {
                // One row (the facts block has a fixed height): `pods unlock` is in the docs.
                format!("{status} · locked (stop/restart/terminate refused)")
            } else {
                status
            }
        },
        gpu,
        endpoint,
        fit(format!("{place}  ·  {rtt}")),
        proxy,
        cost,
        ai.map(|a| format!("ai key:   {}\n", fit(a))).unwrap_or_default(),
        maint,
        health,
        why,
        disk,
        host,
        m.and_then(|m| m.branch.clone()).unwrap_or_else(|| "-".into()),
        backup_summary(m),
        sync_summary(m),
        truncate(&origin, 32),
        ok(origin_ok),
        ok(m.and_then(|m| m.has_name)),
        ok(m.and_then(|m| m.has_key)),
        ok(origin_ok),
        ok(m.and_then(|m| m.has_api_key)),
        ok(m.and_then(|m| m.has_hf_token)),
        ok(m.and_then(|m| m.has_cc_token)),
        progress,
    );
    f.render_widget(Paragraph::new(facts).wrap(Wrap { trim: true }), rows[0]);

    let gpu_rows: Vec<Row> = m
        .map(|m| {
            m.gpus
                .iter()
                .enumerate()
                .map(|(i, g)| {
                    let mem = match (g.mem_used_mb, g.mem_total_mb) {
                        (Some(u), Some(t)) => {
                            format!("{:.1}/{:.1}G", u as f64 / 1024.0, t as f64 / 1024.0)
                        }
                        _ => "-".into(),
                    };
                    Row::new(vec![
                        Cell::from(format!("{i}")),
                        Cell::from(g.util_pct.map(|u| format!("{u}%")).unwrap_or_else(|| "-".into()))
                            .style(util_style(g.util_pct, false)),
                        Cell::from(mem),
                        Cell::from(g.temp_c.map(|t| format!("{t}C")).unwrap_or_else(|| "-".into()))
                            .style(temp_style(g.temp_c)),
                    ])
                })
                .collect()
        })
        .unwrap_or_default();
    let gpu_table = Table::new(
        gpu_rows,
        [
            Constraint::Length(4),
            Constraint::Length(6),
            Constraint::Length(13),
            Constraint::Length(6),
        ],
    )
    .header(
        Row::new(vec!["GPU", "UTIL", "MEM", "TEMP"]).style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().borders(Borders::TOP).title("per-GPU"));
    f.render_widget(gpu_table, rows[1]);

    // GPU compute processes (what's actually holding the cards), under the per-GPU table.
    if let Some(pi) = procs_idx {
        let proc_rows: Vec<Row> = m
            .map(|m| {
                // Biggest VRAM first — surface the "main" jobs, not idle ~300M helpers.
                let mut procs: Vec<&metrics::GpuProc> = m.gpu_procs.iter().collect();
                procs.sort_by_key(|p| std::cmp::Reverse(p.mem_mb.unwrap_or(0)));
                procs
                    .into_iter()
                    .take(n_procs)
                    .map(|p| {
                        let mem = p.mem_mb.map(|mb| format!("{mb}M")).unwrap_or_else(|| "-".into());
                        // Show the basename; the full path is rarely useful and is long.
                        let name = p.name.rsplit('/').next().unwrap_or(&p.name).to_string();
                        Row::new(vec![
                            Cell::from(p.pid.to_string()),
                            Cell::from(mem),
                            Cell::from(name),
                        ])
                    })
                    .collect()
            })
            .unwrap_or_default();
        let proc_table = Table::new(
            proc_rows,
            [Constraint::Length(8), Constraint::Length(7), Constraint::Min(10)],
        )
        .header(
            Row::new(vec!["PID", "VRAM", "PROCESS"]).style(Style::default().add_modifier(Modifier::BOLD)),
        )
        .block(Block::default().borders(Borders::TOP).title("GPU procs"));
        f.render_widget(proc_table, rows[pi]);
    }

    // The last few ARENA_3.0 commits (= backup history), one compact line each.
    if let Some(ci) = commits_idx {
        let text = m
            .map(|m| m.recent_commits.iter().take(n_commits).cloned().collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();
        f.render_widget(
            Paragraph::new(text)
                .block(Block::default().borders(Borders::TOP).title("recent commits (ARENA repo)")),
            rows[ci],
        );
    }

    if show_graphs {
        let hist = shared.history.get(&pod.name);
        let util_data = hist.map(|h| h.util_data()).unwrap_or_default();
        let temp_data = hist.map(|h| h.temp_data()).unwrap_or_default();
        let (util_title, util_colour) = if pod.cpu_only() { ("CPU % (history)", Color::Magenta) } else { ("util % (history)", Color::Green) };
        f.render_widget(
            Sparkline::default()
                .block(Block::default().borders(Borders::TOP).title(util_title))
                .data(&util_data)
                .max(100)
                .style(Style::default().fg(util_colour)),
            rows[idx],
        );
        f.render_widget(
            Sparkline::default()
                .block(Block::default().borders(Borders::TOP).title("temp C (history)"))
                .data(&temp_data)
                .max(100)
                .style(Style::default().fg(Color::Yellow)),
            rows[idx + 1],
        );
    }
}

/// The footer: a live [`Notice`] if there is one (else the refresh status and its age),
/// how many deep checks are running, then the keys for the current mode — the way out first
/// (`[q] quit`, `[esc] back`): the line is cut at the terminal's width.
fn footer_hint(shared: &Shared, ui: &Ui, secs: u64) -> Line<'static> {
    let age = match shared.last_refresh {
        Some(t) => format!("updated {}s ago", t.elapsed().as_secs()),
        None => "never updated".into(),
    };
    let spin = if shared.refreshing { " ⟳" } else { "" };
    let checking = match shared.deep.running() {
        0 => String::new(),
        n => format!(" · deep-checking {n}"),
    };
    // A question out leads the footer, moving and counting, so it reads as alive: an answer
    // can take a minute, and a line that never changed looked like a hang.
    let asking = shared.ask_running.as_ref().map(|asking| {
        let secs = shared.ask_since.map(|t| t.elapsed()).unwrap_or_default();
        Span::styled(format!("{} {asking} {}s · ", spinner(secs), secs.as_secs()), tone_style(Tone::Busy))
    });
    let keys = match &ui.mode {
        Mode::List => "[q] quit  [enter] detail  [c] ssh  [space] mark  [/] select  [x] unmark all  [a] act  [A] all  [d/D] deep check this/all  [?] ask AI  [n] new  [r] refresh",
        Mode::Detail => "[esc] back  [q] quit  [c] ssh  [space] mark  [/] select  [x] unmark all  [a] act  [A] all  [d/D] deep check this/all  [?] ask AI  [n] new  [r] refresh",
        Mode::Menu { .. } => "[↑↓] move  [enter] choose  [letter] pick  [esc] cancel",
        Mode::Confirm(c) if c.action.requires_typed_name() => "type the pod's name  [enter] apply  [esc] cancel",
        Mode::Confirm(c) if c.preview.is_some() => "[y] apply  [v] full command  [n/esc] cancel",
        Mode::Confirm(_) => "[y] apply  [n/esc] cancel",
        Mode::FleetMenu { .. } => "[↑↓] move  [enter] choose  [letter] pick  [esc] cancel",
        Mode::FleetConfirm { .. } => "type the token to confirm  [enter] apply  [esc] cancel",
        Mode::NewPod { .. } => "[↑↓] field  [←→] change  [enter] create  [esc] cancel",
        Mode::Input { .. } => "type the value  [enter] run  [esc] cancel",
        Mode::Select { .. } => "type a selection  [enter] mark  [esc] cancel",
        Mode::Working(_) => "working… (background) — please wait",
        Mode::Result(_) => "[↑↓] scroll  ·  [any other key] dismiss",
        Mode::Ask { .. } => "type the question  [enter] ask  [tab] the other AI  [ctrl-u] clear  [esc] cancel",
        Mode::Answer(_) => "[↑↓ PgUp PgDn] scroll  [esc] close",
    };
    // A notice (`D`'s prompt and what it skips, a finished action) comes first: the question's
    // line can be long, and the notice is what must be read before the next key.
    let mut spans: Vec<Span<'static>> = Vec::new();
    match shared.notice.as_ref().filter(|n| n.at.elapsed() < NOTICE_FOR) {
        Some(n) => {
            let style = if n.error { tone_style(Tone::Bad) } else { tone_style(Tone::Busy) };
            spans.extend([Span::styled(n.text.clone(), style), Span::raw(" · ")]);
            spans.extend(asking);
            let checking = checking.trim_start_matches(" · ");
            spans.push(Span::raw(if checking.is_empty() { keys.to_string() } else { format!("{checking} · {keys}") }));
        }
        None => {
            spans.extend(asking);
            spans.push(Span::raw(format!("{}{}{checking} · {} · every {}s · {}", shared.status, spin, age, secs, keys)));
        }
    }
    Line::from(spans)
}

/// A braille spinner frame for `elapsed` (a new one every 150 ms, the UI's redraw tick).
fn spinner(elapsed: Duration) -> char {
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    FRAMES[(elapsed.as_millis() / 150) as usize % FRAMES.len()]
}

/// A centered popup rect `pct_x` × `pct_y` percent of the screen.
fn centered_rect(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(v[1])[1]
}

/// Render a navigable action menu: a title, one highlighted row per action (with its
/// key + description; destructive ones in red), and a key hint. `sel` is the cursor; `restart`
/// what a restart does to the disk of the pod(s) the menu is for (its label).
fn render_action_menu(f: &mut Frame, title: String, actions: &[(char, Action)], sel: usize, restart: ContainerReset) {
    let mut lines: Vec<Line> = vec![Line::from(title), Line::from("")];
    for (i, (k, a)) in actions.iter().enumerate() {
        let selected = i == sel;
        let base = if a.is_risky() {
            Style::default().fg(Color::Red)
        } else {
            Style::default()
        };
        let style = if selected {
            base.add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            base
        };
        let marker = if selected { "▶ " } else { "  " };
        lines.push(Line::styled(format!("{marker}[{k}] {:<10} {}", a.label(), a.desc(restart)), style));
    }
    lines.push(Line::from(""));
    lines.push(Line::styled(
        "[↑↓] move   [enter] choose   [letter] pick   [esc] cancel",
        Style::default().fg(Color::DarkGray),
    ));
    let area = centered_rect(58, 60, f.area());
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" action ")),
        area,
    );
}

fn render_menu(f: &mut Frame, shared: &Shared, ui: &Ui, sel: usize) {
    let pod = shared.pods.get(ui.selected);
    let name = pod.map(|p| ui.shown_name(&p.name)).unwrap_or_else(|| "?".into());
    render_action_menu(f, format!("Actions for {name}:"), Action::MENU, sel, shared.restart_reset(pod));
}

/// The single-pod confirm. What is about to happen — a plain summary of the action on the pod,
/// its warning, and (`v`) the full command — fills the top of the popup; the prompt (`[y]
/// apply`, or the typed-name field) sits in its own rows at the bottom, so it is always on
/// screen. Live (wave 6): Setup's preview — several long commands — wrapped past the popup's
/// bottom and took the `[y] apply` line with it; the body is now bounded by its rows.
fn render_confirm(f: &mut Frame, c: &Confirm) {
    let border = if c.action.is_destructive() { Color::Red } else { Color::Yellow };
    let area = centered_rect(80, 70, f.area());
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(format!(" confirm {} ", c.action.label()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let prompt = confirm_prompt(c);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(prompt.len() as u16)])
        .split(inner);
    let body = confirm_body(c, rows[0].width as usize, rows[0].height as usize);
    f.render_widget(Paragraph::new(body.join("\n")).wrap(Wrap { trim: false }), rows[0]);
    f.render_widget(Paragraph::new(prompt.join("\n")), rows[1]);
}

/// The confirm's bottom rows: the typed-name field, or the `[y]` keys (and `[v]` when there is
/// a full command to show). Pure.
fn confirm_prompt(c: &Confirm) -> Vec<String> {
    if c.action.requires_typed_name() {
        vec![
            "Type the pod name to confirm:".to_string(),
            format!("  > {}", c.typed),
            String::new(),
            "[enter] apply  [esc] cancel".to_string(),
        ]
    } else {
        let v = match (&c.preview, c.full) {
            (None, _) => "",
            (Some(_), false) => "   [v] full command",
            (Some(_), true) => "   [v] hide command",
        };
        vec![String::new(), format!("[y] apply{v}   [n/esc] cancel")]
    }
}

/// The confirm's body for an area `width` × `rows` (pure, so tested): what the action does to
/// the pod in plain words (`alpha: print the torch version (read-only)`, [`action_summary`])
/// and its warning, word-wrapped. The generated command is shown only on `v`
/// ([`Confirm::full`]): under `Will run:`, wrapped rather than clipped, and when it doesn't
/// fit in the rows left the last row says how many were left out. Live: the confirm opened
/// on `ssh -p 22192 -o BatchMode=yes -o StrictHostKeyChecking=no … -i /…`, clipped at the
/// width — the remote command never showed.
fn confirm_body(c: &Confirm, width: usize, rows: usize) -> Vec<String> {
    let width = width.max(2);
    let summary = action_summary(c.action, std::slice::from_ref(&c.pod_name), None, CONFIRM_NAMES_MAX);
    let mut out: Vec<String> = summary.iter().flat_map(|l| wrap_words(l, width)).collect();
    if let Some(w) = c.warning() {
        out.push(String::new());
        out.extend(wrap_words(w, width));
    }
    let Some(p) = c.preview.as_ref().filter(|_| c.full) else { return out };
    out.push(String::new());
    out.push("Will run:".to_string());
    let room = rows.saturating_sub(out.len());
    let lines: Vec<String> = p.lines().flat_map(|l| wrap_command(l, width)).collect();
    let fits = if lines.len() <= room { lines.len() } else { room.saturating_sub(1) };
    out.extend(lines[..fits].iter().cloned());
    if fits < lines.len() {
        out.push(format!("… {} more line(s) not shown", lines.len() - fits));
    }
    out
}

/// `text` word-wrapped into rows of at most `width` characters (a word longer than a row is
/// cut), so the confirm knows exactly how many rows its text takes. Pure.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut row: Vec<char> = Vec::new();
    for word in text.split(' ').filter(|w| !w.is_empty()) {
        let mut word: Vec<char> = word.chars().collect();
        if !row.is_empty() && row.len() + 1 + word.len() > width {
            rows.push(row.drain(..).collect::<String>());
        }
        if !row.is_empty() {
            row.push(' ');
        }
        while row.len() + word.len() > width {
            let take = width - row.len();
            row.extend(word.drain(..take));
            rows.push(row.drain(..).collect::<String>());
        }
        row.extend(word);
    }
    if !row.is_empty() || rows.is_empty() {
        rows.push(row.into_iter().collect());
    }
    rows
}

/// One command line as rows of at most `width` characters, cut wherever the width runs out (a
/// command has no word breaks worth keeping), each continuation indented two spaces so one
/// command doesn't read as two. Pure.
fn wrap_command(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let indent = if width > 4 { 2 } else { 0 };
    let mut rest: Vec<char> = line.chars().collect();
    let mut rows = Vec::new();
    loop {
        let pad = if rows.is_empty() { 0 } else { indent };
        let take = rest.len().min(width - pad);
        let row: String = rest.drain(..take).collect();
        rows.push(format!("{}{row}", " ".repeat(pad)));
        if rest.is_empty() {
            return rows;
        }
    }
}

/// `(count, human label)` for a scope, e.g. `(19, "all 19 pods")` / `(3, "3 marked pods")`.
fn scope_label(shared: &Shared, marked: &std::collections::HashSet<String>, scope: Scope) -> (usize, String) {
    match scope {
        Scope::All => {
            let n = shared.pods.len();
            (n, format!("all {n} pods"))
        }
        Scope::Marked => {
            let n = marked_pods(&shared.pods, marked).len();
            (n, format!("{n} marked pod{}", if n == 1 { "" } else { "s" }))
        }
    }
}

/// The shown names of the pods a scope targets, in row order.
fn scope_names(shared: &Shared, ui: &Ui, scope: Scope) -> Vec<String> {
    scope_pods_of(shared, ui, scope).iter().map(|p| ui.shown_name(&p.name)).collect()
}

/// How many marked pods a marked-set confirm names before `… and N more`: enough to see
/// what a `/` selection caught (it can mark pods scrolled off screen) without pushing the
/// token prompt out of the fixed-size popup.
const CONFIRM_NAMES_MAX: usize = 10;

fn render_fleet_menu(f: &mut Frame, shared: &Shared, ui: &Ui, scope: Scope, sel: usize) {
    let (_, who) = scope_label(shared, &ui.marked, scope);
    let actions = Action::fleet_menu(scope == Scope::Marked);
    render_action_menu(f, format!("Actions — {who}:"), &actions, sel, shared.restart_reset(scope_pods_of(shared, ui, scope)));
}

/// The listed pods `scope` covers, render-side (as [`scope_names`]).
fn scope_pods_of<'a>(shared: &'a Shared, ui: &Ui, scope: Scope) -> Vec<&'a Pod> {
    match scope {
        Scope::All => shared.pods.iter().collect(),
        Scope::Marked => marked_pods(&shared.pods, &ui.marked),
    }
}

/// The multi-pod confirm. What is about to happen — to which pods, named once (`/` can mark
/// pods scrolled off screen; up to [`CONFIRM_NAMES_MAX`] names), then what it does in plain
/// words ([`action_summary`]), its warning, and `note` (a stop's pods it will skip) — wraps in
/// the top of the popup and is clipped if it must be; the token prompt and what has been typed
/// sit in their own rows at the bottom, so they are always on screen.
fn render_fleet_confirm(f: &mut Frame, shared: &Shared, ui: &Ui, action: Action, typed: &str, scope: Scope, note: &str) {
    let n = scope_label(shared, &ui.marked, scope).0;
    let token = fleet_confirm_token_str(shared, &ui.marked, scope, action);
    // Warned as if each pod's provider wipes (the safe side); worded by the worst answer of
    // the set's backends, as its menu label.
    let reset = shared.restart_reset(scope_pods_of(shared, ui, scope));
    let warn = action.warning(true, reset).map(|w| format!("\n\n{w}")).unwrap_or_default();
    let summary = action_summary(action, &scope_names(shared, ui, scope), None, CONFIRM_NAMES_MAX).join("\n");
    // Why a marked set asks for ALL rather than its count (`state::marked_set_token`).
    let why = if scope == Scope::Marked && token == "ALL" {
        let what = if n >= shared.pods.len() { "every listed pod" } else { "a big set" };
        format!("\n\nThat is {what}: {} on it takes ALL, like the whole fleet.", action.label())
    } else {
        String::new()
    };
    let note = if note.is_empty() { String::new() } else { format!("\n\n{note}") };
    let border = if action.is_destructive() { Color::Red } else { Color::Yellow };
    let area = centered_rect(70, 60, f.area());
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(format!(" confirm {} ", action.label()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(5)])
        .split(inner);
    f.render_widget(
        Paragraph::new(format!("{summary}{warn}{why}{note}")).wrap(Wrap { trim: false }),
        rows[0],
    );
    f.render_widget(
        Paragraph::new(format!("Type {token} to confirm:\n\n  > {typed}\n\n[enter] apply  [esc] cancel")),
        rows[1],
    );
}

/// Render-side confirm token (works off `&Shared`; `fleet_confirm_token` locks and calls it).
fn fleet_confirm_token_str(
    shared: &Shared,
    marked: &std::collections::HashSet<String>,
    scope: Scope,
    action: Action,
) -> String {
    match scope {
        Scope::All => "ALL".to_string(),
        Scope::Marked => marked_set_token(action, scope_label(shared, marked, scope).0, shared.pods.len()),
    }
}

fn render_new_pod(f: &mut Frame, form: &NewPodForm) {
    let sel = form.selected();
    let dim = Style::default().fg(Color::DarkGray);
    let cur = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let label = Style::default().add_modifier(Modifier::BOLD);

    // One line per applicable field: "▶ label  ‹ value ›", selected one highlighted.
    let mut lines: Vec<Line> = Vec::new();
    for fld in form.fields() {
        let selected = fld == sel;
        let (name, value) = match fld {
            NpField::Provider => ("provider", form.provider_name().to_string()),
            NpField::CloudType => ("cloud", form.cloud_type().unwrap_or("-").to_string()),
            NpField::GpuType => {
                let price = arena_core::gpu::price_label(form.gpu_type(), form.provider_name(), form.cloud_type())
                    .map(|p| format!("  {p}"))
                    .unwrap_or_default();
                ("gpu type", format!("{}{price}", arena_core::gpu::label(form.gpu_type())))
            }
            NpField::GpuCount => ("gpus/pod", form.gpu_count.to_string()),
            NpField::Disk => ("disk", format!("{}GB", form.disk_gb)),
            NpField::Volume => (
                "volume",
                if form.volume_gb == 0 { "none".to_string() } else { format!("{}GB", form.volume_gb) },
            ),
            NpField::Pods => ("pods", format!("{} of {} free", form.count, form.free.len())),
        };
        let marker = if selected { "▶ " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(format!("{marker}{name:<9} "), if selected { cur } else { label }),
            Span::styled(
                format!("‹ {value} ›"),
                if selected { cur } else { Style::default() },
            ),
        ]));
    }

    lines.push(Line::from(""));
    // Options for the currently-selected choice field — so the GPU/provider/cloud
    // menus are visible, not guessed.
    match sel {
        NpField::Provider => {
            lines.push(Line::styled("providers:", label));
            for p in &form.providers {
                let mark = if p.name == form.provider_name() { "●" } else { "○" };
                let text = if p.available {
                    format!("  {mark} {}", p.name)
                } else {
                    format!("  {mark} {} (no api key)", p.name)
                };
                lines.push(Line::styled(
                    text,
                    if !p.available {
                        dim
                    } else if p.name == form.provider_name() {
                        cur
                    } else {
                        Style::default()
                    },
                ));
            }
        }
        NpField::CloudType => {
            lines.push(Line::styled("cloud types:", label));
            for (i, c) in form.cloud_types.iter().enumerate() {
                let mark = if i == form.cloud_idx { "●" } else { "○" };
                lines.push(Line::styled(
                    format!("  {mark} {c}"),
                    if i == form.cloud_idx { cur } else { Style::default() },
                ));
            }
        }
        NpField::GpuType => {
            lines.push(Line::styled("gpu types (← → · VRAM · ~price for this provider/cloud):", label));
            for (i, g) in form.gpu_types.iter().enumerate() {
                let mark = if i == form.gpu_idx { "●" } else { "○" };
                let price = arena_core::gpu::price_label(g, form.provider_name(), form.cloud_type())
                    .map(|p| format!("   {p}"))
                    .unwrap_or_default();
                lines.push(Line::styled(
                    format!("  {mark} {:<16}{price}", arena_core::gpu::label(g)),
                    if i == form.gpu_idx { cur } else { Style::default() },
                ));
            }
        }
        NpField::GpuCount => lines.push(Line::styled("GPUs per pod: 1–8", dim)),
        NpField::Disk => lines.push(Line::styled(
            "container disk in 50GB steps — the main storage (wiped if the pod is destroyed).",
            dim,
        )),
        NpField::Volume => lines.push(Line::styled(
            "persistent volume in 100GB steps (0 = none, the default). Survives restarts.",
            dim,
        )),
        NpField::Pods => {
            let planned = form.planned_names();
            lines.push(Line::styled("will create:", label));
            if planned.is_empty() {
                lines.push(Line::styled("  (no free machine names left)", dim));
            } else {
                for n in planned {
                    lines.push(Line::from(format!("  • {n}")));
                }
            }
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::styled(
        "[↑↓] field   [←→] change   [enter] CREATE   [esc] cancel",
        dim,
    ));

    let area = centered_rect(60, 75, f.area());
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(" add pod "),
        ),
        area,
    );
}

fn render_result(f: &mut Frame, msg: &str, scroll: u16, max: &std::cell::Cell<u16>) {
    // Grow the box for multi-line results (e.g. a per-pod fleet readout); it scrolls if
    // taller than the screen.
    let n = msg.lines().count() as u16;
    let pct_y = ((n + 4) * 100 / f.area().height.max(1) + 6).clamp(25, 85);
    let area = centered_rect(70, pct_y, f.area());
    f.render_widget(Clear, area);
    let color = if msg.contains('✗') { Color::Red } else { Color::Green };
    // Clamp scroll so you can't page past the end, and tell the keys where that is.
    let visible = area.height.saturating_sub(2);
    let max_scroll = n.saturating_sub(visible);
    max.set(max_scroll);
    let scroll = scroll.min(max_scroll);
    let footer = if n + 2 > visible { "  [↑↓] scroll · [esc] dismiss" } else { "" };
    f.render_widget(
        Paragraph::new(msg.to_string())
            .scroll((scroll, 0))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(color))
                    .title(format!(" result {footer}")),
            ),
        area,
    );
}

#[cfg(test)]
mod tests {
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
    use std::collections::HashSet;
    use std::sync::atomic::AtomicUsize;

    use arena_core::health::{Check, Status};
    use arena_core::pod::Maintenance;
    use arena_core::remote::{FakeRemote, FakeReply, RemoteCall};
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    const NOW: u64 = 1_791_460_800; // 2026-10-08T12:00:00Z

    /// A healthy 2×A4000 pod as the deep-check script prints it (login-shell chatter first).
    const DEEP_HEALTHY: &str = "\
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

    fn cfg(extra: &str) -> Config {
        Config::parse(&format!(
            "MACHINE_NAME_PREFIX=devtest\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nALLOWED_CUDA_VERSIONS=\"13.0\"\n\
             MACHINE_NAME_LIST=(\n \"alpha\"\n \"bravo\"\n \"charlie\"\n \"delta\"\n)\n{extra}"
        ))
    }

    /// `devtest-<name>`, RUNNING, at `10.0.0.<n>:<port>` when `port` is given.
    fn pod(name: &str, provider: &str, id: &str, n: u8, port: Option<u16>) -> Pod {
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

    /// A notice (`D`'s prompt) leads the footer, before a question's spinner line; one
    /// separator between each part.
    #[test]
    fn a_notice_leads_the_footer_before_the_asking_line() {
        let ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        // No start time: 0s and the first frame, whatever the clock does.
        let mut shared = Shared { ask_running: Some("asking claude about devtest-alpha…".into()), ..Default::default() };
        let text = |s: &Shared| footer_hint(s, &ui, 10).spans.iter().map(|s| s.content.to_string()).collect::<String>();
        let quiet = text(&shared);
        assert!(quiet.starts_with("⠋ asking claude about devtest-alpha… 0s · "), "{quiet}");
        shared.notify("D again to deep-check 2 running pods", false);
        let t = text(&shared);
        let rest = t.strip_prefix("D again to deep-check 2 running pods · ").unwrap_or_else(|| panic!("{t}"));
        assert!(rest.contains("asking claude about devtest-alpha… 0s · [q] quit"), "{t}");
        assert!(!t.contains("·  ·") && !t.contains("· ·"), "{t}");
    }

    /// The first refresh's early paint: the listed rows, no metrics yet, and a status that
    /// says the rest is being read — never "loading…" while the probes run.
    #[test]
    fn the_listing_is_shown_before_the_pods_are_read() {
        let shared = Mutex::new(Shared { status: "loading…".into(), ..Default::default() });
        let pods = [pod("alpha", "runpod", "a", 1, Some(22)), pod("bravo", "hetzner", "b", 2, None)];
        publish_listing(&cfg(""), &shared, &pods, &[], 1_000);
        let s = shared.lock().unwrap();
        assert_eq!(s.pods.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["devtest-alpha", "devtest-bravo"]);
        assert_eq!((s.snap.pods.len(), s.summary.pods, s.metrics.len()), (2, 2, 0));
        assert_eq!(s.status, "listed 2 pods — reading them…");
        assert!(s.last_refresh.is_none(), "still the first refresh: its full publish comes next");
    }

    #[test]
    fn spinner_moves_every_150ms() {
        assert_eq!(spinner(Duration::from_millis(0)), '⠋');
        assert_eq!(spinner(Duration::from_millis(149)), '⠋');
        assert_eq!(spinner(Duration::from_millis(150)), '⠙');
        assert_eq!(spinner(Duration::from_millis(1500)), '⠋', "wraps after ten frames");
    }

    fn verdict(p: &Pod, status: Status, check: Option<(&str, &str)>) -> PodHealth {
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

    fn ui(cfg: Config, remote: Arc<dyn Remote>) -> Ui {
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
    fn proxy_file() -> String {
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

    fn fixture_shared() -> Shared {
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

    fn screen(buf: &Buffer) -> Vec<String> {
        (0..buf.area.height).map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()).collect()
    }

    fn draw(width: u16, height: u16, shared: &Shared, ui: &Ui) -> Vec<String> {
        let mut t = Terminal::new(TestBackend::new(width, height)).unwrap();
        t.draw(|f| view(f, shared, ui, 5)).unwrap();
        screen(t.backend().buffer())
    }

    /// The row of the pod whose short name is `name`.
    fn row<'a>(lines: &'a [String], name: &str) -> &'a str {
        lines
            .iter()
            .find(|l| l.contains(&format!(" {name} ")))
            .unwrap_or_else(|| panic!("no {name} row in\n{}", lines.join("\n")))
    }

    /// The rows read the core snapshot: $/h in each provider's currency (`-` when not
    /// billing), the maintenance badge, the last deep check + its age (or `checking`), the
    /// proxy port + state; the summary bar the fleet total with € kept apart.
    /// Live (wave 6): the Setup confirm's preview — several long commands — wrapped past the
    /// popup and clipped its `[y] apply` line, and the footer said "type to confirm" for a
    /// `[y]` prompt. The prompt has its own rows now; the full command (shown on `v`) is
    /// wrapped within the rows left, the rest counted; and the footer names the keys the
    /// prompt takes.
    #[test]
    fn the_setup_confirm_always_shows_its_prompt() {
        let shared = Shared::default();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let long: Vec<String> = (0..30).map(|i| format!("ssh -p 22001 root@10.0.0.1 'step {i}: {}'", "x".repeat(200))).collect();
        let mut c = Confirm::new(Action::Setup, "devtest-alpha".into(), "id".into(), Some(long.join("\n")));
        c.full = true;
        ui.mode = Mode::Confirm(c);
        for (w, h) in [(120, 40), (80, 24), (60, 16)] {
            let all = draw(w, h, &shared, &ui).join("\n");
            assert!(all.contains("[y] apply") && all.contains("[n/esc] cancel"), "{w}x{h}:\n{all}");
            assert!(all.contains("more line(s) not shown") && all.contains("step 0:"), "{w}x{h}:\n{all}");
            assert!(!all.contains("type to confirm"), "{w}x{h}: the footer offers [y]:\n{all}");
        }
        // A short preview is shown whole.
        let mut c = Confirm::new(Action::Setup, "devtest-alpha".into(), "id".into(), Some("echo one\necho two".into()));
        c.full = true;
        ui.mode = Mode::Confirm(c);
        let all = draw(120, 40, &shared, &ui).join("\n");
        assert!(all.contains("echo two") && !all.contains("not shown") && all.contains("[y] apply"), "{all}");
        // A typed-name action keeps its field at the bottom, and its footer says so.
        ui.mode = Mode::Confirm(Confirm::new(Action::Terminate, "devtest-alpha".into(), "id".into(), None));
        let all = draw(100, 30, &shared, &ui).join("\n");
        assert!(all.contains("Type the pod name to confirm") && all.contains("type the pod's name"), "{all}");
        // The body (pure): the summary (2 rows at 40), then the commands wrapped to the width
        // (each 237–238 characters: 7 rows, 210 in all), the rows that don't fit counted.
        let mut c = Confirm::new(Action::Setup, "p".into(), "id".into(), Some(long.join("\n")));
        c.full = true;
        let body = confirm_body(&c, 40, 10);
        assert!(body.len() <= 10 && body.iter().all(|l| l.chars().count() <= 40), "{body:?}");
        assert_eq!(body[..2], ["p: provision (deploy key, SSH keys,", "repo, tools)"]);
        assert_eq!(body.last().unwrap(), "… 205 more line(s) not shown");
    }

    #[test]
    fn rows_show_cost_maintenance_health_and_proxy_from_the_snapshot() {
        let shared = fixture_shared();
        let ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let lines = draw(180, 12, &shared, &ui);
        let all = lines.join("\n");
        let header = lines.iter().find(|l| l.contains("HEALTH")).unwrap_or_else(|| panic!("{all}"));
        for col in ["$/H", "HEALTH", "PROXY", " M "] {
            assert!(header.contains(col), "{col} in {header}");
        }
        // Rows by name, whatever order the providers listed them in.
        let order: Vec<usize> = ["alpha", "bravo", "charlie", "delta"].iter().map(|n| all.find(&format!(" {n} ")).unwrap()).collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{all}");

        // (row, must show, must not show)
        let cases: &[(&str, &[&str], &[&str])] = &[
            ("alpha", &["init M", "$0.17", "pass 12m", ":9500"], &["stale"]),
            ("bravo", &["init L", "$0.25", "fail 2h", ":9501 stale"], &["init M"]),
            ("charlie", &["€0.006"], &["$0.0", "init M", "init L", "pass", "fail", ":95"]),
            ("delta", &["exit", "checking"], &["$0.13", "exit M"]),
        ];
        for (name, shows, hides) in cases {
            let r = row(&lines, name);
            for s in *shows {
                assert!(r.contains(s), "{name} shows {s:?}: {r}");
            }
            for s in *hides {
                assert!(!r.contains(s), "{name} hides {s:?}: {r}");
            }
        }
        let summary = lines.iter().find(|l| l.contains("fleet:")).unwrap_or_else(|| panic!("{all}"));
        assert!(
            summary.contains(" 4 pods · 0 GPUs · mean util - · fleet: $0.42/h across 3 billing pod(s) + €0.006/h hetzner ≈ $10.08/day + €0.13/day"),
            "{summary}"
        );
        assert!(all.contains("deep-checking 1"), "the footer counts running checks: {all}");
    }

    /// A narrow terminal — or the detail view's 55% split — keeps the essential columns
    /// whole (NAME, HEALTH, PROXY as the port coloured by state, $/H) and drops the mid
    /// columns in turn (TEMP, RTT, MEM, BRANCH, DISK) rather than letting every column be
    /// crushed: at 80–100 columns the names still read `alpha`, not `al`/`alph`.
    #[test]
    fn a_narrow_terminal_drops_mid_columns_and_keeps_names_and_health_whole() {
        let shared = fixture_shared();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        // (mode, terminal width, mid columns shown, mid columns dropped)
        let cases: &[(Mode, u16, &[&str], &[&str])] = &[
            (Mode::List, 80, &[], &["DISK", "BRANCH", "MEM", "RTT", "TEMP"]),
            (Mode::List, 92, &["DISK", "BRANCH"], &["MEM", "RTT", "TEMP"]),
            (Mode::List, 100, &["DISK", "BRANCH", "MEM"], &["RTT", "TEMP"]),
            (Mode::List, 106, &["DISK", "BRANCH", "MEM", "RTT"], &["TEMP"]),
            (Mode::List, 120, &["DISK", "BRANCH", "MEM", "RTT", "TEMP"], &[]),
            // Detail: the table gets 55% of 160 = 88 columns.
            (Mode::Detail, 160, &["DISK"], &["BRANCH", "MEM", "RTT", "TEMP"]),
        ];
        ui.selected = 3; // delta: the detail pane's title (" delta ") isn't a row we look at
        for (mode, w, shown, dropped) in cases {
            ui.mode = mode.clone();
            let lines = draw(*w, 12, &shared, &ui);
            let all = lines.join("\n");
            let header = lines.iter().find(|l| l.contains("HEALTH")).unwrap_or_else(|| panic!("{w}: {all}"));
            for col in ["NAME", "STAT", "SET", "GPU%", "$/H", "HEALTH", "PROXY"].iter().chain(shown.iter()) {
                assert!(header.contains(col), "{mode:?} {w}: {col} in {header}");
            }
            for col in *dropped {
                assert!(!header.contains(col), "{mode:?} {w}: {col} dropped from {header}");
            }
            for (name, cells) in [
                ("alpha", &["pass 12m", "$0.17", ":9500"][..]),
                ("bravo", &["fail 2h", "$0.25", ":9501"]),
                ("charlie", &["€0.006"]),
            ] {
                let r = row(&lines, name); // ` alpha ` — the whole short name
                for c in cells {
                    assert!(r.contains(c), "{mode:?} {w}: {name} shows {c:?}: {r}");
                }
            }
            assert!(!row(&lines, "bravo").contains("stale"), "{w}: compact proxy, the colour says stale");
        }
    }

    /// The RTT column: each pod's round trip from this box as last timed, coloured (green
    /// near, yellow across the Atlantic, red beyond), `-` when not timed or no answer — and
    /// the detail pane says it beside where the pod runs.
    #[test]
    fn rtt_shows_in_its_column_and_the_detail_pane() {
        let mut shared = fixture_shared();
        let by = |shared: &Shared, name: &str| shared.pods.iter().find(|p| p.name.ends_with(name)).unwrap().clone();
        let (alpha, bravo) = (by(&shared, "alpha"), by(&shared, "bravo"));
        let ep = |p: &Pod| (p.ssh_ip.clone().unwrap(), p.ssh_port.unwrap());
        let mut rtts = Rtts::default();
        rtts.record(vec![(ep(&alpha), Some(Duration::from_millis(23))), (ep(&bravo), Some(Duration::from_millis(131)))], &shared.pods, Instant::now());
        shared.rtts = rtts;
        let idx = shared.pods.iter().position(|p| p.name == alpha.name).unwrap();
        shared.pods[idx].location = Some("EUR-IS-1".into());
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let mut t = Terminal::new(TestBackend::new(180, 12)).unwrap();
        t.draw(|f| view(f, &shared, &ui, 5)).unwrap();
        let buf = t.backend().buffer().clone();
        let lines = screen(&buf);
        let header = lines.iter().find(|l| l.contains("HEALTH")).unwrap();
        let col = header[..header.find("RTT").unwrap_or_else(|| panic!("{header}"))].chars().count();
        let cell = |name: &str| {
            let y = lines.iter().position(|l| l.contains(&format!(" {name} "))).unwrap();
            let text: String = lines[y].chars().skip(col).take(5).collect();
            (text.trim().to_string(), buf[(col as u16, y as u16)].fg)
        };
        assert_eq!(cell("alpha"), ("23ms".to_string(), Color::Green));
        assert_eq!(cell("bravo"), ("131ms".to_string(), Color::Red));
        assert_eq!(cell("charlie"), ("-".to_string(), Color::DarkGray), "not timed");
        ui.mode = Mode::Detail;
        ui.selected = idx;
        let pane = draw(160, 40, &shared, &ui).join("\n");
        assert!(pane.contains("where:    EUR-IS-1  ·  rtt 23ms from this box"), "{pane}");
        ui.selected = shared.pods.iter().position(|p| p.name.ends_with("charlie")).unwrap();
        let pane = draw(160, 40, &shared, &ui).join("\n");
        assert!(pane.contains("where:    -  ·  rtt -"), "{pane}");
    }

    /// The pure column fit: always a prefix of the optional columns, so widening only adds.
    #[test]
    fn columns_that_fit_table() {
        // 3 core columns 10 wide: 30 + 2 gaps + 4 = 36.
        let core = [10, 10, 10];
        assert_eq!(table_width(&core), 36);
        // (width, how many of [5, 3, 9] fit)
        for (w, want) in [(35, 0), (36, 0), (41, 0), (42, 1), (45, 1), (46, 2), (55, 2), (56, 3), (200, 3)] {
            assert_eq!(columns_that_fit(w, &core, &[5, 3, 9]), want, "{w}");
        }
        // A later, narrower column doesn't jump the queue.
        assert_eq!(columns_that_fit(45, &core, &[9, 1]), 0);
    }

    /// Prepaid accounts read live now at `rate`/h: `(account, USD, rate)`.
    fn live_views(accounts: &[(balance::Account, f64, f64)]) -> Vec<AccountView> {
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

    /// The prepaid balance in the summary bar: at its end while it needs nothing; right at
    /// its front when an account is low (yellow) or critical (red), so a narrow terminal
    /// still shows it.
    #[test]
    fn the_summary_bar_shows_the_balance_and_leads_with_a_low_one() {
        use balance::Account;
        let mut shared = fixture_shared();
        let ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let bar = |shared: &Shared, w: u16| draw(w, 12, shared, &ui).into_iter().find(|l| l.contains(" pods · ")).unwrap();
        // Nothing observed yet: no balance at all.
        assert!(!bar(&shared, 240).contains("balance"));
        shared.balance = live_views(&[(Account::Runpod, 100.0, 0.42)]); // ≈238h
        let wide = bar(&shared, 240);
        assert!(wide.contains("€0.13/day  ·  balance: runpod $100.00 ≈9.9d"), "{wide}");
        // The colour of the lead: yellow when low, red when critical.
        let lead_colour = |shared: &Shared| {
            let mut t = Terminal::new(TestBackend::new(120, 12)).unwrap();
            t.draw(|f| view(f, shared, &ui, 5)).unwrap();
            let buf = t.backend().buffer().clone();
            let y = (0..12).find(|&y| (0..120).map(|x| buf[(x, y)].symbol()).collect::<String>().contains(" pods · ")).unwrap();
            buf[(3, y)].fg
        };
        shared.balance = live_views(&[(Account::Runpod, 20.0, 0.42)]); // ≈47h, under 72
        for w in [80, 120] {
            let narrow = bar(&shared, w);
            assert!(narrow.starts_with(" ⚠ runpod $20.00 ≈47h · 4 pods"), "{w}: {narrow}");
        }
        assert_eq!(lead_colour(&shared), Color::Yellow);
        shared.balance = live_views(&[(Account::Runpod, 5.0, 0.42)]); // ≈11h
        assert!(bar(&shared, 80).starts_with(" ‼ runpod $5.00 ≈11h · 4 pods"));
        assert_eq!(lead_colour(&shared), Color::Red, "the warning is red");
        // RunPod at $0 with nothing billing is still the alarm (at $0 RunPod stops the pods).
        shared.balance = live_views(&[(Account::Runpod, 0.0, 0.0)]);
        assert!(bar(&shared, 80).starts_with(" ‼ runpod $0.00 no burn · 4 pods"), "{}", bar(&shared, 80));
        assert_eq!(lead_colour(&shared), Color::Red);
    }

    /// Live (2026-10-15): with no Shadeform pods the bar led, in red, with `‼ shadeform balance no
    /// burn` — the opt-in overflow account, unfunded on purpose. It now sits at the end with its
    /// amount, in the bar's own colour; with a billing pod there it leads again.
    #[test]
    fn an_unfunded_overflow_account_does_not_lead_the_bar() {
        use balance::Account;
        let mut shared = fixture_shared();
        let ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let bar = |shared: &Shared, w: u16| draw(w, 12, shared, &ui).into_iter().find(|l| l.contains(" pods · ")).unwrap();
        shared.balance = live_views(&[(Account::Runpod, 100.0, 0.42), (Account::Shadeform, 0.0, 0.0)]);
        let wide = bar(&shared, 240);
        assert!(wide.starts_with(" 4 pods · "), "{wide}");
        assert!(wide.contains("  ·  balance: runpod $100.00 ≈9.9d · shadeform $0.00 unfunded"), "{wide}");
        assert!(!wide.contains('‼'), "{wide}");
        assert_eq!(state::balance_tone(&shared.balance), None);
        shared.balance = live_views(&[(Account::Shadeform, 0.0, 0.57)]);
        assert!(bar(&shared, 80).starts_with(" ‼ shadeform $0.00 <1h · 4 pods"), "{}", bar(&shared, 80));
    }

    /// A refresh times each pod's SSH endpoint alongside the probes — one TCP connect — and
    /// the next refreshes leave it alone until RTT_EVERY has passed (OpenSSH counts each such
    /// connect against the proxy box). A pod without an endpoint isn't timed.
    #[tokio::test]
    async fn refresh_times_each_endpoint_once_alongside_the_probes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut alpha = pod("alpha", "runpod", "rp1", 1, Some(port));
        alpha.ssh_ip = Some("127.0.0.1".into());
        let provider = FakeFleet {
            pods: vec![alpha.clone(), pod("bravo", "runpod", "rp2", 2, None)],
            lists: AtomicUsize::new(0),
            details: AtomicUsize::new(0),
            details_error: None,
        };
        let remote: Arc<dyn Remote> = Arc::new(FakeRemote::new());
        let shared = Mutex::new(Shared::default());
        let mut details = DetailsState { rtt: Some(Rtts::default()), ..Default::default() };
        refresh(&provider, &remote, &cfg(""), &ProbeOpts::default(), &shared, &mut details, false).await;
        let got = shared.lock().unwrap().rtts.of(&alpha);
        assert!(got.is_some_and(|d| d < RTT_WAIT), "{got:?}");
        // Exactly one connect so far (it waits in the backlog), none from the next refresh.
        async fn waiting(l: &tokio::net::TcpListener) -> bool {
            tokio::time::timeout(Duration::from_millis(200), l.accept()).await.is_ok()
        }
        assert!(waiting(&listener).await, "timed once");
        refresh(&provider, &remote, &cfg(""), &ProbeOpts::default(), &shared, &mut details, false).await;
        assert!(!waiting(&listener).await, "not again within RTT_EVERY");
        assert_eq!(shared.lock().unwrap().rtts.of(&alpha), got, "kept between rounds");
        // The dashboard's tests take no timings unless asked: no fixture endpoint is ever dialled.
        let mut quiet = DetailsState::default();
        refresh(&provider, &remote, &cfg(""), &ProbeOpts::default(), &Mutex::new(Shared::default()), &mut quiet, false).await;
        assert!(!waiting(&listener).await);
    }

    /// The balance is observed in the details round (every DETAILS_EVERY, or `r` after the
    /// minimum gap) — never on each refresh — and a live read happens at most once per FRESH
    /// however often that is: the state file remembers the last attempt.
    #[tokio::test(start_paused = true)]
    async fn balance_is_observed_on_the_details_cadence() {
        use balance::{Account, FakeBalanceApi, Live};
        let dir = tmp("balance-cadence");
        let cfg = cfg("");
        let provider = FakeFleet {
            pods: vec![pod("alpha", "runpod", "rp1", 1, None)],
            lists: AtomicUsize::new(0),
            details: AtomicUsize::new(0),
            details_error: None,
        };
        let api = Arc::new(FakeBalanceApi::new().live_ok(
            Account::Runpod,
            Live { usd: 100.0, provider_burn_per_hr: None, spend_limit_per_hr: None, under_balance: None, reduced: false, owed: None },
        ));
        let ctx = BalanceCtx::new(api.clone(), Some(dir.0.join("devtest").join(snapshot::BALANCE_FILE)), BalancePolicy::default());
        let remote: Arc<dyn Remote> = Arc::new(FakeRemote::new());
        let shared = Mutex::new(Shared::default());
        let mut details = DetailsState { balance: Some(ctx), ..Default::default() };
        let opts = ProbeOpts::default();
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, false).await;
        assert_eq!(api.live_calls(), 1, "the first round reads it");
        {
            let s = shared.lock().unwrap();
            assert_eq!((s.balance.len(), s.balance[0].balance), (1, Some(100.0)));
            // Judged against the same listing, after the details filled alpha's $0.17.
            assert_eq!(s.balance[0].listed_per_hr, Some(0.17));
        }
        // An ordinary refresh: no balance work at all.
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, false).await;
        assert_eq!(api.live_calls(), 1);
        // The next details round (and `r` after the gap) observe again — but the reading is
        // younger than FRESH, so no live call: still shown, from the state file.
        tokio::time::advance(state::DETAILS_EVERY).await;
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, false).await;
        tokio::time::advance(state::DETAILS_MIN_GAP).await;
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, true).await;
        assert_eq!(api.live_calls(), 1, "at most one live read per FRESH");
        assert_eq!(shared.lock().unwrap().balance[0].balance, Some(100.0));
    }

    /// The dashboard keeps `SSH_CONFIG_FILE` current, as `pods list` does (spec
    /// ssh-config-include): a complete listing writes it, the same listing again leaves it alone,
    /// a listing a provider failed never rewrites it (its pods would vanish), and a file that
    /// isn't arena's stays as it is, with a warning on the status line.
    #[tokio::test(start_paused = true)]
    async fn a_refresh_keeps_the_ssh_config_file_current() {
        type Listings = Vec<(&'static str, std::result::Result<Vec<Pod>, &'static str>)>;
        struct Scripted(Mutex<Listings>);
        #[async_trait::async_trait]
        impl Provider for Scripted {
            fn name(&self) -> &'static str {
                "fleet"
            }
            fn describe(&self, _spec: &PodSpec) -> String {
                String::new()
            }
            async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
                panic!("the dashboard lists per provider")
            }
            async fn list_by_provider(&self) -> Vec<(String, arena_core::Result<Vec<Pod>>)> {
                self.0.lock().unwrap().iter().map(|(p, r)| (p.to_string(), r.clone().map_err(arena_core::Error::provider))).collect()
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
        use arena_core::sshconfig::{managed_text, render_manual};
        let dir = tmp("sshconfig");
        let path = dir.0.join("arena.conf");
        let cfg = cfg(&format!("SSH_CONFIG_FILE={}\n", path.display()));
        let want = |p: &Pod| managed_text(&render_manual("devtest", "root", "/nonexistent/devtest_key", std::slice::from_ref(p)));
        let alpha = pod("alpha", "runpod", "rp1", 1, Some(22001));
        let provider = Scripted(Mutex::new(vec![("runpod", Ok(vec![alpha.clone()])), ("vast", Ok(vec![]))]));
        let remote: Arc<dyn Remote> = Arc::new(FakeRemote::new());
        let shared = Mutex::new(Shared::default());
        let mut details = DetailsState::default();
        let opts = ProbeOpts::default();
        let read = || std::fs::read_to_string(&path).unwrap();
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, false).await;
        assert_eq!(read(), want(&alpha));
        // alpha moved while vast fails to list: not rewritten.
        let moved = pod("alpha", "runpod", "rp1", 2, Some(22002));
        *provider.0.lock().unwrap() = vec![("runpod", Ok(vec![moved.clone()])), ("vast", Err("vast list HTTP 429"))];
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, false).await;
        assert_eq!(read(), want(&alpha));
        // Complete again: rewritten.
        *provider.0.lock().unwrap() = vec![("runpod", Ok(vec![moved.clone()])), ("vast", Ok(vec![]))];
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, false).await;
        assert_eq!(read(), want(&moved));
        // Someone's own file: untouched, said on the status line.
        std::fs::write(&path, "Host mine\n").unwrap();
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, false).await;
        assert_eq!(read(), "Host mine\n");
        let status = shared.lock().unwrap().status.clone();
        assert!(status.contains("ssh config not rewritten") && status.contains("isn't arena's"), "{status}");
    }

    /// A fleet whose every provider fails to list.
    struct Down;

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

    fn rp_live(usd: f64, rate: f64) -> balance::Live {
        balance::Live { usd, provider_burn_per_hr: Some(rate), spend_limit_per_hr: None, under_balance: None, reduced: false, owed: None }
    }

    /// BS8: with every provider failing to list, the balance is still observed on its
    /// cadence and shown afresh — not the last views frozen on screen.
    #[tokio::test(start_paused = true)]
    async fn balance_is_observed_even_when_every_listing_fails() {
        use balance::{Account, FakeBalanceApi};
        let dir = tmp("balance-down");
        let api = Arc::new(FakeBalanceApi::new().live_ok(Account::Runpod, rp_live(40.0, 0.5)));
        let ctx = BalanceCtx::new(api.clone(), Some(dir.0.join("devtest").join(snapshot::BALANCE_FILE)), BalancePolicy::default());
        let mut details = DetailsState { balance: Some(ctx), ..Default::default() };
        let shared = Mutex::new(Shared { balance: live_views(&[(Account::Runpod, 100.0, 0.5)]), ..Default::default() });
        let remote: Arc<dyn Remote> = Arc::new(FakeRemote::new());
        refresh(&Down, &remote, &cfg(""), &ProbeOpts::default(), &shared, &mut details, false).await;
        let s = shared.lock().unwrap();
        assert!(s.status.starts_with("list error"), "{}", s.status);
        assert_eq!((s.balance.len(), s.balance[0].balance), (1, Some(40.0)), "observed afresh, not the old $100");
        // Burns unknown (nothing listed): the provider's own rate is the burn.
        assert_eq!(s.balance[0].burn.map(|b| b.per_hr), Some(0.5));
        assert_eq!(api.live_calls(), 1);
    }

    /// BS6 for the dashboard: with no state file at all, what it applied last keeps live
    /// reads to one per FRESH — and the reading stays on screen between them.
    #[tokio::test(start_paused = true)]
    async fn without_a_state_file_live_is_still_read_once_per_fresh() {
        use balance::{Account, FakeBalanceApi};
        let provider = FakeFleet {
            pods: vec![pod("alpha", "runpod", "rp1", 1, None)],
            lists: AtomicUsize::new(0),
            details: AtomicUsize::new(0),
            details_error: None,
        };
        let api = Arc::new(FakeBalanceApi::new().live_ok(Account::Runpod, rp_live(40.0, 0.5)));
        let mut details = DetailsState { balance: Some(BalanceCtx::new(api.clone(), None, BalancePolicy::default())), ..Default::default() };
        let shared = Mutex::new(Shared::default());
        let remote: Arc<dyn Remote> = Arc::new(FakeRemote::new());
        for _ in 0..3 {
            refresh(&provider, &remote, &cfg(""), &ProbeOpts::default(), &shared, &mut details, false).await;
            tokio::time::advance(state::DETAILS_EVERY).await;
        }
        assert_eq!(api.live_calls(), 1, "one live read in three rounds");
        assert_eq!(shared.lock().unwrap().balance[0].balance, Some(40.0));
    }

    /// A provider that failed to list is said at the front of the summary bar, so it is on
    /// screen at any common width — the bar is one unwrapped line and its tail gets cut.
    #[test]
    fn a_failed_listing_is_visible_on_a_narrow_terminal() {
        let mut shared = fixture_shared();
        shared.snap.partial = vec!["vast".into()];
        let ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        for w in [80, 100, 120] {
            let lines = draw(w, 12, &shared, &ui);
            let bar = lines.iter().find(|l| l.contains("failed to list")).unwrap_or_else(|| panic!("{w}:\n{}", lines.join("\n")));
            assert!(bar.starts_with(" ⚠ vast failed to list — its pods (and their cost) are missing · 4 pods"), "{w}: {bar}");
        }
    }

    /// The detail pane spells out what the row abbreviates: the proxy state, cost per hour
    /// and day, the maintenance window + note, the last check with its worst issue and the
    /// failing check's own words.
    #[test]
    fn detail_pane_spells_out_proxy_cost_maintenance_and_health() {
        let shared = fixture_shared();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        ui.mode = Mode::Detail;
        let alpha = draw(160, 40, &shared, &ui).join("\n");
        for want in [
            "proxy:    :9500 (live)",
            "cost:     $0.17/h  ($4.08/day)",
            "maint:    maint 10-09 02:00→06:00 UTC · host upgrade",
            "health:   pass 12m  (checked 2026-10-08T11:48:00Z)",
        ] {
            assert!(alpha.contains(want), "{want:?} in\n{alpha}");
        }
        ui.selected = 1; // bravo
        let bravo = draw(160, 40, &shared, &ui).join("\n");
        for want in [
            "proxy:    :9501 stale (`arena proxy apply` re-points it)",
            "health:   fail 2h GPU error",
            "reason:   cuda: RuntimeError: Error 999",
            "maint:    -",
            "status:   init · locked (stop/restart/terminate refused)",
            "progress: -", // the snapshot lines don't push the rest out of the facts block
        ] {
            assert!(bravo.contains(want), "{want:?} in\n{bravo}");
        }
        assert!(!alpha.contains("locked"), "{alpha}");
        ui.selected = 3; // delta
        let delta = draw(160, 40, &shared, &ui).join("\n");
        assert!(delta.contains("health:   checking…") && delta.contains("cost:     -"), "{delta}");
    }

    /// A marked set's confirm names the pods — `/` can mark ones scrolled off screen.
    #[test]
    fn a_marked_set_confirm_names_its_pods_and_the_footer_shows_a_notice() {
        let mut shared = fixture_shared();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        ui.marked = ["runpod:rp1", "runpod:rp2"].map(String::from).into_iter().collect();
        ui.mode = Mode::FleetConfirm { action: Action::Terminate, typed: String::new(), scope: Scope::Marked, note: String::new() };
        let all = draw(180, 30, &shared, &ui).join("\n");
        assert!(all.contains("2 pods: alpha, bravo") && all.contains("terminate (delete)"), "{all}");
        assert!(all.contains("Type 2 to confirm"), "{all}");

        ui.mode = Mode::List;
        shared.notify("✗ a target matched no pod — nothing was done: `alpah` …", true);
        let footer = draw(180, 12, &shared, &ui).pop().unwrap();
        assert!(footer.starts_with("✗ a target matched no pod"), "{footer}");
    }

    /// `n` pods `devtest-m00…`, RunPod, published as the dashboard shows them.
    fn cohort(n: usize) -> Shared {
        let pods: Vec<Pod> = (0..n).map(|i| pod(&format!("m{i:02}"), "runpod", &format!("rp{i}"), 1, None)).collect();
        let snap = dashboard_snapshot(&pods, &[], None, &HealthCache::new(), &Naming::from_config(&cfg("")), NOW);
        let mut shared = Shared::default();
        shared.publish(snap);
        shared
    }

    /// However many pods are marked and however long the warning, the token prompt and
    /// what has been typed stay on screen: the names are capped (`… and N more`) and the
    /// prompt has its own rows at the bottom of the popup.
    #[test]
    fn a_big_marked_set_confirm_keeps_the_token_prompt_on_screen() {
        let shared = cohort(20);
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        ui.short_names = false;
        ui.marked = shared.pods.iter().map(mark_key).collect();
        for (action, typed) in [(Action::Restart, "AL"), (Action::Terminate, "ALL"), (Action::Setup, "2")] {
            ui.mode = Mode::FleetConfirm { action, typed: typed.into(), scope: Scope::Marked, note: String::new() };
            for (w, h) in [(100, 30), (80, 24)] {
                let all = draw(w, h, &shared, &ui).join("\n");
                let token = if action.is_destructive() { "ALL" } else { "20" };
                assert!(all.contains(&format!("Type {token} to confirm:")), "{action:?} {w}x{h}:\n{all}");
                assert!(all.contains(&format!("> {typed}")), "{action:?} {w}x{h}: the typed echo:\n{all}");
                assert!(all.contains("[enter] apply"), "{action:?} {w}x{h}:\n{all}");
            }
            let wide = draw(160, 40, &shared, &ui).join("\n");
            assert!(wide.contains("devtest-m00, devtest-m01") && wide.contains("… and 10 more"), "{wide}");
        }
    }

    /// `/` then Enter, through the handler the key loop calls: a valid selection replaces the
    /// marks (not adds to them) and says what it marked; a typo leaves the marks as they were
    /// and says why in red; either way back to the list.
    #[test]
    fn select_enter_replaces_the_marks_and_a_typo_keeps_them() {
        let mut shared = fixture_shared();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let before: HashSet<String> = ["runpod:rp1", "hetzner:88"].map(String::from).into();
        ui.marked = before.clone();

        ui.mode = Mode::Select { value: "alpah".into() };
        apply_select(&mut ui, &mut shared, "alpah");
        assert_eq!(ui.marked, before, "a typo marks nothing and unmarks nothing");
        let n = shared.notice.clone().unwrap();
        assert!(n.error && n.text.starts_with("✗ ") && n.text.contains("alpah"), "{n:?}");
        assert!(matches!(ui.mode, Mode::List));

        ui.mode = Mode::Select { value: "bravo".into() };
        apply_select(&mut ui, &mut shared, "bravo");
        assert_eq!(ui.marked, HashSet::from(["runpod:rp2".to_string()]), "replaced, not added to");
        let n = shared.notice.clone().unwrap();
        assert_eq!((n.text.as_str(), n.error), ("marked 1 pod: bravo", false));
        assert!(matches!(ui.mode, Mode::List));
    }

    /// `/first..last` marks the whole cohort in one line, so the marked-set bar follows what
    /// is marked: terminate/restart on it ask for `ALL` (as `A` would), not the count; a
    /// small partial set keeps the count, and so do the safe actions.
    #[test]
    fn a_selector_marking_the_whole_fleet_needs_all_to_terminate() {
        let mut shared = fixture_shared();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        apply_select(&mut ui, &mut shared, "alpha..delta");
        assert_eq!(ui.marked.len(), 4);
        // The token is read off the shared state, as the key loop reads it.
        let shared = Arc::new(Mutex::new(shared));
        let token = |ui: &Ui, action| fleet_confirm_token(&shared, &ui.marked, Scope::Marked, action);
        assert_eq!(token(&ui, Action::Terminate), "ALL");
        assert_eq!(token(&ui, Action::Restart), "ALL");
        assert_eq!(token(&ui, Action::Setup), "4");
        assert_eq!(fleet_confirm_token(&shared, &ui.marked, Scope::All, Action::Setup), "ALL");
        apply_select(&mut ui, &mut shared.lock().unwrap(), "alpha,bravo");
        assert_eq!(token(&ui, Action::Terminate), "2");
        // The popup says why it wants ALL.
        apply_select(&mut ui, &mut shared.lock().unwrap(), "alpha..delta");
        ui.mode = Mode::FleetConfirm { action: Action::Terminate, typed: String::new(), scope: Scope::Marked, note: String::new() };
        let all = draw(180, 30, &shared.lock().unwrap(), &ui).join("\n");
        assert!(all.contains("That is every listed pod: terminate on it takes ALL") && all.contains("Type ALL"), "{all}");
    }

    /// The add-pod form applies config `CREATE_EXTRA_JSON` as `arena pods create` does: carried
    /// on each create's spec, or — not one object, or a field arena sets — nothing created and
    /// the reason shown.
    #[tokio::test]
    async fn add_pod_form_creates_with_config_create_extra_json_or_not_at_all() {
        /// A RunPod v2-shaped backend that records each create's spec (rules: the real v2's).
        struct Records {
            rules: arena_core::provider::runpod_v2::RunpodV2Provider,
            specs: Mutex<Vec<PodSpec>>,
        }
        #[async_trait::async_trait]
        impl Provider for Records {
            fn name(&self) -> &'static str {
                "runpod"
            }
            fn describe(&self, _spec: &PodSpec) -> String {
                String::new()
            }
            async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
                Ok(vec![])
            }
            async fn create_pod(&self, spec: &PodSpec) -> arena_core::Result<Pod> {
                self.specs.lock().unwrap().push(spec.clone());
                Ok(Pod { id: format!("id-{}", spec.name), name: spec.name.clone(), provider: "runpod".into(), ..Default::default() })
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
            fn check_create_extra(&self, extra: &arena_core::apiextra::Extra) -> arena_core::Result<()> {
                self.rules.check_create_extra(extra)
            }
        }
        // (CREATE_EXTRA_JSON line, Ok(the extra the create carries) | Err(why nothing was created))
        let cases: [(&str, std::result::Result<Option<&str>, &str>); 4] = [
            ("", Ok(None)),
            (r#"CREATE_EXTRA_JSON='{"dataCenterIds":["EU-RO-1"]}'"#, Ok(Some(r#"{"dataCenterIds":["EU-RO-1"]}"#))),
            (r#"CREATE_EXTRA_JSON='{"name":"x"}'"#, Err("`name` is set by arena")),
            ("CREATE_EXTRA_JSON='[1]'", Err("must be a JSON object")),
        ];
        for (line, want) in cases {
            let rec = Arc::new(Records { rules: arena_core::provider::runpod_v2::RunpodV2Provider::new("k"), specs: Mutex::new(Vec::new()) });
            let p: Arc<dyn Provider> = rec.clone();
            let names = ["devtest-alpha".to_string()];
            let (msg, created) = create_pods(&p, &cfg(line), &names, Some("COMMUNITY"), "NVIDIA RTX A4000", 1, Some(40), Some(0)).await;
            let specs = rec.specs.lock().unwrap();
            match want {
                Ok(extra) => {
                    assert_eq!((msg.as_str(), created.len()), ("created 1/1", 1), "{line}");
                    let want = extra.map(|t| arena_core::apiextra::parse(t, "test").unwrap());
                    assert_eq!(specs[0].api_extra, want, "{line}");
                }
                Err(why) => {
                    assert!(msg.starts_with("created 0/1\n✗ ") && msg.contains(why), "{line}: {msg}");
                    assert!(created.is_empty() && specs.is_empty(), "{line}: nothing created");
                }
            }
        }
    }

    /// The add-pod form remembers each created pod's price in the balance state the CLI keeps
    /// (as `arena pods create` does), under the configured account's key fingerprint; an
    /// unpriced create remembers nothing.
    #[tokio::test]
    async fn add_pod_form_remembers_the_created_pods_prices_for_the_balance() {
        struct Priced;
        #[async_trait::async_trait]
        impl Provider for Priced {
            fn name(&self) -> &'static str {
                "runpod"
            }
            fn describe(&self, _spec: &PodSpec) -> String {
                String::new()
            }
            async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
                Ok(vec![])
            }
            async fn create_pod(&self, spec: &PodSpec) -> arena_core::Result<Pod> {
                // bravo's create answers with no price (RunPod v1's REST create).
                let cost_per_hr = (spec.name != "devtest-bravo").then_some(0.27);
                Ok(Pod { id: format!("id-{}", spec.name), name: spec.name.clone(), provider: "runpod".into(), cost_per_hr, ..Default::default() })
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
        let dir = tmp("tui-prices");
        // A dummy key: it only names the account (its fingerprint) — nothing is called.
        let conf = cfg(&format!("ARENA_STATE_DIR={}\nRUNPOD_API_KEY=not-a-real-key\n", dir.0.display()));
        let p: Arc<dyn Provider> = Arc::new(Priced);
        let names = ["devtest-alpha".to_string(), "devtest-bravo".to_string()];
        let (msg, created) = create_pods(&p, &conf, &names, Some("COMMUNITY"), "NVIDIA RTX A4000", 1, Some(40), Some(0)).await;
        assert_eq!((msg.as_str(), created.len()), ("created 2/2", 2));
        let (state, warning) = balance::load(&dir.0.join("devtest").join(snapshot::BALANCE_FILE));
        assert_eq!(warning, None);
        let runpod = &state.accounts[&balance::Account::Runpod];
        assert_eq!(runpod.key_fp, balance::key_fp("not-a-real-key"));
        let prices: Vec<(&str, f64)> = runpod.prices.iter().map(|(id, (usd, _))| (id.as_str(), *usd)).collect();
        assert_eq!(prices, [("id-devtest-alpha", 0.27)]);
    }

    /// The dashboard refuses restart/stop/terminate on a pod the listing reports locked —
    /// before any call (this fleet panics on one) — saying how to unlock it; a refusal the
    /// listing didn't predict (the provider's `Locked` error) reads the same way.
    #[tokio::test]
    async fn lifecycle_actions_on_a_locked_pod_are_refused_with_the_unlock_hint() {
        let fleet = FakeFleet { pods: vec![], lists: AtomicUsize::new(0), details: AtomicUsize::new(0), details_error: None };
        let remote = FakeRemote::new();
        let cfg = cfg("");
        let mut bravo = pod("bravo", "runpod", "rp2", 2, Some(22002));
        bravo.locked = Some(true);
        for action in [Action::Restart, Action::Stop, Action::Terminate] {
            let out = execute(&fleet, &remote, &cfg, action, &bravo, None).await;
            assert_eq!(
                out,
                format!("✗ {} devtest-bravo refused: devtest-bravo is locked — `arena pods unlock devtest-bravo` first", action.label())
            );
        }

        /// Locked since it was listed: the provider refuses, tagged `Locked`.
        struct Refuses;
        #[async_trait::async_trait]
        impl Provider for Refuses {
            fn name(&self) -> &'static str {
                "runpod"
            }
            fn describe(&self, _spec: &PodSpec) -> String {
                String::new()
            }
            async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
                Ok(vec![])
            }
            async fn create_pod(&self, _spec: &PodSpec) -> arena_core::Result<Pod> {
                unimplemented!()
            }
            /// The recorded live refusal, through the v2 backend's own classification.
            async fn stop_pod(&self, _id: &str) -> arena_core::Result<()> {
                Err(arena_core::provider::runpod_v2::recorded::locked_refusal("stop pod"))
            }
            async fn restart_pod(&self, _id: &str) -> arena_core::Result<()> {
                Ok(())
            }
            async fn terminate_pod(&self, _id: &str) -> arena_core::Result<()> {
                Ok(())
            }
        }
        bravo.locked = Some(false);
        let out = execute(&Refuses, &remote, &cfg, Action::Stop, &bravo, None).await;
        assert_eq!(out, "✗ stop devtest-bravo failed: devtest-bravo is locked — `arena pods unlock devtest-bravo` first");
        assert_eq!(execute(&Refuses, &remote, &cfg, Action::Terminate, &bravo, None).await, "✓ terminated devtest-bravo");
    }

    /// The result line says only what the backend knows: "reset to the image" after a restart
    /// that resets (RunPod v2), "may have been" after one that may not (RunPod v1, live
    /// 2026-10-09: marker files survived), nothing for a kept disk — and a Hetzner stop that
    /// had to be forced says so.
    #[tokio::test]
    async fn restart_and_stop_lines_say_only_what_happened() {
        use arena_core::provider::StopOutcome;
        struct Says(ContainerReset, StopOutcome);
        #[async_trait::async_trait]
        impl Provider for Says {
            fn name(&self) -> &'static str {
                "runpod"
            }
            fn describe(&self, _spec: &PodSpec) -> String {
                String::new()
            }
            async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
                Ok(vec![])
            }
            async fn create_pod(&self, _spec: &PodSpec) -> arena_core::Result<Pod> {
                unimplemented!()
            }
            async fn stop_pod(&self, _id: &str) -> arena_core::Result<()> {
                unimplemented!("the dashboard asks for the stop's outcome")
            }
            async fn stop_pod_outcome(&self, _id: &str) -> arena_core::Result<StopOutcome> {
                Ok(self.1)
            }
            async fn restart_pod(&self, _id: &str) -> arena_core::Result<()> {
                Ok(())
            }
            async fn terminate_pod(&self, _id: &str) -> arena_core::Result<()> {
                unimplemented!()
            }
            fn restart_resets_container(&self, _pod: &Pod) -> ContainerReset {
                self.0
            }
        }
        let (remote, cfg) = (FakeRemote::new(), cfg(""));
        let bravo = pod("bravo", "runpod", "rp2", 2, Some(22002));
        for (reset, want) in [
            (ContainerReset::Yes, "✓ restarted devtest-bravo — reset to the image: run setup (p) once it's up"),
            (ContainerReset::Unknown, "✓ restarted devtest-bravo — it may have been reset to the image: run setup (p) once it's up"),
            (ContainerReset::No, "✓ restarted devtest-bravo"),
        ] {
            let p = Says(reset, StopOutcome::Stopped);
            assert_eq!(execute(&p, &remote, &cfg, Action::Restart, &bravo, None).await, want, "{reset:?}");
        }
        let plain = Says(ContainerReset::No, StopOutcome::Stopped);
        assert_eq!(execute(&plain, &remote, &cfg, Action::Stop, &bravo, None).await, "✓ stopped devtest-bravo");
        let forced = Says(ContainerReset::No, StopOutcome::ForcedOff { waited: Duration::from_secs(90) });
        let out = execute(&forced, &remote, &cfg, Action::Stop, &bravo, None).await;
        assert!(out.starts_with("✓ stopped devtest-bravo — ⚠ it didn't shut down within 90s, so it was powered off"), "{out}");
    }

    /// A pod-side fleet for [`refresh`]: lists runpod (two pods) and a vast that 429s,
    /// counts list and details calls, and panics on anything that would change the fleet.
    struct FakeFleet {
        pods: Vec<Pod>,
        lists: AtomicUsize,
        details: AtomicUsize,
        /// Make the details query fail after filling rp1 (a GraphQL reply with data *and*
        /// `errors`, or one backend of the fleet failing).
        details_error: Option<&'static str>,
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
    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tmp(tag: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("arena-tui-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }

    /// One refresh = the core snapshot over the per-provider listing, the local proxy file
    /// and the health cache (a failed provider named, not silently missing); one list call
    /// per refresh as before, the details query only on its slower cadence or on `r` (and
    /// its answer kept on the rows in between); over SSH only the metrics probe.
    #[tokio::test(start_paused = true)]
    async fn a_refresh_is_the_core_snapshot_with_details_on_a_slower_cadence() {
        let dir = tmp("refresh");
        let proxy_path = dir.0.join("proxy.conf");
        std::fs::write(&proxy_path, proxy_file()).unwrap();
        let cfg = cfg(&format!(
            "SSH_PROXY_HOST=proxy.example.com\nSSH_PROXY_NGINX_CONFIG_PATH={}\nARENA_STATE_DIR={}\n",
            proxy_path.display(),
            dir.0.join("state").display()
        ));
        let alpha = pod("alpha", "runpod", "rp1", 1, Some(22001));
        let bravo = pod("bravo", "runpod", "rp2", 2, Some(22002));
        let cache = health_cache_path(&cfg).unwrap().unwrap();
        assert!(cache.starts_with(&dir.0), "never the real state dir: {}", cache.display());
        snapshot::record_health(&cache, &[verdict(&alpha, Status::Pass, None)], None, snapshot::unix_now() - 600).unwrap();

        let provider = FakeFleet {
            pods: vec![bravo, alpha],
            lists: AtomicUsize::new(0),
            details: AtomicUsize::new(0),
            details_error: None,
        };
        let fake = Arc::new(FakeRemote::new());
        let remote: Arc<dyn Remote> = fake.clone();
        let shared = Mutex::new(Shared::default());
        let mut details = DetailsState::default();
        let opts = ProbeOpts::default();
        let calls = || (provider.lists.load(Ordering::SeqCst), provider.details.load(Ordering::SeqCst));
        let alpha_row = |shared: &Mutex<Shared>| {
            let s = shared.lock().unwrap();
            let a = s.snap.pods.iter().find(|p| p.pod.id == "rp1").unwrap().clone();
            (fleet::price_label(&a.pod), state::has_maintenance(&a.pod))
        };

        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, false).await;
        {
            let s = shared.lock().unwrap();
            let names: Vec<&str> = s.pods.iter().map(|p| p.name.as_str()).collect();
            assert_eq!(names, ["devtest-alpha", "devtest-bravo"]);
            assert_eq!(s.snap.pods.len(), 2, "rows and snapshot stay in step");
            let (a, b) = (&s.snap.pods[0], &s.snap.pods[1]);
            assert_eq!((a.proxy, a.proxy_port), (snapshot::ProxyState::Live, Some(9500)));
            assert_eq!((b.proxy, b.proxy_port), (snapshot::ProxyState::Stale, Some(9501)));
            assert_eq!(health_cell(a.health.as_ref(), false, s.snap.generated_at).0, "pass 10m");
            assert!(b.health.is_none());
            assert_eq!(s.snap.partial, ["vast"]);
            let summary = summary_text(&s.summary, &s.snap.cost, &s.snap.partial, &[], None);
            assert!(summary.contains("fleet: $0.17/h across 2 billing pod(s) (1 unpriced)"), "{summary}");
            assert!(summary.starts_with(" ⚠ vast failed to list — its pods (and their cost) are missing"), "{summary}");
        }
        assert_eq!(alpha_row(&shared), ("$0.17".to_string(), true));
        assert_eq!(calls(), (1, 1), "the first refresh fills the details");
        // Over SSH: one metrics probe per pod, nothing else.
        let probes: Vec<String> = fake.calls().iter().map(|c| c.host().to_string()).collect();
        assert_eq!(probes.len(), 2);
        assert!(fake.calls().iter().all(|c| matches!(c, RemoteCall::Exec { timeout: Some(t), .. } if *t == arena_core::remote::PROBE_TIMEOUT)));

        // Right away again: one more list, no details query — the details stay on the row.
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, false).await;
        assert_eq!(calls(), (2, 1));
        assert_eq!(alpha_row(&shared), ("$0.17".to_string(), true));
        // `r` within the minimum gap: still none; once it has passed: one.
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, true).await;
        assert_eq!(calls(), (3, 1));
        tokio::time::advance(state::DETAILS_MIN_GAP).await;
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, true).await;
        assert_eq!(calls(), (4, 2));
        // Unprompted, once a DETAILS_EVERY has gone by.
        tokio::time::advance(state::DETAILS_EVERY).await;
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, false).await;
        assert_eq!(calls(), (5, 3));
    }

    /// A details query that errs after filling some pods (GraphQL data + `errors`, one
    /// backend failing) still shows what it filled — as `pods list` and `arena snapshot`
    /// do, working on the pods in place — and the footer says the details are incomplete.
    #[tokio::test(start_paused = true)]
    async fn details_filled_before_a_query_error_still_show() {
        let cfg = cfg("");
        let provider = FakeFleet {
            pods: vec![pod("alpha", "runpod", "rp1", 1, None), pod("bravo", "runpod", "rp2", 2, None)],
            lists: AtomicUsize::new(0),
            details: AtomicUsize::new(0),
            details_error: Some("pod details: some field errored"),
        };
        let remote: Arc<dyn Remote> = Arc::new(FakeRemote::new());
        let shared = Mutex::new(Shared::default());
        let mut details = DetailsState::default();
        refresh(&provider, &remote, &cfg, &ProbeOpts::default(), &shared, &mut details, false).await;
        let s = shared.lock().unwrap();
        let alpha = &s.snap.pods.iter().find(|p| p.pod.id == "rp1").unwrap().pod;
        assert_eq!((fleet::price_label(alpha), state::has_maintenance(alpha)), ("$0.17".to_string(), true));
        assert_eq!(alpha.gpu_type.as_deref(), Some("RTX A4000"));
        assert!(s.status.contains("⚠ pod details incomplete: ") && s.status.contains("some field errored"), "{}", s.status);
        // The fleet total counts the price it got.
        assert!(summary_text(&s.summary, &s.snap.cost, &s.snap.partial, &[], None).contains("$0.17/h"));
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
        let want = RemoteCall::Exec {
            host: String::new(),
            cmd: deep_check_command(Some("arena-env")),
            timeout: Some(DEEP_CHECK_TIMEOUT),
        };
        for host in ["10.0.0.1:22001", "10.0.0.2:22002"] {
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
            assert!(s.pods.iter().all(|p| health_cell(None, s.deep.is_running(p), NOW).0 == "checking"));
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

    /// Every action's SSH goes through `Remote` with its budget, so a wedged pod ends the
    /// action with `timed out after …` instead of a `working…` modal that never closes.
    #[tokio::test(start_paused = true)]
    async fn actions_go_through_remote_with_their_budgets() {
        let host = "10.0.0.1:22001";
        let wrong_origin = || {
            FakeReply::exit(5, "")
                .with_stdout("WRONG_ORIGIN nickypro/arena-sandbox-materials https://github.com/ARENA-education/ARENA_materials\n")
        };
        let cfg = cfg("ARENA_REPO_OWNER=nickypro\nARENA_REPO_NAME=arena-sandbox-materials\nGIT_SSH_KEY_LOCAL=/nonexistent/deploy_key\n");
        let alpha = pod("alpha", "runpod", "rp1", 1, Some(22001));
        let budgets = |fake: &FakeRemote| -> Vec<Option<Duration>> {
            fake.calls()
                .into_iter()
                .map(|c| match c {
                    RemoteCall::Exec { timeout, .. } | RemoteCall::Copy { timeout, .. } => timeout,
                })
                .collect()
        };
        // (what, the scripted replies, the outcome line starts with, budgets used)
        let setup = arena_core::setup::SetupTimeouts::default();
        type Case = (&'static str, Vec<FakeReply>, &'static str, Vec<Duration>);
        let cases: Vec<Case> = vec![
            ("test", vec![FakeReply::stdout("noise\n2.9.0+cu130\n")], "✓ devtest-alpha torch: 2.9.0+cu130", vec![TEST_TIMEOUT]),
            ("test-hung", vec![FakeReply::hang()], "✗ devtest-alpha torch failed: timed out after 90s", vec![TEST_TIMEOUT]),
            ("run-hung", vec![FakeReply::hang()], "✗ devtest-alpha run failed: timed out after 1800s", vec![RUN_TIMEOUT]),
            ("backup", vec![FakeReply::exit(1, "rejected")], "✗ backup devtest-alpha (exit 1): rejected", vec![BACKUP_TIMEOUT]),
            // The branch it's on backed up first, then switched.
            ("set-branch", vec![FakeReply::stdout("NO_CHANGES autocommit-x\n"), FakeReply::ok()], "✓ devtest-alpha → w1d2", vec![BACKUP_TIMEOUT, BRANCH_TIMEOUT]),
            // …and not switched when that push fails (the CLI copies its files first; the TUI can't).
            (
                "set-branch",
                vec![FakeReply::exit(4, "remote: error: GH013"), FakeReply::ok()],
                "✗ devtest-alpha not switched to w1d2: the branch it's on wasn't pushed first (✗ backup devtest-alpha (exit 4): \
                 remote: error: GH013) — `arena pods set-branch w1d2 devtest-alpha` copies its files, then switches",
                vec![BACKUP_TIMEOUT],
            ),
            // A pod whose origin isn't the configured repo: refused by the backup (exit 5), said with
            // the fix — and not switched, the CLI wouldn't either.
            (
                "backup",
                vec![wrong_origin()],
                "✗ backup devtest-alpha: origin is https://github.com/ARENA-education/ARENA_materials, not the configured \
                 nickypro/arena-sandbox-materials — not backed up to git; run `arena pods setup devtest-alpha`",
                vec![BACKUP_TIMEOUT],
            ),
            // A pod mid-merge (review F5): not switched, said as what is in progress and how to end
            // it, `--hard` named as the destructive way — never "the CLI copies its files, then
            // switches": the CLI's gentle switch refuses it too.
            (
                "set-branch",
                vec![FakeReply::exit(6, "").with_stdout("BRANCH autocommit-x\nBUSY autocommit-x merge unmerged\n"), FakeReply::ok()],
                "✗ devtest-alpha not switched to w1d2: a merge is in progress on autocommit-x — not committed or switched (a switch \
                 now would end the merge); resolve the conflicts and `git add` them, then `git commit` — or abort it (`git merge \
                 --abort`) on the pod, then switch again. Or, destructively: `arena pods set-branch w1d2 devtest-alpha --hard` \
                 copies its files first, then discards its uncommitted changes to tracked files (the merge with them)",
                vec![BACKUP_TIMEOUT],
            ),
            (
                "set-branch",
                vec![FakeReply::exit(6, "").with_stdout("BRANCH autocommit-x\nBUSY autocommit-x conflicts\n"), FakeReply::ok()],
                "✗ devtest-alpha not switched to w1d2: autocommit-x has unresolved conflicts — not committed or switched; resolve \
                 them and `git add` them, or undo them (`git reset --merge`) on the pod, then switch again. Or, destructively: \
                 `arena pods set-branch w1d2 devtest-alpha --hard` copies its files first, then discards its uncommitted changes to \
                 tracked files (the unresolved conflicts with them)",
                vec![BACKUP_TIMEOUT],
            ),
            // Review pr4: `git am` (HEAD attached) — a gentle switch would carry it onto the other
            // branch, half-done, not end it.
            (
                "set-branch",
                vec![FakeReply::exit(6, "").with_stdout("BRANCH autocommit-x\nBUSY autocommit-x am\n"), FakeReply::ok()],
                "✗ devtest-alpha not switched to w1d2: a `git am` is in progress on autocommit-x — not committed or switched (a \
                 switch now would carry the `git am`, half-done, onto the other branch); finish it (`git am --continue`) or abort it \
                 (`git am --abort`) on the pod, then switch again. Or, destructively: `arena pods set-branch w1d2 devtest-alpha \
                 --hard` copies its files first, then discards its uncommitted changes to tracked files (the `git am` with them)",
                vec![BACKUP_TIMEOUT],
            ),
            (
                "set-branch",
                vec![wrong_origin(), FakeReply::ok()],
                "✗ devtest-alpha not switched to w1d2: origin is https://github.com/ARENA-education/ARENA_materials, not the \
                 configured nickypro/arena-sandbox-materials — not backed up to git; run `arena pods setup devtest-alpha`",
                vec![BACKUP_TIMEOUT],
            ),
            ("setup", vec![FakeReply::ok(), FakeReply::ok(), FakeReply::ok()], "✓ set up devtest-alpha", vec![setup.copy, setup.relocate, setup.config]),
            (
                "setup-hung",
                vec![FakeReply::ok(), FakeReply::ok(), FakeReply::hang()],
                "✗ setup devtest-alpha: timed out at repo + keys config after 300s",
                vec![setup.copy, setup.relocate, setup.config],
            ),
            // What the relocation says reaches the line: the repo did NOT go onto the volume.
            (
                "setup-warns",
                vec![
                    FakeReply::ok(),
                    FakeReply::stdout("arena-warning: repo not moved onto the /workspace volume: in use (pid 42) - re-run setup when it is idle\n"),
                    FakeReply::ok(),
                ],
                "✓ set up devtest-alpha (warning: repo onto volume: repo not moved onto the /workspace volume: in use (pid 42) - re-run setup when it is idle)",
                vec![setup.copy, setup.relocate, setup.config],
            ),
            // …and so does what the config step says.
            (
                "setup-config-warns",
                vec![FakeReply::ok(), FakeReply::ok(), FakeReply::stdout("arena-warning: another setup is still moving the repo onto the volume - the update ran anyway\n")],
                "✓ set up devtest-alpha (warning: repo + keys config: another setup is still moving the repo onto the volume - the update ran anyway)",
                vec![setup.copy, setup.relocate, setup.config],
            ),
        ];
        for (what, replies, want, budget) in cases {
            let fake = FakeRemote::new();
            fake.script(host, replies);
            let line = match what {
                "test" | "test-hung" => run_ssh_oneline(&fake, &cfg, &alpha, TORCH_TEST_CMD, "torch", TEST_TIMEOUT).await,
                "run-hung" => run_ssh_output(&fake, &cfg, &alpha, "sleep 99999").await,
                "backup" => run_backup(&fake, &cfg, &alpha).await,
                "set-branch" => run_set_branch(&fake, &cfg, &alpha, "w1d2").await,
                _ => run_setup(&fake, &cfg, &alpha).await,
            };
            assert_eq!(line, want, "{what}");
            assert_eq!(budgets(&fake), budget.into_iter().map(Some).collect::<Vec<_>>(), "{what}");
        }
        // The backup it runs carries the configured repo for that check.
        let fake = FakeRemote::new();
        run_backup(&fake, &cfg, &alpha).await;
        let sent: Vec<String> = fake.calls().into_iter().filter_map(|c| match c { RemoteCall::Exec { cmd, .. } => Some(cmd), _ => None }).collect();
        assert!(sent.len() == 1 && sent[0].ends_with(" 'nickypro/arena-sandbox-materials'"), "{sent:?}");
        // No endpoint: said, and nothing is attempted.
        let fake = FakeRemote::new();
        let charlie = pod("charlie", "runpod", "rp3", 3, None);
        assert!(run_backup(&fake, &cfg, &charlie).await.starts_with("✗ devtest-charlie: "));
        // RR10: the pod's backup lock held by another backup (a pull, the cron, the reaper):
        // busy, said, nothing run on the pod.
        let dir = std::env::temp_dir().join(format!("arena-tui-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let locked = self::cfg(&format!(
            "ARENA_REPO_OWNER=nickypro\nARENA_REPO_NAME=arena-sandbox-materials\nGIT_SSH_KEY_LOCAL=/nonexistent/deploy_key\nLOCAL_BACKUP_DIR={}\n",
            dir.display()
        ));
        let held = arena_core::pull::try_lock(arena_core::pull::open_pod_lock(&dir.display().to_string(), &alpha.name).unwrap()).unwrap().expect("free");
        let quiet = FakeRemote::new();
        let line = run_backup(&quiet, &locked, &alpha).await;
        assert!(line.starts_with("⊘ ") && line.contains("busy"), "{line}");
        assert!(quiet.calls().is_empty(), "{:?}", quiet.calls());
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(fake.calls().is_empty());
    }
    /// Review ai-review claude #6, checked: does the TUI's set-branch honour `DISCARD_MAX_GB`, the
    /// most a `--hard` save copies? It has nothing to honour: the TUI only ever switches gently —
    /// `--hard` is the CLI's — and a gentle switch saves nothing. Pinned: the switch it sends is
    /// `checkout_command(…, false)`, holding no save, the same whatever the limit says (a bad value
    /// included), and the same as the limited command with any limit. Were the TUI ever to switch
    /// with `--hard`, this fails, and the limit would have to be passed as the CLI passes it.
    #[tokio::test]
    async fn the_tuis_set_branch_is_gentle_so_no_save_limit_applies() {
        use arena_core::backup::{checkout_command, checkout_command_limited, discard_max_kb};
        let alpha = pod("alpha", "runpod", "rp1", 1, Some(22001));
        for extra in ["", "DISCARD_MAX_GB=0.001\n", "DISCARD_MAX_GB=lots\n"] {
            let cfg = cfg(&format!("GIT_SSH_KEY_REMOTE=/k\n{extra}"));
            let fake = FakeRemote::new();
            fake.script("10.0.0.1:22001", vec![FakeReply::stdout("NO_CHANGES autocommit-x\n"), FakeReply::ok()]);
            assert_eq!(run_set_branch(&fake, &cfg, &alpha, "w1d2").await, "✓ devtest-alpha → w1d2", "{extra:?}");
            let sent: Vec<String> = fake.calls().into_iter().filter_map(|c| match c { RemoteCall::Exec { cmd, .. } => Some(cmd), _ => None }).collect();
            let gentle = checkout_command("/root/ARENA_materials", "w1d2", Some("/k"), false);
            assert_eq!(sent.get(1), Some(&gentle), "{extra:?}");
            assert!(!gentle.contains("discarded_save"));
            if let Ok(kb) = discard_max_kb(&cfg) {
                assert_eq!(checkout_command_limited("/root/ARENA_materials", "w1d2", Some("/k"), false, kb), gentle, "{extra:?}");
            }
        }
    }

    /// A provider for the action tests that never answers a fleet call it isn't asked.
    fn quiet_fleet() -> FakeFleet {
        FakeFleet { pods: vec![], lists: AtomicUsize::new(0), details: AtomicUsize::new(0), details_error: None }
    }

    /// Esc steps back and never quits: from the detail view to the list (the marks kept), and
    /// on the list it clears the marks, else does nothing; `q` quits, and the footer says so.
    /// Live: Esc on the list quit, so the Esc-Esc that backs out of a popup closed the dashboard.
    #[test]
    fn esc_steps_back_and_never_quits_and_q_does() {
        let shared = fixture_shared();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let marks: HashSet<String> = ["runpod:rp1".to_string()].into();
        ui.marked = marks.clone();
        ui.mode = Mode::Detail;
        assert_eq!(list_key(&mut ui, KeyCode::Esc, 4), ListKey::Done);
        assert!(matches!(ui.mode, Mode::List) && ui.marked == marks, "detail → list, the marks kept");
        assert_eq!(list_key(&mut ui, KeyCode::Esc, 4), ListKey::Done);
        assert!(matches!(ui.mode, Mode::List) && ui.marked.is_empty(), "on the list: the marks cleared");
        assert_eq!(list_key(&mut ui, KeyCode::Esc, 4), ListKey::Done, "nothing marked: nothing happens");
        assert!(matches!(ui.mode, Mode::List));
        assert_eq!(list_key(&mut ui, KeyCode::Char('q'), 4), ListKey::Quit);
        ui.mode = Mode::Detail;
        assert_eq!(list_key(&mut ui, KeyCode::Char('q'), 4), ListKey::Quit);
        // The keys that moved out of the loop with it still do what they did.
        ui.mode = Mode::List;
        ui.selected = 0;
        list_key(&mut ui, KeyCode::Down, 4);
        list_key(&mut ui, KeyCode::Char('j'), 4);
        list_key(&mut ui, KeyCode::Up, 4);
        assert_eq!(ui.selected, 1);
        list_key(&mut ui, KeyCode::Enter, 4);
        assert!(matches!(ui.mode, Mode::Detail));
        ui.mode = Mode::List;
        list_key(&mut ui, KeyCode::Char('a'), 4);
        assert!(matches!(ui.mode, Mode::Menu { sel: 0 }));
        ui.mode = Mode::List;
        ui.marked = marks.clone();
        list_key(&mut ui, KeyCode::Char('a'), 4);
        assert!(matches!(ui.mode, Mode::FleetMenu { scope: Scope::Marked, .. }));
        ui.mode = Mode::List;
        list_key(&mut ui, KeyCode::Char('A'), 4);
        assert!(matches!(ui.mode, Mode::FleetMenu { scope: Scope::All, .. }));
        ui.mode = Mode::List;
        list_key(&mut ui, KeyCode::Char('x'), 4);
        assert!(ui.marked.is_empty());
        assert_eq!(list_key(&mut ui, KeyCode::Char('A'), 0), ListKey::Done);
        assert!(matches!(ui.mode, Mode::List), "no pods: no fleet menu");
        list_key(&mut ui, KeyCode::Char('/'), 4);
        assert!(matches!(ui.mode, Mode::Select { .. }));
        ui.mode = Mode::List;
        assert_eq!(list_key(&mut ui, KeyCode::Char('r'), 4), ListKey::Other);
        // The footer says how to quit — first, as the line is cut at the terminal's width.
        for mode in [Mode::List, Mode::Detail] {
            ui.mode = mode;
            let footer = draw(80, 12, &shared, &ui).pop().unwrap();
            assert!(footer.contains("[q] quit"), "{footer}");
        }
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

    /// Live ASK F3: both popups — the `?` answer and the older result modal — scroll no further
    /// than their text's end, so after PgDn past it one PgUp moves the text at once (it took
    /// 11 after 10 extra PgDn). The offset is clamped by `scroll_by` to what the popup last
    /// drawn can scroll; one that fits doesn't scroll at all.
    #[test]
    fn a_popup_scrolls_no_further_than_its_text() {
        assert_eq!([scroll_by(0, 5, -1), scroll_by(3, 5, 10), scroll_by(5, 5, -10), scroll_by(0, 0, 10), scroll_by(2, 5, 1)], [0, 5, 0, 0, 3]);
        assert_eq!(scroll_by(40, 5, -1), 4, "past the end (the window grew): the first key back moves");
        assert_eq!(scroll_by(u16::MAX, u16::MAX, 10), u16::MAX);
        let shared = fixture_shared();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let none = KeyModifiers::NONE;
        let long: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let shared_arc = Arc::new(Mutex::new(fixture_shared()));
        let answer = || Mode::Answer(ask::Answer { title: "t".into(), text: long.clone() });
        for (popup, mode) in [("answer", answer()), ("result", Mode::Result(long.clone()))] {
            ui.mode = mode;
            ui.result_scroll = 0;
            let press = |ui: &mut Ui, code: KeyCode| {
                if popup == "answer" {
                    assert!(ask::key(ui, &shared_arc, code, none));
                } else {
                    assert!(result_key(ui, code));
                }
            };
            draw(120, 40, &shared, &ui);
            let max = ui.result_max.get();
            assert!(max > 50 && max < 100, "{popup}: {max}");
            for _ in 0..30 {
                press(&mut ui, KeyCode::PageDown);
            }
            assert_eq!(ui.result_scroll, max, "{popup}: PgDn stops at the end");
            assert!(draw(120, 40, &shared, &ui).join("\n").contains("line 99"), "{popup}");
            press(&mut ui, KeyCode::Up);
            assert_eq!(ui.result_scroll, max - 1, "{popup}: one Up moves at once");
            press(&mut ui, KeyCode::PageUp);
            assert_eq!(ui.result_scroll, max - 11, "{popup}");
            for _ in 0..30 {
                press(&mut ui, KeyCode::PageUp);
            }
            assert_eq!(ui.result_scroll, 0, "{popup}");
            press(&mut ui, KeyCode::Down);
            assert_eq!(ui.result_scroll, 1, "{popup}");
        }
        // A result that fits: no scrolling, and any other key dismisses it (left to the loop).
        ui.mode = Mode::Result("✓ done".into());
        ui.result_scroll = 0;
        draw(120, 40, &shared, &ui);
        assert!(result_key(&mut ui, KeyCode::PageDown) && ui.result_scroll == 0);
        assert!(!result_key(&mut ui, KeyCode::Esc));
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
            assert!(footer.contains("[?] ask AI"), "{footer}");
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

    /// `run` shows what the command printed — every line, not the last one (live: `nvidia-smi
    /// -L; hostname` read as just the hostname) — with stderr when it fails, as a terminal would
    /// show it, and past RUN_OUTPUT_LINES the earlier lines counted; per pod for several pods.
    /// `test` stays its one-liner (`actions_go_through_remote_with_their_budgets`).
    #[tokio::test]
    async fn run_shows_the_whole_output_per_pod() {
        let cfg = cfg("");
        let alpha = pod("alpha", "runpod", "rp1", 1, Some(22001));
        let hundred: String = (1..=100).map(|i| format!("line {i}\n")).collect();
        // (the reply, the outcome)
        let cases: Vec<(FakeReply, String)> = vec![
            (
                FakeReply::stdout("GPU 0: NVIDIA RTX A4000 (UUID: GPU-1)\nGPU 1: NVIDIA RTX A4000 (UUID: GPU-2)\nalpha-host\n"),
                "✓ devtest-alpha run:\n  GPU 0: NVIDIA RTX A4000 (UUID: GPU-1)\n  GPU 1: NVIDIA RTX A4000 (UUID: GPU-2)\n  alpha-host".into(),
            ),
            (FakeReply::stdout("alpha-host\n"), "✓ devtest-alpha run: alpha-host".into()),
            (FakeReply::ok(), "✓ devtest-alpha run: (no output)".into()),
            (
                FakeReply::exit(2, "ls: cannot access 'nope': No such file or directory\n").with_stdout("found:\n"),
                "✗ devtest-alpha run (exit 2):\n  found:\n  ls: cannot access 'nope': No such file or directory".into(),
            ),
            (FakeReply::stdout("\u{1b}[32mok\u{1b}[0m\n 10%\r100%\n\n"), "✓ devtest-alpha run:\n  ok\n  100%".into()),
            (
                FakeReply::stdout(&hundred),
                format!(
                    "✓ devtest-alpha run:\n  … 60 earlier line(s) not shown\n{}",
                    (61..=100).map(|i| format!("  line {i}")).collect::<Vec<_>>().join("\n")
                ),
            ),
        ];
        for (reply, want) in cases {
            let fake = FakeRemote::new();
            fake.script("10.0.0.1:22001", [reply]);
            assert_eq!(execute(&quiet_fleet(), &fake, &cfg, Action::Run, &alpha, Some("nvidia-smi -L; hostname")).await, want);
        }
        // Several pods: each pod's output under its own line.
        let fake = Arc::new(FakeRemote::new());
        fake.script("10.0.0.1:22001", [FakeReply::stdout("alpha-host\nup 3 days\n")]);
        fake.script("10.0.0.2:22002", [FakeReply::exit(127, "sh: uptim: not found\n")]);
        let (provider, remote): (Arc<dyn Provider>, Arc<dyn Remote>) = (Arc::new(quiet_fleet()), fake.clone());
        let pods = vec![alpha.clone(), pod("bravo", "runpod", "rp2", 2, Some(22002))];
        let out = execute_fleet(&provider, &remote, &cfg, Action::Run, pods, Some("hostname; uptim".into())).await;
        assert_eq!(out, "run — 1/2 ok\n✓ devtest-alpha run:\n  alpha-host\n  up 3 days\n✗ devtest-bravo run (exit 127): sh: uptim: not found");
    }

    /// The confirm says what the action does in plain words — never the generated `ssh -p …
    /// -o … -i …` line (live: clipped at the width, it hid the remote command) — for every
    /// action that has a confirm; the full command is there on `v`, wrapped, not clipped. The
    /// input modal (run/set-branch's confirm) and a multi-pod confirm summarise the same way,
    /// naming several pods once.
    #[test]
    fn confirms_summarise_the_action_instead_of_the_ssh_line() {
        let shared = fixture_shared();
        let conf = cfg("ARENA_REPO_OWNER=nickypro\nARENA_REPO_NAME=arena-sandbox-materials\nGIT_SSH_KEY_LOCAL=/nonexistent/deploy_key\n");
        let mut ui = ui(conf, Arc::new(FakeRemote::new()));
        let alpha = shared.pods.iter().find(|p| p.id == "rp1").unwrap().clone();
        // (action, its summary)
        for (action, want) in [
            (Action::Test, "alpha: print the torch version (read-only)"),
            (Action::Backup, "alpha: commit + push the current branch"),
            (Action::Setup, "alpha: provision (deploy key, SSH keys, repo, tools)"),
            (Action::Restart, "alpha: restart in place (a hard reset)"),
            (Action::Stop, "alpha: stop (it can be started again)"),
            (Action::Terminate, "alpha: terminate (delete)"),
        ] {
            let preview = build_preview(&ui.cfg, action, &alpha);
            assert_eq!(preview.is_some(), !action.requires_typed_name(), "{action:?}");
            ui.mode = Mode::Confirm(Confirm::new(action, "alpha".into(), alpha.id.clone(), preview));
            let all = draw(100, 30, &shared, &ui).join("\n");
            assert!(all.contains(want), "{action:?}:\n{all}");
            assert!(!all.contains("ssh -p") && !all.contains("BatchMode") && !all.contains("Will run"), "{action:?}:\n{all}");
            if action.requires_typed_name() {
                assert!(all.contains("Type the pod name to confirm"), "{action:?}:\n{all}");
            } else {
                assert!(all.contains("[v] full command"), "{action:?}:\n{all}");
            }
        }
        // The warnings stay.
        ui.mode = Mode::Confirm(Confirm::new(Action::Terminate, "alpha".into(), alpha.id.clone(), None));
        assert!(draw(100, 30, &shared, &ui).join("\n").contains("⚠ IRREVERSIBLE"));
        // `v`: the full command, wrapped whole — its end (the python3 fallback) included.
        let mut c = Confirm::new(Action::Test, "alpha".into(), alpha.id.clone(), build_preview(&ui.cfg, Action::Test, &alpha));
        c.full = true;
        let body = confirm_body(&c, 50, 40);
        let at = body.iter().position(|l| l == "Will run:").unwrap();
        let rebuilt: String =
            body[at + 1..].iter().enumerate().map(|(i, l)| if i == 0 { l.as_str() } else { l.strip_prefix("  ").unwrap() }).collect();
        assert_eq!(Some(rebuilt), c.preview.clone());
        assert!(body.iter().all(|l| l.chars().count() <= 50), "{body:?}");
        ui.mode = Mode::Confirm(c);
        let all = draw(100, 30, &shared, &ui).join("\n");
        assert!(all.contains("Will run:") && all.contains("[v] hide command"), "{all}");
        // Several pods: named once, then what.
        ui.marked = ["runpod:rp1", "runpod:rp2", "hetzner:88"].map(String::from).into();
        ui.mode = Mode::FleetConfirm { action: Action::Backup, typed: String::new(), scope: Scope::Marked, note: String::new() };
        let all = draw(180, 30, &shared, &ui).join("\n");
        assert!(all.contains("3 pods: alpha, bravo, charlie") && all.contains("commit + push the current branch"), "{all}");
        // run / set-branch: the input modal says what Enter does, the command verbatim.
        ui.mode = Mode::Input { action: Action::Run, scope: InputScope::Set(Scope::Marked), value: "nvidia-smi -L; hostname".into() };
        let all = draw(180, 30, &shared, &ui).join("\n");
        assert!(all.contains("3 pods: alpha, bravo, charlie") && all.contains("nvidia-smi -L; hostname"), "{all}");
        ui.mode = Mode::Input { action: Action::SetBranch, scope: InputScope::Pod { name: "alpha".into(), id: "rp1".into() }, value: "w3d2".into() };
        let all = draw(180, 30, &shared, &ui).join("\n");
        assert!(all.contains("alpha: switch to branch w3d2 (fast-forward only)"), "{all}");
    }

    /// The text wrappers the confirm counts its rows with (pure).
    #[test]
    fn wrap_words_and_wrap_command_table() {
        // (text, width, rows)
        let words: &[(&str, usize, &[&str])] = &[
            ("a bb ccc", 4, &["a bb", "ccc"]),
            ("a bb ccc", 8, &["a bb ccc"]),
            ("abcdefghij k", 4, &["abcd", "efgh", "ij k"]),
            ("", 4, &[""]),
        ];
        for (text, width, want) in words {
            assert_eq!(wrap_words(text, *width), *want, "{text:?} at {width}");
        }
        let cmds: &[(&str, usize, &[&str])] = &[
            ("echo hi", 10, &["echo hi"]),
            ("abcdefghijkl", 6, &["abcdef", "  ghij", "  kl"]),
            ("", 6, &[""]),
        ];
        for (line, width, want) in cmds {
            assert_eq!(wrap_command(line, *width), *want, "{line:?} at {width}");
        }
    }

    /// The add-pod form's footer names the form's own keys (it said `[↑↓] pods  [+-] gpus  [←→]
    /// type` — keys the form doesn't have — under a form saying `[↑↓] field  [←→] change`).
    #[test]
    fn the_new_pod_footer_matches_the_form() {
        let shared = fixture_shared();
        let mut ui = ui(cfg("RUNPOD_API_KEY=x\n"), Arc::new(FakeRemote::new()));
        ui.mode = Mode::NewPod(build_new_pod_form(&ui.cfg, "runpod", &[]));
        let lines = draw(120, 40, &shared, &ui);
        let footer = lines.last().unwrap();
        assert!(footer.contains("[↑↓] field  [←→] change  [enter] create  [esc] cancel"), "{footer}");
        assert!(!footer.contains("[+-]") && !footer.contains("[↑↓] pods"), "{footer}");
        assert!(lines.join("\n").contains("[↑↓] field   [←→] change   [enter] CREATE   [esc] cancel"));
    }

    /// The menus' restart label says what each pod's backend says a restart does to its disk —
    /// live: RunPod v2's wiped it, a Hetzner VM keeps it, RunPod v1 (seen keeping it) and Vast
    /// aren't known to — from the real fleet's answers, as the refresh records them; a marked
    /// set's menu says the worst of its pods'. It used to say "WIPES" for every pod. The confirm
    /// is as strict as before: the name typed back, and a ⚠ wherever the backend may wipe.
    /// A config without `RUNPOD_API` ("") is v2's: its RunPod pods say WIPES.
    #[test]
    fn the_restart_label_follows_each_pods_backend() {
        let fleet = |api: &str| -> Arc<dyn Provider> {
            let line = if api.is_empty() { String::new() } else { format!("RUNPOD_API={api}\n") };
            let keys = format!("RUNPOD_API_KEY=r\n{line}VAST_API_KEY=v\nHETZNER_API_KEY=h\n");
            Arc::from(arena_core::provider::build_fleet("runpod", &Config::parse(&keys), false).unwrap())
        };
        let (rp, va, hz) = (pod("alpha", "runpod", "rp1", 1, Some(22001)), pod("bravo", "vast", "va1", 2, Some(22002)), pod("charlie", "hetzner", "hz1", 3, Some(22)));
        let pods = [rp.clone(), va.clone(), hz.clone()];
        let shared = Arc::new(Mutex::new(Shared::default()));
        let snap = dashboard_snapshot(&pods, &[], None, &HealthCache::new(), &Naming::from_config(&cfg("")), NOW);
        shared.lock().unwrap().publish(snap);
        let at = |id: &str| shared.lock().unwrap().pods.iter().position(|p| p.id == id).unwrap();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        // Nothing recorded yet: never "keeps", never "WIPES".
        ui.mode = Mode::Menu { sel: 0 };
        assert!(draw(120, 40, &shared.lock().unwrap(), &ui).join("\n").contains("restart — may wipe the container disk"));
        for (api, runpod) in [("v2", "WIPES the container disk"), ("v1", "may wipe the container disk"), ("", "WIPES the container disk")] {
            let f = fleet(api);
            shared.lock().unwrap().resets = backend_resets(f.as_ref(), &pods);
            for (id, want) in [("rp1", runpod), ("va1", "may wipe the container disk"), ("hz1", "keeps the disk")] {
                (ui.selected, ui.mode) = (at(id), Mode::Menu { sel: 0 });
                let text = draw(120, 40, &shared.lock().unwrap(), &ui).join("\n");
                assert!(text.contains(&format!("[r] restart    restart — {want}")), "{api} {id}: {text}");
            }
            for (marked, want) in [(vec![&hz], "keeps the disk"), (vec![&hz, &va], "may wipe the container disk"), (vec![&hz, &va, &rp], runpod)] {
                ui.marked = marked.iter().map(|p| mark_key(p)).collect();
                ui.mode = Mode::FleetMenu { scope: Scope::Marked, sel: 0 };
                let text = draw(120, 40, &shared.lock().unwrap(), &ui).join("\n");
                assert!(text.contains(&format!("restart — {want}")), "{api} {marked:?}: {text}");
            }
            ui.marked.clear();
            // The single-pod confirm: the name typed back whatever the answer; the warning where
            // the backend may wipe, worded by its answer.
            for (id, warn) in [("rp1", if api == "v1" { "⚠ MAY WIPE THE CONTAINER DISK" } else { "⚠ WIPES THE CONTAINER DISK" }), ("va1", "⚠ MAY WIPE"), ("hz1", "the disk is kept")] {
                (ui.selected, ui.mode) = (at(id), Mode::List);
                choose_pod_action(&shared, &mut ui, &f, Action::Restart);
                let Mode::Confirm(c) = &ui.mode else { panic!("{:?}", ui.mode) };
                assert!(c.action.requires_typed_name() && !c.is_satisfied(), "{api} {id}");
                assert!(c.warning().is_some_and(|w| w.contains(warn)), "{api} {id}: {:?}", c.warning());
            }
        }
    }

    /// A multi-pod stop sorts its pods as `pods stop` does, through the core helpers
    /// (`status::is_billing`, and `Provider::stop_support` — here the real fleet's, where
    /// Shadeform says it has no stop): a billing pod with no stop is skipped and named, a pod
    /// not running is left alone, the rest are stopped; a selection of only no-stop pods is
    /// refused, as `pods stop` refuses it — and one pod's stop of such a pod is refused before
    /// its confirm. The marked-set menu has stop; the whole fleet's doesn't.
    #[test]
    fn a_marked_stop_skips_what_it_cannot_stop_and_refuses_it_alone() {
        use arena_core::provider::shadeform::NO_STOP;
        let fleet: Arc<dyn Provider> =
            Arc::from(arena_core::provider::build_fleet("runpod", &Config::parse("RUNPOD_API_KEY=r\nSHADEFORM_API_KEY=k"), false).unwrap());
        let alpha = pod("alpha", "runpod", "rp1", 1, Some(22001));
        let mayor = pod("mayor", "shadeform", "sf1", 5, Some(22));
        let mut delta = pod("delta", "runpod", "rp4", 4, None);
        delta.status = "EXITED".into();
        let plan = stop_plan(fleet.as_ref(), vec![alpha.clone(), mayor.clone(), delta.clone()]);
        assert_eq!(plan.stop.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["devtest-alpha"]);
        assert_eq!(plan.refusal(), None);
        assert_eq!(
            plan.skip_lines(),
            [
                "⊘ devtest-delta — skipped: not running or starting (EXITED)".to_string(),
                format!("⊘ devtest-mayor — skipped, still billing: {NO_STOP}"),
            ]
        );
        assert!(plan.still_billing().unwrap().starts_with("✗ 1 pod(s) still billing — their provider has no stop: devtest-mayor"));
        let alone = stop_plan(fleet.as_ref(), vec![mayor.clone(), delta.clone()]);
        assert_eq!(alone.refusal().unwrap(), format!("✗ can't stop devtest-mayor — {NO_STOP}"));
        assert!(stop_plan(fleet.as_ref(), vec![delta.clone()]).refusal().unwrap().starts_with("⊘ nothing to stop"));

        // Through the menu picks, as the key loop calls them.
        let shared = Arc::new(Mutex::new(Shared::default()));
        let snap = dashboard_snapshot(&[alpha.clone(), mayor.clone(), delta.clone()], &[], None, &HealthCache::new(), &Naming::from_config(&cfg("")), NOW);
        shared.lock().unwrap().publish(snap);
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        ui.marked = [&alpha, &mayor, &delta].iter().map(|p| mark_key(p)).collect();
        choose_fleet_action(&shared, &mut ui, &fleet, Action::Stop, Scope::Marked);
        let Mode::FleetConfirm { action: Action::Stop, ref note, .. } = ui.mode else { panic!("{:?}", ui.mode) };
        assert!(note.contains("devtest-mayor — skipped, still billing: Shadeform has no stop") && note.contains("devtest-delta — skipped"), "{note}");
        let all = draw(180, 40, &shared.lock().unwrap(), &ui).join("\n");
        assert!(all.contains("skipped, still billing") && all.contains("Type ALL to confirm"), "{all}");
        // Only the Shadeform pod: refused, nothing to confirm.
        ui.marked = [mark_key(&mayor)].into();
        choose_fleet_action(&shared, &mut ui, &fleet, Action::Stop, Scope::Marked);
        assert!(matches!(&ui.mode, Mode::Result(r) if r.starts_with("✗ can't stop devtest-mayor — Shadeform has no stop")), "{:?}", ui.mode);
        // One pod's stop: refused before the confirm for it, the confirm for another.
        let row = |id: &str| shared.lock().unwrap().pods.iter().position(|p| p.id == id).unwrap();
        ui.selected = row("sf1");
        choose_pod_action(&shared, &mut ui, &fleet, Action::Stop);
        assert!(matches!(&ui.mode, Mode::Result(r) if r.starts_with("✗ can't stop devtest-mayor — Shadeform has no stop")), "{:?}", ui.mode);
        ui.selected = row("rp1");
        choose_pod_action(&shared, &mut ui, &fleet, Action::Stop);
        assert!(matches!(&ui.mode, Mode::Confirm(c) if c.action == Action::Stop), "{:?}", ui.mode);
        // The menus.
        ui.mode = Mode::FleetMenu { scope: Scope::Marked, sel: 0 };
        assert!(draw(120, 40, &shared.lock().unwrap(), &ui).join("\n").contains("[s] stop"));
        ui.mode = Mode::FleetMenu { scope: Scope::All, sel: 0 };
        assert!(!draw(120, 40, &shared.lock().unwrap(), &ui).join("\n").contains("[s] stop"));
    }

    /// The marked stop as it runs: the stoppable pods stopped (through the stop's outcome), the
    /// skipped ones named after them, and a ✗ while one still bills (`pods stop` exits non-zero).
    #[tokio::test]
    async fn a_marked_stop_stops_the_rest_and_says_what_still_bills() {
        /// Records each stop; a `shadeform` pod has no stop, as that backend says.
        struct Stops(Mutex<Vec<String>>);
        #[async_trait::async_trait]
        impl Provider for Stops {
            fn name(&self) -> &'static str {
                "fleet"
            }
            fn describe(&self, _spec: &PodSpec) -> String {
                String::new()
            }
            async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
                Ok(vec![])
            }
            async fn create_pod(&self, _spec: &PodSpec) -> arena_core::Result<Pod> {
                unimplemented!()
            }
            async fn stop_pod(&self, id: &str) -> arena_core::Result<()> {
                assert_ne!(id, "sf1", "a pod with no stop was sent a stop");
                self.0.lock().unwrap().push(id.to_string());
                Ok(())
            }
            async fn restart_pod(&self, _id: &str) -> arena_core::Result<()> {
                unimplemented!()
            }
            async fn terminate_pod(&self, _id: &str) -> arena_core::Result<()> {
                unimplemented!()
            }
            fn stop_support(&self, pod: &Pod) -> arena_core::Result<()> {
                match pod.provider.as_str() {
                    "shadeform" => Err(arena_core::Error::NotImplemented("Shadeform has no stop (fake)".into())),
                    _ => Ok(()),
                }
            }
        }
        let stops = Arc::new(Stops(Mutex::new(Vec::new())));
        let provider: Arc<dyn Provider> = stops.clone();
        let remote: Arc<dyn Remote> = Arc::new(FakeRemote::new());
        let mut delta = pod("delta", "runpod", "rp4", 4, None);
        delta.status = "EXITED".into();
        let pods = vec![pod("alpha", "runpod", "rp1", 1, None), pod("mayor", "shadeform", "sf1", 5, None), delta, pod("bravo", "runpod", "rp2", 2, None)];
        let out = execute_fleet(&provider, &remote, &cfg(""), Action::Stop, pods, None).await;
        assert_eq!(
            out,
            "stop — 2/2 ok\n✓ stopped devtest-alpha\n✓ stopped devtest-bravo\n⊘ devtest-delta — skipped: not running or starting (EXITED)\n\
             ⊘ devtest-mayor — skipped, still billing: Shadeform has no stop (fake)\n\
             ✗ 1 pod(s) still billing — their provider has no stop: devtest-mayor (back up, then `arena pods terminate`)"
        );
        let mut stopped = stops.0.lock().unwrap().clone();
        stopped.sort();
        assert_eq!(stopped, ["rp1", "rp2"]);
    }

    /// `n` lists the fleet in the background: `listing…` at once, the dashboard drawing on;
    /// the form opens when the listing lands, its names past the listed ones. Awaiting it in
    /// the key loop froze the dashboard for as long as the slowest provider took.
    #[tokio::test(start_paused = true)]
    async fn n_lists_in_the_background_and_opens_the_form_when_it_lands() {
        struct Slow;
        #[async_trait::async_trait]
        impl Provider for Slow {
            fn name(&self) -> &'static str {
                "runpod"
            }
            fn describe(&self, _spec: &PodSpec) -> String {
                String::new()
            }
            async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
                tokio::time::sleep(Duration::from_secs(10)).await;
                Ok(vec![Pod { id: "rp1".into(), name: "devtest-alpha".into(), provider: "runpod".into(), ..Default::default() }])
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
        let shared = Arc::new(Mutex::new(Shared::default()));
        let mut ui = ui(cfg("RUNPOD_API_KEY=x\n"), Arc::new(FakeRemote::new()));
        let provider: Arc<dyn Provider> = Arc::new(Slow);
        start_new_pod_listing(&shared, &provider, &mut ui);
        assert!(matches!(&ui.mode, Mode::Working(m) if m.starts_with("listing")), "{:?}", ui.mode);
        assert!(shared.lock().unwrap().new_pod_listing.is_none(), "still listing");
        tokio::time::sleep(Duration::from_secs(11)).await;
        let listed = shared.lock().unwrap().new_pod_listing.take().expect("the listing landed");
        let Mode::NewPod(form) = new_pod_mode(&ui, listed) else { panic!("no form") };
        assert_eq!(form.free.first().map(String::as_str), Some("devtest-bravo"), "alpha is taken");
        assert!(matches!(new_pod_mode(&ui, Err("vast 429".into())), Mode::Result(r) if r == "✗ list failed: vast 429"));
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
            let shared = Arc::new(Mutex::new(Shared { pods: vec![pod("alpha", "runpod", "rp1", 1, Some(22))], status: "test".into(), ..Default::default() }));
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
}
