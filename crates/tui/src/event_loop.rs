//! The event loop: what landed from the background, when a frame is drawn, and the input taken in batches.

use std::io::Stdout;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use arena_core::provider::Provider;
use crossterm::event::{self, Event};
use ratatui::prelude::*;
use tokio::sync::Notify;

use crate::input::{handle_event, Ctx, Flow};
use crate::terminal::terminal_ours;
use crate::view::view;
use crate::{ask, backups, lag, newpod, Mode, Shared, Ui};

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
pub(crate) async fn run(
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
pub(crate) fn live(s: &Shared, ui: &Ui, now: Instant) -> bool {
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
