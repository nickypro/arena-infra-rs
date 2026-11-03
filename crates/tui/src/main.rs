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
mod event_loop;
mod info;
mod input;
mod keytest;
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
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ratatui::prelude::*;
use tokio::sync::Notify;

use arena_core::balance::{AccountView, BalancePolicy, HttpBalanceApi};
use arena_core::openrouter::{CohortUsage, OpenRouter, UsageView};
use arena_core::metrics::PodMetrics;
use arena_core::capability::Op;
use arena_core::provider::{ContainerReset, Provider};
use arena_core::remote::{Remote, SshRemote};
use arena_core::snapshot::{self, FleetSnapshot};
use arena_core::{Config, Pod};

use prefs::Prefs;
use state::{
    display_name, mark_key, marked_pods,
    Action, Confirm, DeepChecks,
    FleetSummary, History, RefreshStatus, Rtts,
};
use newpod::NewPodForm;

use arena_core::config::LEGACY_CONFIG as DEFAULT_CONFIG;

use event_loop::run;
use refresh::{balance_state_path, fetch_loop, AiCtx, BalanceCtx, Cadence, DetailsState};
use terminal::{end_now, exit_on_hangup, exit_on_signals, restore_terminal, setup_terminal};
// Reached as `crate::x` / `super::x` by the modules that predate the split; new code imports from the
// module itself.
use actions::choose_pod_action;
use modals::{centered_rect, scope_pods_of, scroll_by};
use table::tone_style;
#[cfg(test)]
use actions::cli_action_args;
#[cfg(test)]
use input::handle_mouse;
#[cfg(test)]
use ssh_actions::backup_and_report;
#[cfg(test)]
use view::mode_keys;

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
    /// against one pod or a multi-pod scope, then execute on Enter. `prefilled`: `value` is the
    /// box's default, untouched (the raise-cap box's `+5`) — the first key typed replaces it,
    /// Backspace clears it, Enter runs it.
    Input { action: Action, scope: InputScope, value: String, prefilled: bool },
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

/// Read the currently selected pod (a clone, so we don't hold the lock).
fn selected_pod(shared: &Arc<Mutex<Shared>>, idx: usize) -> Option<Pod> {
    shared.lock().unwrap().pods.get(idx).cloned()
}

#[cfg(test)]
mod tests;
