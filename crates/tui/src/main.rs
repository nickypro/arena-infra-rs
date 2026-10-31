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
//! wedged pod can't hang an action. The top line groups the fleet by provider, each with its
//! prepaid account's balance and runway (`arena balance`'s views, observed inline in the
//! refresh's slower details round — at most one live read per account per 5 min — and
//! recorded in the same state file the CLI keeps; a low or critical one marked `⚠`), and is
//! built to fit the terminal's width without leaving out a warning (`state::top_parts`).
//!
//! Safety against live prod is built into the *interaction*, not bolted on: a mutating
//! action always pops a confirmation modal. Lifecycle actions (restart/stop/terminate)
//! make you type the pod's exact name back before they apply; backup/setup say what they
//! will do in plain words (`v` shows the generated command) and take a single `y`. There is
//! no way to mutate a pod without going through that modal — the dashboard's reads stay reads.
//!
//! Config and provider as the CLI picks them (`arena_core::config::find_config`, `pick_provider`):
//! the config is `ARENA_CONFIG`, else `~/.config/arena/config.env`, else the built-in default; the
//! provider `ARENA_PROVIDER`, else the config's `PROVIDER`, else `runpod`. `arena tui` sets both.
//! Initial cadence by `ARENA_REFRESH_SECS` (default 5).

mod aikey;
mod ask;
mod backups;
mod columns;
mod demo;
mod lag;
mod newpod;
mod offers;
mod prefs;
mod runs;
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
        self, DisableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
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
use arena_core::capability::Op;
use arena_core::provider::{ContainerReset, Provider};
use arena_core::proxy::Listing;
use arena_core::remote::{describe_error, Remote, SshRemote};
use arena_core::selector::Naming;
use arena_core::snapshot::{self, FleetSnapshot, HealthCache};
use arena_core::ssh::SshTarget;
use arena_core::{Config, Pod};

use prefs::Prefs;
use arena_core::status::pod_status;
use state::{
    action_summary, capture_details, carry_over, remember_good, dashboard_snapshot, details_due, display_name, health_cell, listing_note, mark_key, marked_pods,
    marked_set_token, merge_details, overlay_details, proxy_cell, proxy_detail, rtt_cell,
    provider_title, select_marks, short_branch, spark, summarize, summary_text, count_of, top_parts, Action, Confirm, DeepChecks, Endpoint,
    FleetSummary, History, PodDetails, RefreshStatus, Rtts, Throttle, Tone, RTT_EVERY, RTT_WAIT,
};
use newpod::NewPodForm;

use arena_core::config::LEGACY_CONFIG as DEFAULT_CONFIG;
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

/// The longest a dashboard provider call waits out a rate limit (a held quota, a 429's
/// `Retry-After`) before giving up for this refresh — its provider's last rows kept — instead:
/// waiting a minute window's reset would freeze the probes and the screen with it.
const PROVIDER_WAIT: Duration = Duration::from_secs(2);

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
    /// Interactive add-pod form (↑↓ field, ←→ value) — `newpod.rs`.
    NewPod(NewPodForm),
    /// The add-pod form's confirm: the `arena pods up` it will run; `y` runs it (`runs.rs`).
    UpConfirm(NewPodForm),
    /// The runs panel (`p`): run `id`'s progress and output, following its end unless scrolled
    /// (`scroll`); `stop`: asking whether to send it Ctrl+C. Esc hides it; the run goes on.
    Runs { id: u64, scroll: Option<u16>, stop: bool },
    /// `q` with a run that changes pods still going: leave it running and quit, stop it and
    /// quit, or stay (`runs.rs`).
    Quit,
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
    /// `b`, or a click on the 💾 header: the backups screen — which backups happen, when, how
    /// they're doing, per pod (`backups.rs`).
    Backups(backups::View),
    /// `i`, or a click on the footer's `[i] info`: everything the top line and the footer leave out
    /// (provider, config, the whole status, refresh, deep checks, the fleet's words, balances,
    /// AI, every key, what the glyphs mean), scrollable. `detail`: opened from the detail view
    /// (Esc goes back to it).
    Info { scroll: u16, detail: bool },
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
    /// The refresh's status: the footer's short form, the info panel's whole line.
    status: RefreshStatus,
    last_refresh: Option<Instant>,
    /// True while a sweep is in flight (the footer says `refreshing`).
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
    /// The CLI commands started from the dashboard (`runs.rs`), oldest first, and the last id
    /// given out.
    runs: Vec<runs::Run>,
    next_run: u64,
    /// `arena offers` answers for the add-pod form, set by their background tasks; the loop
    /// hands them to `Ui::offers`.
    offers_landed: Vec<(offers::Query, std::result::Result<String, String>)>,
    /// What a restart does to each row's container disk, by [`mark_key`]: its backend's answer
    /// (`Provider::restart_resets_container`, pure), set with the rows each refresh — what the
    /// action menus' restart label says ([`Shared::restart_reset`]).
    resets: HashMap<String, ContainerReset>,
    /// What each row's provider can't do of the menus' operations, by [`mark_key`]: why and what
    /// to do instead — the capability table's answer (`Provider::supports`, pure), set with the
    /// rows each refresh ([`backend_refusals`]): what the action menus grey out
    /// ([`Shared::refusal`]).
    refusals: HashMap<String, Vec<(Op, String)>>,
    /// A `?` question out (`asking claude about …`, for the footer); `None` when none is.
    ask_running: Option<String>,
    /// When it was asked: the footer's seconds while it's out.
    ask_since: Option<Instant>,
    /// Its answer, set by the background task; the loop opens it ([`ask::land`]).
    ask_answer: Option<ask::Answer>,
    /// The RTT from this box to each pod's SSH endpoint, as last timed (the RTT column).
    rtts: Rtts,
    /// A run that changed AI keys ended (`runs.rs`): the next refresh reads them at once, not on
    /// the minute's cadence, so the `AI$` cell and the summary show the change.
    ai_wanted: bool,
    /// The backups' status as the screen last read it (`backups.rs`).
    backups: backups::Landed,
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

    /// Why `pod`'s provider can't do `op` (and what instead), as of the last refresh; `None`: it
    /// can, or the pod wasn't answered for yet — the pick then asks the provider itself
    /// ([`choose_pod_action`]).
    fn refusal(&self, pod: &Pod, op: Op) -> Option<&str> {
        self.refusals.get(&mark_key(pod))?.iter().find(|(o, _)| *o == op).map(|(_, why)| why.as_str())
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
    /// `ARENA_TUI_TRACE`'s frames and events (`lag.rs`); off unless a file is named.
    trace: lag::Trace,
    /// How to run the CLI (`runs.rs`): the `arena` beside this executable — or why there's none
    /// (then the form, start, rename, jobs and logs say so).
    cli: std::result::Result<runs::Cli, String>,
    /// Wakes the refresh (a run made or finished a pod).
    nudge: Arc<Notify>,
    /// The add-pod form's catalogs and availability, kept for the dashboard's life (`offers.rs`).
    offers: offers::State,
    /// `ARENA_TUI_DEMO`: the rows are made up (`demo.rs`) — every key that would act on a pod,
    /// reach one or create one is refused ([`demo_refuses`]).
    demo: bool,
    /// Where the footer's `[i] info` was last drawn (its first cell), set by `view`: a click on it opens the
    /// info panel ([`handle_mouse`]).
    menu_at: std::cell::Cell<Option<(u16, u16)>>,
    /// The row the top line was last drawn on, set by `view`: a click on it opens the info panel too.
    top_at: std::cell::Cell<Option<u16>>,
    /// A provisioning key is configured: the action menus have the AI key's rows (`aikey.rs`).
    ai_keys: bool,
    /// The marked set's pods and keys an AI key confirm was opened with — what it runs on
    /// (`aikey::choose_set`).
    ai_set: Vec<(String, Option<arena_core::openrouter::KeyInfo>)>,
    /// The backups screen's state beyond its mode (`backups.rs`).
    backups: backups::State,
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
        "arena-tui takes no arguments: the config comes from ARENA_CONFIG (else ~/.config/arena/config.env, \
         else {DEFAULT_CONFIG}), the provider from ARENA_PROVIDER (else the config's PROVIDER). \
         Run `arena [--config <file>] [--provider <name>] tui`."
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
    // As the CLI picks them (`arena tui` hands its own choice over in ARENA_CONFIG/ARENA_PROVIDER).
    let (config_file, source) = arena_core::config::find_config(None);
    let cfg = Config::load(&config_file).with_context(|| arena_core::config::load_context(&config_file, source))?;
    let config_path = config_file.display().to_string();
    let (provider_name, provider_source) = arena_core::config::pick_provider(std::env::var("ARENA_PROVIDER").ok().as_deref(), &cfg);
    let demo = demo::wanted(std::env::var("ARENA_TUI_DEMO").ok().as_deref());
    // Fleet-spanning provider (like the CLI): the dashboard lists pods across ALL configured
    // backends — runpod + vast + hetzner — not just the primary. `warn_on_partial=false` so a
    // provider hiccup (e.g. Vast 429) doesn't print onto the alternate screen. Creates still
    // go to the chosen primary (ARENA_PROVIDER). A demo asks no provider anything.
    let provider: Arc<dyn Provider> = match demo {
        Some(_) => Arc::new(demo::NoProvider),
        None => {
            let fleet = arena_core::provider::build_fleet(&provider_name, &cfg, false);
            Arc::from(match provider_source {
                // A bad PROVIDER in the config is named as that, as `arena` names it.
                arena_core::config::ProviderSource::Config => fleet.with_context(|| format!("the config's PROVIDER=\"{provider_name}\" ({config_path})"))?,
                _ => fleet?,
            })
        }
    };
    let trace = lag::Trace::from_env();
    let progress_cmd = cfg.get("PROGRESS_CMD").map(String::from);
    let initial_secs = std::env::var("ARENA_REFRESH_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);

    let shared = Arc::new(Mutex::new(Shared {
        status: RefreshStatus::busy("loading…"),
        ..Default::default()
    }));
    let interval = Arc::new(AtomicU64::new(initial_secs));
    let nudge = Arc::new(Notify::new());
    let details_wanted = Arc::new(AtomicBool::new(false));
    let remote: Arc<dyn Remote> = if demo.is_some() { Arc::new(demo::NoRemote) } else { Arc::new(SshRemote) };

    // The prepaid balances for the summary bar: observed in the refresh's details round
    // (bounded, quiet), recorded in the CLI's state file. A bad BALANCE_WARN_HOURS falls
    // back to the default (`arena balance` is the place that says so).
    let (policy, _) = BalancePolicy::from_config(&cfg);
    let balance = BalanceCtx::new(Arc::new(HttpBalanceApi::from_config(&cfg)), balance_state_path(&cfg).and_then(Result::ok), policy);
    // OpenRouter's keys and credit, on the same cadence, when a provisioning key is set.
    let ai = OpenRouter::from_config(&cfg).map(|api| AiCtx { nudge: Some(nudge.clone()), ..AiCtx::new(api) });
    let ai_keys = ai.is_some() && demo.is_none();

    // Background fetcher: keeps `shared` fresh without blocking the UI. A demo's makes its
    // rows up instead.
    match demo {
        Some(n) => {
            tokio::spawn(demo::refresh_loop(shared.clone(), cfg.clone(), n, interval.clone(), nudge.clone()));
        }
        None => {
            tokio::spawn(fetch_loop(
                provider.clone(),
                remote.clone(),
                cfg.clone(),
                progress_cmd,
                shared.clone(),
                Cadence { interval: interval.clone(), nudge: nudge.clone(), details_wanted: details_wanted.clone() },
                DetailsState { balance: Some(balance), ai, rtt: Some(Rtts::default()), ..Default::default() },
            ));
        }
    }

    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena").to_string();
    // The CLI the form and the menu run: the `arena` installed beside this dashboard — none in a
    // demo, which acts on nothing.
    let cli = match demo {
        Some(_) => Err("demo: the pods are made up — nothing is run".to_string()),
        None => runs::Cli::locate(std::env::current_exe(), &config_path, snapshot::state_dir_for(&cfg)),
    };
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
        trace,
        cli,
        nudge: nudge.clone(),
        offers: offers::State::new(true),
        demo: demo.is_some(),
        menu_at: std::cell::Cell::new(None),
        top_at: std::cell::Cell::new(None),
        ai_keys,
        ai_set: Vec::new(),
        backups: backups::State::default(),
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
        // which exits. A run left going (`runs.rs`) is said once the terminal is back; it isn't
        // one of the children stopped here.
        Ok(notes) => {
            if !arena_core::remote::end_children() {
                return std::future::pending().await;
            }
            let restored = restore_terminal(&mut terminal);
            for n in notes {
                eprintln!("{n}");
            }
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
    /// Each provider's last good listing ([`state::remember_good`]): a rate-limited
    /// provider's rows come from it.
    listed: Option<Listing>,
    /// When each provider last listed (tokio's clock): how old its carried rows are.
    listed_at: HashMap<String, tokio::time::Instant>,
    /// The footer's note on the rate-limited providers, this refresh.
    throttle_note: Option<String>,
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
    /// Woken when a read asked for by a run that changed keys lands ([`Shared::ai_wanted`]): the
    /// refresh then shows it at once.
    nudge: Option<Arc<Notify>>,
}

impl AiCtx {
    fn new(api: OpenRouter) -> Self {
        AiCtx { api: Arc::new(api), last: None, view: Arc::default(), reading: None, nudge: None }
    }
}

/// How long the key listing may take (every page, the workspace lookup first) before the
/// last one is kept.
const AI_KEYS_TIMEOUT: Duration = Duration::from_secs(20);
/// How long the credit read may take: as long as an inline balance read.
const AI_CREDITS_TIMEOUT: Duration = balance::INLINE_TIMEOUT;

/// Read OpenRouter's keys and credit, each within its own bound, and apply them to `view`
/// apart ([`UsageView::apply`]): one failing never holds back the other's fresh answer. `done`
/// is woken once they're applied.
async fn read_ai(api: Arc<OpenRouter>, view: Arc<Mutex<UsageView>>, done: Option<Arc<Notify>>) {
    async fn bounded<T>(d: Duration, f: impl std::future::Future<Output = arena_core::Result<T>>) -> std::result::Result<T, String> {
        match arena_core::ratelimit::bounded(d, f).await {
            Ok(r) => r.map_err(|e| e.to_string()),
            Err(_) => Err(format!("timed out after {}s", d.as_secs())),
        }
    }
    let (keys, credits) = tokio::join!(bounded(AI_KEYS_TIMEOUT, api.list_keys()), bounded(AI_CREDITS_TIMEOUT, api.credits()));
    view.lock().unwrap_or_else(std::sync::PoisonError::into_inner).apply(keys, credits);
    if let Some(n) = done {
        n.notify_one();
    }
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
/// API calls per refresh are at most one list per provider ([`list_all`]: none for a provider
/// whose quota is held — a 429's wait, a spent or low window). The details query
/// (`Provider::enrich`: RunPod's GraphQL for GPU, $/h and the host maintenance window) runs
/// alongside the probes only when `state::details_due` says so (every
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
    // The spend ledger records the listing as of when it began (a stop the dashboard made
    // meanwhile is newer, and stands).
    let listed_at = snapshot::unix_now();
    let (fresh, listing) = list_all(provider, details).await;
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
                Some(balance_views(b, seen, &fresh, &[], now))
            }
            None => None,
        };
        let errs: Vec<String> = listing.errors().iter().map(|(p, e)| format!("{p}: {e}")).collect();
        let held = details.throttle_note.as_ref().map(|n| format!(" · ⚠ {n}")).unwrap_or_default();
        let mut s = shared.lock().unwrap();
        s.status = RefreshStatus::list_error(format!("list error: {}{held}", errs.join("; ")));
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
    // OpenRouter on the details cadence, in the background (never two reads at once) — and at
    // once after a run that changed keys (`Shared::ai_wanted`, taken only when a read can start:
    // one already going may have begun before the change).
    if let Some(a) = details.ai.as_mut() {
        let idle = a.reading.as_ref().is_none_or(|h| h.is_finished());
        let wanted = idle && std::mem::take(&mut shared.lock().unwrap().ai_wanted);
        if idle && (wanted || details_due(a.last.map(|t| t.elapsed()), forced)) {
            let done = if wanted { a.nudge.clone() } else { None };
            a.reading = Some(tokio::spawn(read_ai(a.api.clone(), a.view.clone(), done)));
            a.last = Some(tokio::time::Instant::now());
        }
    }
    // `SSH_CONFIG_FILE` (and staff's `SSH_ADMIN_CONFIG_FILE`) kept current, as `pods list`
    // keeps them: from a complete listing only (a provider that failed would drop its pods'
    // blocks — and a rate-limited one's carried rows aren't a listing), written only when an
    // endpoint moved; a failure is a status-line warning, never more.
    let ssh_warning = if fresh.errors().is_empty() {
        // No home under test: a `~/` value can't reach the developer's own ~/.ssh.
        let home = if cfg!(test) { None } else { std::env::var("HOME").ok() };
        arena_core::sshconfig::sync_all(cfg, &pods, home.as_deref()).into_iter().find_map(Result::err).map(|e| format!("ssh config not rewritten: {e}"))
    } else {
        None
    };
    let observing = async {
        match details.balance.as_ref().filter(|_| balance_due) {
            Some(b) => Some(observe_balance(b, now).await),
            None => None,
        }
    };
    // RTT: the endpoints due a timing ([`RTT_EVERY`]), timed alongside the SSH probes, all at
    // once, the whole timing bounded by [`RTT_WAIT`] — inside the probes' own budget when they
    // run; at worst a refresh with an endpoint that doesn't answer and nothing else to wait on
    // takes up to that second longer, once a minute.
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
        r.record(timed, Instant::now());
    }
    overlay_details(&mut pods, &details.details);
    // What each machine costs, for `arena spend` — quietly, and an unchanged machine isn't
    // rewritten more than once a minute. Only the providers that listed this time count
    // (`fresh`): a rate-limited one's carried rows say nothing new.
    arena_core::spend::record_listing(spend_path(cfg).as_deref(), &fresh, &pods, &Naming::from_config(cfg), listed_at);
    // The balance's runway is judged against what is billing as listed (not the pending
    // placeholders, and after the details filled the prices), a provider that failed to
    // list — or was rate-limited, its rows carried — counting as unknown, never as $0/h.
    let balance_views = match (observed, details.balance.as_mut()) {
        (Some(seen), Some(b)) => Some(balance_views(b, seen, &fresh, &pods, now)),
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
    let warnings: Vec<&str> = [&details.throttle_note, &details.warning, &proxy_warning, &health_warning, &ssh_warning]
        .into_iter()
        .flatten()
        .map(String::as_str)
        .collect();
    let (quota, rate) = quota_line(&listing);
    s.status = RefreshStatus::of(status_line(&s.summary, quota.as_deref(), &warnings), &warnings, rate);
    s.metrics = metrics;
    if let Some(r) = &details.rtt {
        s.rtts = r.clone();
    }
    if let Some(v) = balance_views {
        s.balance = v;
    }
    s.resets = backend_resets(provider, &display);
    s.refusals = backend_refusals(provider, &display);
    s.publish(snap);
    s.last_refresh = Some(Instant::now());
    s.refreshing = false;
}

/// This refresh's listing, every provider asked — each one's rate-limit waits bounded by
/// [`PROVIDER_WAIT`], so one whose API quota is held (a 429's wait, a spent window, a low
/// window's spacing: `arena_core::ratelimit`) is refused at once, **without a request**,
/// while the others list as usual. Returned as listed (`fresh`: for what records or acts on
/// it — the balance's burns, the SSH config) and as shown, a rate-limited provider's last
/// good rows kept ([`state::carry_over`]), with the footer's note on it
/// ([`state::listing_note`]).
async fn list_all(provider: &dyn Provider, details: &mut DetailsState) -> (Listing, Listing) {
    let results = arena_core::ratelimit::waits_within(PROVIDER_WAIT, provider.list_by_provider()).await;
    let throttled: Vec<String> = results
        .iter()
        .filter(|(_, r)| matches!(r, Err(e) if e.kind() == Some(arena_core::ProviderErrorKind::RateLimited)))
        .map(|(p, _)| p.clone())
        .collect();
    let fresh = Listing::from_results(results);
    let mut shown = fresh.clone();
    carry_over(details.listed.as_ref(), &mut shown, &throttled);
    let now = tokio::time::Instant::now();
    for pl in fresh.providers.iter().filter(|pl| pl.pods.is_ok()) {
        details.listed_at.insert(pl.provider.clone(), now);
    }
    let notes: Vec<Throttle> = throttled
        .iter()
        .map(|p| Throttle {
            provider: p.clone(),
            hold: arena_core::ratelimit::retry_in(p),
            rows_age: shown
                .providers
                .iter()
                .any(|pl| &pl.provider == p && pl.pods.is_ok())
                .then(|| details.listed_at.get(p).map(|t| now - *t))
                .flatten(),
        })
        .collect();
    details.throttle_note = listing_note(&notes);
    details.listed = Some(remember_good(details.listed.as_ref(), &fresh));
    (fresh, shown)
}

/// The footer's quota part: per provider listed, its API requests in the last minute and
/// what its quota has left this minute ([`arena_core::ratelimit::footer`]) — from this
/// process's own requests; nothing for a provider that sent none and reports nothing. With the
/// requests of the last minute across them (the footer's short status, `ok · API 13/min`).
fn quota_line(listing: &Listing) -> (Option<String>, Option<usize>) {
    let views = arena_core::ratelimit::observed();
    let mut seen: Vec<&str> = Vec::new();
    let listed: Vec<&str> = listing
        .providers
        .iter()
        .map(|pl| pl.provider.as_str())
        .filter(|p| {
            let new = !seen.contains(p);
            seen.push(p);
            new
        })
        .collect();
    let parts: Vec<String> =
        listed.iter().filter_map(|p| arena_core::ratelimit::footer(&views, p, &provider_title(p))).collect();
    let rate: usize = views.iter().filter(|v| listed.contains(&v.provider)).map(|v| v.last_minute).sum();
    ((!parts.is_empty()).then(|| parts.join(" · ")), (!parts.is_empty()).then_some(rate))
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
    s.status = RefreshStatus::listed(&count_label(pods.len()));
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

/// What each of `pods`' providers can't do of the menus' operations ([`Action::op`]), by
/// [`mark_key`]: the capability table's refusal (`Provider::supports` — pure, no request), for the
/// menus to grey those rows out with why.
fn backend_refusals(provider: &dyn Provider, pods: &[Pod]) -> HashMap<String, Vec<(Op, String)>> {
    let ops: Vec<Op> = Action::MENU.iter().filter_map(|(_, a)| a.op()).collect();
    pods.iter()
        .map(|p| (mark_key(p), ops.iter().filter_map(|&op| provider.supports(op, Some(p)).refusal().map(|why| (op, why))).collect()))
        .collect()
}

/// The provider's best-effort details query, bounded by [`ENRICH_TIMEOUT`]. `Some(why)`
/// when the details are incomplete (the pods keep whatever was filled before a failure).
async fn enrich_bounded(provider: &dyn Provider, pods: &mut [Pod]) -> Option<String> {
    // Its rate-limit waits as short as the listing's: a held quota is a warning, not a stall.
    match tokio::time::timeout(ENRICH_TIMEOUT, arena_core::ratelimit::waits_within(PROVIDER_WAIT, provider.enrich(pods))).await {
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

/// Time each endpoint's RTT ([`arena_core::rtt::connect_once`]: exactly one TCP connect, name
/// lookup included within [`RTT_WAIT`]), all at once. `None` for one that didn't answer in time.
async fn time_endpoints(due: Vec<Endpoint>) -> Vec<(Endpoint, Option<Duration>)> {
    let mut set = JoinSet::new();
    for ep in due {
        set.spawn(async move {
            let d = arena_core::rtt::connect_once((ep.0.as_str(), ep.1), RTT_WAIT).await;
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

/// The footer's refresh status: liveness (the pod count lives in the summary bar), the
/// providers' API rate and quota ([`quota_line`]), then any warning from this refresh's reads
/// (a held listing, details query, proxy file, health cache).
fn status_line(s: &FleetSummary, quota: Option<&str>, warnings: &[&str]) -> String {
    let mut out = format!("{} reporting", s.reporting);
    if s.unreachable > 0 {
        out.push_str(&format!(" · {} unreachable", s.unreachable));
    }
    if let Some(q) = quota {
        out.push_str(&format!(" · {q}"));
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
    // Clicks and the wheel: scroll the list, click to select/multi-select rows.
    execute!(out, EnterAlternateScreen, MouseClicks)?;
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

/// Mouse reporting for clicks, drags and the wheel only: crossterm's `EnableMouseCapture` also
/// asks for every motion (mode 1003), so moving the pointer over the dashboard sent a stream of
/// events (each one a frame, then) — and over SSH, their bytes. `DisableMouseCapture` turns
/// every mode off again.
struct MouseClicks;

impl crossterm::Command for MouseClicks {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        // 1000: presses and releases (the wheel too); 1002: motion only with a button down;
        // 1006: SGR coordinates (any column).
        f.write_str("\x1b[?1000h\x1b[?1002h\x1b[?1006h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        crossterm::event::EnableMouseCapture.execute_winapi()
    }
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

/// Mouse handling: wheel-scroll the pod list (or the result modal, or the info panel), and in
/// the list left-click a row to select it. Clicking the far-left of a row — or shift-clicking
/// anywhere on it — toggles that pod's multi-select mark (same `marked` set as `space`). A click
/// on the footer's `[i] info`, or anywhere on the top line (the info panel has every word it
/// leaves out), opens the info panel from the list or the detail view, and closes it.
/// `false`: nothing changed (a move, a click outside the rows) — no frame for it.
fn handle_mouse(m: MouseEvent, ui: &mut Ui, shared: &Arc<Mutex<Shared>>) -> bool {
    let in_list = matches!(ui.mode, Mode::List);
    let in_result = matches!(ui.mode, Mode::Result(_));
    // The footer's `[i] info` (its cells, [`MENU_LABEL`]) or the top line: opens the info panel
    // from the list or the detail view, and closes it again.
    let on_menu = ui.menu_at.get().is_some_and(|(x, y)| m.row == y && (x..x.saturating_add(MENU_LABEL.chars().count() as u16)).contains(&m.column))
        || ui.top_at.get() == Some(m.row);
    if let (MouseEventKind::Down(MouseButton::Left), true) = (m.kind, on_menu) {
        ui.mode = match ui.mode {
            Mode::List => Mode::Info { scroll: 0, detail: false },
            Mode::Detail => Mode::Info { scroll: 0, detail: true },
            Mode::Info { detail: true, .. } => Mode::Detail,
            Mode::Info { detail: false, .. } => Mode::List,
            _ => return false,
        };
        return true;
    }
    // The 💾 header: the backups screen.
    let on_saved = ui.backups.saved_at.get().is_some_and(|(x, y)| m.row == y && (x..x.saturating_add(2)).contains(&m.column));
    if let (MouseEventKind::Down(MouseButton::Left), true, Mode::List | Mode::Detail) = (m.kind, on_saved, &ui.mode) {
        backups::open(ui, shared);
        return true;
    }
    if let Mode::Backups(_) = ui.mode {
        return backups::mouse(m, ui, shared);
    }
    if let Mode::Info { scroll, detail } = ui.mode {
        let delta = match m.kind {
            MouseEventKind::ScrollUp => -1,
            MouseEventKind::ScrollDown => 1,
            _ => return false,
        };
        ui.mode = Mode::Info { scroll: scroll_by(scroll, ui.result_max.get(), delta), detail };
        return true;
    }
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
                return false;
            }
            let (area, offset) = ui.last_table.get();
            // Data rows sit below the top border (1) + header (1), above the bottom border.
            let rows_top = area.y.saturating_add(2);
            let rows_bot = area.y.saturating_add(area.height).saturating_sub(1);
            let (mx, my) = (m.column, m.row);
            if my < rows_top || my >= rows_bot || mx < area.x || mx >= area.x.saturating_add(area.width)
            {
                return false; // click outside the table rows
            }
            let idx = offset + (my - rows_top) as usize;
            let key = {
                let s = shared.lock().unwrap();
                if idx >= s.pods.len() {
                    return false;
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
        _ => return false,
    }
    true
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
        KeyCode::Char('i') => ui.mode = Mode::Info { scroll: 0, detail: matches!(ui.mode, Mode::Detail) },
        // Act on the marked set if any are marked, else the cursor pod.
        KeyCode::Char('a') if !ui.marked.is_empty() => ui.mode = Mode::FleetMenu { scope: Scope::Marked, sel: 0 },
        KeyCode::Char('a') if len > 0 => ui.mode = Mode::Menu { sel: 0 },
        KeyCode::Char('A') if len > 0 => ui.mode = Mode::FleetMenu { scope: Scope::All, sel: 0 },
        KeyCode::Char('a' | 'A') => {}
        _ => return ListKey::Other,
    }
    ListKey::Done
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

/// The dashboard's parts the key handling needs besides `ui`.
struct Ctx<'a> {
    shared: &'a Arc<Mutex<Shared>>,
    provider: &'a Arc<dyn Provider>,
    interval: &'a AtomicU64,
    nudge: &'a Notify,
    details_wanted: &'a AtomicBool,
}

/// What handling an event came to.
#[derive(Debug, Clone, PartialEq)]
enum Flow {
    /// Nothing on screen changed (a mouse moving, a key with no use here).
    Same,
    /// Draw again.
    Changed,
    /// Leave the dashboard, saying these lines once the terminal is back (a run left running).
    Quit(Vec<String>),
}

/// The key loop. Each pass:
/// 1. what landed from the background is taken in ([`land`]: an action's result, a `?` answer,
///    `offers` answers) and the add-pod form's fetches are started (`newpod::tick`) — started,
///    never awaited: **no network or SSH call is awaited in this loop**; each runs in a task of
///    its own and lands through `Shared` (the form's provider switch awaited a whole-fleet
///    listing here: a second per keypress, the dashboard frozen);
/// 2. a frame is drawn when something changed — a key, a landing, a refresh, a notice — or the
///    tick is up ([`lag::frame_due`]), not on every pass; the shared lock is released before the
///    terminal is written ([`draw_frame`]);
/// 3. it waits for input until the tick, in [`lag::POLL_SLICE`]s, then takes **every** event
///    already waiting ([`lag::drain`]) before it draws again. It drew after each one: a held key,
///    a mouse dragged over the table or a slow terminal over SSH queued events faster than frames
///    went out, and the lag grew with the queue.
///
/// `Ok`: the lines to say once the terminal is back (a run left running).
#[allow(clippy::too_many_arguments)]
async fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    shared: &Arc<Mutex<Shared>>,
    provider: &Arc<dyn Provider>,
    interval: &AtomicU64,
    nudge: &Notify,
    details_wanted: &AtomicBool,
    mut ui: Ui,
) -> Result<Vec<String>> {
    let cx = Ctx { shared, provider, interval, nudge, details_wanted };
    let mut last_frame: Option<Instant> = None;
    let mut dirty = true;
    let mut seen = None;
    let mut events = 0usize;
    loop {
        let now = Instant::now();
        dirty |= land(&mut ui, shared, nudge, now);
        dirty |= newpod::tick(&mut ui, shared, now);
        dirty |= backups::tick(&mut ui, shared, now);
        let (len, stamp, live) = {
            let s = shared.lock().unwrap();
            (s.pods.len(), stamp(&s), live(&s, &ui, now))
        };
        // A refresh, a notice, a pending row or a run's output since the last frame.
        if seen != Some(stamp) {
            seen = Some(stamp);
            dirty = true;
        }
        // Clamp the cursor in case the fleet shrank under us.
        ui.selected = if len == 0 { 0 } else { ui.selected.min(len - 1) };
        let tick = if live { lag::TICK_LIVE } else { lag::TICK_IDLE };
        if lag::frame_due(Instant::now(), last_frame, dirty, tick) {
            // Put back by an end from outside the loop ([`end_now`]): nothing more to draw — the
            // loop ends, and that end exits.
            if !draw_frame(terminal, shared, &mut ui, interval.load(Ordering::Relaxed), events)? {
                return Ok(Vec::new());
            }
            last_frame = Some(Instant::now());
            dirty = false;
            events = 0;
        }
        if !event::poll(lag::wait_for(Instant::now(), last_frame, tick))? {
            continue;
        }
        let next = || -> std::io::Result<Option<Event>> { if event::poll(Duration::ZERO)? { event::read().map(Some) } else { Ok(None) } };
        let batch = lag::drain(event::read()?, next, Instant::now(), lag::BATCH_BUDGET, Instant::now)?;
        for ev in batch {
            events += 1;
            if ui.trace.on() {
                ui.trace.event(Instant::now(), &lag::event_label(&ev));
            }
            match handle_event(terminal, &cx, &mut ui, ev)? {
                Flow::Same => {}
                Flow::Changed => dirty = true,
                Flow::Quit(notes) => return Ok(notes),
            }
        }
    }
}

/// What on screen the background can change without a key: the refresh, the footer's notice,
/// the pending rows, the deep checks out, the runs' output and ends. Compared pass to pass — a
/// change draws a frame at once rather than at the next tick.
fn stamp(s: &Shared) -> (Option<Instant>, Option<Instant>, usize, usize, usize, bool) {
    let lines = s.runs.iter().map(|r| r.lines.len() + r.dropped + usize::from(r.end.is_some())).sum();
    (s.last_refresh, s.notice.as_ref().map(|n| n.at), s.pending.len(), s.deep.running(), lines, s.refreshing)
}

/// Whether something on screen moves by itself faster than the clock (a check waiting out its
/// debounce, a run's output open): frames then come every [`lag::TICK_LIVE`], else every
/// [`lag::TICK_IDLE`]. A question out counts seconds, so the idle tick is enough for it (the
/// footer has no spinner any more).
fn live(s: &Shared, ui: &Ui, now: Instant) -> bool {
    ui.offers.busy(now) || matches!(ui.mode, Mode::Working(_)) || (matches!(ui.mode, Mode::Runs { .. }) && s.runs.iter().any(|r| r.running()))
}

/// What landed from the background since the last pass: an action's result (its modal, and a
/// refresh), a `?` answer (its popup), `arena offers` answers for the form. `true`: something did.
fn land(ui: &mut Ui, shared: &Arc<Mutex<Shared>>, nudge: &Notify, now: Instant) -> bool {
    let (result, offers) = {
        let mut s = shared.lock().unwrap();
        (s.action_result.take(), std::mem::take(&mut s.offers_landed))
    };
    let mut changed = !offers.is_empty();
    if let Some(msg) = result {
        ui.result_scroll = 0;
        ui.mode = Mode::Result(msg);
        nudge.notify_one();
        changed = true;
    }
    for (q, out) in offers {
        ui.offers.landed(q, out, now);
    }
    let before = std::mem::discriminant(&ui.mode);
    ask::land(ui, shared);
    changed || std::mem::discriminant(&ui.mode) != before
}

/// Draw a frame: the view rendered under the shared lock, the terminal written once it's
/// released — a terminal slow to take it (over SSH) never holds the refresh, or any background
/// task, on the lock (`Terminal::draw` did both under it). `false`: the terminal isn't the
/// dashboard's any more ([`TERMINAL`]) and nothing was drawn. Traced (`lag.rs`).
fn draw_frame(terminal: &mut Terminal<CrosstermBackend<Stdout>>, shared: &Mutex<Shared>, ui: &mut Ui, secs: u64, events: usize) -> Result<bool> {
    let ours = terminal_ours();
    if !*ours {
        return Ok(false);
    }
    let start = Instant::now();
    terminal.autoresize()?;
    {
        let s = shared.lock().unwrap();
        let mut frame = terminal.get_frame();
        view(&mut frame, &s, ui, secs);
    }
    let rendered = Instant::now();
    terminal.flush()?;
    terminal.hide_cursor()?;
    terminal.swap_buffers();
    ratatui::backend::Backend::flush(terminal.backend_mut())?;
    drop(ours);
    let end = Instant::now();
    ui.trace.frame(end, rendered.duration_since(start), end.duration_since(rendered), events);
    Ok(true)
}

/// One input event: a mouse event (a move changes nothing), a resize (draw again), a key press.
fn handle_event(terminal: &mut Terminal<CrosstermBackend<Stdout>>, cx: &Ctx, ui: &mut Ui, ev: Event) -> Result<Flow> {
    Ok(match ev {
        Event::Mouse(m) => {
            if handle_mouse(m, ui, cx.shared) {
                Flow::Changed
            } else {
                Flow::Same
            }
        }
        Event::Resize(..) => Flow::Changed,
        // Key releases and repeats on terminals that send them: ignored.
        Event::Key(k) if k.kind == KeyEventKind::Press => return handle_key(terminal, cx, ui, k.code, k.modifiers),
        _ => Flow::Same,
    })
}

/// One key press, in whatever view or popup is open. Never awaits: a slow action is started
/// in the background ([`start_action`], `runs::start`, …) and lands through `Shared`.
fn handle_key(terminal: &mut Terminal<CrosstermBackend<Stdout>>, cx: &Ctx, ui: &mut Ui, code: KeyCode, mods: KeyModifiers) -> Result<Flow> {
    let flow = handle_key_in(terminal, cx, ui, code, mods);
    // An action the backups screen started has ended (its result or confirm closed): back to it.
    backups::come_back(ui, cx.shared);
    flow
}

fn handle_key_in(terminal: &mut Terminal<CrosstermBackend<Stdout>>, cx: &Ctx, ui: &mut Ui, code: KeyCode, mods: KeyModifiers) -> Result<Flow> {
    let Ctx { shared, provider, interval, nudge, details_wanted } = *cx;
    // Ctrl+C quits from any mode (raw mode swallows the usual SIGINT) — asking first while a run
    // that changes pods is going; at that question it quits, leaving the run going.
    if mods.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
        if matches!(ui.mode, Mode::Quit) {
            return Ok(match runs::key(ui, shared, KeyCode::Char('k')) {
                runs::Flow::Quit(notes) => Flow::Quit(notes),
                _ => Flow::Quit(Vec::new()),
            });
        }
        return Ok(if runs::quit_or_ask(ui, shared) { Flow::Quit(Vec::new()) } else { Flow::Changed });
    }
    if let Some(why) = demo_refuses(ui, code) {
        shared.lock().unwrap().notify(why, true);
        return Ok(Flow::Changed);
    }
    if let Some(changed) = info_key(ui, code) {
        return Ok(if changed { Flow::Changed } else { Flow::Same });
    }
    // `?` and its two popups (ask.rs); the add-pod form and its confirm (newpod.rs); the runs
    // panel, `p`, and the quit question (runs.rs).
    if ask::key(ui, shared, code, mods) || newpod::key(ui, shared, code) || backups::key(ui, shared, provider, code) {
        return Ok(Flow::Changed);
    }
    match runs::key(ui, shared, code) {
        runs::Flow::Handled => return Ok(Flow::Changed),
        runs::Flow::Quit(notes) => return Ok(Flow::Quit(notes)),
        runs::Flow::NotOurs => {}
    }
    let len = shared.lock().unwrap().pods.len();
    match ui.mode.clone() {
        Mode::List | Mode::Detail => match list_key(ui, code, len) {
            ListKey::Quit => return Ok(if runs::quit_or_ask(ui, shared) { Flow::Quit(Vec::new()) } else { Flow::Changed }),
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
                        start_deep_check(shared, ui, pods);
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
                        start_deep_check(shared, ui, pods);
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
                    connect_ssh(terminal, shared, ui)?;
                    nudge.notify_one();
                }
                KeyCode::Char('n') => newpod::open(ui, shared),
                _ => return Ok(Flow::Same),
            },
        },
        Mode::Menu { mut sel } => {
            let rows = {
                let s = shared.lock().unwrap();
                aikey::pod_rows(&s, ui, s.pods.get(ui.selected)).0
            };
            let n = rows.len();
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
                KeyCode::Enter => choose_pod_action(shared, ui, provider, rows[sel.min(n - 1)].1),
                KeyCode::Char(ch) => match rows.iter().find(|(k, _)| *k == ch) {
                    Some((_, action)) => choose_pod_action(shared, ui, provider, *action),
                    None => ui.mode = Mode::Menu { sel },
                },
                _ => ui.mode = Mode::Menu { sel },
            }
        }
        Mode::Confirm(mut c) => {
            let typed = c.action.requires_typed_name();
            match code {
                KeyCode::Esc => ui.mode = Mode::List,
                KeyCode::Enter if c.is_satisfied() => apply_action(shared, ui, provider, &c),
                KeyCode::Char('y') if !typed => apply_action(shared, ui, provider, &c),
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
            let actions = Action::fleet_menu(scope == Scope::Marked, ui.ai_keys);
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
                    choose_fleet_action(shared, ui, provider, actions[sel.min(n - 1)].1, scope);
                }
                KeyCode::Char(ch) => match actions.iter().find(|(k, _)| *k == ch) {
                    Some((_, action)) => choose_fleet_action(shared, ui, provider, *action, scope),
                    None => ui.mode = Mode::FleetMenu { scope, sel },
                },
                _ => ui.mode = Mode::FleetMenu { scope, sel },
            }
        }
        Mode::FleetConfirm { action, mut typed, scope, note } => {
            let token = fleet_confirm_token(shared, &ui.marked, scope, action);
            match code {
                KeyCode::Esc => ui.mode = Mode::List,
                KeyCode::Enter if typed == token && action.ai_key() => {
                    let keyed = aikey::keyed(&std::mem::take(&mut ui.ai_set));
                    aikey::start(ui, shared, action, &keyed, None, None);
                }
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
        Mode::Input { action, scope, mut value } => match code {
            KeyCode::Esc => ui.mode = Mode::List,
            KeyCode::Enter if !value.trim().is_empty() => submit_input(shared, ui, provider, action, scope, value),
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
            KeyCode::Enter => apply_select(ui, &mut shared.lock().unwrap(), &value),
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
        // Handled above: `info_key`, `ask::key`, `newpod::key`, `runs::key`.
        Mode::Info { .. } | Mode::Ask { .. } | Mode::Answer(_) | Mode::NewPod(_) | Mode::UpConfirm(_) | Mode::Runs { .. } | Mode::Quit | Mode::Backups(_) => {}
        Mode::Result(_) => {
            if !result_key(ui, code) {
                ui.result_scroll = 0;
                ui.mode = Mode::List;
                nudge.notify_one();
            }
        }
    }
    Ok(Flow::Changed)
}

/// Enter in an input box: `action` with `value` on its pod or its set — one of the CLI's commands
/// as a run (`runs.rs`), the rest in the background. A cap the box shows as refused (an amount
/// that isn't one, a cap under the spend) stays in the box, which says why.
fn submit_input(shared: &Arc<Mutex<Shared>>, ui: &mut Ui, provider: &Arc<dyn Provider>, action: Action, scope: InputScope, value: String) {
    if action.ai_key() {
        // The box's own pods and keys (`Ui::ai_set`), as previewed.
        if aikey::limit_preview(&ui.ai_set, &value).is_err() {
            ui.mode = Mode::Input { action, scope, value };
            return;
        }
        let keyed = aikey::keyed(&std::mem::take(&mut ui.ai_set));
        let own = match &scope {
            InputScope::Pod { id, .. } => shared.lock().unwrap().pods.iter().find(|p| &p.id == id).map(|p| p.provider.clone()),
            InputScope::Set(_) => None,
        };
        return aikey::start(ui, shared, action, &keyed, own.as_deref(), Some(&value));
    }
    let (provider, remote, cfg) = (provider.clone(), ui.remote.clone(), ui.cfg.clone());
    match scope {
        InputScope::Pod { name, id } => {
            let pod = {
                let s = shared.lock().unwrap();
                s.pods.iter().find(|p| p.id == id).cloned()
            };
            match pod {
                None => ui.mode = Mode::Result(format!("{name} is gone — refresh")),
                Some(pod) if action.cli() => start_cli_action(ui, shared, action, &pod, Some(&value)),
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

/// The info panel's keys ([`Mode::Info`]): Esc, `i` or `q` go back to the view it was opened
/// from; ↑↓ (j k), PgUp/PgDn (space), Home/End scroll it. `None`: the panel isn't open;
/// `Some(false)`: a key it ignores.
fn info_key(ui: &mut Ui, code: KeyCode) -> Option<bool> {
    let Mode::Info { scroll, detail } = ui.mode else { return None };
    let max = ui.result_max.get();
    let to = |delta: i32| Mode::Info { scroll: scroll_by(scroll, max, delta), detail };
    ui.mode = match code {
        KeyCode::Esc | KeyCode::Char('i' | 'q') => if detail { Mode::Detail } else { Mode::List },
        KeyCode::Up | KeyCode::Char('k') => to(-1),
        KeyCode::Down | KeyCode::Char('j') => to(1),
        KeyCode::PageUp => to(-10),
        KeyCode::PageDown | KeyCode::Char(' ') => to(10),
        KeyCode::Home => Mode::Info { scroll: 0, detail },
        KeyCode::End => Mode::Info { scroll: max, detail },
        _ => return Some(false),
    };
    Some(true)
}

/// In a demo (`ARENA_TUI_DEMO`) the keys that would act on a pod, reach one, ask about one or
/// create one are refused — with why, for the footer. The view's own keys all work.
fn demo_refuses(ui: &Ui, code: KeyCode) -> Option<&'static str> {
    if !ui.demo {
        return None;
    }
    // The add-pod form opens (it reads nothing); what it would run is refused by `runs.rs`.
    let refused = matches!(ui.mode, Mode::List | Mode::Detail) && matches!(code, KeyCode::Char('a' | 'A' | 'c' | 'd' | 'D' | '?'));
    refused.then_some("demo: the pods are made up — nothing to act on, reach or create")
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
/// ([`Provider::supports`] — Shadeform: it bills until it's deleted) is skipped and named,
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
        } else if let Some(why) = provider.supports(Op::Stop, Some(&p)).refusal() {
            plan.no_stop.push((p, why));
        } else {
            plan.stop.push(p);
        }
    }
    plan
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

/// A menu pick for the cursor pod: run/set-branch collect an argument first; everything
/// else goes through a confirm modal. Shared by the arrow-Enter and letter-key paths. An action
/// the pod's provider can't do — the capability table's answer (`Provider::supports`), the one
/// the CLI refuses on, and why the menu greys the row out — is refused here, before a confirm or
/// a new name is asked for: a stop of a Shadeform pod (no stop: the confirm's "keeps no data"
/// warning would only mislead), say.
fn choose_pod_action(shared: &Arc<Mutex<Shared>>, ui: &mut Ui, provider: &Arc<dyn Provider>, action: Action) {
    let Some(pod) = selected_pod(shared, ui.selected) else {
        ui.mode = Mode::List;
        return;
    };
    let shown = ui.shown_name(&pod.name);
    if action.ai_key() {
        return aikey::choose(shared, ui, pod, shown, action);
    }
    if let Some(why) = action.op().and_then(|op| provider.supports(op, Some(&pod)).refusal()) {
        ui.mode = Mode::Result(format!("✗ can't {} {} — {why}", action.label(), pod.name));
        return;
    }
    match action {
        // Read-only: straight to the runs panel.
        Action::Jobs | Action::Logs => return start_cli_action(ui, shared, action, &pod, None),
        Action::Start if arena_core::status::is_billing(&pod.status) => {
            ui.mode = Mode::Result(format!("✗ {} is {} — start is for a stopped pod", pod.name, pod.status));
            return;
        }
        _ => {}
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
        _ if action.ai_key() => aikey::choose_set(shared, ui, action, scope),
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
        _ => {
            // A restart or terminate of a set with a pod its provider can't do it for (none can't
            // today) is refused whole, naming each — as the CLI refuses it.
            let cant: Vec<String> = match action.op() {
                Some(op) => scope_pods(shared, &ui.marked, scope)
                    .iter()
                    .filter_map(|p| provider.supports(op, Some(p)).refusal().map(|why| format!("{} — {why}", p.name)))
                    .collect(),
                None => Vec::new(),
            };
            ui.mode = if cant.is_empty() {
                Mode::FleetConfirm { action, typed: String::new(), scope, note: String::new() }
            } else {
                Mode::Result(format!("✗ can't {}:\n{}", action.label(), cant.join("\n")))
            };
        }
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
    if c.action.ai_key() {
        // The key as the confirm showed it (review codex r3 #1), not as read since.
        return aikey::start(ui, shared, c.action, &c.ai, Some(&pod.provider), None);
    }
    if c.action.cli() {
        return start_cli_action(ui, shared, c.action, &pod, None);
    }
    ui.mode = Mode::Working(format!("{} {}…", c.action.doing(), pod.name));
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

/// The fleet's spend ledger (`arena spend`): its state file — none under test unless the
/// test's config sets `ARENA_STATE_DIR` (a test never touches the developer's own).
fn spend_path(cfg: &Config) -> Option<std::path::PathBuf> {
    if cfg!(test) && cfg.get("ARENA_STATE_DIR").is_none() {
        return None;
    }
    snapshot::state_file_path_for(cfg, arena_core::spend::SPEND_FILE).ok()
}

/// Record a stop or terminate the dashboard just made in the spend ledger (quietly).
fn record_spend(cfg: &Config, ev: &arena_core::spend::Event) {
    let now = snapshot::unix_now();
    arena_core::spend::record_quietly(spend_path(cfg).as_deref(), now, |s| arena_core::spend::apply_event(s, ev, now));
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
            Ok(outcome) => {
                record_spend(cfg, &arena_core::spend::Event::Stopped { provider: Some(&pod.provider), id: &pod.id });
                match outcome.note() {
                    Some(note) => format!("✓ stopped {name} — ⚠ {note}"),
                    None => format!("✓ stopped {name}"),
                }
            }
            Err(e) => format!("✗ stop {name} failed: {}", lock::explain(name, &e)),
        },
        Action::Terminate => match provider.terminate_pod(&pod.id).await {
            Ok(()) => {
                record_spend(cfg, &arena_core::spend::Event::Terminated { provider: Some(&pod.provider), id: &pod.id });
                format!("✓ terminated {name}")
            }
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
        // Runs of the CLI (`start_cli_action`), never an action of this one.
        Action::Start | Action::Rename | Action::Jobs | Action::Logs | Action::Pull | Action::KeyLimit | Action::KeyPause | Action::KeyResume | Action::KeyRotate => {
            format!("✗ {} {name}: runs as `arena {}`", action.label(), action.command())
        }
    }
}

/// The `arena` command for one of the CLI's actions on `name` (`arg`: rename's new name): `jobs`
/// and `logs` read; `start` and `rename` change the pod, after the dashboard's confirm or input
/// (`--yes`). `None`: not one of them. Pure.
fn cli_action_args(action: Action, name: &str, arg: Option<&str>) -> Option<(runs::Kind, Vec<String>)> {
    let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    Some(match action {
        Action::Start => (runs::Kind::Start, v(&["pods", "start", name, "--yes"])),
        Action::Rename => (runs::Kind::Rename, v(&["pods", "rename", name, arg?.trim(), "--yes"])),
        Action::Jobs => (runs::Kind::Jobs, v(&["pods", "jobs", name])),
        Action::Logs => (runs::Kind::Logs, v(&["pods", "logs", name, "--tail", "100"])),
        Action::Pull => (runs::Kind::Pull, v(&["pods", "pull", name, "--yes"])),
        _ => return None,
    })
}

/// Run one of the CLI's actions on `pod` in the background (`runs.rs`) and open the runs panel
/// on it — or say why it can't run.
fn start_cli_action(ui: &mut Ui, shared: &Arc<Mutex<Shared>>, action: Action, pod: &Pod, arg: Option<&str>) {
    if action.ai_key() {
        let keyed = aikey::keyed(&aikey::snapshot(&shared.lock().unwrap(), std::slice::from_ref(pod)));
        return aikey::start(ui, shared, action, &keyed, Some(&pod.provider), arg);
    }
    let Some((kind, args)) = cli_action_args(action, &pod.name, arg) else { return };
    let title = format!("{} {}", action.command(), ui.shown_name(&pod.name));
    // The pod's own provider: the CLI finds a pod fleet-wide whatever `--provider` says, but its
    // messages then name the right one.
    let provider = if pod.provider.is_empty() { ui.provider_name.clone() } else { pod.provider.clone() };
    ui.mode = match runs::start(ui, shared, kind, &provider, args, title) {
        Ok(id) => Mode::Runs { id, scroll: None, stop: false },
        Err(e) => Mode::Result(format!("✗ {e}")),
    };
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
    // The CLI's backup dir (`LOCAL_BACKUP_DIR`, else the fleet's `backup/`), so the lock is the same one.
    let (dir, _) = cfg.local_backup_dir();
    let _lock = match arena_core::pull::try_pod_lock_now(&dir, &pod.name) {
        Ok(lock) => lock,
        Err(()) => return (format!("⊘ {} — busy: another backup of it (a pull, the cron or the reaper) is running; try again shortly", pod.name), None),
    };
    let repo = backup_repo_path(cfg);
    let origin = backup::configured_repo(cfg);
    let cmd = backup::backup_command(&repo, cfg.get("GIT_SSH_KEY_REMOTE"), &format!("arena-tui backup {}", pod.name), origin.as_deref());
    use arena_core::backups::record::Outcome;
    let note = |outcome, detail: String| backups::note_git(cfg, &pod.name, outcome, detail);
    match remote.exec(&target, &cmd, Some(BACKUP_TIMEOUT)).await {
        Ok(out) if out.success => {
            let report = parse_backup_report(&out.stdout);
            let notes = report.notes();
            let also = if notes.is_empty() { String::new() } else { format!(" — {}", notes.join("; ")) };
            let line = match &report.outcome {
                Some((backup::BACKUP_PUSHED, branch)) => {
                    note(Outcome::Pushed, format!("→ {branch}"));
                    format!("✓ backed up {} → {branch}{also}", pod.name)
                }
                Some((backup::BACKUP_NO_CHANGES, branch)) => {
                    note(Outcome::NoChanges, format!("on {branch}"));
                    format!("✓ {} — no changes (on {branch}){also}", pod.name)
                }
                Some((backup::BACKUP_SKIPPED, branch)) => {
                    note(Outcome::NotSaved, format!("on {branch}"));
                    format!("⊘ {} — skipped (on protected branch {branch})", pod.name)
                }
                _ => {
                    note(Outcome::Pushed, String::new());
                    format!("✓ backed up {}{also}", pod.name)
                }
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
            note(Outcome::Failed, report.failure_brief(out.code).unwrap_or_else(|| format!("{}: {}", out.exit_label(), out.stderr.trim())));
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
        Err(e) => {
            note(Outcome::Failed, describe_error(&e));
            (format!("✗ backup {} failed: {}", pod.name, describe_error(&e)), None)
        }
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
            Constraint::Length(1), // the top line: fleet summary (and a non-default provider/config)
            Constraint::Min(0),    // body
            Constraint::Length(1), // footer: [i] info · status · age · cadence, keys
        ])
        .split(f.area());

    top_line(f, shared, ui, chunks[0]);
    ui.top_at.set(Some(chunks[0].y));

    if let Mode::Backups(v) = &ui.mode {
        backups::render(f, shared, ui, v, chunks[1]);
    } else if matches!(ui.mode, Mode::Detail) || matches!(ui.mode, Mode::Info { detail: true, .. }) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(chunks[1]);
        // No room for the history sparklines in the split view.
        pods_table(f, shared, ui, cols[0], false);
        detail_pane(f, shared, ui, cols[1]);
    } else {
        pods_table(f, shared, ui, chunks[1], true);
    }

    f.render_widget(Paragraph::new(footer_hint(shared, ui, secs)), chunks[2]);
    // Where the footer's `[i] info` is (its first cell): a click there opens the info panel.
    ui.menu_at.set(Some((chunks[2].x, chunks[2].y)));

    match &ui.mode {
        Mode::Menu { sel } => render_menu(f, shared, ui, *sel),
        Mode::Confirm(c) => render_confirm(f, c),
        Mode::FleetMenu { scope, sel } => render_fleet_menu(f, shared, ui, *scope, *sel),
        Mode::FleetConfirm { action, typed, scope, note } => render_fleet_confirm(f, shared, ui, *action, typed, *scope, note),
        Mode::NewPod(form) => newpod::render(f, form, &ui.offers),
        Mode::UpConfirm(form) => newpod::render_confirm(f, form, &ui.offers),
        Mode::Runs { id, scroll, stop } => runs::render(f, shared, *id, *scroll, *stop, &ui.result_max),
        Mode::Quit => runs::render_quit(f, shared),
        Mode::Input { action, scope, value } => render_input(f, shared, ui, *action, scope, value),
        Mode::Select { value } => render_select(f, value),
        Mode::Working(msg) => render_result(f, msg, 0, &ui.result_max),
        Mode::Result(msg) => render_result(f, msg, ui.result_scroll, &ui.result_max),
        Mode::Ask { scope, value, pick } => ask::render_input(f, shared, ui, scope, value, pick),
        Mode::Answer(answer) => ask::render_answer(f, answer, ui.result_scroll, &ui.result_max),
        Mode::Info { detail, .. } => render_info(f, shared, ui, *detail, secs),
        _ => {}
    }
}

/// The top line ([`state::top_parts`]): the fleet in short, a group per provider and
/// OpenRouter's, laid out for the terminal's width — the least needed words left out, never a
/// warning. A provider other than RunPod (where the add-pod form starts) or a config other than
/// the default goes at the right while there's room (always in the info panel): a dashboard on
/// a second fleet should say so. A click on it opens the info panel ([`handle_mouse`]).
fn top_line(f: &mut Frame, shared: &Shared, ui: &Ui, area: Rect) {
    let tag = where_tag(ui);
    let top = top_parts(&shared.summary, &shared.pods, &shared.snap.partial, &shared.balance, shared.ai_cohort.as_ref(), tag.as_deref(), area.width as usize);
    let spans: Vec<Span> = top
        .parts
        .into_iter()
        .map(|p| match p.tone {
            Some(t) => Span::styled(p.text, tone_style(t)),
            None => Span::raw(p.text),
        })
        .collect();
    f.render_widget(Paragraph::new(Line::from(spans).style(Style::default().add_modifier(Modifier::BOLD))), area);
    if let Some(tag) = top.tag {
        let w = Span::raw(&tag).width() as u16 + 1;
        let at = Rect { x: area.x + area.width.saturating_sub(w), width: w.min(area.width), ..area };
        f.render_widget(Paragraph::new(Span::styled(tag, Style::default().fg(Color::Cyan))), at);
    }
}

/// The provider (when not RunPod) and the config (when not the default), for the top line's
/// right end: `vast · ~/sandbox/config.env`; `None` when both are the defaults.
fn where_tag(ui: &Ui) -> Option<String> {
    let mut out = Vec::new();
    if ui.provider_name != "runpod" {
        out.push(ui.provider_name.clone());
    }
    if ui.config_path != DEFAULT_CONFIG {
        out.push(home_short(&ui.config_path));
    }
    (!out.is_empty()).then(|| out.join(" · "))
}

/// A path with the home directory as `~`.
fn home_short(path: &str) -> String {
    match std::env::var("HOME").ok().filter(|h| !h.is_empty() && h != "/") {
        Some(h) => match path.strip_prefix(h.trim_end_matches('/')) {
            Some(rest) if rest.starts_with('/') => format!("~{rest}"),
            _ => path.to_string(),
        },
        None => path.to_string(),
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
    // The raise-cap box: what the key spent of its cap and the new cap as typed — or why Enter
    // won't run it.
    let preview = match action {
        Action::KeyLimit => match aikey::limit_preview(&ui.ai_set, value) {
            Ok(lines) => format!("{}\n\n", lines.join("\n")),
            Err(why) => format!("✗ {}\n\n", why.join("\n")),
        },
        _ => String::new(),
    };
    let text = format!(
        "{}\n\n{preview}{}\n\n  > {value}\n\n[enter] run  [esc] cancel",
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

/// A pod's `AI$` cell: its OpenRouter key's spend against the cap (`-`: no key named after
/// it), yellow from 80% of the cap, red at the cap or disabled, a grey `paused` for a $0 cap
/// nothing was spent against — grey when the last listing failed (`stale`: these are older
/// numbers, the summary bar says `(n/a)`).
fn ai_cell(k: Option<&KeyUsage>, stale: bool, read: bool) -> Cell<'static> {
    match k {
        // Before the first read lands: not known yet, rather than "no key".
        None if !read => Cell::from("…").style(tone_style(Tone::Dim)),
        None => Cell::from("-").style(tone_style(Tone::Dim)),
        Some(k) if stale || k.paused() => Cell::from(k.cell()).style(tone_style(Tone::Dim)),
        Some(k) => {
            let style = state::fill_tone(k.fill()).map(tone_style).unwrap_or_default();
            Cell::from(k.cell()).style(style)
        }
    }
}

/// The GPU column: the probe's count and card (`2×RTX A4000`) — the provider's when it wasn't
/// read (`fleet::gpu_label`) — with each GPU's VRAM (`16G`); `RTX ` dropped when `narrow`. A MIG
/// slice is named as every surface names it, `1×B300 MIG 34G`: nvidia-smi names only the parent
/// card under MIG (`B300 SXM6 AC`, its memory unreadable, one row for two slices of it), so the
/// slice's name, memory and count come from the provider (`fleet::mig_slice`) — the probe's
/// rows only when the provider gave no count (review ai-review codex r1 #1). Pure.
fn gpu_text(p: &Pod, m: Option<&PodMetrics>, narrow: bool) -> String {
    let probed = m.map(|m| m.gpus.len()).filter(|&n| n > 0);
    if let Some((model, gb)) = p.gpu_type.as_deref().and_then(fleet::mig_slice) {
        let n = p.gpu_count.filter(|&n| n > 0).map(|n| n as usize).or(probed).unwrap_or(1);
        return format!("{n}×{model} MIG {gb}G");
    }
    let mut gpu = m.and_then(|m| m.gpu_summary()).unwrap_or_else(|| fleet::gpu_label(p));
    if narrow {
        gpu = gpu.replace("RTX ", "");
    }
    if gpu != "-" {
        if let Some(g) = m.and_then(|m| m.vram_gb()) {
            gpu = format!("{gpu} {g}G");
        }
    }
    gpu
}

fn pods_table(f: &mut Frame, shared: &Shared, ui: &Ui, area: Rect, with_spark: bool) {
    // Which columns show at this width, and how wide: `columns.rs` (its rules, in order, are
    // there). PROXY is as wide as its widest cell, never under its header.
    let proxy_w = shared.snap.pods.iter().map(|sp| Span::raw(proxy_cell(sp).0).width()).max().unwrap_or(0);
    // AI$: each pod's OpenRouter key, spent against its cap — only with a provisioning key.
    // PROGRESS is what PROGRESS_CMD reports, so only with one set.
    let want_progress = ui.cfg.get("PROGRESS_CMD").is_some_and(|c| !c.trim().is_empty());
    let c = columns::columns(area.width as usize, proxy_w, shared.ai.is_some(), want_progress, with_spark);
    let now = shared.snap.generated_at;

    let mut header_cells = vec!["", "P", "NAME", "STATUS", "M", "SET", "GPU", "GPU%"];
    for (show, title) in [(c.mem, "MEM"), (c.temp, "TEMP"), (c.disk, "DISK")] {
        if show {
            header_cells.push(title);
        }
    }
    header_cells.extend(["$/H", "HLTH", "PROXY"]);
    if c.rtt {
        header_cells.push("RTT");
    }
    if c.branch {
        header_cells.push("BRANCH");
    }
    if c.saved {
        header_cells.push(columns::SAVED_HEADER);
    }
    if c.ai {
        header_cells.push("AI$");
    }
    if c.host {
        header_cells.push("CPU%");
        header_cells.push("RAM%");
    }
    if c.progress {
        header_cells.push("PROGRESS");
    }
    if c.spark {
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
            let (proxy, proxy_tone) = sp.map(proxy_cell).unwrap_or(("?".into(), Tone::Dim));
            let gpu = gpu_text(p, m, c.narrow_gpu);
            let branch = match m.and_then(|m| m.branch.clone()) {
                Some(b) => truncate(&short_branch(&b, &ui.prefix), 6),
                None => "-".into(),
            };
            // RUNNING-but-unreachable reads as "init" (still coming up), in yellow.
            let status_label = pod_status(p, m.map(|m| m.error.is_none()));
            let status_cell = Cell::from(status_label.clone()).style(status_style(&status_label));
            let mark = if ui.marked.contains(&mark_key(p)) {
                Cell::from("•").style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
            } else {
                Cell::from(" ")
            };
            let mut cells = vec![
                mark,
                provider_cell(&p.provider),
                // Force the short name when cramped, regardless of the persisted pref.
                Cell::from(if c.compact_names {
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
            if c.mem {
                cells.push(Cell::from(mem));
            }
            if c.temp {
                cells.push(Cell::from(temp_str).style(temp_style(temp)));
            }
            if c.disk {
                cells.push(Cell::from(disk).style(capacity_style(disk_pct)));
            }
            cells.extend([
                Cell::from(cost),
                Cell::from(health).style(tone_style(health_tone)),
                Cell::from(proxy).style(tone_style(proxy_tone)),
            ]);
            if c.rtt {
                let (rtt, tone) = rtt_cell(shared.rtts.of(p));
                cells.push(Cell::from(rtt).style(tone_style(tone)));
            }
            if c.branch {
                cells.push(Cell::from(branch));
            }
            if c.saved {
                // A sync glyph (↑ unpushed / ↓ behind / ⇕ diverged) sits right before the
                // time, so a blocked/failed push is obvious next to "when it was saved".
                let (sync, sync_style) = sync_marker(m);
                cells.push(Cell::from(Line::from(vec![
                    Span::styled(sync, sync_style),
                    Span::styled(rel_time_short(m), saved_style(m)),
                ])));
            }
            if c.ai {
                let ai = shared.ai.as_ref();
                cells.push(ai_cell(
                    ai.and_then(|a| a.keys.get(&p.name)),
                    ai.is_some_and(|a| a.keys_error.is_some()),
                    ai.is_some_and(|a| a.read),
                ));
            }
            if c.host {
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
            if c.progress {
                let (detail, detail_style) = detail_cell(m);
                cells.push(Cell::from(detail).style(detail_style));
            }
            if c.spark {
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
                cells.push(Cell::from(spark(&gpu_hist, c.spark_w)).style(Style::default().fg(gpu_c)));
                cells.push(Cell::from(spark(&mem_hist, c.spark_w)).style(Style::default().fg(mem_c)));
            }
            Row::new(cells)
        })
        .collect();

    let mut widths = vec![
        Constraint::Length(1),  // mark (•)
        Constraint::Length(1),  // P (provider glyph)
        Constraint::Length(c.name_w as u16), // NAME (auto-short when cramped)
        Constraint::Length(4),  // STATUS (abbreviated: run/exit/stop…)
        Constraint::Length(1),  // M (host maintenance badge; L = locked)
        Constraint::Length(4),  // SET (.name/key/origin/api)
        Constraint::Length(c.gpu_w as u16), // GPU (count, card, VRAM: `2×A4000 16G`, `1×B300 MIG 34G`)
        Constraint::Length(4),  // GPU% (`100%`)
    ];
    if c.mem {
        widths.push(Constraint::Length(columns::MID_W[2] as u16)); // MEM (e.g. "120/240G")
    }
    if c.temp {
        widths.push(Constraint::Length(columns::MID_W[4] as u16)); // TEMP (e.g. "85C")
    }
    if c.disk {
        widths.push(Constraint::Length(columns::MID_W[0] as u16)); // DISK (e.g. "12/100G")
    }
    widths.extend([
        Constraint::Length(6),                        // $/H (e.g. "$0.17", "€0.006", "$21.52")
        Constraint::Length(columns::HEALTH_W as u16), // HLTH (e.g. "✓12m", "✗2h", "…")
        Constraint::Length(c.proxy_w as u16),         // PROXY (e.g. "9500", "9501!" stale)
    ]);
    if c.rtt {
        widths.push(Constraint::Length(columns::MID_W[3] as u16)); // RTT (e.g. "23ms", "180ms")
    }
    if c.branch {
        widths.push(Constraint::Length(columns::MID_W[1] as u16)); // BRANCH (e.g. "w1d2")
    }
    if c.saved {
        widths.push(Constraint::Length(columns::SAVED_W as u16)); // 💾 SAVED (e.g. "3h", "↑2d")
    }
    if c.ai {
        widths.push(Constraint::Length(columns::AI_W as u16)); // AI$ (e.g. "1.23/5")
    }
    if c.host {
        widths.push(Constraint::Length(4)); // CPU%
        widths.push(Constraint::Length(4)); // RAM%
    }
    if c.progress {
        // Fixed budget when sparks are filling the rest; otherwise flex to fill.
        widths.push(if c.spark { Constraint::Length(columns::PROGRESS_W as u16) } else { Constraint::Min(10) });
    }
    if c.spark {
        widths.push(Constraint::Length(c.spark_w as u16)); // GPU% history
        widths.push(Constraint::Length(c.spark_w as u16)); // MEM% history
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
    // And where the 💾 header landed: a click on it opens the backups screen.
    let header_y = area.y.saturating_add(1);
    let buf = f.buffer_mut();
    let saved = (area.x..area.x.saturating_add(area.width)).find(|&x| header_y < buf.area.bottom() && buf[(x, header_y)].symbol() == columns::SAVED_HEADER);
    ui.backups.saved_at.set(saved.map(|x| (x, header_y)));
}

/// STATUS's colour (the table's and the detail pane's): `run` plain (most rows), a pod coming up
/// — `init` (listed running, not reachable yet), `prov`, `new`, `rstr` — yellow, `no-ssh` red,
/// a stopped pod grey.
fn status_style(label: &str) -> Style {
    match label {
        "init" | "prov" | "new" | "rstr" => Style::default().fg(Color::Yellow),
        "no-ssh" => Style::default().fg(Color::Red),
        "exit" | "stop" | "term" => Style::default().fg(Color::DarkGray),
        _ => Style::default(),
    }
}

/// SAVED's time colour: yellow when there's uncommitted work; grey when unknown OR when it's a
/// clean tree that's been quiet for a day+ (nothing to do); plain otherwise.
fn saved_style(m: Option<&PodMetrics>) -> Style {
    let stale = m
        .and_then(|m| m.last_commit)
        .map(|ts| now_unix().saturating_sub(ts) > 86_400)
        .unwrap_or(false);
    match m.and_then(|m| m.dirty_files) {
        Some(n) if n > 0 => Style::default().fg(Color::Yellow),
        Some(0) if stale => Style::default().fg(Color::DarkGray),
        None => Style::default().fg(Color::DarkGray),
        _ => Style::default(),
    }
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

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(format!(" {} ", ui.shown_name(&pod.name)), Style::default().add_modifier(Modifier::BOLD)));
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
    // In the AI$ cell's colour ([`ai_cell`]): yellow from 80% of the cap, red at it, grey when
    // paused, unknown or as last read.
    let ai = shared.ai.as_ref().map(|a| match (a.keys.get(&pod.name), a.read, a.keys_error.as_deref()) {
        (None, false, _) => ("… (reading OpenRouter's keys)".to_string(), Some(Tone::Dim)),
        (None, _, Some(e)) => (format!("? (OpenRouter's keys couldn't be read: {e})"), Some(Tone::Warn)),
        (None, _, None) => ("- (no OpenRouter key named after this pod — `arena keys gen` makes one)".to_string(), Some(Tone::Dim)),
        (Some(k), _, None) if k.paused() => (k.detail(), Some(Tone::Dim)),
        // Where to act on it — this pod's menu (`a`), its `ai …` rows — said first: the pane cuts
        // the line's end.
        (Some(k), _, None) if k.disabled && ui.ai_keys => {
            (k.detail().replacen("disabled · ", "disabled (a → ai resume) · ", 1), state::fill_tone(k.fill()))
        }
        (Some(k), _, None) if k.fill() == arena_core::openrouter::Fill::Full && ui.ai_keys => {
            (format!("at its cap (a → raise cap) · {}", k.detail()), state::fill_tone(k.fill()))
        }
        (Some(k), _, None) => (k.detail(), state::fill_tone(k.fill())),
        (Some(k), _, Some(_)) => (format!("{} (as last read; the latest read failed)", k.detail()), Some(Tone::Dim)),
    });
    // The cohort's keys at their cap, under it, while there are any: every name (the summary
    // bar has room for two) and where to look — a key can be at its cap with no pod in the
    // table (its machine terminated or renamed).
    let caps = shared.ai.as_ref().and(shared.ai_cohort.as_ref()).filter(|c| !c.capped.is_empty()).map(|c| {
        let names: Vec<String> = c.capped.iter().map(|k| if k.pod { k.name.clone() } else { format!("{} (no pod)", k.name) }).collect();
        // Where to look first: a long list is cut at the pane's edge.
        format!("⚠ {} at cap (`arena keys list` has each one's cap and spend): {}", count_of(names.len(), "key", "keys"), names.join(", "))
    });
    let fact_lines = FACT_LINES + u16::from(ai.is_some()) + u16::from(caps.is_some());
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

    // Each fact a line: its label dim, its value in the tone the table gives it (the same
    // helpers), so a glance at the colours says good/warn/bad — the words still say it all.
    let dim = Style::default().fg(Color::DarkGray);
    let plain = Style::default();
    let fact = |label: &str, spans: Vec<Span<'static>>| {
        let mut l = vec![Span::styled(format!("{label:<10}"), dim)];
        l.extend(spans);
        Line::from(l)
    };
    let one = |text: String, style: Style| vec![Span::styled(text, style)];
    // A value that's `-` (nothing to say) is dim.
    let or_dim = |text: String, style: Style| {
        let style = if text == "-" { dim } else { style };
        vec![Span::styled(text, style)]
    };
    let glyph = |b: Option<bool>| match b {
        Some(true) => Span::styled("✓", tone_style(Tone::Good)),
        Some(false) => Span::styled("✗", tone_style(Tone::Bad)),
        None => Span::styled("·", dim),
    };

    let endpoint = match (&pod.ssh_ip, pod.ssh_port) {
        (Some(ip), Some(port)) => one(format!("{ip}:{port}"), plain),
        _ => one("(no SSH endpoint yet)".into(), tone_style(Tone::Warn)),
    };
    // Where it runs (the provider's name for the place) and how far that is from here — the
    // proxy box, on prod — as the RTT column times it.
    let place = pod.location.clone().unwrap_or_else(|| "-".into());
    let (_, rtt_tone) = rtt_cell(shared.rtts.of(pod));
    let rtt = match (shared.rtts.of(pod), &pod.ssh_ip) {
        (Some(d), _) => format!("rtt {}ms from this box", d.as_millis()),
        (None, Some(_)) => format!("rtt - (a TCP connect to the SSH port, every {}s)", RTT_EVERY.as_secs()),
        (None, None) => "rtt -".into(),
    };
    let progress = match m {
        Some(m) if m.progress.is_some() => one(m.progress.clone().unwrap_or_default(), plain),
        Some(m) if m.error.is_some() => one(m.error.clone().unwrap_or_default(), tone_style(Tone::Bad)),
        _ => one("-".into(), dim),
    };
    let gpu = gpu_detail(pod, m);
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
    let proxy = match sp {
        Some(sp) => one(fit(proxy_detail(sp)), tone_style(proxy_cell(sp).1)),
        None => one("?".into(), dim),
    };
    // The last deep check: verdict + age + worst issue as `arena snapshot` words it, and
    // the first failing/warning check's own words (the operator's view, so the raw text) —
    // both in the HLTH cell's colour.
    let record = sp.and_then(|sp| sp.health.as_ref());
    let health_style = match record {
        _ if shared.deep.is_running(pod) => tone_style(Tone::Busy),
        None => dim,
        Some(h) => tone_style(state::health_tone(h.status, shared.snap.generated_at.saturating_sub(h.checked_at))),
    };
    let health = match record {
        _ if shared.deep.is_running(pod) => "checking… (deep check running in the background)".to_string(),
        None => "- (never deep-checked — d runs one)".to_string(),
        Some(h) => format!(
            "{}  (checked {})",
            snapshot::health_label(Some(h), shared.snap.generated_at),
            snapshot::rfc3339(h.checked_at)
        ),
    };
    let why = fit(record.and_then(|h| h.reasons.first().cloned()).unwrap_or_else(|| "-".into()));
    let maint = fit(fleet::maintenance_label(pod.maintenance.as_ref()));
    let disk = match m.and_then(|m| m.disk_summary()) {
        Some((u, t)) => {
            let pct = if t > 0 { u as f64 / t as f64 * 100.0 } else { 0.0 };
            one(format!("{:.1}/{:.1}G ({pct:.0}%)", u as f64 / 1024.0, t as f64 / 1024.0), capacity_style(Some(pct as u32)))
        }
        None => one("-".into(), dim),
    };
    let origin = m.and_then(|m| m.origin.clone()).unwrap_or_else(|| "-".into());
    let origin_ok = m.and_then(|m| m.origin.as_deref().map(|o| o.contains("github.com")));
    // Host: CPU% and RAM (exact GB + %), coloured as the CPU%/RAM% columns.
    let host = {
        let cpu = m.and_then(|m| m.cpu_pct);
        let err = m.is_some_and(|m| m.error.is_some());
        let cpu_text = cpu.map(|c| format!("CPU {c}%")).unwrap_or_else(|| "CPU -".into());
        let ram = match m.and_then(|m| m.host_mem_summary()) {
            Some((u, t)) => {
                let pct = if t > 0 { u as f64 / t as f64 * 100.0 } else { 0.0 };
                Span::styled(format!("RAM {:.1}/{:.1}G ({pct:.0}%)", u as f64 / 1024.0, t as f64 / 1024.0), capacity_style(Some(pct as u32)))
            }
            None => Span::styled("RAM -", dim),
        };
        vec![Span::styled(cpu_text, if cpu.is_some() { util_style(cpu, err) } else { dim }), Span::raw("   "), ram]
    };
    // The sync line in the colour of SAVED's glyph: red unpushed/diverged, cyan behind, green in
    // sync, dim with no upstream.
    let sync = {
        let text = sync_summary(m);
        let (_, glyph_style) = sync_marker(m);
        let style = if text.starts_with('✓') {
            tone_style(Tone::Good)
        } else if text.starts_with('·') || text == "-" {
            dim
        } else {
            glyph_style
        };
        one(text, style)
    };
    let status = {
        let label = pod_status(pod, m.map(|m| m.error.is_none()));
        let mut v = vec![Span::styled(label.clone(), status_style(&label))];
        if arena_core::lock::is_locked(pod) {
            // One row (the facts block has a fixed height): `pods unlock` is in the docs.
            v.push(Span::raw(" · "));
            v.push(Span::styled("locked (stop/restart/terminate refused)", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
        }
        v
    };
    let mut facts = vec![
        fact("status:", status),
        fact("gpu:", or_dim(fit(gpu), plain)),
        fact("endpoint:", endpoint),
        // One row, as every fact (the block has a fixed height): the whole value cut to it (a
        // long place too — review ai-review codex r2), the RTT in its colour.
        fact("where:", {
            let line = fit(format!("{place}  ·  {rtt}"));
            let cut = place.chars().count() + 5;
            match line.char_indices().nth(cut) {
                Some((at, _)) => vec![Span::raw(line[..at].to_string()), Span::styled(line[at..].to_string(), tone_style(rtt_tone))],
                None => vec![Span::raw(line)],
            }
        }),
        fact("proxy:", proxy),
        fact("cost:", or_dim(cost, plain)),
    ];
    if let Some((text, tone)) = ai {
        facts.push(fact("ai key:", one(fit(text), tone.map(tone_style).unwrap_or(plain))));
    }
    if let Some(c) = caps {
        facts.push(fact("ai caps:", one(fit(c), tone_style(Tone::Warn))));
    }
    facts.extend([
        fact("maint:", or_dim(maint, tone_style(Tone::Warn).add_modifier(Modifier::BOLD))),
        fact("health:", one(fit(health), health_style)),
        fact("reason:", or_dim(why, health_style)),
        fact("disk:", disk),
        fact("host:", host),
        fact("branch:", or_dim(m.and_then(|m| m.branch.clone()).unwrap_or_else(|| "-".into()), plain)),
        fact("backup:", or_dim(backup_summary(m), saved_style(m))),
        fact("sync:", sync),
        fact("origin:", vec![Span::raw(truncate(&origin, 32)), Span::raw(" "), glyph(origin_ok)]),
        fact(
            "setup:",
            vec![
                Span::raw(".name "),
                glyph(m.and_then(|m| m.has_name)),
                Span::raw("   deploy-key "),
                glyph(m.and_then(|m| m.has_key)),
                Span::raw("   origin→gh "),
                glyph(origin_ok),
                Span::raw("   api-key "),
                glyph(m.and_then(|m| m.has_api_key)),
            ],
        ),
        fact("tokens:", vec![Span::raw("HF "), glyph(m.and_then(|m| m.has_hf_token)), Span::raw("   Claude-Code "), glyph(m.and_then(|m| m.has_cc_token))]),
        fact("progress:", progress),
    ]);
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
    .block(Block::default().borders(Borders::TOP).title(section("per-GPU")));
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
        .block(Block::default().borders(Borders::TOP).title(section("GPU procs")));
        f.render_widget(proc_table, rows[pi]);
    }

    // The last few ARENA_3.0 commits (= backup history), one compact line each.
    if let Some(ci) = commits_idx {
        let text = m
            .map(|m| m.recent_commits.iter().take(n_commits).cloned().collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();
        f.render_widget(
            Paragraph::new(text)
                .block(Block::default().borders(Borders::TOP).title(section("recent commits (ARENA repo)"))),
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
                .block(Block::default().borders(Borders::TOP).title(section(util_title)))
                .data(&util_data)
                .max(100)
                .style(Style::default().fg(util_colour)),
            rows[idx],
        );
        f.render_widget(
            Sparkline::default()
                .block(Block::default().borders(Borders::TOP).title(section("temp C (history)")))
                .data(&temp_data)
                .max(100)
                .style(Style::default().fg(Color::Yellow)),
            rows[idx + 1],
        );
    }
}

/// A detail-pane section's title: bold, in the accent colour.
fn section(title: &str) -> Span<'static> {
    Span::styled(title.to_string(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
}

/// The detail pane's `gpu:` line: the GPU column's words ([`gpu_text`], `RTX ` kept), and for a
/// MIG slice the provider's name with its memory spelled `GB` and the card it is cut from, as
/// nvidia-smi names it (`1×B300 MIG 34GB (MIG slice of B300 SXM6 AC)`). Pure.
fn gpu_detail(p: &Pod, m: Option<&PodMetrics>) -> String {
    match p.gpu_type.as_deref().and_then(fleet::mig_slice) {
        Some(_) => {
            let label = gpu_text(p, m, false);
            let label = label.strip_suffix('G').map(|l| format!("{l}GB")).unwrap_or(label);
            match m.and_then(|m| m.gpus.iter().find_map(|g| g.name.clone())) {
                Some(parent) => format!("{label} (MIG slice of {parent})"),
                None => label,
            }
        }
        None => gpu_text(p, m, false),
    }
}

/// The footer's first words, always in the same cells: a click on them, or `i`, opens the info
/// panel. Words, not a `☰`: the human (2026-10-30) would "slightly prefer if it was all normal text"
/// — some terminals draw those symbols as pictures.
const MENU_LABEL: &str = "[i] info";

/// The footer's one symbol, before the refresh's age and cadence (`↻ 7s ago · every 5s`), cyan
/// while a refresh runs. The human (2026-10-30), after asking for plain words: "no i think i like
/// them :) maybe even a small refresh icon or smth at the bottom right is good. its just if you
/// have too many then the text sizing becomes pretty unreliable" — so one, a single cell on
/// every terminal (`↻` has no emoji form), and the rest of the footer stays words.
const REFRESH_MARK: &str = "↻";

/// The footer, one line, cut at the terminal's edge, in plain words: [`MENU_LABEL`] first (a click
/// on it, or `i`, opens the info panel with everything that isn't here), then what must be read
/// before the next key — a live [`Notice`], a run going (`running: pods up …`), a `?` question out
/// with its seconds — then the refresh's short status (`ok · API 13/min`, `warn: …` in yellow), how
/// long ago it refreshed (`↻ 7s ago`, [`REFRESH_MARK`]; `refreshing` while it does) and its cadence
/// (`every 5s`), then the keys for the current mode: the way out first (`[q] quit`, `[esc] back`),
/// pairs merged.
fn footer_hint(shared: &Shared, ui: &Ui, secs: u64) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = vec![Span::styled(MENU_LABEL, Style::default().add_modifier(Modifier::BOLD)), Span::raw(" · ")];
    // A notice (`D`'s prompt and what it skips, a finished action) comes first: the question's
    // line can be long, and the notice is what must be read before the next key.
    if let Some(n) = shared.notice.as_ref().filter(|n| n.at.elapsed() < NOTICE_FOR) {
        let style = if n.error { tone_style(Tone::Bad) } else { tone_style(Tone::Busy) };
        spans.extend([Span::styled(without_symbol(&n.text), style), Span::raw(" · ")]);
    }
    // A run going (`pods up`…) leads too: it bills, and `p` shows it.
    if let Some(r) = runs::footer(shared) {
        spans.push(Span::styled(r, tone_style(Tone::Busy)));
    }
    // A question out, counting, so it reads as alive: an answer can take a minute, and a line that
    // never changed looked like a hang.
    if let Some(asking) = shared.ask_running.as_ref() {
        let secs = shared.ask_since.map(|t| t.elapsed()).unwrap_or_default();
        spans.push(Span::styled(format!("{asking} {}s · ", secs.as_secs()), tone_style(Tone::Busy)));
    }
    let st = &shared.status;
    if !st.short.is_empty() {
        spans.push(match st.tone {
            Some(t) => Span::styled(st.short.clone(), tone_style(t)),
            None => Span::raw(st.short.clone()),
        });
        spans.push(Span::raw(" · "));
    }
    // While a refresh runs it says so and keeps the age: a slow refresh at a short cadence would
    // otherwise hide it most of the time (pre-merge review of #44).
    let refreshed = match (shared.last_refresh, shared.refreshing) {
        (Some(t), true) => format!("refreshing, last {}s ago", t.elapsed().as_secs()),
        (None, true) => "refreshing".into(),
        (Some(t), false) => format!("{}s ago", t.elapsed().as_secs()),
        (None, false) => "-".into(),
    };
    let mark = if shared.refreshing { tone_style(Tone::Busy) } else { Style::default() };
    spans.push(Span::styled(format!("{REFRESH_MARK} "), mark));
    spans.push(Span::raw(format!("{refreshed} · every {secs}s  ")));
    spans.push(Span::styled(mode_keys(&ui.mode).to_string(), Style::default().fg(Color::Gray)));
    Line::from(spans)
}

/// A notice's text without a leading `✗`/`✓`/`⚠`: its colour already says it, and the footer is
/// plain words ([`MENU_LABEL`]). Notices are written all over with those marks; the footer drops them.
fn without_symbol(text: &str) -> String {
    let t = text.trim_start();
    ["✗ ", "✓ ", "⚠️ ", "⚠ "].iter().find_map(|m| t.strip_prefix(m)).unwrap_or(t).to_string()
}

/// The footer's keys for a mode: the few most used, pairs merged — the info panel has them all
/// ([`ALL_KEYS`]). Modal popups keep their own.
fn mode_keys(mode: &Mode) -> &'static str {
    match mode {
        Mode::List => "[q] quit  [enter] detail  [a/A] act/all  [space/x] mark/clear  [d/D] deep  [c] ssh  [?] ask  [n] new  [p] runs  [b] backups",
        Mode::Detail => "[esc] back  [q] quit  [a/A] act/all  [space/x] mark/clear  [d/D] deep  [c] ssh  [?] ask  [n] new  [p] runs  [b] backups",
        Mode::Info { .. } => "[esc/i] close  [↑↓ PgUp PgDn] scroll",
        Mode::Menu { .. } => "[↑↓] move  [enter] choose  [letter] pick  [esc] cancel",
        Mode::Confirm(c) if c.action.requires_typed_name() => "type the pod's name  [enter] apply  [esc] cancel",
        Mode::Confirm(c) if c.preview.is_some() => "[y] apply  [v] full command  [n/esc] cancel",
        Mode::Confirm(_) => "[y] apply  [n/esc] cancel",
        Mode::FleetMenu { .. } => "[↑↓] move  [enter] choose  [letter] pick  [esc] cancel",
        Mode::FleetConfirm { .. } => "type the token to confirm  [enter] apply  [esc] cancel",
        Mode::NewPod { .. } => "[↑↓] field  [←→] change  [o] availability  [enter] review  [esc] cancel",
        Mode::UpConfirm(_) => "[y] create  [n/esc] back to the form",
        Mode::Runs { .. } => "[esc] hide (a run keeps going)  [←→] other runs  [↑↓ PgUp PgDn] scroll  [end] follow  [s] stop",
        Mode::Quit => "[k] quit, leave it running  [s] quit, send it Ctrl+C  [esc] stay",
        Mode::Input { .. } => "type the value  [enter] run  [esc] cancel",
        Mode::Select { .. } => "type a selection  [enter] mark  [esc] cancel",
        Mode::Working(_) => "working… (background) — please wait",
        Mode::Result(_) => "[↑↓] scroll  ·  [any other key] dismiss",
        Mode::Ask { .. } => "type the question  [enter] ask  [tab] the other AI  [ctrl-u] clear  [esc] cancel",
        Mode::Answer(_) => "[↑↓ PgUp PgDn] scroll  [esc] close",
        Mode::Backups(v) => backups::keys(v),
    }
}

/// Every key of the list and detail views, and the mouse, for the info panel: `(keys, what)`.
const ALL_KEYS: &[(&str, &str)] = &[
    ("q / ctrl-c", "quit (asks first while a run that changes pods is going)"),
    ("esc", "back to the list from the detail view; on the list, clear the marks"),
    ("↑↓ / j k", "move the cursor"),
    ("enter", "the detail view of the cursor pod"),
    ("i  (or click [i] info)", "this panel"),
    ("space", "mark / unmark the cursor pod"),
    ("x", "clear the marks"),
    ("/", "mark pods by selector (the CLI's syntax)"),
    ("a", "act on the marked pods, else the cursor pod (menu)"),
    ("a a / a o", "the cursor pod's AI key: raise its cap / pause or resume it (with a provisioning key)"),
    ("A", "act on every pod (menu)"),
    ("d", "deep-check the marked pods, else the cursor pod"),
    ("D", "deep-check every running pod nobody is using (press twice)"),
    ("c", "ssh into the cursor pod"),
    ("?", "ask an AI about the cursor pod or the marked ones (tab: the other AI)"),
    ("n", "add pods (runs `arena pods up`)"),
    ("p", "the runs panel (pods up, start, rename, jobs, logs, copy home, the AI key's)"),
    ("b  (or click 💾)", "the backups screen: each backup job (how often, last run, next), what's running, every pod's last git save, home copy and archive copy (enter: its history, back up now)"),
    ("r", "refresh now (and re-read the providers' details)"),
    ("f", "refresh cadence: 2 → 5 → 10 → 20 → 60 s"),
    ("s", "short / full pod names (kept)"),
    ("mouse", "click a row: cursor · click its far left or shift-click: mark · wheel: move · [i] info: this panel · 💾: backups"),
];

/// What the table's glyphs mean, for the info panel: `(column, legend)`.
const LEGEND: &[(&str, &str)] = &[
    ("P", "provider: R runpod · V vast · H hetzner · S shadeform"),
    ("M", "M host maintenance on record · L locked (stop/restart/terminate refused)"),
    ("SET", ".name · deploy key · origin→GitHub · API key: ✓ yes  ✗ no  · unknown"),
    ("HLTH", "last deep check and its age: ✓ pass (grey after 26h)  ! warn  ✗ fail  · skipped  … checking  - never"),
    ("PROXY", "the forward's port: green live · `!` yellow stale (`arena proxy apply` re-points it) · - none · ? unreadable"),
    ("💾", "when the repo was last committed: ↑ unpushed  ↓ behind  ⇕ diverged · yellow: uncommitted work · grey: quiet a day+"),
    ("AI$", "the pod's OpenRouter key: spent/cap this period · yellow from 80% · red at the cap · paused: a $0 cap"),
    ("RTT", "round trip from this box to the pod's SSH port: green < 60 ms · yellow < 120 ms · red beyond"),
    ("↻ 7s ago · every 5s", "refreshed 7 s ago, every 5 s (`refreshing` while it does, the ↻ in cyan)"),
];

/// The info panel's text ([`Mode::Info`]): every word the top line and the footer leave out —
/// under `dashboard` the provider, config, whole refresh status, cadence and deep checks; under
/// `fleet` the summary bar as it used to read ([`summary_text`]); each prepaid account; the
/// OpenRouter credit and keys; every key of the list and detail views; what the glyphs mean.
/// `detail`: opened from the detail view (its keys are the same but for `esc`).
fn info_lines(shared: &Shared, ui: &Ui, secs: u64) -> Vec<Line<'static>> {
    let head = |t: &str| Line::styled(t.to_string(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
    let dim = Style::default().fg(Color::DarkGray);
    let row = |label: &str, value: String, style: Style| Line::from(vec![Span::styled(format!("  {label:<13}"), dim), Span::styled(value, style)]);
    let toned = |t: Option<Tone>| t.map(tone_style).unwrap_or_default();
    let mut out = vec![head("dashboard")];
    out.push(row("provider", format!("{} (where the add-pod form starts; the table lists every configured provider)", ui.provider_name), Style::default()));
    out.push(row("config", ui.config_path.clone(), Style::default()));
    out.push(row("status", shared.status.full.clone(), toned(shared.status.tone)));
    let age = match shared.last_refresh {
        Some(t) => format!("last {}s ago", t.elapsed().as_secs()),
        None => "not refreshed yet".into(),
    };
    let now = if shared.refreshing { " · refreshing now" } else { "" };
    out.push(row("refresh", format!("every {secs}s ([f] changes it, [r] refreshes now) · {age}{now}"), Style::default()));
    let deep = match shared.deep.running() {
        0 => ("none running ([d] this pod or the marked ones, [D] every idle one)".to_string(), Style::default()),
        n => (format!("{n} running"), tone_style(Tone::Busy)),
    };
    out.push(row("deep checks", deep.0, deep.1));
    out.push(Line::raw(""));

    out.push(head("fleet"));
    // The bar as it used to read: its warnings each on a line in their colours, then the rest.
    let fleet = summary_text(&shared.summary, &shared.snap.cost, &shared.snap.partial, &shared.balance, shared.ai_cohort.as_ref());
    let mut rest = fleet.as_str();
    for (lead, tone) in [
        (state::partial_notice(&shared.snap.partial), Some(Tone::Warn)),
        (balance::tui_parts(&shared.balance).0, state::balance_tone(&shared.balance)),
        (shared.ai_cohort.as_ref().and_then(|a| a.tui_parts().0), state::ai_tone(shared.ai_cohort.as_ref())),
    ] {
        if let Some(lead) = lead.filter(|l| rest.starts_with(l.as_str())) {
            rest = &rest[lead.len()..];
            out.push(Line::styled(format!("  {}", lead.trim().trim_end_matches('·').trim_end()), toned(tone)));
        }
    }
    for part in rest.split("  ·  ").map(str::trim).filter(|p| !p.is_empty()) {
        out.push(Line::raw(format!("  {part}")));
    }
    out.push(Line::raw(""));

    out.push(head("balances (prepaid accounts)"));
    let stamp = snapshot::unix_now();
    let views: Vec<&AccountView> = shared.balance.iter().collect();
    if views.is_empty() {
        out.push(Line::styled("  none read yet — read every few minutes when a prepaid account's key is configured (`arena balance` has them all)", dim));
    }
    for v in views {
        let style = match v.level {
            balance::Level::Critical => tone_style(Tone::Bad),
            balance::Level::Low => tone_style(Tone::Warn),
            balance::Level::Unknown => tone_style(Tone::Dim),
            _ => Style::default(),
        };
        let symbol = if v.currency == "EUR" { "€" } else { "$" };
        let burn = v.burn.as_ref().map(|b| format!(" · burn {}/h", fleet::fmt_money(symbol, b.per_hr))).unwrap_or_default();
        out.push(row(
            &v.account,
            format!("{} {} · runway {}{burn}", balance::balance_label(v), balance::source_label(v, stamp), balance::fmt_runway(v)),
            style,
        ));
    }
    out.push(Line::raw(""));

    if let Some(ai) = shared.ai.as_ref() {
        out.push(head("AI (OpenRouter)"));
        let money = |x: f64| fleet::fmt_money("$", x);
        let c = shared.ai_cohort.as_ref();
        let credit = match (ai.credits, &ai.credits_error) {
            (Some(cr), None) => format!("{} left", money(cr.left())),
            (Some(cr), Some(e)) => format!("{} left as last read (the latest read failed: {e})", money(cr.left())),
            (None, Some(e)) => format!("? ({e})"),
            (None, None) => "… (not read yet)".into(),
        };
        out.push(row("credit", credit, toned(state::ai_tone(c))));
        if let Some(c) = c {
            let today = c.today.map(money).unwrap_or_else(|| "?".into());
            let uncapped = if c.uncapped > 0 { format!(" · {} with no cap", c.uncapped) } else { String::new() };
            out.push(row("cohort", format!("{} · {today} today · {} more allowed under their caps{uncapped}", count_of(c.keys, "key", "keys"), money(c.headroom)), Style::default()));
            if !c.capped.is_empty() {
                let names: Vec<String> = c.capped.iter().map(|k| if k.pod { k.name.clone() } else { format!("{} (no pod)", k.name) }).collect();
                out.push(row("at cap", format!("{} — `arena keys list` has each one's cap and spend", names.join(", ")), tone_style(Tone::Warn)));
            }
        }
        if let Some(e) = &ai.keys_error {
            out.push(row("keys", format!("⚠ the latest read failed: {e} (the numbers are the last good ones)"), tone_style(Tone::Warn)));
        }
        out.push(Line::raw(""));
    }

    out.push(head("keys"));
    for (k, what) in ALL_KEYS {
        out.push(Line::from(vec![Span::styled(format!("  {k:<17}"), Style::default().add_modifier(Modifier::BOLD)), Span::raw(*what)]));
    }
    out.push(Line::raw(""));
    out.push(head("columns"));
    for (col, what) in LEGEND {
        out.push(Line::from(vec![Span::styled(format!("  {col:<10}"), Style::default().add_modifier(Modifier::BOLD)), Span::raw(*what)]));
    }
    out
}

/// The info panel ([`info_lines`]), scrolled by `ui.mode`'s `scroll` — clamped, and where the end is
/// told back through `result_max` (`scroll_by`).
fn render_info(f: &mut Frame, shared: &Shared, ui: &Ui, _detail: bool, secs: u64) {
    let Mode::Info { scroll, .. } = ui.mode else { return };
    let area = centered_rect(90, 86, f.area());
    f.render_widget(Clear, area);
    let text = Paragraph::new(info_lines(shared, ui, secs)).wrap(Wrap { trim: false });
    let inner = area.height.saturating_sub(2);
    let total = u16::try_from(text.line_count(area.width.saturating_sub(2))).unwrap_or(u16::MAX);
    ui.result_max.set(total.saturating_sub(inner));
    let scroll = scroll.min(ui.result_max.get());
    f.render_widget(
        text.scroll((scroll, 0)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(" info — [esc/i] close · [↑↓ PgUp PgDn] scroll "),
        ),
        area,
    );
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
/// `ai_off`: why the AI key's rows can't act now (`aikey::pod_rows`) — they're greyed, with it.
/// An action menu: one row per action, `off[i]` (when set) greying row `i` out with why — what
/// it can't do there (the capability table's answer, [`menu_offs`]; the AI key's).
fn render_action_menu(f: &mut Frame, title: String, actions: &[(char, Action)], sel: usize, restart: ContainerReset, off: &[Option<String>]) {
    let mut lines: Vec<Line> = vec![Line::from(title), Line::from("")];
    for (i, (k, a)) in actions.iter().enumerate() {
        let selected = i == sel;
        let off = off.get(i).and_then(|o| o.as_deref());
        let base = if off.is_some() {
            Style::default().fg(Color::DarkGray)
        } else if a.is_risky() {
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
        let desc = off.map_or_else(|| a.desc(restart).to_string(), |why| format!("({})", why.split(" — ").next().unwrap_or(why)));
        lines.push(Line::styled(format!("{marker}[{k}] {:<10} {desc}", a.label()), style));
    }
    lines.push(Line::from(""));
    lines.push(Line::styled(
        "[↑↓] move   [enter] choose   [letter] pick   [esc] cancel",
        Style::default().fg(Color::DarkGray),
    ));
    // As tall as its lines (twelve actions), within the screen.
    let pct_y = ((lines.len() as u16 + 2) * 100 / f.area().height.max(1) + 2).clamp(40, 96);
    let area = centered_rect(58, pct_y, f.area());
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" action ")),
        area,
    );
}

fn render_menu(f: &mut Frame, shared: &Shared, ui: &Ui, sel: usize) {
    let pod = shared.pods.get(ui.selected);
    let name = pod.map(|p| ui.shown_name(&p.name)).unwrap_or_else(|| "?".into());
    let (rows, ai_off) = aikey::pod_rows(shared, ui, pod);
    let off = menu_offs(shared, &rows, pod.into_iter().collect(), ai_off);
    render_action_menu(f, format!("Actions for {name}:"), &rows, sel, shared.restart_reset(pod), &off);
}

/// Why each of `rows` can't act on `pods` (`None`: it can): an AI key row, `ai_off`; an
/// operation's ([`Action::op`]), the capability table's refusal when NONE of the pods' providers
/// can do it (a set of some that can and some that can't is the pick's to sort: a stop skips
/// and names them, a restart or terminate refuses). Pure.
fn menu_offs(shared: &Shared, rows: &[(char, Action)], pods: Vec<&Pod>, ai_off: Option<String>) -> Vec<Option<String>> {
    rows.iter()
        .map(|(_, a)| {
            if a.ai_key() {
                return ai_off.clone();
            }
            let op = a.op()?;
            let whys: Vec<Option<&str>> = pods.iter().map(|p| shared.refusal(p, op)).collect();
            if whys.is_empty() || whys.iter().any(Option::is_none) {
                None
            } else {
                whys[0].map(String::from)
            }
        })
        .collect()
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
    if let Some(note) = &c.note {
        out.push(String::new());
        out.extend(note.lines().flat_map(|l| wrap_words(l, width)));
    }
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
    let actions = Action::fleet_menu(scope == Scope::Marked, ui.ai_keys);
    let pods = scope_pods_of(shared, ui, scope);
    let ai_off = aikey::set_why(shared, &pods);
    let off = menu_offs(shared, &actions, pods.clone(), ai_off);
    render_action_menu(f, format!("Actions — {who}:"), &actions, sel, shared.restart_reset(pods), &off);
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
    use arena_core::PodSpec;
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

    pub(crate) fn cfg(extra: &str) -> Config {
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

    /// The footer, in plain words: `[i] info` in its first cells, then a notice (`D`'s prompt),
    /// then a question's line with its seconds, then the short status, the refresh's age and
    /// cadence behind its one symbol (`↻`, cyan while refreshing), then the keys; one separator
    /// between each part, and no ☰ ✓ ⚠ or spinner.
    #[test]
    fn a_notice_leads_the_footer_before_the_asking_line() {
        let ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        // No start time: 0s and the first frame, whatever the clock does.
        let mut shared = Shared {
            ask_running: Some("asking claude about devtest-alpha…".into()),
            status: RefreshStatus::of("3 reporting · RunPod API 13/min".into(), &[], Some(13)),
            ..Default::default()
        };
        let text = |s: &Shared| footer_hint(s, &ui, 10).spans.iter().map(|s| s.content.to_string()).collect::<String>();
        let quiet = text(&shared);
        assert!(quiet.starts_with("[i] info · asking claude about devtest-alpha… 0s · ok · API 13/min · ↻ - · every 10s  [q] quit  [enter] detail"), "{quiet}");
        shared.notify("D again to deep-check 2 running pods", false);
        let t = text(&shared);
        let rest = t.strip_prefix("[i] info · D again to deep-check 2 running pods · ").unwrap_or_else(|| panic!("{t}"));
        assert!(rest.starts_with("asking claude about devtest-alpha… 0s · ok · API 13/min"), "{t}");
        assert!(!t.contains("·  ·") && !t.contains("· ·"), "{t}");
        // A refresh that warned: its first words, yellow, and how many more.
        shared.status = RefreshStatus::of("3 reporting · ⚠ pod details incomplete: x · ⚠ ssh config not rewritten: y".into(), &["pod details incomplete: x", "ssh config not rewritten: y"], None);
        let line = footer_hint(&shared, &ui, 5);
        let warn = line.spans.iter().find(|s| s.content.starts_with("warn:")).unwrap_or_else(|| panic!("{line:?}"));
        assert_eq!((warn.content.as_ref(), warn.style.fg), ("warn: pod details incomplete +1", Some(Color::Yellow)));
        // A refresh under way says so, in words.
        shared.refreshing = true;
        let t = text(&shared);
        assert!(t.contains(" · ↻ refreshing · every 10s  "), "{t}");
        let mark = footer_hint(&shared, &ui, 10).spans.into_iter().find(|s| s.content.starts_with(REFRESH_MARK)).unwrap();
        assert_eq!(mark.style.fg, Some(Color::Cyan), "the mark says a refresh runs");
        shared.last_refresh = Some(Instant::now());
        assert!(text(&shared).contains(" · ↻ refreshing, last 0s ago · every 10s  "), "the age stays while it refreshes");
        let all = t.chars().filter(|c| !c.is_ascii()).collect::<String>();
        assert!(!all.chars().any(|c| "☰⟳✓✗⚠⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(c)), "only words: {all:?} in {t}");
        assert_eq!(t.matches(REFRESH_MARK).count(), 1, "one symbol: {t}");
    }

    /// The first refresh's early paint: the listed rows, no metrics yet, and a status that
    /// says the rest is being read — never "loading…" while the probes run.
    #[test]
    fn the_listing_is_shown_before_the_pods_are_read() {
        let shared = Mutex::new(Shared { status: RefreshStatus::busy("loading…"), ..Default::default() });
        let pods = [pod("alpha", "runpod", "a", 1, Some(22)), pod("bravo", "hetzner", "b", 2, None)];
        publish_listing(&cfg(""), &shared, &pods, &[], 1_000);
        let s = shared.lock().unwrap();
        assert_eq!(s.pods.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["devtest-alpha", "devtest-bravo"]);
        assert_eq!((s.snap.pods.len(), s.summary.pods, s.metrics.len()), (2, 2, 0));
        assert_eq!((s.status.full.as_str(), s.status.short.as_str()), ("listed 2 pods — reading them…", "reading 2 pods…"));
        assert!(s.last_refresh.is_none(), "still the first refresh: its full publish comes next");
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
    fn row<'a>(lines: &'a [String], name: &str) -> &'a str {
        lines
            .iter()
            .find(|l| l.contains(&format!(" {name} ")))
            .unwrap_or_else(|| panic!("no {name} row in\n{}", lines.join("\n")))
    }

    /// Not a test: how long the view takes over the 60-pod demo fleet, whole and in parts (µs per
    /// frame, the median of 200) — what to cache. `cargo test --release -p arena-tui -- --ignored
    /// --nocapture view_cost`.
    #[test]
    #[ignore = "a measurement"]
    fn view_cost() {
        let shared = Mutex::new(Shared::default());
        let conf = cfg("");
        for round in 0..60 {
            demo::publish(&shared, &conf, 60, round);
        }
        let shared = shared.into_inner().unwrap();
        let ui = ui(conf, Arc::new(FakeRemote::new()));
        let median = |f: &mut dyn FnMut()| {
            let mut t: Vec<u128> = (0..200)
                .map(|_| {
                    let s = Instant::now();
                    f();
                    s.elapsed().as_micros()
                })
                .collect();
            t.sort_unstable();
            t[100]
        };
        for (w, h) in [(200u16, 50u16), (120, 40)] {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            let whole = median(&mut || {
                term.draw(|f| view(f, &shared, &ui, 5)).unwrap();
            });
            let table = median(&mut || {
                term.draw(|f| pods_table(f, &shared, &ui, f.area(), true)).unwrap();
            });
            let rest = median(&mut || {
                term.draw(|f| {
                    top_line(f, &shared, &ui, f.area());
                    f.render_widget(Paragraph::new(footer_hint(&shared, &ui, 5)), f.area());
                })
                .unwrap();
            });
            println!("{w}x{h}: view {whole}µs, pods_table {table}µs, summary+footer {rest}µs");
        }
    }

    /// Not a test: the README's picture of the dashboard — a made-up fleet of five, drawn by
    /// the renderer. `cargo test -p arena-tui -- --ignored --nocapture readme_picture`.
    #[test]
    #[ignore = "prints the README's picture"]
    fn readme_picture() {
        use arena_core::metrics::GpuStat;
        let gpu = |name: &str, util: u32, used: Option<u32>, total: Option<u32>, temp: u32| GpuStat {
            name: Some(name.into()),
            util_pct: Some(util),
            mem_used_mb: used,
            mem_total_mb: total,
            temp_c: Some(temp),
        };
        // name, provider, id, GPU type, $/h, nvidia-smi's GPUs, branch, saved (s ago), verdict,
        // checked (s ago), proxy port.
        type Row = (&'static str, &'static str, &'static str, &'static str, f64, Vec<GpuStat>, &'static str, i64, Status, u64, u16);
        let rows: [Row; 5] = [
            ("apple", "runpod", "rp1", "NVIDIA RTX A4000", 0.17, vec![gpu("RTX A4000", 87, Some(10_240), Some(16_376), 71)], "w1d2", 240, Status::Pass, 720, 22001),
            ("bloom", "runpod", "rp2", "NVIDIA RTX A4000", 0.17, vec![gpu("RTX A4000", 0, Some(900), Some(16_376), 34)], "w1d2", 780, Status::Pass, 3_000, 22002),
            ("cedar", "vast", "v3", "RTX 3090", 0.22, vec![gpu("RTX 3090", 0, Some(300), Some(24_576), 30)], "w1d2", 10_800, Status::Fail, 7_300, 22003),
            ("delta", "runpod", "rp4", "NVIDIA RTX A4000", 0.17, vec![gpu("RTX A4000", 42, Some(7_100), Some(16_376), 63)], "main", 480, Status::Pass, 900, 22004),
            ("ember", "runpod", "rp5", "NVIDIA B300 SXM6 AC MIG 1g.34gb", 0.89, vec![gpu("B300 SXM6 AC", 98, None, None, 78)], "w1d2", 120, Status::Pass, 600, 22005),
        ];
        let mut pods = Vec::new();
        let mut shared = Shared::default();
        let mut cache = HealthCache::new();
        for (i, (name, provider, id, ty, cost, gpus, branch, saved, verdict_status, checked, port)) in rows.into_iter().enumerate() {
            let mut p = pod(name, provider, id, i as u8 + 1, Some(port));
            p.gpu_type = Some(ty.into());
            p.gpu_count = Some(1);
            p.cost_per_hr = Some(cost);
            p.locked = Some(name == "delta");
            let branch = if branch == "main" { "main".to_string() } else { format!("autocommit-devtest-{branch}-{name}") };
            shared.metrics.insert(
                p.name.clone(),
                PodMetrics {
                    gpus,
                    branch: Some(branch),
                    origin: Some("git@github.com:example/ARENA_3.0.git".into()),
                    has_name: Some(true),
                    has_key: Some(true),
                    has_api_key: Some(true),
                    disk_used_mb: Some(40_960),
                    disk_total_mb: Some(100_352),
                    last_commit: Some(now_unix() - saved),
                    dirty_files: Some(0),
                    ..Default::default()
                },
            );
            cache.merge(&[verdict(&p, verdict_status, (verdict_status == Status::Fail).then_some(("cuda", "RuntimeError: Error 999")))], NOW - checked);
            pods.push(p);
        }
        use arena_core::proxy::{render_nginx, Forward};
        let fwd = |i: usize, p: &Pod| Forward {
            name: p.name.clone(),
            public_port: 9500 + i as u16,
            target_ip: p.ssh_ip.clone().unwrap(),
            target_port: p.ssh_port.unwrap() + u16::from(i == 2),
            provider: Some(p.provider.clone()),
            pod_id: None,
        };
        let proxy = render_nginx(&pods.iter().enumerate().map(|(i, p)| fwd(i, p)).collect::<Vec<_>>());
        let snap = dashboard_snapshot(&pods, &[], Some(&proxy), &cache, &Naming::from_config(&cfg("")), NOW);
        shared.summary = summarize(&pods, &shared.metrics);
        shared.publish(snap);
        shared.balance = live_views(&[(balance::Account::Runpod, 214.30, 1.17)]);
        shared.status = RefreshStatus::of("5 reporting · RunPod API 6/min".into(), &[], Some(6));
        shared.last_refresh = Some(Instant::now());
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        ui.config_path = "./config.env".into();
        println!("{}", draw(120, 11, &shared, &ui).join("\n"));
        ui.mode = Mode::Detail;
        ui.selected = 4;
        println!("{}", draw(120, 30, &shared, &ui).join("\n"));
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
        let header = lines.iter().find(|l| l.contains("HLTH")).unwrap_or_else(|| panic!("{all}"));
        for col in ["$/H", "HLTH", "PROXY", " M "] {
            assert!(header.contains(col), "{col} in {header}");
        }
        // Rows by name, whatever order the providers listed them in.
        let order: Vec<usize> = ["alpha", "bravo", "charlie", "delta"].iter().map(|n| all.find(&format!(" {n} ")).unwrap()).collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{all}");

        // (row, must show, must not show)
        let cases: &[(&str, &[&str], &[&str])] = &[
            ("alpha", &["init M", "$0.17", "✓12m", " 9500 "], &["9500!"]),
            ("bravo", &["init L", "$0.25", "✗2h", " 9501! "], &["init M"]),
            ("charlie", &["€0.006"], &["$0.0", "init M", "init L", "✓", "✗", "95"]),
            ("delta", &["exit", " … "], &["$0.13", "exit M"]),
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
        // The top line, in short: each provider's pods and burn, in its currency (Hetzner's €).
        assert!(lines[0].starts_with(" 4 pods, 0 GPUs │ 3 RunPod $0.42/h │ 1 Hetzner €0.006/h  "), "{all}");
        // The rest is the info panel's: the old bar's words, and the deep check running.
        let mut ui = ui;
        ui.mode = Mode::Info { scroll: 0, detail: false };
        let info = draw(180, 60, &shared, &ui).join("\n");
        for want in [
            "4 pods · 0 GPUs · mean util - · fleet: $0.42/h across 3 billing pod(s) + €0.006/h hetzner ≈ $10.08/day + €0.13/day",
            "deep checks  1 running",
        ] {
            assert!(info.contains(want), "{want:?} in\n{info}");
        }
    }

    /// The human (2026-10-30, prod's dashboard): "B300 SXM6 AC -> B300 MIG". Under MIG
    /// nvidia-smi names the parent card (`NVIDIA B300 SXM6 AC`, its memory unreadable), and the
    /// GPU column showed that, cut: `1×B300 SXM6 AC`. A slice reads as every surface names it,
    /// with its memory as every GPU has it — `1×B300 MIG 34G`, whole at any width — whether the
    /// provider lists its id (v2) or its name (v1), read or not; the detail pane spells it out
    /// with the card it's cut from. Other GPUs are as before.
    #[test]
    fn a_mig_slice_reads_as_the_slice_in_the_table_and_the_detail_pane() {
        let smi = |out: &str| PodMetrics { gpus: metrics::parse_nvidia_smi(out), ..Default::default() };
        let slice = smi("NVIDIA B300 SXM6 AC, 7, [Insufficient Permissions], [Insufficient Permissions], 43\n");
        let mut mig = pod("alpha", "runpod", "rp1", 1, Some(22001));
        mig.gpu_type = Some("NVIDIA B300 SXM6 AC MIG 1g.34gb".into());
        mig.gpu_count = Some(1);
        let mut a4000 = pod("bravo", "runpod", "rp2", 2, Some(22002));
        a4000.gpu_type = Some("NVIDIA RTX A4000".into());
        a4000.gpu_count = Some(2);
        let two = smi("NVIDIA RTX A4000, 3, 100, 16376, 40\nNVIDIA RTX A4000, 0, 100, 16376, 41\n");
        let mut cx = pod("charlie", "hetzner", "88", 3, Some(22));
        cx.gpu_type = Some("cx23".into());
        // (pod, its probe, narrow, the GPU cell)
        let cases: &[(&Pod, Option<&PodMetrics>, bool, &str)] = &[
            (&mig, Some(&slice), false, "1×B300 MIG 34G"),
            (&mig, Some(&slice), true, "1×B300 MIG 34G"),
            (&mig, None, true, "1×B300 MIG 34G"),
            (&a4000, Some(&two), false, "2×RTX A4000 16G"),
            (&a4000, Some(&two), true, "2×A4000 16G"),
            (&a4000, None, false, "2×RTX A4000"),
            (&cx, None, true, "cx23"),
        ];
        for (p, m, narrow, want) in cases {
            assert_eq!(gpu_text(p, *m, *narrow), *want, "{} narrow={narrow}", p.name);
        }
        let mut v1 = mig.clone();
        v1.gpu_type = Some("B300 MIG 34GB".into());
        assert_eq!(gpu_text(&v1, Some(&slice), true), "1×B300 MIG 34G", "v1's name for it");
        // Two slices of one card: nvidia-smi lists the parent once; the provider's count stands.
        let mut two_slices = mig.clone();
        two_slices.gpu_count = Some(2);
        assert_eq!(gpu_text(&two_slices, Some(&slice), false), "2×B300 MIG 34G");
        two_slices.gpu_count = None;
        assert_eq!(gpu_text(&two_slices, Some(&slice), false), "1×B300 MIG 34G", "no count from the provider: the probe's");
        assert_eq!(gpu_detail(&mig, Some(&slice)), "1×B300 MIG 34GB (MIG slice of B300 SXM6 AC)");
        assert_eq!(gpu_detail(&mig, None), "1×B300 MIG 34GB");
        assert_eq!(gpu_detail(&a4000, Some(&two)), "2×RTX A4000 16G");

        // On screen: whole in the narrowest table, and in the detail pane.
        let pods = vec![mig.clone(), a4000.clone()];
        let snap = dashboard_snapshot(&pods, &[], None, &HealthCache::new(), &Naming::from_config(&cfg("")), NOW);
        let mut shared = Shared::default();
        shared.publish(snap);
        shared.metrics.insert(mig.name.clone(), slice);
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        for w in [72, 100, 200] {
            let lines = draw(w, 12, &shared, &ui);
            assert!(row(&lines, "alpha").contains(" 1×B300 MIG 34G "), "{w}: {}", row(&lines, "alpha"));
        }
        ui.mode = Mode::Detail;
        let pane = draw(160, 40, &shared, &ui).join("\n");
        assert!(pane.contains("gpu:      1×B300 MIG 34GB (MIG slice of B300 SXM6 AC)"), "{pane}");
    }

    /// SAVED's header is a floppy disk: two cells wide (as `unicode-width`, tmux and the common
    /// terminals count it), the cell after it blank, and every column after it starts in the
    /// header where it starts in the rows. The column is as wide as its cells (`↑23h`).
    #[test]
    fn the_saved_header_is_a_floppy_disk_and_the_columns_after_it_line_up() {
        assert_eq!(Span::raw(columns::SAVED_HEADER).width(), 2);
        let shared = fixture_shared();
        let ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let mut t = Terminal::new(TestBackend::new(160, 12)).unwrap();
        t.draw(|f| view(f, &shared, &ui, 5)).unwrap();
        let buf = t.backend().buffer().clone();
        let cells = |y: u16| (0..160u16).map(|x| buf[(x, y)].symbol().to_string()).collect::<Vec<_>>();
        let hy = (0..12).find(|&y| cells(y).concat().contains("HLTH")).unwrap();
        let header = cells(hy);
        let disk = header.iter().position(|c| c == columns::SAVED_HEADER).unwrap_or_else(|| panic!("{}", header.concat()));
        assert_eq!(header[disk + 1], " ", "the second cell of the glyph");
        // The next column (CPU%: no AI key here) starts where its cells do, in every row.
        let cpu = (disk..160).find(|&x| header[x] == "C" && header[x + 1] == "P").unwrap();
        assert_eq!(cpu, disk + columns::SAVED_W + 1, "{}", header.concat());
        for y in hy + 1..hy + 5 {
            let row = cells(y);
            assert_eq!(row[cpu], "-", "row {y}: {}", row.concat());
            assert_eq!(row[cpu - 1], " ", "row {y}: {}", row.concat());
        }
    }

    /// The top line is one line — no title box — with the fleet in short words and, at its right
    /// while there's room, a provider or config that isn't the default (a second fleet's
    /// dashboard says so). The table starts right under it. A click anywhere on it opens the info
    /// panel (every word it leaves out), and another closes it.
    #[test]
    fn the_top_line_is_one_short_line_and_names_a_second_fleet() {
        let shared = fixture_shared();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let lines = draw(120, 12, &shared, &ui);
        assert!(lines[0].starts_with(" 4 pods, 0 GPUs │ 3 RunPod $0.42/h │ 1 Hetzner €0.006/h "), "{}", lines[0]);
        assert!(lines[0].trim_end().ends_with(" /test/config.env"), "{}", lines[0]);
        assert!(lines[1].starts_with("┌Pods"), "{}", lines[1]);
        ui.provider_name = "vast".into();
        assert!(draw(120, 12, &shared, &ui)[0].trim_end().ends_with(" vast · /test/config.env"));
        let narrow = &draw(50, 12, &shared, &ui)[0];
        assert_eq!(narrow.trim_end(), " 4 pods │ 3 RunPod $0.42/h │ 1 Hetzner €0.006/h", "no room: the tag first, then the GPUs (the info panel has them)");
        ui.provider_name = "runpod".into();
        ui.config_path = DEFAULT_CONFIG.into();
        assert_eq!(draw(120, 12, &shared, &ui)[0].trim_end(), " 4 pods, 0 GPUs │ 3 RunPod $0.42/h │ 1 Hetzner €0.006/h", "the defaults: nothing");
        let shared = Arc::new(Mutex::new(shared));
        let click = |column: u16, row: u16| MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column, row, modifiers: KeyModifiers::NONE };
        assert!(handle_mouse(click(30, 0), &mut ui, &shared));
        assert!(matches!(ui.mode, Mode::Info { detail: false, .. }), "{:?}", ui.mode);
        draw(120, 12, &shared.lock().unwrap(), &ui);
        assert!(handle_mouse(click(100, 0), &mut ui, &shared));
        assert!(matches!(ui.mode, Mode::List), "{:?}", ui.mode);
        ui.mode = Mode::Detail;
        assert!(handle_mouse(click(3, 0), &mut ui, &shared));
        assert!(matches!(ui.mode, Mode::Info { detail: true, .. }), "{:?}", ui.mode);
    }

    /// `i` — or a click on the footer's `[i] info` — opens the info panel over the list or the detail
    /// view; it holds every word the bars left out; Esc (or `i`, or another click) goes back to
    /// where it was opened; ↑↓ scroll it, no further than its end.
    #[test]
    fn i_or_the_menu_glyph_opens_the_info_panel_and_esc_goes_back() {
        let shared = Arc::new(Mutex::new(fixture_shared()));
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        assert_eq!(list_key(&mut ui, KeyCode::Char('i'), 4), ListKey::Done);
        assert!(matches!(ui.mode, Mode::Info { scroll: 0, detail: false }), "{:?}", ui.mode);
        let all = draw(160, 70, &shared.lock().unwrap(), &ui).join("\n");
        for want in [
            "provider     runpod (where the add-pod form starts",
            "config       /test/config.env",
            "refresh      every 5s ([f] changes it, [r] refreshes now) · not refreshed yet",
            "deep checks  1 running",
            "4 pods · 0 GPUs · mean util - · fleet: $0.42/h across 3 billing pod(s)",
            "none read yet",
            "D                deep-check every running pod nobody is using (press twice)",
            "HLTH      last deep check and its age: ✓ pass",
            "PROXY     the forward's port",
        ] {
            assert!(all.contains(want), "{want:?} in\n{all}");
        }
        assert!(all.lines().last().unwrap().starts_with("[i] info · ") && all.contains("[esc/i] close"), "{all}");
        // Scrolled, it stops at its end.
        let before = draw(160, 20, &shared.lock().unwrap(), &ui);
        for _ in 0..500 {
            info_key(&mut ui, KeyCode::Down);
        }
        let Mode::Info { scroll, .. } = ui.mode else { panic!("{:?}", ui.mode) };
        assert!(scroll > 0 && scroll == ui.result_max.get(), "{scroll} {}", ui.result_max.get());
        assert_ne!(draw(160, 20, &shared.lock().unwrap(), &ui), before);
        assert_eq!(info_key(&mut ui, KeyCode::Char('z')), Some(false), "not the panel's: nothing");
        assert_eq!(info_key(&mut ui, KeyCode::Esc), Some(true));
        assert!(matches!(ui.mode, Mode::List));
        assert_eq!(info_key(&mut ui, KeyCode::Esc), None, "closed: not its key any more");
        // From the detail view, back to it.
        ui.mode = Mode::Detail;
        list_key(&mut ui, KeyCode::Char('i'), 4);
        assert!(matches!(ui.mode, Mode::Info { detail: true, .. }));
        info_key(&mut ui, KeyCode::Char('i'));
        assert!(matches!(ui.mode, Mode::Detail));
        // A click on `[i] info` (bottom-left, cells 0–7) opens it and closes it; a click beside it doesn't.
        ui.mode = Mode::List;
        let footer_y = draw(120, 20, &shared.lock().unwrap(), &ui).len() as u16 - 1;
        let click = |column: u16, row: u16| MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column, row, modifiers: KeyModifiers::NONE };
        assert!(!handle_mouse(click(9, footer_y), &mut ui, &shared));
        assert!(matches!(ui.mode, Mode::List));
        assert!(handle_mouse(click(0, footer_y), &mut ui, &shared));
        assert!(matches!(ui.mode, Mode::Info { detail: false, .. }));
        assert!(handle_mouse(click(7, footer_y), &mut ui, &shared));
        assert!(matches!(ui.mode, Mode::List));
    }

    /// The human (2026-10-30): "the [enter] -> [overview] should have more colours". The detail
    /// pane's labels are dim, each value with a state in the table's tone for it — the words
    /// unchanged — and its sections' titles in the accent colour.
    #[test]
    fn the_detail_pane_colours_each_value_by_its_state() {
        let shared = fixture_shared();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        ui.mode = Mode::Detail;
        let colour_of = |ui: &Ui, text: &str| {
            let mut t = Terminal::new(TestBackend::new(160, 40)).unwrap();
            t.draw(|f| view(f, &shared, ui, 5)).unwrap();
            let buf = t.backend().buffer().clone();
            let lines = screen(&buf);
            let (y, line) = lines.iter().enumerate().find(|(_, l)| l.contains(text)).unwrap_or_else(|| panic!("{text:?} in\n{}", lines.join("\n")));
            let x = line[..line.find(text).unwrap()].chars().count() as u16;
            buf[(x, y as u16)].fg
        };
        ui.selected = 1; // bravo: failed its check, stale forward, locked
        for (text, colour) in [
            ("status:", Color::DarkGray),
            ("fail 2h GPU error", Color::Red),
            ("cuda: RuntimeError", Color::Red),
            (":9501 stale (`arena proxy apply`", Color::Yellow),
            ("locked (stop/restart/terminate refused)", Color::Cyan),
            ("per-GPU", Color::Cyan),
        ] {
            assert_eq!(colour_of(&ui, text), colour, "{text}");
        }
        ui.selected = 0; // alpha: passed, live forward, a maintenance window
        for (text, colour) in [("pass 12m", Color::Green), (":9500 (live)", Color::Green), ("maint 10-09 02:00", Color::Yellow)] {
            assert_eq!(colour_of(&ui, text), colour, "{text}");
        }
        ui.selected = 3; // delta: being checked
        assert_eq!(colour_of(&ui, "checking… (deep check running"), Color::Cyan);
    }

    /// A narrow terminal — or the detail view's 55% split — keeps the essential columns
    /// whole (NAME, HLTH, PROXY as the port, marked when stale, $/H) and drops the mid
    /// columns in turn (TEMP, RTT, MEM, BRANCH, DISK) rather than letting every column be
    /// crushed: at 70–100 columns the names still read `alpha`, not `al`/`alph`.
    #[test]
    fn a_narrow_terminal_drops_mid_columns_and_keeps_names_and_health_whole() {
        let shared = fixture_shared();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        // (mode, terminal width, mid columns shown, mid columns dropped)
        let cases: &[(Mode, u16, &[&str], &[&str])] = &[
            (Mode::List, 72, &[], &["DISK", "BRANCH", "MEM", "RTT", "TEMP"]),
            (Mode::List, 80, &["DISK"], &["BRANCH", "MEM", "RTT", "TEMP"]),
            (Mode::List, 90, &["DISK", "BRANCH"], &["MEM", "RTT", "TEMP"]),
            (Mode::List, 100, &["DISK", "BRANCH", "MEM"], &["RTT", "TEMP"]),
            (Mode::List, 104, &["DISK", "BRANCH", "MEM", "RTT"], &["TEMP"]),
            (Mode::List, 110, &["DISK", "BRANCH", "MEM", "RTT", "TEMP"], &[]),
            // Detail: the table gets 55% of 160 = 88 columns.
            (Mode::Detail, 160, &["DISK", "BRANCH"], &["MEM", "RTT", "TEMP"]),
        ];
        ui.selected = 3; // delta: the detail pane's title (" delta ") isn't a row we look at
        for (mode, w, shown, dropped) in cases {
            ui.mode = mode.clone();
            let lines = draw(*w, 12, &shared, &ui);
            let all = lines.join("\n");
            let header = lines.iter().find(|l| l.contains("HLTH")).unwrap_or_else(|| panic!("{w}: {all}"));
            for col in ["NAME", "STAT", "SET", "GPU%", "$/H", "HLTH", "PROXY"].iter().chain(shown.iter()) {
                assert!(header.contains(col), "{mode:?} {w}: {col} in {header}");
            }
            for col in *dropped {
                assert!(!header.contains(col), "{mode:?} {w}: {col} dropped from {header}");
            }
            for (name, cells) in [
                ("alpha", &["✓12m", "$0.17", " 9500 "][..]),
                ("bravo", &["✗2h", "$0.25", " 9501! "]),
                ("charlie", &["€0.006"]),
            ] {
                let r = row(&lines, name); // ` alpha ` — the whole short name
                for c in cells {
                    assert!(r.contains(c), "{mode:?} {w}: {name} shows {c:?}: {r}");
                }
            }
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
        rtts.record(vec![(ep(&alpha), Some(Duration::from_millis(23))), (ep(&bravo), Some(Duration::from_millis(131)))], Instant::now());
        shared.rtts = rtts;
        let idx = shared.pods.iter().position(|p| p.name == alpha.name).unwrap();
        shared.pods[idx].location = Some("EUR-IS-1".into());
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let mut t = Terminal::new(TestBackend::new(180, 12)).unwrap();
        t.draw(|f| view(f, &shared, &ui, 5)).unwrap();
        let buf = t.backend().buffer().clone();
        let lines = screen(&buf);
        let header = lines.iter().find(|l| l.contains("HLTH")).unwrap();
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
        // A place too long for the pane is cut with the rest: one row (review ai-review codex r2).
        ui.selected = idx;
        shared.pods[idx].location = Some("United States, California, San Francisco, Mission District".into());
        let lines = draw(120, 40, &shared, &ui);
        let at = lines.iter().position(|l| l.contains("where:")).unwrap();
        assert!(lines[at].contains("United States, California") && lines[at].contains('…'), "{}", lines[at]);
        assert!(lines[at + 1].contains("proxy:"), "the next fact on the next row:\n{}", lines.join("\n"));
    }

    /// The human (2026-10-29, prod's dashboard): "why does it say 1 ai key at cap?" — every AI$
    /// cell on screen was under its cap. The bar names the keys at their cap, `(no pod)` for one
    /// no row shows; the detail pane lists them and says where to look. A $0 key nothing was
    /// spent against reads `paused` in its row and isn't counted.
    #[test]
    fn keys_at_cap_are_named_and_a_paused_key_isnt_one() {
        use arena_core::openrouter::KeyInfo;
        let mut shared = fixture_shared();
        let key = |name: &str, limit: f64, left: f64, today: f64| KeyInfo {
            hash: name.into(),
            name: Some(name.into()),
            limit: Some(limit),
            limit_remaining: Some(left),
            usage_daily: Some(today),
            limit_reset: Some("daily".into()),
            ..Default::default()
        };
        let view = UsageView::from_keys(&[key("devtest-alpha", 0.0, 0.0, 0.0), key("devtest-bravo", 5.0, 3.0, 2.0), key("devtest-gone", 5.0, 0.0, 5.0)], None);
        let names: Vec<String> = shared.pods.iter().map(|p| p.name.clone()).collect();
        shared.ai_cohort = Some(view.cohort("devtest", &names));
        shared.ai = Some(view);
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let lines = draw(220, 40, &shared, &ui);
        let screen = lines.join("\n");
        assert!(screen.contains(" │ ⚠ OpenRouter key at cap: devtest-gone (no pod) · $7.00 today"), "{screen}");
        assert!(row(&lines, "alpha").contains(" paused "), "{}", row(&lines, "alpha"));
        assert!(row(&lines, "bravo").contains(" 2.00/5 "), "{}", row(&lines, "bravo"));
        ui.mode = Mode::Detail;
        ui.selected = shared.pods.iter().position(|p| p.name == "devtest-alpha").unwrap();
        let pane = draw(220, 50, &shared, &ui).join("\n");
        assert!(pane.contains("ai key:   paused (a $0 cap, nothing spent against it) · $0.00 of $0.00 today"), "{pane}");
        // A key at its cap, or disabled: where to act on it, first (with the rows there to act).
        ui.ai_keys = true;
        let mut at_cap = shared.ai.clone().unwrap();
        at_cap.keys.insert("devtest-alpha".into(), at_cap.keys["devtest-gone"].clone());
        let mut s2 = Shared { ai: Some(at_cap.clone()), ..fixture_shared() };
        let hinted = draw(220, 50, &s2, &ui).join("\n");
        assert!(hinted.contains("ai key:   at its cap (a → raise cap) · $5.00 of $5.00 today"), "{hinted}");
        at_cap.keys.get_mut("devtest-alpha").unwrap().disabled = true;
        s2.ai = Some(at_cap);
        let hinted = draw(220, 50, &s2, &ui).join("\n");
        assert!(hinted.contains("ai key:   disabled (a → ai resume) · $5.00 of $5.00"), "{hinted}");
        assert!(pane.contains("ai caps:  ⚠ 1 key at cap (`arena keys list` has each one's cap and spend): devtest-gone (no pod)"), "{pane}");
        // A pod named after it: no `(no pod)`. None at cap (only the paused one): no line.
        shared.ai_cohort = Some(shared.ai.as_ref().unwrap().cohort("devtest", &[names, vec!["devtest-gone".into()]].concat()));
        let pane = draw(220, 50, &shared, &ui).join("\n");
        assert!(pane.contains("spend): devtest-gone ") && !pane.contains("(no pod)"), "{pane}");
        // A key at its cap that a row carries: the top line says how to raise it.
        assert!(pane.contains(" ⚠ OpenRouter key at cap: devtest-gone (a → raise cap) · "), "{pane}");
        shared.ai = Some(UsageView::from_keys(&[key("devtest-alpha", 0.0, 0.0, 0.0)], None));
        shared.ai_cohort = Some(shared.ai.as_ref().unwrap().cohort("devtest", &[]));
        assert!(!draw(220, 50, &shared, &ui).join("\n").contains("ai caps:"));
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

    /// The top line's colour at the first cell of `text` (on its first row), and the line.
    fn top_colour(shared: &Shared, ui: &Ui, width: u16, text: &str) -> (Color, String) {
        let mut t = Terminal::new(TestBackend::new(width, 12)).unwrap();
        t.draw(|f| view(f, shared, ui, 5)).unwrap();
        let line = screen(t.backend().buffer())[0].clone();
        let x = line[..line.find(text).unwrap_or_else(|| panic!("{text:?} in {line}"))].chars().count() as u16;
        (t.backend().buffer()[(x, 0)].fg, line)
    }

    /// The prepaid balance in its provider's group, beside the burn of its pods there: the name
    /// green while it needs nothing; `⚠` in front and yellow when low, red when critical — at 80
    /// columns as at 120, its words never cut.
    #[test]
    fn the_top_line_shows_each_balance_in_its_providers_group() {
        use balance::Account;
        let mut shared = fixture_shared();
        let ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let top = |shared: &Shared, w: u16| draw(w, 12, shared, &ui)[0].trim_end().to_string();
        // Nothing observed yet: no balance at all.
        assert!(top(&shared, 240).starts_with(" 4 pods, 0 GPUs │ 3 RunPod $0.42/h │ 1 Hetzner €0.006/h "), "{}", top(&shared, 240));
        shared.balance = live_views(&[(Account::Runpod, 100.0, 0.42)]); // ≈238h
        assert!(top(&shared, 240).starts_with(" 4 pods, 0 GPUs │ 3 RunPod $100.00 ≈9.9d · $0.42/h │ 1 Hetzner "), "{}", top(&shared, 240));
        assert_eq!(top_colour(&shared, &ui, 120, "RunPod").0, Color::Green);
        shared.balance = live_views(&[(Account::Runpod, 20.0, 0.42)]); // ≈47h, under 72
        for w in [80, 120] {
            let (colour, line) = top_colour(&shared, &ui, w, "⚠ 3 RunPod $20.00 ≈47h");
            assert_eq!(colour, Color::Yellow, "{w}: {line}");
        }
        shared.balance = live_views(&[(Account::Runpod, 5.0, 0.42)]); // ≈11h
        assert_eq!(top_colour(&shared, &ui, 80, "⚠ 3 RunPod $5.00 ≈11h").0, Color::Red, "critical is red");
        // RunPod at $0 with nothing billing is still the alarm (at $0 RunPod stops the pods).
        shared.balance = live_views(&[(Account::Runpod, 0.0, 0.0)]);
        assert_eq!(top_colour(&shared, &ui, 80, "⚠ 3 RunPod $0.00 no burn").0, Color::Red);
    }

    /// The demo fleet's top line (`ARENA_TUI_DEMO=51`, the default config) at 160 and 80 columns,
    /// as the terminal shows it: a group per provider and OpenRouter's, its warnings at both.
    #[test]
    fn the_demo_top_line_at_160_and_80_columns() {
        let shared = Mutex::new(Shared::default());
        demo::publish(&shared, &cfg(""), 51, 0);
        let shared = shared.into_inner().unwrap();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        ui.config_path = DEFAULT_CONFIG.into();
        let at = |w: u16| draw(w, 12, &shared, &ui)[0].trim_end().to_string();
        assert_eq!(
            at(160),
            " 51 pods · 2 unreachable │ 43 RunPod $2262.90 · $11.91/h │ ⚠ 5 Vast $47.60 ≈40h · $1.19/h │ 3 Hetzner €0.048/h │ ⚠ OpenRouter key at cap · $422.09 left"
        );
        assert_eq!(at(80), " 2 unreachable │ RunPod │ ⚠ Vast $47.60 ≈40h │ Hetzner │ ⚠ OpenRouter key at cap");
    }

    /// Live (2026-10-15): with no Shadeform pods the bar led, in red, with `‼ shadeform balance no
    /// burn` — the opt-in overflow account, unfunded on purpose. With no pods there and nothing
    /// wrong it has no group on the top line (the info panel has it, `shadeform $0.00
    /// unfunded`); with a billing pod there it is the alarm again.
    #[test]
    fn an_unfunded_overflow_account_stays_off_the_top_line() {
        use balance::Account;
        let mut shared = fixture_shared();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        shared.balance = live_views(&[(Account::Runpod, 100.0, 0.42), (Account::Shadeform, 0.0, 0.0)]);
        let wide = draw(240, 12, &shared, &ui)[0].clone();
        assert!(!wide.contains("Shadeform") && !wide.contains('⚠'), "{wide}");
        assert_eq!(state::balance_tone(&shared.balance), None);
        ui.mode = Mode::Info { scroll: 0, detail: false };
        let info = draw(200, 60, &shared, &ui).join("\n");
        assert!(info.contains("balance: runpod $100.00 ≈9.9d · shadeform $0.00 unfunded"), "{info}");
        ui.mode = Mode::List;
        shared.balance = live_views(&[(Account::Shadeform, 0.0, 0.57)]);
        let (colour, line) = top_colour(&shared, &ui, 80, "⚠ Shadeform $0.00 <1h");
        assert_eq!(colour, Color::Red, "{line}");
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
        let status = shared.lock().unwrap().status.full.clone();
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
        assert!(s.status.full.starts_with("list error"), "{}", s.status.full);
        assert_eq!(s.status.short, "list error");
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

    /// A provider that failed to list has its group on the top line, in red, at any common width
    /// — the line is built to fit, and a warning is never what it leaves out.
    #[test]
    fn a_failed_listing_is_visible_on_a_narrow_terminal() {
        let mut shared = fixture_shared();
        shared.snap.partial = vec!["vast".into()];
        let ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        for w in [80, 100, 120] {
            let (colour, line) = top_colour(&shared, &ui, w, "⚠ Vast: list failed");
            assert_eq!(colour, Color::Red, "{w}: {line}");
            assert!(line.starts_with(" 4 pods, 0 GPUs │ 3 RunPod $0.42/h │ ⚠ Vast: list failed │ 1 Hetzner €0.006/h"), "{w}: {line}");
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
        assert!(footer.starts_with("[i] info · a target matched no pod"), "{footer}");
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
            assert_eq!(health_cell(a.health.as_ref(), false, s.snap.generated_at).0, "✓10m");
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

    /// RunPod's listing rate-limited on the given list calls (1-based); every other call
    /// lists `pods`. Counts list and details calls; panics on anything that would change
    /// the fleet.
    struct Throttled {
        pods: Vec<Pod>,
        on: &'static [usize],
        lists: AtomicUsize,
        details: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Provider for Throttled {
        fn name(&self) -> &'static str {
            "runpod"
        }
        fn describe(&self, _spec: &PodSpec) -> String {
            String::new()
        }
        async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
            let n = self.lists.fetch_add(1, Ordering::SeqCst) + 1;
            if self.on.contains(&n) {
                return Err(arena_core::Error::Provider {
                    kind: arena_core::ProviderErrorKind::RateLimited,
                    message: "list pods HTTP 429 Too Many Requests".into(),
                });
            }
            Ok(self.pods.clone())
        }
        async fn enrich(&self, _pods: &mut [Pod]) -> arena_core::Result<()> {
            self.details.fetch_add(1, Ordering::SeqCst);
            Ok(())
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

    /// A rate-limited listing: the rows stay (as last listed, not "failed to list"), the
    /// probes keep running, and the footer says when RunPod is listed again (its API's hold,
    /// as `arena_core::ratelimit` keeps it), why, and how old the rows are. A listing that
    /// works clears the note; the details query keeps its own cadence.
    #[tokio::test(start_paused = true)]
    async fn a_throttled_listing_keeps_the_rows_and_says_when_it_lists_again() {
        let cfg = cfg("");
        let provider = Throttled {
            pods: vec![pod("alpha", "runpod", "rp1", 1, Some(22001)), pod("bravo", "runpod", "rp2", 2, Some(22002))],
            on: &[2, 3],
            lists: AtomicUsize::new(0),
            details: AtomicUsize::new(0),
        };
        let fake = Arc::new(FakeRemote::new());
        let remote: Arc<dyn Remote> = fake.clone();
        let shared = Mutex::new(Shared::default());
        let mut details = DetailsState::default();
        let opts = ProbeOpts::default();
        let calls = || (provider.lists.load(Ordering::SeqCst), provider.details.load(Ordering::SeqCst), fake.calls().len());
        let rows = |shared: &Mutex<Shared>| {
            let s = shared.lock().unwrap();
            (s.pods.iter().map(|p| p.name.clone()).collect::<Vec<_>>(), s.snap.partial.clone(), s.status.full.clone())
        };
        let step = |secs: u64| tokio::time::advance(Duration::from_secs(secs));

        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, false).await;
        assert_eq!(calls(), (1, 1, 2));
        // Throttled, RunPod's quota held for 12 s: the rows as last listed, nothing called
        // partial, the wait and the rows' age in the footer; the probes ran.
        step(5).await;
        arena_core::ratelimit::hold_for_test("runpod", "RunPod REST v2", Duration::from_secs(12), "the minute window is spent");
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, false).await;
        let (names, partial, status) = rows(&shared);
        assert_eq!(names, ["devtest-alpha", "devtest-bravo"]);
        assert!(partial.is_empty(), "{partial:?}");
        assert!(
            status.contains("⚠ RunPod rate limit: listing again in 12 s (the minute window is spent) · its rows are 5 s old"),
            "{status}"
        );
        assert_eq!(calls(), (2, 1, 4));
        assert!(details.listed_at.contains_key("runpod"));
        // Still throttled 5 s on: the same rows, older.
        step(5).await;
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, false).await;
        assert!(rows(&shared).2.contains("its rows are 10 s old"), "{}", rows(&shared).2);
        assert_eq!(rows(&shared).0, ["devtest-alpha", "devtest-bravo"]);
        // Then a listing that works: no note.
        step(5).await;
        refresh(&provider, &remote, &cfg, &opts, &shared, &mut details, false).await;
        assert_eq!(calls().0, 4);
        assert!(!rows(&shared).2.contains("rate limit"), "{}", rows(&shared).2);
        assert_eq!(details.throttle_note, None);
    }

    /// No rows to keep yet (the very first listing rate-limited): the error, with the note.
    #[tokio::test(start_paused = true)]
    async fn a_first_listing_rate_limited_says_so() {
        let provider = Throttled { pods: vec![pod("alpha", "runpod", "rp1", 1, Some(22001))], on: &[1], lists: AtomicUsize::new(0), details: AtomicUsize::new(0) };
        let remote: Arc<dyn Remote> = Arc::new(FakeRemote::new());
        let shared = Mutex::new(Shared::default());
        let mut details = DetailsState::default();
        refresh(&provider, &remote, &cfg(""), &ProbeOpts::default(), &shared, &mut details, false).await;
        let status = shared.lock().unwrap().status.full.clone();
        assert!(status.starts_with("list error: runpod: provider error: list pods HTTP 429"), "{status}");
        assert!(status.contains("⚠ RunPod rate limit: listing again") && status.contains("no rows yet"), "{status}");
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
        assert!(s.status.full.contains("⚠ pod details incomplete: ") && s.status.full.contains("some field errored"), "{}", s.status.full);
        assert_eq!(s.status.short, "warn: pod details incomplete");
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
        ui.mode = Mode::NewPod(newpod::build_form(&ui.cfg, "runpod", &[], true));
        let lines = draw(120, 40, &shared, &ui);
        let footer = lines.last().unwrap();
        assert!(footer.contains("[↑↓] field  [←→] change  [o] availability  [enter] review  [esc] cancel"), "{footer}");
        assert!(!footer.contains("[+-]") && !footer.contains("[↑↓] pods"), "{footer}");
        assert!(lines.join("\n").contains("[↑↓] field   [←→] change   [o] availability   [enter] review   [esc] cancel"));
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
    /// (`status::is_billing`, and `Provider::supports` — here the real fleet's, where the
    /// capability table says Shadeform has no stop): a billing pod with no stop is skipped and
    /// named, a pod not running is left alone, the rest are stopped; a selection of only no-stop
    /// pods is refused, as `pods stop` refuses it — and one pod's stop of such a pod is refused
    /// before its confirm, its menu row greyed out with why. The marked-set menu has stop; the
    /// whole fleet's doesn't.
    #[test]
    fn a_marked_stop_skips_what_it_cannot_stop_and_refuses_it_alone() {
        let no_stop = arena_core::capability::support(arena_core::capability::Backend::Shadeform, Op::Stop).refusal().unwrap();
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
                format!("⊘ devtest-mayor — skipped, still billing: {no_stop}"),
            ]
        );
        assert!(plan.still_billing().unwrap().starts_with("✗ 1 pod(s) still billing — their provider has no stop: devtest-mayor"));
        let alone = stop_plan(fleet.as_ref(), vec![mayor.clone(), delta.clone()]);
        assert_eq!(alone.refusal().unwrap(), format!("✗ can't stop devtest-mayor — {no_stop}"));
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
        // The Shadeform pod's menu greys out stop and start, saying why; the RunPod pod's doesn't
        // — from the refresh's answers, the table's.
        shared.lock().unwrap().refusals = backend_refusals(fleet.as_ref(), &[alpha.clone(), mayor.clone(), delta.clone()]);
        (ui.selected, ui.mode) = (row("sf1"), Mode::Menu { sel: 0 });
        let menu = draw(160, 50, &shared.lock().unwrap(), &ui).join("\n");
        assert!(menu.contains("[s] stop       (Shadeform has no stop: an instance bills") && menu.contains("[u] start      (Shadeform has no stop"), "{menu}");
        assert!(menu.contains("[t] terminate  terminate — irreversible"), "{menu}");
        (ui.selected, ui.mode) = (row("rp1"), Mode::Menu { sel: 0 });
        let menu = draw(160, 50, &shared.lock().unwrap(), &ui).join("\n");
        assert!(menu.contains("[s] stop       stop the pod") && menu.contains("[u] start      start a stopped pod"), "{menu}");
        // A marked set with a pod that can stop: offered; only the Shadeform pod: greyed.
        ui.marked = [&alpha, &mayor].iter().map(|p| mark_key(p)).collect();
        ui.mode = Mode::FleetMenu { scope: Scope::Marked, sel: 0 };
        assert!(draw(160, 50, &shared.lock().unwrap(), &ui).join("\n").contains("[s] stop       stop the pod"));
        ui.marked = [mark_key(&mayor)].into();
        assert!(draw(160, 50, &shared.lock().unwrap(), &ui).join("\n").contains("[s] stop       (Shadeform has no stop"));
        ui.marked = [&alpha, &mayor, &delta].iter().map(|p| mark_key(p)).collect();
        // The menus.
        ui.mode = Mode::FleetMenu { scope: Scope::Marked, sel: 0 };
        assert!(draw(120, 40, &shared.lock().unwrap(), &ui).join("\n").contains("[s] stop"));
        ui.mode = Mode::FleetMenu { scope: Scope::All, sel: 0 };
        assert!(!draw(120, 40, &shared.lock().unwrap(), &ui).join("\n").contains("[s] stop"));
    }

    /// Rename is offered where the pod's backend can rename it — every backend now, Hetzner and
    /// Vast included (prod 2026-10-30: the dashboard offered it on a Hetzner VM and the CLI
    /// refused) — asked of the real fleet, the answer `pods rename` goes by: a pod no configured
    /// backend owns is refused before a new name is asked for, with the fleet's own words.
    #[test]
    fn rename_is_offered_where_the_pods_backend_can_rename() {
        let fleet: Arc<dyn Provider> = Arc::from(
            arena_core::provider::build_fleet("runpod", &Config::parse("RUNPOD_API_KEY=r\nHETZNER_API_KEY=h\nVAST_API_KEY=v"), false).unwrap(),
        );
        let pods = [pod("alpha", "runpod", "rp1", 1, Some(22001)), pod("delta", "hetzner", "hz1", 4, Some(22)), pod("echo", "vast", "va1", 5, Some(22)), pod("mayor", "shadeform", "sf1", 3, Some(22))];
        let shared = Arc::new(Mutex::new(Shared::default()));
        let snap = dashboard_snapshot(&pods, &[], None, &HealthCache::new(), &Naming::from_config(&cfg("")), NOW);
        shared.lock().unwrap().publish(snap);
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        let row = |id: &str| shared.lock().unwrap().pods.iter().position(|p| p.id == id).unwrap();
        for id in ["rp1", "hz1", "va1"] {
            (ui.selected, ui.mode) = (row(id), Mode::List);
            choose_pod_action(&shared, &mut ui, &fleet, Action::Rename);
            assert!(matches!(&ui.mode, Mode::Input { action: Action::Rename, .. }), "{id}: {:?}", ui.mode);
        }
        // Shadeform isn't configured here: nothing owns its pod, so nothing can rename it.
        (ui.selected, ui.mode) = (row("sf1"), Mode::List);
        choose_pod_action(&shared, &mut ui, &fleet, Action::Rename);
        assert!(
            matches!(&ui.mode, Mode::Result(r) if r.starts_with("✗ can't rename devtest-mayor — no configured provider owns this pod")),
            "{:?}",
            ui.mode
        );
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
            fn supports(&self, op: Op, pod: Option<&Pod>) -> arena_core::capability::Support {
                match (op, pod.map(|p| p.provider.as_str())) {
                    (Op::Stop, Some("shadeform")) => arena_core::capability::Support::No { why: "Shadeform has no stop (fake)", instead: "terminate it" },
                    _ => arena_core::capability::Support::Yes,
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
             ⊘ devtest-mayor — skipped, still billing: Shadeform has no stop (fake) — terminate it\n\
             ✗ 1 pod(s) still billing — their provider has no stop: devtest-mayor (back up, then `arena pods terminate`)"
        );
        let mut stopped = stops.0.lock().unwrap().clone();
        stopped.sort();
        assert_eq!(stopped, ["rp1", "rp2"]);
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

    /// The AI key's rows end to end, against a stub `arena`: with a provisioning key the per-pod
    /// menu has them — greyed, with why, for a pod without a key, and refused if picked anyway; `a`
    /// opens the raise-cap box prefilled with one key's worth, saying what the key spent and the new
    /// cap; a cap under the spend keeps the box open; Enter runs `arena keys limit <pod> <amount>
    /// --yes`; `o` on a paused key is resume, confirmed with `y`; a marked set's cap is one run for
    /// its pods with a key. A key run's end asks for the keys to be read again.
    #[tokio::test]
    async fn the_ai_key_rows_run_the_clis_keys_commands() {
        use arena_core::openrouter::KeyInfo;
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("arena-tui-aikey-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let stub = dir.join("arena");
        std::fs::write(&stub, "#!/bin/sh\necho \"args: $*\"\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let key = |name: &str, spent: f64, reset: Option<&str>, off: bool| KeyInfo {
            hash: format!("h{}", name.replace('-', "")),
            name: Some(name.into()),
            limit: Some(5.0),
            limit_remaining: Some(5.0 - spent),
            limit_reset: reset.map(String::from),
            usage: Some(spent),
            usage_monthly: Some(spent),
            disabled: off,
            ..Default::default()
        };
        let mut fixture = fixture_shared();
        fixture.ai = Some(UsageView::from_keys(&[key("devtest-alpha", 0.42, Some("monthly"), false), key("devtest-bravo", 1.0, None, true)], None));
        let shared = Arc::new(Mutex::new(fixture));
        let provider: Arc<dyn Provider> = Arc::from(arena_core::provider::build_fleet("runpod", &Config::parse("RUNPOD_API_KEY=r\n"), false).unwrap());
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        ui.cli = Ok(runs::Cli { exe: stub.clone(), config: "/test/config.env".into(), dir: dir.join("runs") });
        let at = |name: &str| shared.lock().unwrap().pods.iter().position(|p| p.name == name).unwrap();
        async fn first_line_when_ended(shared: &Mutex<Shared>, id: u64) -> Option<String> {
            for _ in 0..200 {
                {
                    let s = shared.lock().unwrap();
                    let r = s.runs.iter().find(|r| r.id == id).unwrap();
                    if !r.running() {
                        return r.lines.front().cloned();
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            None
        }

        // No provisioning key: no rows.
        ui.selected = at("devtest-alpha");
        ui.mode = Mode::Menu { sel: 0 };
        assert!(!draw(120, 40, &shared.lock().unwrap(), &ui).join("\n").contains("ai cap"));
        ui.ai_keys = true;
        // A pod with no key: the rows greyed with why; picked anyway, refused.
        ui.selected = at("devtest-charlie");
        let menu = draw(120, 40, &shared.lock().unwrap(), &ui);
        let cap_row = menu.iter().find(|l| l.contains("[a] ai cap")).unwrap_or_else(|| panic!("{}", menu.join("\n")));
        assert!(cap_row.contains("(no OpenRouter key is named after it)"), "{cap_row}");
        choose_pod_action(&shared, &mut ui, &provider, Action::KeyLimit);
        let Mode::Result(msg) = &ui.mode else { panic!("{:?}", ui.mode) };
        assert!(msg.contains("✗ can't ai cap devtest-charlie — no OpenRouter key is named after it — `arena keys gen devtest-charlie`"), "{msg}");

        // `a` (the menu), `a` (ai cap): the box, prefilled.
        ui.selected = at("devtest-alpha");
        ui.mode = Mode::Menu { sel: 0 };
        let menu = draw(120, 40, &shared.lock().unwrap(), &ui).join("\n");
        assert!(menu.contains("[a] ai cap     AI key: raise or set its cap") && menu.contains("[o] ai pause"), "{menu}");
        let i = at("devtest-alpha");
        let alpha = shared.lock().unwrap().pods[i].clone();
        let rows = aikey::pod_rows(&shared.lock().unwrap(), &ui, Some(&alpha)).0;
        let pick = rows.iter().find(|(k, _)| *k == 'a').unwrap().1;
        choose_pod_action(&shared, &mut ui, &provider, pick);
        let Mode::Input { action: Action::KeyLimit, scope, value } = ui.mode.clone() else { panic!("{:?}", ui.mode) };
        assert_eq!(value, "+5");
        let boxed = draw(120, 40, &shared.lock().unwrap(), &ui).join("\n");
        assert!(boxed.contains("spent $0.42 of $5.00 this month") && boxed.contains("new cap: $10.00"), "{boxed}");
        // Under the spend: refused in the box, Enter keeps it.
        ui.mode = Mode::Input { action: Action::KeyLimit, scope: scope.clone(), value: "0.1".into() };
        assert!(draw(120, 40, &shared.lock().unwrap(), &ui).join("\n").contains("✗ a $0.10 cap is under what it has spent this month ($0.42)"));
        submit_input(&shared, &mut ui, &provider, Action::KeyLimit, scope.clone(), "0.1".into());
        assert!(matches!(&ui.mode, Mode::Input { value, .. } if value == "0.1"), "{:?}", ui.mode);
        assert!(shared.lock().unwrap().runs.is_empty());
        // A refresh while the box is open (another operator raised the cap to $25) changes neither
        // the preview nor what Enter runs on (review codex r4 #2): the CLI then refuses the moved key.
        let before = shared.lock().unwrap().ai.clone();
        shared.lock().unwrap().ai = Some(UsageView::from_keys(&[KeyInfo { limit: Some(25.0), ..key("devtest-alpha", 0.42, Some("monthly"), false) }], None));
        ui.mode = Mode::Input { action: Action::KeyLimit, scope: scope.clone(), value: "+5".into() };
        assert!(draw(120, 40, &shared.lock().unwrap(), &ui).join("\n").contains("spent $0.42 of $5.00 this month"));
        // Enter on +5: `arena keys limit devtest-alpha +5 --yes`, then the keys are read again.
        submit_input(&shared, &mut ui, &provider, Action::KeyLimit, scope, "+5".into());
        shared.lock().unwrap().ai = before;
        let Mode::Runs { id, .. } = ui.mode else { panic!("{:?}", ui.mode) };
        assert_eq!(first_line_when_ended(&shared, id).await.as_deref(), Some("args: --config /test/config.env --provider runpod keys limit devtest-alpha +5 --expect hdevtestalpha=5/monthly/on --yes"));
        {
            let mut s = shared.lock().unwrap();
            assert!(std::mem::take(&mut s.ai_wanted), "the AI$ column is read again");
            assert_eq!(s.notice.as_ref().map(|n| n.text.as_str()), Some("✓ keys limit done — [p] shows it"));
        }

        // A paused key: its `o` row is resume, a `y` confirm saying the key's state.
        ui.selected = at("devtest-bravo");
        ui.mode = Mode::Menu { sel: 0 };
        assert!(draw(120, 40, &shared.lock().unwrap(), &ui).join("\n").contains("[o] ai resume"));
        let i = at("devtest-bravo");
        let bravo = shared.lock().unwrap().pods[i].clone();
        let rows = aikey::pod_rows(&shared.lock().unwrap(), &ui, Some(&bravo)).0;
        choose_pod_action(&shared, &mut ui, &provider, rows.iter().find(|(k, _)| *k == 'o').unwrap().1);
        let Mode::Confirm(c) = ui.mode.clone() else { panic!("{:?}", ui.mode) };
        assert_eq!(c.action, Action::KeyResume);
        assert!(c.note.as_deref().unwrap().starts_with("ai key now: disabled · $1.00 of $5.00"), "{:?}", c.note);
        assert_eq!(c.preview.as_deref(), Some("arena keys resume devtest-bravo --expect hdevtestbravo=5/never/off --yes"));
        apply_action(&shared, &mut ui, &provider, &c);
        let Mode::Runs { id, .. } = ui.mode else { panic!("{:?}", ui.mode) };
        assert_eq!(first_line_when_ended(&shared, id).await.as_deref(), Some("args: --config /test/config.env --provider runpod keys resume devtest-bravo --expect hdevtestbravo=5/never/off --yes"));
        // A confirm runs on the key it showed, not on one read since (review codex r3 #1): the key
        // rotated while the pause confirm is open — the run still names the old one, which the CLI
        // then refuses.
        ui.selected = at("devtest-alpha");
        choose_pod_action(&shared, &mut ui, &provider, Action::KeyPause);
        let Mode::Confirm(c) = ui.mode.clone() else { panic!("{:?}", ui.mode) };
        shared.lock().unwrap().ai = Some(UsageView::from_keys(&[KeyInfo { hash: "hnew".into(), ..key("devtest-alpha", 0.0, None, false) }], None));
        apply_action(&shared, &mut ui, &provider, &c);
        let Mode::Runs { id, .. } = ui.mode else { panic!("{:?}", ui.mode) };
        let line = first_line_when_ended(&shared, id).await.unwrap();
        assert!(line.ends_with("keys pause devtest-alpha --expect hdevtestalpha=5/monthly/on --yes"), "{line}");
        shared.lock().unwrap().ai = Some(UsageView::from_keys(&[key("devtest-alpha", 0.42, Some("monthly"), false), key("devtest-bravo", 1.0, None, true)], None));
        // Rotate: red, its warning, and `--copy`.
        ui.selected = at("devtest-alpha");
        choose_pod_action(&shared, &mut ui, &provider, Action::KeyRotate);
        let Mode::Confirm(c) = ui.mode.clone() else { panic!("{:?}", ui.mode) };
        assert!(c.warning().unwrap().contains("the old key stops working at once") && c.note.as_deref().unwrap().contains("the new key: cap $5.00"), "{c:?}");
        assert_eq!(c.preview.as_deref(), Some("arena keys rotate devtest-alpha --copy --yes"));

        // A marked set: one run for the pods with a key; the one without is left out.
        let pods = shared.lock().unwrap().pods.clone();
        ui.marked = pods.iter().filter(|p| ["devtest-alpha", "devtest-bravo", "devtest-charlie"].contains(&p.name.as_str())).map(mark_key).collect();
        ui.mode = Mode::FleetMenu { scope: Scope::Marked, sel: 0 };
        let menu = draw(120, 40, &shared.lock().unwrap(), &ui).join("\n");
        assert!(menu.contains("[a] ai cap") && menu.contains("[O] ai resume") && !menu.contains("ai rotate"), "{menu}");
        choose_fleet_action(&shared, &mut ui, &provider, Action::KeyPause, Scope::Marked);
        assert!(matches!(&ui.mode, Mode::FleetConfirm { action: Action::KeyPause, note, .. } if note == "Left out (no AI key that was read): charlie"), "{:?}", ui.mode);
        choose_fleet_action(&shared, &mut ui, &provider, Action::KeyLimit, Scope::Marked);
        let Mode::Input { action: Action::KeyLimit, scope, .. } = ui.mode.clone() else { panic!("{:?}", ui.mode) };
        submit_input(&shared, &mut ui, &provider, Action::KeyLimit, scope, "+1".into());
        let Mode::Runs { id, .. } = ui.mode else { panic!("{:?}", ui.mode) };
        let line = first_line_when_ended(&shared, id).await.unwrap();
        let mut named: Vec<&str> = line.split(' ').filter(|w| w.starts_with("devtest-")).collect();
        named.sort();
        assert_eq!(named, ["devtest-alpha", "devtest-bravo"], "{line}");
        assert!(line.starts_with("args: --config /test/config.env --provider runpod keys limit ") && line.contains(" +1 --expect ") && line.ends_with(" --yes"), "{line}");
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
}
