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

mod actions;
mod aikey;
mod ask;
mod backups;
mod columns;
mod deep_check;
mod demo;
mod detail;
mod lag;
mod modals;
mod newpod;
mod offers;
mod prefs;
mod refresh;
mod runs;
mod ssh_actions;
mod state;
mod table;
mod terminal;
mod view;

use std::collections::HashMap;
use std::io::Stdout;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::{
    event::{
        self, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEvent, MouseEventKind,
    },
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use tokio::sync::Notify;

use arena_core::balance::{self, AccountView, BalancePolicy, HttpBalanceApi};
use arena_core::openrouter::{CohortUsage, OpenRouter, UsageView};
use arena_core::fleet;
use arena_core::metrics::PodMetrics;
use arena_core::capability::Op;
use arena_core::provider::{ContainerReset, Provider};
use arena_core::remote::{Remote, SshRemote};
use arena_core::selector::Naming;
use arena_core::snapshot::{self, FleetSnapshot};
use arena_core::{Config, Pod};

use prefs::Prefs;
use state::{
    display_name, mark_key, marked_pods,
    select_marks, summary_text, count_of, Action, Confirm, DeepChecks,
    FleetSummary, History, RefreshStatus, Rtts, Tone,
};
use newpod::NewPodForm;

use arena_core::config::LEGACY_CONFIG as DEFAULT_CONFIG;

use actions::{apply_action, choose_fleet_action, choose_pod_action, execute, execute_fleet, scope_pods, start_action, start_cli_action};
use deep_check::{deep_all_confirm, deep_all_plan, skipped_words, start_deep_check, DEEP_ALL_CONFIRM};
use modals::{centered_rect, fleet_confirm_token, result_key, scroll_by};
use refresh::{balance_state_path, fetch_loop, AiCtx, BalanceCtx, Cadence, DetailsState};
use table::tone_style;
use terminal::{connect_ssh, end_now, exit_on_hangup, exit_on_signals, restore_terminal, setup_terminal, terminal_ours};
use view::{view, MENU_LABEL};
// Reached as `crate::x` / `super::x` by the modules that predate the split; new code imports from the
// module itself.
use modals::scope_pods_of;
#[cfg(test)]
use actions::cli_action_args;
#[cfg(test)]
use ssh_actions::backup_and_report;
#[cfg(test)]
use view::mode_keys;

/// The cadences `f` cycles through (seconds).
const REFRESH_STEPS: &[u64] = &[2, 5, 10, 20, 60];

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
    /// Who each row's machine instance is for, by [`mark_key`] (`--owner`, `spend assign`: the
    /// spend ledger, read after each refresh records its listing) — the OWNER column and the
    /// detail view's `owner:` line. Rows with no owner aren't in it.
    owners: HashMap<String, String>,
    /// Which Claude Code account each machine is on (`claude.json`, as each refresh reads it):
    /// the detail view's `tokens:` line. `None`: no file yet, or one that can't be read.
    claude: Option<arena_core::claude::State>,
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

/// Advance the cadence to the next step in `REFRESH_STEPS` (wrapping).
fn cycle_interval(interval: &AtomicU64) {
    let cur = interval.load(Ordering::Relaxed);
    let idx = REFRESH_STEPS.iter().position(|&s| s == cur).unwrap_or(usize::MAX);
    let next = REFRESH_STEPS[idx.wrapping_add(1) % REFRESH_STEPS.len()];
    interval.store(next, Ordering::Relaxed);
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
    use std::path::PathBuf;
    use arena_core::health::PodHealth;
    use arena_core::snapshot::HealthCache;
    use crate::state::{dashboard_snapshot, summarize};
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
}
