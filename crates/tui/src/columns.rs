//! Which columns the pods table shows at a width, and how wide each is. Pure, so the order the
//! columns come in as the terminal widens is a table test, not a set of magic thresholds.
//!
//! The rules, in order:
//! 1. **Core**, always shown (69 columns with the narrowest NAME/GPU): mark, P, NAME (10: short
//!    names), STAT, M, SET, GPU (14: `RTX ` dropped, count + VRAM, `1×B300 MIG 34G` whole), GPU%,
//!    $/H, HLTH (`✓12m`), PROXY (the port, `!` when stale). Below that the table is crushed.
//! 2. **Mid**, each while it fits: DISK (a disk filling up breaks a participant's work), BRANCH
//!    (which iteration they're on), MEM, RTT, TEMP (MEM/TEMP/RTT are in the detail pane too).
//! 3. **Extras**, each while it fits, in this order: 💾 SAVED, AI$ (with a provisioning key),
//!    OWNER (8, while some pod has an owner), the full GPU name (16: `RTX ` kept), full NAMEs
//!    (16: the persisted short/full choice), the host's CPU% and RAM%.
//! 4. **Graphs** (UTIL%~/MEM%~) take what's left from 20 columns, then PROGRESS (with
//!    `PROGRESS_CMD`) a fixed 16 once there's room for both (alone in the detail view's split,
//!    which has no graphs).
//!
//! Always a prefix of each list: widening the terminal only ever adds.

/// The SAVED column's header: a floppy disk, two cells wide in every terminal that knows emoji
/// (`unicode-width` says 2, as tmux 3.x and the common terminals do — checked in tmux 3.4 over
/// a real pty), so the column is the width of its cells (`↑23h`).
pub(crate) const SAVED_HEADER: &str = "💾";

/// HLTH: a glyph and an age, `✓12m`, `✗400d` at most.
pub(crate) const HEALTH_W: usize = 5;
/// PROXY's narrowest: its header, and a 4-digit port with the stale mark (`9501!`).
pub(crate) const PROXY_W: usize = 5;
/// SAVED: a sync glyph and an age (`↑23h`); its header is two cells.
pub(crate) const SAVED_W: usize = 4;
/// AI$: `1.23/5`, `12.3/50`, `0.02/0.02`, `paused`.
pub(crate) const AI_W: usize = 9;
/// OWNER: a first name (`alice`, `christo…`); the detail view has the whole one.
pub(crate) const OWNER_W: usize = 8;
/// PROGRESS's fixed budget beside the graphs.
pub(crate) const PROGRESS_W: usize = 16;
/// DISK, BRANCH, MEM, RTT, TEMP: `12/100G`, `w1d2`, `120/240G`, `180ms`, `85C`.
pub(crate) const MID_W: [usize; 5] = [9, 6, 9, 5, 4];
/// NAME and GPU, narrow and full.
const NAME_W: (usize, usize) = (10, 16);
const GPU_W: (usize, usize) = (14, 16);

/// What the table shows, and the widths that vary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Columns {
    pub name_w: usize,
    /// NAME is forced short (`apple`, not `arena8-apple`) whatever the persisted choice.
    pub compact_names: bool,
    pub gpu_w: usize,
    /// GPU drops the `RTX ` noise.
    pub narrow_gpu: bool,
    pub proxy_w: usize,
    pub disk: bool,
    pub branch: bool,
    pub mem: bool,
    pub rtt: bool,
    pub temp: bool,
    pub saved: bool,
    pub ai: bool,
    pub owner: bool,
    pub host: bool,
    pub progress: bool,
    pub spark: bool,
    pub spark_w: usize,
}

/// The pods table's width for columns of these widths: their sum, one space between each,
/// and 4 for the block's two borders and the `▶ ` highlight column.
pub(crate) fn table_width(cols: &[usize]) -> usize {
    cols.iter().sum::<usize>() + cols.len().saturating_sub(1) + 4
}

/// How many of `optional` (column widths, most wanted first) fit in a table `w` wide beside
/// the `core` columns. Always a prefix, so widening the terminal only ever adds columns.
pub(crate) fn columns_that_fit(w: usize, core: &[usize], optional: &[usize]) -> usize {
    let mut cols = core.to_vec();
    optional
        .iter()
        .take_while(|&&c| {
            cols.push(c);
            table_width(&cols) <= w
        })
        .count()
}

/// The columns for a table `w` wide. `proxy_w`: the widest PROXY cell this refresh (never under
/// [`PROXY_W`]); `ai`: an `AI$` column is wanted (a provisioning key is configured); `owner`:
/// an OWNER column is wanted (some pod has an owner); `want_progress`: `PROGRESS_CMD` is set;
/// `with_spark`: the graphs are wanted (not in the detail view's split). Pure.
pub(crate) fn columns(w: usize, proxy_w: usize, ai: bool, owner: bool, want_progress: bool, with_spark: bool) -> Columns {
    let proxy_w = proxy_w.max(PROXY_W);
    // mark, P, NAME, STAT, M, SET, GPU, GPU%, $/H, HLTH, PROXY.
    let core = [1, 1, NAME_W.0, 4, 1, 4, GPU_W.0, 4, 6, HEALTH_W, proxy_w];
    let mut c = Columns { name_w: NAME_W.0, compact_names: true, gpu_w: GPU_W.0, narrow_gpu: true, proxy_w, ..Default::default() };
    let mid = columns_that_fit(w, &core, &MID_W);
    (c.disk, c.branch, c.mem, c.rtt, c.temp) = (mid > 0, mid > 1, mid > 2, mid > 3, mid > 4);
    let mut used = table_width(&core) + MID_W[..mid].iter().map(|m| m + 1).sum::<usize>();
    // The extras, while each fits (and only while every mid column did): a new column costs its
    // width and a space; a widened one only what it adds.
    #[derive(Clone, Copy)]
    enum Extra {
        Saved,
        Ai,
        Owner,
        GpuFull,
        NamesFull,
        Host,
    }
    let extras = [
        (Extra::Saved, SAVED_W + 1),
        (Extra::Ai, AI_W + 1),
        (Extra::Owner, OWNER_W + 1),
        (Extra::GpuFull, GPU_W.1 - GPU_W.0),
        (Extra::NamesFull, NAME_W.1 - NAME_W.0),
        (Extra::Host, 4 + 1 + 4 + 1),
    ];
    if mid == MID_W.len() {
        let wanted = |e: &Extra| match e {
            Extra::Ai => ai,
            Extra::Owner => owner,
            _ => true,
        };
        for (extra, cost) in extras.into_iter().filter(|(e, _)| wanted(e)) {
            if used + cost > w {
                break;
            }
            used += cost;
            match extra {
                Extra::Saved => c.saved = true,
                Extra::Ai => c.ai = true,
                Extra::Owner => c.owner = true,
                Extra::GpuFull => (c.gpu_w, c.narrow_gpu) = (GPU_W.1, false),
                Extra::NamesFull => (c.name_w, c.compact_names) = (NAME_W.1, false),
                Extra::Host => c.host = true,
            }
        }
    }
    // Sparklines (GPU%/MEM% history) flex to fill whatever is left after the other columns —
    // they grow on a wide screen and vanish when there's no room. PROGRESS takes its fixed
    // budget only from what the graphs leave at their narrowest.
    let leftover = w.saturating_sub(used);
    c.spark = with_spark && leftover >= 20; // ~2×9 + spacing
    // Where graphs are wanted, PROGRESS waits until there's room for them too — else widening
    // the terminal past the graphs' 20 would take PROGRESS away again (review ai-review codex
    // r1 #2: shown at 157, gone at 160, back at 177).
    c.progress = want_progress && leftover >= PROGRESS_W + 1 + if with_spark { 20 } else { 0 };
    let spark_room = if c.progress { leftover - PROGRESS_W - 1 } else { leftover };
    c.spark_w = if c.spark { (spark_room.saturating_sub(3) / 2).clamp(9, 30) } else { 0 };
    c
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The first width each column shows at (with an AI key, and without), and that each one
    /// stays from there on. Before (2026-10-30, HEALTH 8 and PROXY 6–11 wide), with every
    /// column whole: BRANCH from 90, RTT 106, SAVED 114, the full GPU 118, AI$ 132 (shown from
    /// 122 by crushing the rest), host 152.
    #[test]
    fn each_column_comes_in_once_and_stays() {
        type Pick = fn(&Columns) -> bool;
        let cases: &[(&str, Pick, usize, usize)] = &[
            // (column, shown, first width with AI, without)
            ("DISK", |c| c.disk, 79, 79),
            ("BRANCH", |c| c.branch, 86, 86),
            ("MEM", |c| c.mem, 96, 96),
            ("RTT", |c| c.rtt, 102, 102),
            ("TEMP", |c| c.temp, 107, 107),
            ("SAVED", |c| c.saved, 112, 112),
            ("AI$", |c| c.ai, 122, usize::MAX),
            ("full GPU", |c| !c.narrow_gpu, 124, 114),
            ("full NAME", |c| !c.compact_names, 130, 120),
            ("CPU%/RAM%", |c| c.host, 140, 130),
            ("graphs", |c| c.spark, 160, 150),
            ("PROGRESS", |c| c.progress, 177, 167),
        ];
        for &(name, shown, with_ai, without) in cases {
            for (ai, first) in [(true, with_ai), (false, without)] {
                let at = (60..=260).find(|&w| shown(&columns(w, 5, ai, false, true, true)));
                assert_eq!(at.unwrap_or(usize::MAX), first, "{name} ai={ai}");
                assert!((first.min(261)..=260).all(|w| shown(&columns(w, 5, ai, false, true, true))), "{name} ai={ai} stays");
            }
        }
        // Without graphs (the detail view's split), PROGRESS needs only its own room.
        assert!(!columns(146, 5, true, false, true, false).progress && columns(157, 5, true, false, true, false).progress);
        // Every column the layout picks fits whole: the table is never wider than the terminal.
        for w in 69..=260 {
            for ai in [true, false] {
                let c = columns(w, 5, ai, false, true, true);
                let mut cols = vec![1, 1, c.name_w, 4, 1, 4, c.gpu_w, 4, 6, HEALTH_W, c.proxy_w];
                for (on, width) in [(c.disk, 9), (c.branch, 6), (c.mem, 9), (c.rtt, 5), (c.temp, 4), (c.saved, SAVED_W), (c.ai, AI_W)] {
                    if on {
                        cols.push(width);
                    }
                }
                if c.host {
                    cols.extend([4, 4]);
                }
                if c.progress {
                    cols.push(PROGRESS_W);
                }
                if c.spark {
                    cols.extend([c.spark_w, c.spark_w]);
                }
                assert!(table_width(&cols) <= w, "{w} ai={ai}: {c:?} is {} wide", table_width(&cols));
            }
        }
        // A wide stale port (`10003!`) widens PROXY rather than losing its mark.
        assert_eq!(columns(120, 6, false, false, false, true).proxy_w, 6);
        assert_eq!(columns(120, 3, false, false, false, true).proxy_w, 5);
        // The detail view's split gets no graphs.
        assert!(!columns(250, 5, true, false, false, false).spark);
    }

    /// OWNER (while some pod has an owner) comes after AI$ and before the wider GPU names, and
    /// is the first extra to go when the terminal narrows: it never pushes a core or mid column
    /// out, and without owners the layout is what it was.
    #[test]
    fn the_owner_column_fits_after_ai_and_drops_first() {
        for ai in [true, false] {
            let first = (60..=260).find(|&w| columns(w, 5, ai, true, true, true).owner).unwrap();
            let after_ai = if ai { 122 } else { 112 } + OWNER_W + 1;
            assert_eq!(first, after_ai, "ai={ai}");
            assert!((first..=260).all(|w| columns(w, 5, ai, true, true, true).owner), "stays");
            for w in 60..=260 {
                let (with, without) = (columns(w, 5, ai, true, true, true), columns(w, 5, ai, false, true, true));
                assert!(!without.owner);
                assert_eq!((with.disk, with.branch, with.mem, with.rtt, with.temp, with.saved, with.ai), (without.disk, without.branch, without.mem, without.rtt, without.temp, without.saved, without.ai), "{w}");
            }
        }
    }
}
