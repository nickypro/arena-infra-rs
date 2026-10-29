//! What keeps the dashboard responsive, and how to tell when it isn't.
//!
//! - [`Trace`]: `ARENA_TUI_TRACE=<file>` records every frame (how long the view took to render
//!   and the terminal to take it) and every input event, with when — so "it feels laggy" has
//!   numbers: a held key's burst, from its first event to the frame that shows its last.
//!   [`summarize`] reads such a file back. Off (no file named): nothing is measured.
//! - [`drain`]: the key loop takes **every** event already waiting before it draws again. It
//!   used to draw after each one, so a held `j` (or a mouse dragged over the table, or a slow
//!   draw over SSH) queued events faster than frames went out and the lag grew with the queue.
//! - [`frame_due`]: a frame is drawn when something changed (a key, a landing) or the tick is
//!   up — not on every pass of the loop.
//!
//! All pure but [`Trace`]'s file writes.

use std::io::Write;
use std::time::{Duration, Instant};

/// The most events one pass takes before it draws: a flood (a paste, a stuck key) still gets
/// a frame out now and then.
pub(crate) const MAX_BATCH: usize = 256;

/// How long one pass may spend handling events before it draws.
pub(crate) const BATCH_BUDGET: Duration = Duration::from_millis(40);

/// How often a frame is drawn with nothing changed: the footer's `updated Ns ago` moves once a
/// second.
pub(crate) const TICK_IDLE: Duration = Duration::from_secs(1);

/// … and while something animates (a spinner, a run's output streaming, a check waiting out its
/// debounce).
pub(crate) const TICK_LIVE: Duration = Duration::from_millis(200);

/// The longest the loop waits for input before it looks at the shared state again: how late a
/// background landing (a refresh, a result) can show.
pub(crate) const POLL_SLICE: Duration = Duration::from_millis(100);

/// Take `first` and every event already waiting after it (`next(ZERO)` → `Some`), up to
/// [`MAX_BATCH`] or `budget` since `start` — whichever comes first. `next` is crossterm's
/// `poll(0)` + `read` in the loop, a script in tests.
pub(crate) fn drain<E>(
    first: E,
    mut next: impl FnMut() -> std::io::Result<Option<E>>,
    start: Instant,
    budget: Duration,
    now: impl Fn() -> Instant,
) -> std::io::Result<Vec<E>> {
    let mut out = vec![first];
    while out.len() < MAX_BATCH && now().duration_since(start) < budget {
        match next()? {
            Some(e) => out.push(e),
            None => break,
        }
    }
    Ok(out)
}

/// Whether to draw now: something changed since the last frame (`dirty`), there's never been
/// one, or `tick` has passed since it.
pub(crate) fn frame_due(now: Instant, last: Option<Instant>, dirty: bool, tick: Duration) -> bool {
    dirty || last.is_none_or(|t| now.duration_since(t) >= tick)
}

/// How long to wait for input before the next look: until the tick is up, never more than a
/// [`POLL_SLICE`].
pub(crate) fn wait_for(now: Instant, last: Option<Instant>, tick: Duration) -> Duration {
    let left = last.map_or(Duration::ZERO, |t| tick.saturating_sub(now.duration_since(t)));
    left.min(POLL_SLICE)
}

/// The opt-in trace (`ARENA_TUI_TRACE`): one tab-separated line per frame and per event, times
/// in microseconds since the dashboard started.
/// - `frame <t> <render_us> <flush_us> <events>`: a frame, after `events` events since the last;
/// - `event <t> <what>`: an event as it was read (a key's code, `mouse`, `resize`).
pub(crate) struct Trace {
    out: Option<std::io::BufWriter<std::fs::File>>,
    start: Instant,
}

impl Trace {
    /// The trace `ARENA_TUI_TRACE` names (created, or emptied), or none. A file that can't be
    /// created is said once on stderr — before the screen is taken — and nothing is traced.
    pub(crate) fn from_env() -> Self {
        let out = std::env::var_os("ARENA_TUI_TRACE").filter(|p| !p.is_empty()).and_then(|p| match std::fs::File::create(&p) {
            Ok(f) => Some(std::io::BufWriter::new(f)),
            Err(e) => {
                eprintln!("arena-tui: ARENA_TUI_TRACE {}: {e} — not tracing", std::path::Path::new(&p).display());
                None
            }
        });
        Trace { out, start: Instant::now() }
    }

    /// A trace that records nothing (tests).
    #[cfg(test)]
    pub(crate) fn off() -> Self {
        Trace { out: None, start: Instant::now() }
    }

    pub(crate) fn on(&self) -> bool {
        self.out.is_some()
    }

    fn at(&self, t: Instant) -> u128 {
        t.duration_since(self.start).as_micros()
    }

    pub(crate) fn frame(&mut self, at: Instant, render: Duration, flush: Duration, events: usize) {
        let t = self.at(at);
        if let Some(o) = self.out.as_mut() {
            let _ = writeln!(o, "frame\t{t}\t{}\t{}\t{events}", render.as_micros(), flush.as_micros());
            let _ = o.flush();
        }
    }

    pub(crate) fn event(&mut self, at: Instant, what: &str) {
        let t = self.at(at);
        if let Some(o) = self.out.as_mut() {
            let _ = writeln!(o, "event\t{t}\t{what}");
        }
    }
}

/// What a trace says, for the numbers in a report: frames, their render and flush times
/// (median, 95th percentile, max — µs), and per **burst** of events (events less than
/// `gap` apart) the time from its first event to the end of the first frame after its last —
/// how long a held key or a pasted run of keys took to show. Pure. (Read by the `trace_summary`
/// test: `ARENA_TUI_TRACE_SUMMARY=<file> cargo test -p arena-tui -- --ignored trace_summary`.)
#[cfg(test)]
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Summary {
    pub frames: usize,
    pub render_us: [u128; 3],
    pub flush_us: [u128; 3],
    pub bursts: usize,
    /// Events in the bursts, in all.
    pub burst_events: usize,
    /// First event to the frame after the last, per burst: median, p95, max (µs).
    pub settle_us: [u128; 3],
    /// Frames drawn per burst, at most: one per event before the loop drained them.
    pub max_frames_per_burst: usize,
}

#[cfg(test)]
pub(crate) fn summarize(trace: &str, gap: Duration) -> Summary {
    let mut frames: Vec<(u128, u128, u128)> = Vec::new();
    let mut events: Vec<u128> = Vec::new();
    for line in trace.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let num = |i: usize| f.get(i).and_then(|v| v.parse::<u128>().ok());
        match (f.first().copied(), num(1)) {
            (Some("frame"), Some(t)) => frames.push((t, num(2).unwrap_or(0), num(3).unwrap_or(0))),
            (Some("event"), Some(t)) => events.push(t),
            _ => {}
        }
    }
    let stats = |mut v: Vec<u128>| -> [u128; 3] {
        if v.is_empty() {
            return [0; 3];
        }
        v.sort_unstable();
        let at = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
        [at(0.5), at(0.95), *v.last().unwrap()]
    };
    let mut bursts: Vec<(u128, u128)> = Vec::new();
    for &t in &events {
        match bursts.last_mut() {
            Some((_, last)) if t.saturating_sub(*last) < gap.as_micros() => *last = t,
            _ => bursts.push((t, t)),
        }
    }
    let mut settle = Vec::new();
    let mut max_frames = 0;
    let mut burst_events = 0;
    for &(first, last) in &bursts {
        burst_events += events.iter().filter(|&&t| t >= first && t <= last).count();
        // The frame that shows the last event: the first one ending after it.
        if let Some(&(end, _, _)) = frames.iter().find(|(t, _, _)| *t >= last) {
            settle.push(end - first);
            let n = frames.iter().filter(|(t, _, _)| *t >= first && *t <= end).count();
            max_frames = max_frames.max(n);
        }
    }
    Summary {
        frames: frames.len(),
        render_us: stats(frames.iter().map(|f| f.1).collect()),
        flush_us: stats(frames.iter().map(|f| f.2).collect()),
        bursts: bursts.len(),
        burst_events,
        settle_us: stats(settle),
        max_frames_per_burst: max_frames,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// The loop takes every event already waiting before it draws — up to the cap, or the time
    /// budget — and stops at the first `poll(0)` that has nothing.
    #[test]
    fn drain_takes_what_is_waiting_up_to_the_cap_and_the_budget() {
        let t0 = Instant::now();
        // (events waiting after the first, the clock's step per call, the budget) → taken
        let cases: [(usize, Duration, Duration, usize); 5] = [
            (0, Duration::ZERO, BATCH_BUDGET, 1),
            (9, Duration::ZERO, BATCH_BUDGET, 10),
            (1_000, Duration::ZERO, BATCH_BUDGET, MAX_BATCH),
            // 10 ms a call: four calls fit in a 40 ms budget (0, 10, 20, 30 ms).
            (1_000, Duration::from_millis(10), Duration::from_millis(40), 5),
            (1_000, Duration::from_millis(10), Duration::ZERO, 1),
        ];
        for (waiting, step, budget, want) in cases {
            let left = Cell::new(waiting);
            let clock = Cell::new(t0);
            let next = || {
                Ok(if left.get() == 0 {
                    None
                } else {
                    left.set(left.get() - 1);
                    Some(left.get())
                })
            };
            let now = || {
                let t = clock.get();
                clock.set(t + step);
                t
            };
            let got = drain(usize::MAX, next, t0, budget, now).unwrap();
            assert_eq!(got.len(), want, "{waiting} waiting, {step:?} a call, budget {budget:?}");
            assert_eq!(got[0], usize::MAX, "the first event leads");
        }
        // A read that fails ends the pass with the error (the loop takes the terminal as gone).
        let failing = || Err::<Option<u8>, _>(std::io::Error::other("gone"));
        assert!(drain(0u8, failing, t0, BATCH_BUDGET, Instant::now).is_err());
    }

    /// A frame when something changed, the first time, or once the tick is up — never just
    /// because the loop came round; and the wait for input ends no later than the tick, in
    /// slices.
    #[test]
    fn frames_come_on_change_or_tick() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        assert!(frame_due(t0, None, false, TICK_IDLE), "the first frame");
        assert!(frame_due(t0 + ms(10), Some(t0), true, TICK_IDLE), "a change");
        assert!(!frame_due(t0 + ms(999), Some(t0), false, TICK_IDLE), "nothing changed, tick not up");
        assert!(frame_due(t0 + ms(1_000), Some(t0), false, TICK_IDLE), "the tick");
        assert!(frame_due(t0 + ms(200), Some(t0), false, TICK_LIVE), "a live tick is shorter");
        assert_eq!(wait_for(t0 + ms(950), Some(t0), TICK_IDLE), ms(50));
        assert_eq!(wait_for(t0 + ms(10), Some(t0), TICK_IDLE), POLL_SLICE, "never longer than a slice");
        assert_eq!(wait_for(t0 + ms(5_000), Some(t0), TICK_IDLE), Duration::ZERO, "overdue");
        assert_eq!(wait_for(t0, None, TICK_IDLE), Duration::ZERO);
    }

    /// A burst is events read less than `gap` apart; it settles at the end of the first frame
    /// after its last event. The old loop read one event per frame, so a burst of keys sent at
    /// once reads as events spread out between frames — one frame each.
    #[test]
    fn a_trace_summary_measures_bursts_to_the_frame_that_shows_them() {
        // Three keys sent at once, read one per frame (frames end at 5, 10, 15 ms); then one key
        // at 500 ms, shown at 503 ms.
        let trace = "event\t0\tj\nframe\t5000\t3000\t1000\t1\nevent\t5100\tj\nframe\t10000\t3000\t1000\t1\nevent\t10100\tj\n\
                     frame\t15000\t4000\t2000\t1\nevent\t500000\tk\nframe\t503000\t2000\t500\t1\nnoise\n";
        let s = summarize(trace, Duration::from_millis(50));
        assert_eq!((s.frames, s.bursts, s.burst_events), (4, 2, 4));
        assert_eq!(s.settle_us, [15_000, 15_000, 15_000], "the first event to the frame after the last");
        assert_eq!(s.max_frames_per_burst, 3, "one frame per key");
        assert_eq!(s.render_us, [3_000, 4_000, 4_000]);
        assert_eq!(s.flush_us, [1_000, 2_000, 2_000]);
        assert_eq!(summarize("", Duration::from_millis(50)), Summary::default());
    }

    /// Not a test: prints the summary of the trace `ARENA_TUI_TRACE_SUMMARY` names (bursts: events
    /// under 50 ms apart) — the lag numbers for a report.
    #[test]
    #[ignore = "a tool: reads ARENA_TUI_TRACE_SUMMARY"]
    fn trace_summary() {
        let Some(path) = std::env::var_os("ARENA_TUI_TRACE_SUMMARY") else { return };
        let text = std::fs::read_to_string(&path).unwrap();
        println!("{:#?}", summarize(&text, Duration::from_millis(50)));
    }
}
