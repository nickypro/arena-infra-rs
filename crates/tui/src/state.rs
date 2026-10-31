//! Pure, unit-tested state for the interactive dashboard: per-pod sample history
//! (for sparklines), the fleet-wide summary, and the rules governing which actions
//! exist and how an action is confirmed. None of this does any IO — `main.rs` owns
//! the terminal, the provider calls, and the SSH; this module just decides *what*
//! should be shown and *when* a confirmation is satisfied, so the risky bits (e.g.
//! "a destructive action needs the exact pod name typed back") are testable offline.
//!
//! Every fleet datum (cost, maintenance, proxy port + state, last deep check) comes from
//! `arena_core`'s [`FleetSnapshot`] builder and its labels — the same ones `pods list` and
//! `arena snapshot` print. What lives here is only what is the dashboard's own: which cell
//! gets which colour, how a background deep check's results fold into what is on screen,
//! how often the provider's slower details query may run, and what `/` marks. The prepaid
//! balances on the top line are `arena_core::balance`'s views, worded by its `inline_account`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use arena_core::balance::{self, AccountView, Level};
use arena_core::openrouter::{CohortUsage, Fill};
use arena_core::fleet::{self, FleetCost};
use arena_core::health::{render_summary, PodHealth, Status};
use arena_core::metrics::PodMetrics;
use arena_core::pod::Maintenance;
use arena_core::provider::ContainerReset;
use arena_core::proxy::Listing;
use arena_core::selector::{Naming, SelectArgs, Selector, PROVIDERS};
use arena_core::snapshot::{self, cache_key, fmt_age, FleetSnapshot, HealthCache, HealthRecord, ProxyState, SnapshotPod};
use arena_core::Pod;

/// How many recent samples we keep per pod for the sparklines.
pub const HISTORY_LEN: usize = 60;

/// A rolling window of recent util/temp samples for one pod. Missing readings (pod
/// unreachable, no GPU) are recorded as 0 so the sparkline keeps a steady time axis
/// rather than collapsing gaps.
#[derive(Debug, Clone, Default)]
pub struct History {
    pub util: VecDeque<u64>,
    pub temp: VecDeque<u64>,
    /// GPU memory usage as a percentage of total.
    pub mem: VecDeque<u64>,
}

impl History {
    /// Append one refresh's reading, evicting the oldest beyond [`HISTORY_LEN`].
    pub fn push(&mut self, util: Option<u32>, temp: Option<u32>, mem_pct: Option<u32>) {
        push_capped(&mut self.util, util.unwrap_or(0) as u64);
        push_capped(&mut self.temp, temp.unwrap_or(0) as u64);
        push_capped(&mut self.mem, mem_pct.unwrap_or(0) as u64);
    }

    pub fn util_data(&self) -> Vec<u64> {
        self.util.iter().copied().collect()
    }

    pub fn temp_data(&self) -> Vec<u64> {
        self.temp.iter().copied().collect()
    }

    pub fn mem_data(&self) -> Vec<u64> {
        self.mem.iter().copied().collect()
    }
}

fn push_capped(q: &mut VecDeque<u64>, v: u64) {
    q.push_back(v);
    while q.len() > HISTORY_LEN {
        q.pop_front();
    }
}

/// Render the last `width` values of `data` (each a 0–100 percentage) as a one-line
/// block-eighths sparkline (`▁▂▃▄▅▆▇█`), left-padded with spaces if there's less
/// history than `width`. Empty history renders all spaces.
pub fn spark(data: &[u64], width: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let start = data.len().saturating_sub(width);
    let slice = &data[start..];
    let mut s = String::with_capacity(width);
    for _ in 0..width.saturating_sub(slice.len()) {
        s.push(' ');
    }
    for &v in slice {
        // Map 0..=100 to one of 8 bar heights (rounded).
        let level = ((v.min(100) * 7 + 50) / 100) as usize;
        s.push(BARS[level.min(7)]);
    }
    s
}

/// At-a-glance numbers across the whole fleet, for the summary bar.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FleetSummary {
    pub pods: usize,
    /// Pods reporting at least one GPU reading.
    pub reporting: usize,
    /// Pods whose metric fetch errored (down / no nvidia-smi / unreachable).
    pub unreachable: usize,
    pub total_gpus: usize,
    /// Mean utilization across *every* GPU in the fleet (not a mean of means).
    pub mean_util: Option<u32>,
    pub mem_used_mb: u32,
    pub mem_total_mb: u32,
}

/// Compute the fleet summary from the current pods + their metrics. (Cost isn't here: it
/// is the snapshot's [`FleetCost`], which keeps Hetzner's € apart from the $ providers.)
pub fn summarize(pods: &[Pod], metrics: &HashMap<String, PodMetrics>) -> FleetSummary {
    let mut s = FleetSummary {
        pods: pods.len(),
        ..Default::default()
    };
    let mut util_sum = 0u64;
    let mut util_n = 0u64;
    for pod in pods {
        let Some(m) = metrics.get(&pod.name) else { continue };
        if m.error.is_some() {
            s.unreachable += 1;
        }
        if !m.gpus.is_empty() {
            s.reporting += 1;
        }
        s.total_gpus += m.gpus.len();
        for g in &m.gpus {
            if let Some(u) = g.util_pct {
                util_sum += u as u64;
                util_n += 1;
            }
        }
        if let Some((u, t)) = m.mem_summary() {
            s.mem_used_mb += u;
            s.mem_total_mb += t;
        }
    }
    // No GPU reported a utilization (util_n == 0): no mean.
    s.mean_util = util_sum.checked_div(util_n).map(|m| m as u32);
    s
}

/// The info panel's warning when a provider failed to list (the top line's `⚠ Vast: list
/// failed`): its pods are missing from the rows and their cost from the fleet total, so the
/// numbers after it are a floor.
pub fn partial_notice(partial: &[String]) -> Option<String> {
    (!partial.is_empty())
        .then(|| format!(" ⚠ {} failed to list — its pods (and their cost) are missing ·", partial.join(", ")))
}

/// The colour of the balance warning at the front of the info panel's fleet lines: bad when
/// an account is critical, warn when one is low; `None` when there is none.
pub fn balance_tone(balance: &[AccountView]) -> Option<Tone> {
    match balance::tui_parts(balance).2 {
        Level::Critical => Some(Tone::Bad),
        Level::Low => Some(Tone::Warn),
        _ => None,
    }
}

/// The summary bar as it used to read, now the info panel's fleet lines: every word the top
/// line ([`top_parts`]) leaves out at a width. When a provider failed to list,
/// [`partial_notice`] first, then a low or critical prepaid balance the same way (` ⚠ runpod
/// $1.20 ≈1.9d ·`, `balance::tui_parts`), then pod/GPU/util counts from the probes, then the
/// fleet's burn exactly as `pods list`'s footer words it ([`fleet::fleet_footer`]: billing pods
/// only, `$` and Hetzner's `€` kept apart, unpriced pods called out), the same per day, and
/// last the balances that need nothing (`balance: runpod $32.54 ≈4.0d · vast ~$118.30 est.
/// ≈49d`). OpenRouter, when its key is configured, the same way: a warning after the
/// balance's (credit the keys' caps could spend, keys at their cap), its credit and the
/// cohort's spend today last.
pub fn summary_text(
    s: &FleetSummary,
    cost: &FleetCost,
    partial: &[String],
    balance: &[AccountView],
    ai: Option<&CohortUsage>,
) -> String {
    let util = s.mean_util.map(|u| format!("{u}%")).unwrap_or_else(|| "-".into());
    let (first, last, _) = balance::tui_parts(balance);
    let (ai_first, ai_last, _) = ai.map(CohortUsage::tui_parts).unwrap_or((None, None, Fill::Ok));
    let mut out = partial_notice(partial).unwrap_or_default();
    out.push_str(&first.unwrap_or_default());
    out.push_str(&ai_first.unwrap_or_default());
    out.push_str(&format!(
        " {} · {} · mean util {util} · {}",
        count_of(s.pods, "pod", "pods"),
        count_of(s.total_gpus, "GPU", "GPUs"),
        fleet::fleet_footer(cost)
    ));
    let mut per_day = Vec::new();
    if cost.priced_usd > 0 {
        per_day.push(format!("{}/day", fleet::fmt_money("$", cost.usd_per_hr * 24.0)));
    }
    if cost.priced_eur > 0 {
        per_day.push(format!("{}/day", fleet::fmt_money("€", cost.eur_per_hr * 24.0)));
    }
    if !per_day.is_empty() {
        out.push_str(&format!(" ≈ {}", per_day.join(" + ")));
    }
    if s.unreachable > 0 {
        out.push_str(&format!("  ·  {} unreachable", s.unreachable));
    }
    if let Some(rest) = last {
        out.push_str(&format!("  ·  balance: {rest}"));
    }
    if let Some(rest) = ai_last {
        out.push_str(&format!("  ·  {rest}"));
    }
    out
}

/// A piece of the top line or the footer's status: its words and its colour (`None`: the
/// line's own).
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    pub text: String,
    pub tone: Option<Tone>,
}

impl Part {
    fn plain(text: impl Into<String>) -> Self {
        Part { text: text.into(), tone: None }
    }

    fn toned(text: impl Into<String>, tone: Option<Tone>) -> Self {
        Part { text: text.into(), tone }
    }
}

/// The warning mark in front of a top-line group that needs looking at — once per group, never
/// per piece, so the line keeps few symbols. One cell on every terminal: `⚠` in text
/// presentation, never `⚠️` with its VS16, which some terminals draw two cells wide and others
/// one, and the line's width-fitting would be off by it. The human (2026-10-30, of icons): "no i
/// think i like them :) … its just if you have too many then the text sizing becomes pretty
/// unreliable".
pub const WARN_MARK: &str = "⚠";

/// The top line laid out for a width ([`top_parts`]): its parts in order, the dim `│` between
/// groups among them, each in its colour; and the provider/config tag for its right end
/// (`main.rs`'s `where_tag`) when it still fits beside them.
#[derive(Debug, Clone, PartialEq)]
pub struct TopLine {
    pub parts: Vec<Part>,
    pub tag: Option<String>,
}

#[cfg(test)]
impl TopLine {
    /// Its words as the terminal shows them, the tag apart.
    pub fn text(&self) -> String {
        self.parts.iter().map(|p| p.text.as_str()).collect()
    }
}

/// A string's width in terminal cells, as ratatui measures it when it draws the line.
fn cells(s: &str) -> usize {
    ratatui::text::Span::raw(s).width()
}

/// The dashboard's top line, one line, in groups: the fleet in short (pods, GPUs and their mean
/// util, unreachable pods in yellow), then one group per provider — its pod count and name, the
/// account's balance and runway, the burn of its pods (`3 RunPod $120.30 ≈4.1d · $1.20/h`,
/// Hetzner's `€/h` with no balance) — then OpenRouter's (`$12.40 today · $422.09 left`), a dim
/// `│` between groups. The human (2026-10-30): "can u make the top bar of balances maybe more
/// verbally compact / organised?" — it was one `·`-joined list, balances, burn and counts mixed,
/// and which provider held which pods, cost what, or had how long left took reading all of it.
///
/// A group's name is green while all is well, yellow with [`WARN_MARK`] in front when its
/// account is low, red when critical or when the provider failed to list (`⚠ Vast: list
/// failed`: its pods are missing); OpenRouter's the same for credit under what the keys' caps
/// allow, keys at their cap (named, [`CohortUsage::capped_list`]) and credit gone. A provider
/// with no pods and nothing wrong isn't here (the info panel has its balance).
///
/// Built to fit `width`: the line was cut at the terminal's edge, so warnings had to lead it.
/// Now the least needed words go first, each kind everywhere at once — the `tag` at the right
/// end, then ([`Cut`]'s order) GPU util, the GPU count, a fine account's runway, today's AI
/// spend, keys with no cap, a warning's details (`key at cap`, unnamed), the burns, … — and
/// never a warning or a group's name. Past every cut the worst groups go first (red, yellow,
/// then the calm names) and those that still don't fit are counted at the end (`+2 more`, in
/// the worst colour among them), so a warning is never cut off at the edge unsaid. Every word
/// left out is the info panel's ([`summary_text`] there). Pure.
pub fn top_parts(
    s: &FleetSummary,
    pods: &[Pod],
    partial: &[String],
    balance: &[AccountView],
    ai: Option<&CohortUsage>,
    tag: Option<&str>,
    width: usize,
) -> TopLine {
    let mut groups = vec![fleet_group(s)];
    let shares = by_provider(pods);
    let mut names: Vec<&str> = shares.iter().map(|p| p.provider.as_str()).chain(partial.iter().map(String::as_str)).chain(balance.iter().map(|v| v.account.as_str())).collect();
    names.sort_by_key(|n| (provider_rank(n), *n));
    names.dedup();
    for name in names {
        let share = shares.iter().find(|p| p.provider == name);
        let account = balance.iter().find(|v| v.account == name).and_then(balance::inline_account);
        groups.extend(provider_group(name, share, partial.iter().any(|p| p == name), account));
    }
    groups.extend(ai.and_then(openrouter_group));
    let width_of = |parts: &[Part]| parts.iter().map(|p| cells(&p.text)).sum::<usize>();
    // The tag goes first: two cells' gap, and one free at the right edge.
    if let Some(tag) = tag {
        let parts = lay_out(&groups, None);
        if width_of(&parts) + 2 + cells(tag) < width {
            return TopLine { parts, tag: Some(tag.to_string()) };
        }
    }
    for cut in std::iter::once(None).chain(CUTS.map(Some)) {
        let parts = lay_out(&groups, cut);
        if width_of(&parts) <= width {
            return TopLine { parts, tag: None };
        }
    }
    // Past every cut: the worst first (red, then yellow, then the calm names), and the groups
    // that still don't fit counted at the end (`+2 more`, in the worst colour among them: the
    // info panel has them) — never a warning cut off at the edge unsaid.
    let last = CUTS.last().copied();
    let tone_of = |severity: u8| [Tone::Bad, Tone::Warn, Tone::Dim][usize::from(severity)];
    groups.retain(|g| g.iter().any(|p| p.cut.is_none()));
    groups.sort_by_key(|g| severity(g));
    for keep in (1..=groups.len()).rev() {
        let mut parts = lay_out(&groups[..keep], last);
        let hidden = &groups[keep..];
        if let Some(worst) = hidden.iter().map(|g| severity(g)).min() {
            parts.push(Part::toned(" │ ", Some(Tone::Dim)));
            parts.push(Part::toned(format!("+{} more", hidden.len()), Some(tone_of(worst))));
        }
        if width_of(&parts) <= width {
            return TopLine { parts, tag: None };
        }
    }
    // Not even one group whole: how many warnings there are (`⚠ 4 warnings`), else the calm
    // names cut at the edge.
    let warned: Vec<u8> = groups.iter().map(|g| severity(g)).filter(|s| *s < 2).collect();
    match warned.iter().min() {
        Some(&worst) => TopLine {
            parts: vec![Part::plain(" "), Part::toned(format!("{WARN_MARK} {}", count_of(warned.len(), "warning", "warnings")), Some(tone_of(worst)))],
            tag: None,
        },
        None => TopLine { parts: lay_out(&groups, last), tag: None },
    }
}

/// How bad a group is, worst first when sorted: 0 red (a critical account, a failed listing,
/// credit gone), 1 yellow, 2 nothing wrong.
fn severity(g: &[Piece]) -> u8 {
    let has = |t: Tone| g.iter().any(|p| p.tone == Some(t));
    if has(Tone::Bad) {
        0
    } else if has(Tone::Warn) {
        1
    } else {
        2
    }
}

/// What the top line gives up when the terminal is narrow, in this order, each kind in every
/// group at once (no group reads as if it had no runway while the next shows one). A warning
/// and a group's name have none: they stay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Cut {
    /// The fleet's mean GPU util.
    Util,
    /// The fleet's GPU count.
    Gpus,
    /// The runway of an account that needs nothing.
    Runway,
    /// The cohort's AI spend today.
    Today,
    /// How many AI keys have no cap.
    Uncapped,
    /// A warning's details: it says it in short (`key at cap`, `$9.00 < min`; the names are the
    /// info panel's, and the `AI$` column's red cells).
    Long,
    /// A provider's burn.
    Burn,
    /// The balance of an account that needs nothing; OpenRouter's credit (and its `n/a`).
    Balance,
    /// A provider's pod count.
    Count,
    /// The fleet's pod count.
    Pods,
}

const CUTS: [Cut; 10] = [Cut::Util, Cut::Gpus, Cut::Runway, Cut::Today, Cut::Uncapped, Cut::Long, Cut::Burn, Cut::Balance, Cut::Count, Cut::Pods];

/// How a piece joins the one before it in its group: ` `, `, `, `: `, or ` · ` — but a space
/// after the group's head (`3 RunPod $120.30 ≈4.1d · $1.20/h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Join {
    Space,
    Comma,
    Colon,
    Dot,
}

/// A piece of a top-line group: its words, colour, how it joins the one before, when it goes
/// ([`Cut`]; `None`: never), whether it is the group's head (mark, count, name), and a warning's
/// words in short, from [`Cut::Long`] on.
#[derive(Debug, Clone)]
struct Piece {
    text: String,
    tone: Option<Tone>,
    join: Join,
    cut: Option<Cut>,
    head: bool,
    short: Option<String>,
}

impl Piece {
    fn new(text: impl Into<String>, join: Join, cut: Option<Cut>) -> Self {
        Piece { text: text.into(), tone: None, join, cut, head: false, short: None }
    }

    fn head(text: impl Into<String>, tone: Option<Tone>, cut: Option<Cut>) -> Self {
        Piece { text: text.into(), tone, join: Join::Space, cut, head: true, short: None }
    }

    /// A warning: never cut, said in short (`short`) from [`Cut::Long`] on.
    fn warning(text: impl Into<String>, short: impl Into<String>, tone: Option<Tone>) -> Self {
        Piece { short: Some(short.into()), ..Piece::new(text, Join::Dot, None).toned(tone) }
    }

    fn toned(mut self, tone: Option<Tone>) -> Self {
        self.tone = tone;
        self
    }
}

/// The groups as parts, without the pieces `cut` (and every [`Cut`] before it) gives up; a
/// group left with nothing goes, and with it its `│`.
fn lay_out(groups: &[Vec<Piece>], cut: Option<Cut>) -> Vec<Part> {
    let mut out = vec![Part::plain(" ")];
    let mut first = true;
    for g in groups {
        let shown: Vec<&Piece> = g.iter().filter(|p| p.cut.is_none_or(|c| Some(c) > cut)).collect();
        if shown.is_empty() {
            continue;
        }
        if !first {
            out.push(Part::toned(" │ ", Some(Tone::Dim)));
        }
        first = false;
        for (i, p) in shown.iter().enumerate() {
            if i > 0 {
                out.push(Part::plain(match p.join {
                    Join::Space => " ",
                    Join::Comma => ", ",
                    Join::Colon => ": ",
                    Join::Dot if shown[i - 1].head => " ",
                    Join::Dot => " · ",
                }));
            }
            let text = p.short.as_ref().filter(|_| cut >= Some(Cut::Long)).unwrap_or(&p.text);
            out.push(Part::toned(text.clone(), p.tone));
        }
    }
    out
}

/// The fleet's group: `51 pods, 48 GPUs 37%`, and `· 2 unreachable` in yellow when some are.
fn fleet_group(s: &FleetSummary) -> Vec<Piece> {
    let mut g = vec![Piece::new(count_of(s.pods, "pod", "pods"), Join::Space, Some(Cut::Pods))];
    g.push(Piece::new(count_of(s.total_gpus, "GPU", "GPUs"), Join::Comma, Some(Cut::Gpus)));
    if let Some(u) = s.mean_util {
        g.push(Piece::new(format!("{u}%"), Join::Space, Some(Cut::Util)));
    }
    if s.unreachable > 0 {
        g.push(Piece::new(format!("{} unreachable", s.unreachable), Join::Dot, None).toned(Some(Tone::Warn)));
    }
    g
}

/// A provider's group (see [`top_parts`]), or `None` when it has no pods and nothing wrong.
/// `account`: its prepaid account's words ([`balance::inline_account`]; none for Hetzner, or
/// before the first read).
fn provider_group(provider: &str, share: Option<&ProviderShare>, failed: bool, account: Option<balance::InlineAccount>) -> Option<Vec<Piece>> {
    let level = account.as_ref().map(|a| a.level);
    let low = matches!(level, Some(Level::Low | Level::Critical));
    let pods = share.map_or(0, |p| p.pods);
    if pods == 0 && !failed && !low {
        return None;
    }
    let tone = match level {
        _ if failed => Some(Tone::Bad),
        Some(Level::Critical) => Some(Tone::Bad),
        Some(Level::Low) => Some(Tone::Warn),
        // No balance while it burns, or pods nobody priced: not well, not a warning either.
        Some(Level::Unknown) => None,
        _ => Some(Tone::Good),
    };
    let mut g = Vec::new();
    if failed || low {
        g.push(Piece::head(WARN_MARK, tone, None));
    }
    if pods > 0 {
        g.push(Piece::head(pods.to_string(), tone, Some(Cut::Count)));
    }
    g.push(Piece::head(provider_title(provider), tone, None));
    if failed {
        g.push(Piece::new("list failed", Join::Colon, None).toned(Some(Tone::Bad)));
    }
    // The amount and its runway side by side, as one (`$120.30 ≈4.1d`); a low or critical
    // account's are the warning: they stay, in its colour.
    let warn = if low { tone } else { None };
    let unless_low = |c: Cut| (!low).then_some(c);
    if let Some(a) = account {
        g.push(Piece::new(a.amount, Join::Dot, unless_low(Cut::Balance)).toned(warn));
        g.extend(a.runway.map(|r| {
            let short = (r == balance::UNDER_MINIMUM).then(|| "< min".to_string());
            Piece { short, ..Piece::new(r, Join::Space, unless_low(Cut::Runway)).toned(warn) }
        }));
    }
    if let Some(cost) = share.map(|p| &p.cost).filter(|c| c.billing > 0) {
        g.push(Piece::new(provider_burn(provider, cost), Join::Dot, Some(Cut::Burn)));
    }
    Some(g)
}

/// OpenRouter's group (see [`top_parts`]): its warnings, the cohort's spend today, the credit
/// left, keys with no cap, `n/a` when the last read failed (the numbers are the last good ones).
/// `None` when there is nothing to say.
fn openrouter_group(ai: &CohortUsage) -> Option<Vec<Piece>> {
    let money = |x: f64| fleet::fmt_money("$", x);
    // (the warning, in short)
    let mut warnings = Vec::new();
    let mut tone = if ai.failed { None } else { Some(Tone::Good) };
    match ai.credit_left {
        Some(left) if left <= 0.005 => {
            warnings.push((format!("{} left, calls fail", money(left)), format!("{} left", money(left))));
            tone = Some(Tone::Bad);
        }
        Some(left) if left < ai.headroom => {
            warnings.push((format!("{} left < caps' {}", money(left), money(ai.headroom)), format!("{} left", money(left))));
            tone = Some(Tone::Warn);
        }
        _ => {}
    }
    // The credit, when its warning hasn't said it.
    let credit = ai.credit_left.filter(|_| warnings.is_empty());
    if let Some(list) = ai.capped_list() {
        // One a row carries can be raised from its menu: `a`, then `ai cap` (`a` again).
        let raise = if ai.capped.iter().any(|k| k.pod) { " (a → raise cap)" } else { "" };
        let keys = match ai.capped.len() {
            1 => "key".to_string(),
            n => format!("{n} keys"),
        };
        warnings.push((format!("{keys} at cap: {list}{raise}"), format!("{keys} at cap")));
        if tone != Some(Tone::Bad) {
            tone = Some(Tone::Warn);
        }
    }
    let mut g = Vec::new();
    if !warnings.is_empty() {
        g.push(Piece::head(WARN_MARK, tone, None));
    }
    g.push(Piece::head("OpenRouter", tone, None));
    let warned = !warnings.is_empty();
    g.extend(warnings.into_iter().map(|(long, short)| Piece::warning(long, short, tone)));
    if ai.keys > 0 {
        g.push(Piece::new(format!("{} today", ai.today.map(money).unwrap_or_else(|| "?".into())), Join::Dot, Some(Cut::Today)));
    }
    if let Some(left) = credit {
        g.push(Piece::new(format!("{} left", money(left)), Join::Dot, Some(Cut::Balance)));
    }
    if ai.uncapped > 0 {
        g.push(Piece::new(format!("{} uncapped", ai.uncapped), Join::Dot, Some(Cut::Uncapped)));
    }
    if ai.failed {
        g.push(Piece::new("n/a", Join::Dot, Some(Cut::Balance)));
    }
    let said = ai.keys > 0 || ai.credit_left.is_some() || ai.failed || warned;
    said.then_some(g)
}

/// A provider's burn in short, in its currency: `$1.20/h`, `€0.011/h`, and `(2 unpriced)` when
/// some of its billing pods have no price — the figure is then a floor. Pure.
fn provider_burn(provider: &str, c: &FleetCost) -> String {
    let symbol = fleet::currency_symbol(provider);
    let per_hr = if symbol == "€" { c.eur_per_hr } else { c.usd_per_hr };
    let mut out = format!("{}/h", fleet::fmt_money(symbol, per_hr));
    if c.unpriced > 0 {
        out.push_str(&format!(" ({} unpriced)", c.unpriced));
    }
    out
}

/// One provider's share of the fleet: its pods (listed, billing or not) and what they burn —
/// [`fleet::fleet_cost`] over its pods alone, the same pricing as the fleet's total and `pods
/// list`'s footer, never a second one.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderShare {
    pub provider: String,
    pub pods: usize,
    pub cost: FleetCost,
}

/// The fleet by provider, those with pods: RunPod, Vast, Hetzner, Shadeform (the selector's
/// order), any other after them by name — a group never moves as the counts change. Pure.
pub fn by_provider(pods: &[Pod]) -> Vec<ProviderShare> {
    let mut names: Vec<&str> = pods.iter().map(|p| p.provider.as_str()).collect();
    names.sort_by_key(|n| (provider_rank(n), *n));
    names.dedup();
    names
        .into_iter()
        .map(|name| {
            let mine = || pods.iter().filter(move |p| p.provider == name);
            ProviderShare { provider: name.to_string(), pods: mine().count(), cost: fleet::fleet_cost(mine()) }
        })
        .collect()
}

/// Where a provider goes in the top line: [`PROVIDERS`]' order, any other after.
fn provider_rank(provider: &str) -> usize {
    PROVIDERS.iter().position(|p| *p == provider).unwrap_or(PROVIDERS.len())
}

/// The refresh's status: the footer's short form and its colour (`ok · API 13/min`, `warn: RunPod
/// rate limit +1`, `list error`, `reading 3 pods…`) and the whole line, for the info panel. Plain
/// words, no symbols: some terminals draw `✓`/`⚠` as emoji pictures (the human, 2026-10-30:
/// "i'd slightly prefer if it was all normal text"; later, of icons, "its just if you have too
/// many then the text sizing becomes pretty unreliable" — the footer keeps one, its refresh
/// mark).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RefreshStatus {
    pub short: String,
    pub tone: Option<Tone>,
    pub full: String,
}

impl RefreshStatus {
    /// Work under way (`loading…`): the short form is the line.
    pub fn busy(text: impl Into<String>) -> Self {
        let text = text.into();
        RefreshStatus { short: text.clone(), tone: Some(Tone::Busy), full: text }
    }

    /// A first listing shown before its pods are read.
    pub fn listed(pods: &str) -> Self {
        RefreshStatus { short: format!("reading {pods}…"), tone: Some(Tone::Busy), full: format!("listed {pods} — reading them…") }
    }

    /// No provider listed: the rows are the last ones.
    pub fn list_error(full: String) -> Self {
        RefreshStatus { short: "list error".into(), tone: Some(Tone::Bad), full }
    }

    /// A refresh that listed: `full` (the dashboard's `status_line`) whole; short, the first warning's
    /// [`warning_label`] and how many more (yellow), else `ok` and the providers' API requests in
    /// the last minute (`rate`, when any were counted).
    pub fn of(full: String, warnings: &[&str], rate: Option<usize>) -> Self {
        match warnings.first() {
            Some(w) => {
                let more = if warnings.len() > 1 { format!(" +{}", warnings.len() - 1) } else { String::new() };
                RefreshStatus { short: format!("warn: {}{more}", warning_label(w)), tone: Some(Tone::Warn), full }
            }
            None => RefreshStatus { short: rate.map_or_else(|| "ok".to_string(), |r| format!("ok · API {r}/min")), tone: Some(Tone::Good), full },
        }
    }
}

/// A refresh warning's few first words, for the footer (`RunPod rate limit`, `pod details
/// incomplete`, `ssh config not rewritten`): up to its first `:`, ` (` or ` — `, at most 28
/// characters. Pure.
pub fn warning_label(w: &str) -> String {
    let w = w.strip_prefix("warning: ").unwrap_or(w);
    let end = [": ", " (", " — "].iter().filter_map(|sep| w.find(sep)).min().unwrap_or(w.len());
    let head = &w[..end];
    if head.chars().count() > 28 {
        format!("{}…", head.chars().take(27).collect::<String>())
    } else {
        head.to_string()
    }
}

/// The colour of the OpenRouter warning in the info panel (`None`: there's none).
pub fn ai_tone(ai: Option<&CohortUsage>) -> Option<Tone> {
    match ai.map(|a| a.tui_parts())? {
        (Some(_), _, Fill::Full) => Some(Tone::Bad),
        (Some(_), _, _) => Some(Tone::Warn),
        _ => None,
    }
}

/// An `AI$` cell's colour: plain under 80% of the cap, warn from there, bad at the cap or
/// disabled.
pub fn fill_tone(f: Fill) -> Option<Tone> {
    match f {
        Fill::Ok => None,
        Fill::Near => Some(Tone::Warn),
        Fill::Full => Some(Tone::Bad),
    }
}

/// `1 pod`, `0 pods`, `3 GPUs` — the bar said `1 pods`.
pub fn count_of(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// How a cell should read at a glance; `main` maps it to a colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Good,
    Warn,
    Bad,
    /// Work in flight (a deep check running).
    Busy,
    /// Nothing to say (never checked, no forward, unknown).
    Dim,
}

/// How often each endpoint's RTT is timed again. RTT barely moves, and each timing is a TCP
/// connect closed before any SSH — what OpenSSH ≥ 9.8 counts against the source address
/// (`arena_core::rtt`): once a minute keeps several dashboards on the proxy box far from it.
pub const RTT_EVERY: Duration = Duration::from_secs(60);

/// How long one timing may take (name lookup and the connect together): far above any real
/// round trip (Singapore from Nuremberg: ~160 ms); past it the RTT is unknown.
pub const RTT_WAIT: Duration = Duration::from_secs(1);

/// A timing this old is forgotten (its endpoint is long gone, or will be timed afresh).
pub const RTT_FORGET: Duration = Duration::from_secs(600);

/// An SSH endpoint, `(host, port)`.
pub type Endpoint = (String, u16);

/// The round-trip times from this box (where the proxy runs) to each pod's SSH endpoint,
/// timed by the refresh ([`Rtts::due`]) and kept between rounds. By endpoint, not by name: a
/// pod that moved (restart, replacement) is timed afresh, never shown its old host's RTT.
#[derive(Debug, Clone, Default)]
pub struct Rtts {
    timed: HashMap<Endpoint, (std::time::Instant, Option<Duration>)>,
}

impl Rtts {
    /// The endpoints of `pods` to time now: those never timed, or last timed [`RTT_EVERY`] or
    /// more before `now` — each once, in the pods' order. A pod without an endpoint has none.
    pub fn due(&self, pods: &[Pod], now: std::time::Instant) -> Vec<Endpoint> {
        let mut out: Vec<Endpoint> = Vec::new();
        for p in pods {
            let (Some(ip), Some(port)) = (p.ssh_ip.as_deref().map(str::trim).filter(|ip| !ip.is_empty()), p.ssh_port) else { continue };
            let ep = (ip.to_string(), port);
            let fresh = self.timed.get(&ep).is_some_and(|(at, _)| now.saturating_duration_since(*at) < RTT_EVERY);
            if !fresh && !out.contains(&ep) {
                out.push(ep);
            }
        }
        out
    }

    /// Record a round of timings (`None`: no answer within [`RTT_WAIT`]) at `now`, and forget
    /// the ones [`RTT_FORGET`] old — by age, not by absence from this listing: a provider that
    /// missed one listing mustn't get its pods dialled again on the next refresh.
    pub fn record(&mut self, timed: Vec<(Endpoint, Option<Duration>)>, now: std::time::Instant) {
        for (ep, d) in timed {
            self.timed.insert(ep, (now, d));
        }
        self.timed.retain(|_, (at, _)| now.saturating_duration_since(*at) < RTT_FORGET);
    }

    /// `pod`'s RTT as last timed; `None`: no endpoint, not timed yet, or no answer.
    pub fn of(&self, pod: &Pod) -> Option<Duration> {
        let ep = (pod.ssh_ip.as_deref()?.trim().to_string(), pod.ssh_port?);
        self.timed.get(&ep).and_then(|(_, d)| *d)
    }
}

/// The RTT cell: `23ms` coloured by what a typist feels — green under 60 ms (Europe from
/// Nuremberg, Iceland included), yellow under 120 (the US east coast: every keystroke
/// noticeably late), red beyond (the US west, Asia); `-` dim when unknown. Pure.
pub fn rtt_cell(rtt: Option<Duration>) -> (String, Tone) {
    match rtt {
        None => ("-".into(), Tone::Dim),
        Some(d) => {
            let ms = d.as_millis();
            let tone = match ms {
                0..60 => Tone::Good,
                60..120 => Tone::Warn,
                _ => Tone::Bad,
            };
            let label = if ms >= 1000 { ">1s".to_string() } else { format!("{ms}ms") };
            (label, tone)
        }
    }
}

/// The HEALTH cell (`HLTH`, five cells): a glyph for the last cached verdict and its age —
/// `✓12m` pass, `!5h` warn, `✗2h` fail, `·3d` skipped ([`health_glyph`] + `snapshot::fmt_age`,
/// at most `365d`) — `…` while a deep check of this pod runs, `-` for a pod never checked. The
/// words, and the worst issue, are the detail pane's (`fail 2h GPU error`, as `arena snapshot`
/// words them). `now` is the snapshot's clock. Pure.
pub fn health_cell(h: Option<&HealthRecord>, checking: bool, now: u64) -> (String, Tone) {
    if checking {
        return ("…".into(), Tone::Busy);
    }
    let Some(h) = h else { return ("-".into(), Tone::Dim) };
    let age = now.saturating_sub(h.checked_at);
    (format!("{}{}", health_glyph(h.status), fmt_age(age)), health_tone(h.status, age))
}

/// A verdict's glyph in the HEALTH cell: `✓` pass, `!` warn, `✗` fail, `·` skipped — each one
/// cell wide in every terminal (the legend in the info panel says which is which).
pub fn health_glyph(s: Status) -> &'static str {
    match s {
        Status::Pass => "✓",
        Status::Warn => "!",
        Status::Fail => "✗",
        Status::Skip => "·",
    }
}

/// A verdict's colour, `age` seconds after it: a pass says little about today once it's over a
/// day old ([`HEALTH_FRESH`]) — grey, so it can't pass for a fresh one. A warning or a failure
/// keeps its colour (nobody has said otherwise).
pub fn health_tone(s: Status, age: u64) -> Tone {
    match s {
        Status::Pass if age > HEALTH_FRESH => Tone::Dim,
        Status::Pass => Tone::Good,
        Status::Warn => Tone::Warn,
        Status::Fail => Tone::Bad,
        Status::Skip => Tone::Dim,
    }
}

/// How long a deep check's pass reads as current in the HEALTH column (then grey): the health
/// cron's daily cadence plus two hours, as it checks the pods one by one and its run drifts —
/// a working cron never leaves a pass grey.
pub const HEALTH_FRESH: u64 = 26 * 3_600;

/// The PROXY cell: the forward's public port at every width — `9500` live (green), `9500!`
/// stale (yellow: the forward still points at the pod's old endpoint, `arena proxy apply`
/// re-points it), `-` no forward, `?` the proxy file couldn't be read. The words are the detail
/// pane's ([`proxy_detail`]). Pure.
pub fn proxy_cell(p: &SnapshotPod) -> (String, Tone) {
    match (p.proxy, p.proxy_port) {
        (ProxyState::Live, Some(port)) => (port.to_string(), Tone::Good),
        (ProxyState::Stale, Some(port)) => (format!("{port}{PROXY_STALE}"), Tone::Warn),
        (ProxyState::Unknown, _) => ("?".into(), Tone::Dim),
        _ => ("-".into(), Tone::Dim),
    }
}

/// The glyph after a stale forward's port in the PROXY cell.
pub const PROXY_STALE: &str = "!";

/// The detail pane's proxy line, spelling out what the state means for the operator.
pub fn proxy_detail(p: &SnapshotPod) -> String {
    let label = snapshot::proxy_label(p);
    match p.proxy {
        ProxyState::Live => format!("{label} (live)"),
        ProxyState::Stale => format!("{label} (`arena proxy apply` re-points it)"),
        ProxyState::None => "- (no forward for this name)".into(),
        ProxyState::Unknown => "? (remote/unreadable proxy config)".into(),
    }
}

/// Whether the pod's host has a maintenance window (or note) on record — the badge. Same
/// test as `pods list`'s MAINT column: [`fleet::maintenance_label`] says something.
pub fn has_maintenance(pod: &Pod) -> bool {
    fleet::maintenance_label(pod.maintenance.as_ref()) != "-"
}

/// The dashboard's rows for one refresh: [`snapshot::build`] over what was listed, ordered
/// by name as the dashboard always has been (the builder groups by provider for `pods
/// list`; a stable sort keeps that order between pods sharing a name).
pub fn dashboard_snapshot(
    pods: &[Pod],
    partial: &[String],
    proxy_file: Option<&str>,
    health: &HealthCache,
    naming: &Naming,
    now: u64,
) -> FleetSnapshot {
    let mut snap = snapshot::build(pods, partial, proxy_file, health, naming, now);
    snap.pods.sort_by(|a, b| a.pod.name.cmp(&b.pod.name));
    snap
}

/// Deep checks started from the dashboard: which pods are being checked now (so a second
/// `d` doesn't double up and the row can say `checking`), and the verdicts this session
/// produced. Those are also written to the shared health cache file, but kept here too so
/// a result shows even when that write failed, and isn't lost when the next refresh
/// re-reads the file — [`DeepChecks::layer`] puts them on top of it, newest wins.
#[derive(Debug, Clone, Default)]
pub struct DeepChecks {
    running: HashSet<String>,
    session: HealthCache,
}

impl DeepChecks {
    pub fn is_running(&self, pod: &Pod) -> bool {
        self.running.contains(&cache_key(&pod.provider, &pod.id))
    }

    /// How many pods are being checked right now.
    pub fn running(&self) -> usize {
        self.running.len()
    }

    /// Claim `pods` for a check: returns those not already being checked, now marked
    /// running (the caller checks exactly these, then hands them back to [`Self::finish`]).
    pub fn begin(&mut self, pods: Vec<Pod>) -> Vec<Pod> {
        pods.into_iter().filter(|p| self.running.insert(cache_key(&p.provider, &p.id))).collect()
    }

    /// Fold a finished check in: release every claimed pod (`claimed`, even one that got
    /// no result), record the verdicts (checked at `now`), show them on the rows of `snap`
    /// right away, and return the line for the status bar — the tally as `pods test
    /// --deep` prints it, plus which pods failed.
    pub fn finish(&mut self, claimed: &[Pod], results: &[PodHealth], now: u64, snap: &mut FleetSnapshot) -> String {
        for p in claimed {
            self.running.remove(&cache_key(&p.provider, &p.id));
        }
        self.session.merge(results, now);
        for row in &mut snap.pods {
            if let Some(r) = self.session.get(&row.pod) {
                if row.health.as_ref().is_none_or(|h| h.checked_at <= r.checked_at) {
                    row.health = Some(r.clone());
                }
            }
        }
        let tally = render_summary(results).into_iter().next().unwrap_or_default();
        let failed: Vec<&str> = results.iter().filter(|h| h.status == Status::Fail).map(|h| h.name.as_str()).collect();
        if failed.is_empty() {
            format!("deep check: {tally}")
        } else {
            format!("deep check: {tally} — failed: {}", failed.join(", "))
        }
    }

    /// The cache to show: `file` (what every `arena` command recorded) with this session's
    /// verdicts on top wherever they are at least as new.
    pub fn layer(&self, mut file: HealthCache) -> HealthCache {
        for (k, r) in &self.session.pods {
            if file.pods.get(k).is_none_or(|f| f.checked_at <= r.checked_at) {
                file.pods.insert(k.clone(), r.clone());
            }
        }
        file
    }
}

/// How often the provider's details query (`Provider::enrich`: RunPod's GraphQL for GPU,
/// $/h and the host maintenance window) runs at most, unprompted. The pod list is polled
/// every few seconds; the details change rarely, and doubling the call volume against a
/// rate-limited API would cost more than a minute-old maintenance window.
pub const DETAILS_EVERY: Duration = Duration::from_secs(60);

/// The shortest gap between two details queries even when `r` asks for one, so holding the
/// key can't hammer the API.
pub const DETAILS_MIN_GAP: Duration = Duration::from_secs(10);

/// Whether this refresh should also run the details query: never run yet, the last one
/// [`DETAILS_EVERY`] ago, or asked for (`forced`, the `r` key) at least
/// [`DETAILS_MIN_GAP`] after the last.
pub fn details_due(since_last: Option<Duration>, forced: bool) -> bool {
    match since_last {
        None => true,
        Some(d) => d >= DETAILS_EVERY || (forced && d >= DETAILS_MIN_GAP),
    }
}

/// A provider whose listing came back rate-limited this refresh — a 429, or refused before
/// sending because its API's quota is held (`arena_core::ratelimit`: a 429's wait, a spent
/// window, a low window's spacing) — for the footer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Throttle {
    pub provider: String,
    /// Its API's hold, when the quota is held ([`arena_core::ratelimit::retry_in`]).
    pub hold: Option<arena_core::ratelimit::Hold>,
    /// How old its rows on screen are (`None`: it has none to show).
    pub rows_age: Option<Duration>,
}

/// The footer's words for the rate-limited providers (`RunPod rate limit: listing again in
/// 12 s (the minute window is spent) · its rows are 40 s old`); `None` when none is. The
/// dashboard asks every refresh — a held quota refuses it without a request, so nothing is
/// sent until the hold has passed. Pure.
pub fn listing_note(throttled: &[Throttle]) -> Option<String> {
    let parts: Vec<String> = throttled
        .iter()
        .map(|t| {
            let when = match &t.hold {
                Some(h) => format!("listing again in {} ({})", arena_core::ratelimit::wait_words(h.wait), h.why),
                None => "listing again next refresh".to_string(),
            };
            let rows = match t.rows_age {
                Some(age) => format!(" · its rows are {} old", arena_core::ratelimit::wait_words(age)),
                None => " · no rows yet".to_string(),
            };
            format!("{} rate limit: {when}{rows}", provider_title(&t.provider))
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// A provider's name as people write it (`runpod` → `RunPod`). Pure.
pub fn provider_title(provider: &str) -> String {
    match provider {
        "runpod" => "RunPod".into(),
        "vast" => "Vast".into(),
        "hetzner" => "Hetzner".into(),
        "shadeform" => "Shadeform".into(),
        other => other.into(),
    }
}

/// `fresh` with each `throttled` provider's failed listing replaced by its last good one in
/// `prev`: a rate-limited provider keeps its rows (as last listed) instead of vanishing from
/// the screen while it is backed off. A provider that failed any other way, or has no good
/// listing to fall back on, keeps its failure. Pure.
pub fn carry_over(prev: Option<&Listing>, fresh: &mut Listing, throttled: &[String]) {
    let Some(prev) = prev else { return };
    for pl in fresh.providers.iter_mut().filter(|pl| pl.pods.is_err() && throttled.contains(&pl.provider)) {
        if let Some(Ok(pods)) = prev.providers.iter().find(|p| p.provider == pl.provider).map(|p| &p.pods) {
            pl.pods = Ok(pods.clone());
        }
    }
}

/// Each provider's last good listing, after `fresh`: its own where it listed, else what
/// `prev` had for it — so a provider that failed (a 502), then was rate-limited, still has
/// rows to fall back on ([`carry_over`]). A provider no longer listed at all is dropped. Pure.
pub fn remember_good(prev: Option<&Listing>, fresh: &Listing) -> Listing {
    let providers = fresh
        .providers
        .iter()
        .map(|pl| match (&pl.pods, prev.and_then(|p| p.providers.iter().find(|q| q.provider == pl.provider))) {
            (Err(_), Some(old)) if old.pods.is_ok() => old.clone(),
            _ => pl.clone(),
        })
        .collect();
    Listing { providers }
}

/// What the details query told us about one pod, kept between queries.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PodDetails {
    pub gpu_type: Option<String>,
    pub gpu_count: Option<u32>,
    pub cost_per_hr: Option<f64>,
    pub maintenance: Option<Maintenance>,
}

/// The details of every pod just enriched, keyed like the health cache (`provider:id`).
pub fn capture_details(enriched: &[Pod]) -> HashMap<String, PodDetails> {
    enriched
        .iter()
        .map(|p| {
            let d = PodDetails {
                gpu_type: p.gpu_type.clone(),
                gpu_count: p.gpu_count,
                cost_per_hr: p.cost_per_hr,
                maintenance: p.maintenance.clone(),
            };
            (cache_key(&p.provider, &p.id), d)
        })
        .collect()
}

/// Fold one details query's answer (`fresh`, [`capture_details`] of the pods it ran on) into
/// what is kept between queries. A `complete` answer replaces everything — a maintenance
/// window that ended is gone. An incomplete one (the query erred or timed out *after*
/// filling some pods: a GraphQL reply carrying both data and `errors`, one backend of the
/// fleet failing — `MultiProvider::enrich` writes back what it got, and `pods list` shows
/// it) adds what it did fill, field by field, over the last good details: dropping it would
/// leave the dashboard blank where `pods list` shows a GPU, $/h or maintenance badge, and a
/// field it didn't fill can't be told from one that is really empty, so that keeps the
/// last good value.
pub fn merge_details(kept: &mut HashMap<String, PodDetails>, fresh: HashMap<String, PodDetails>, complete: bool) {
    if complete {
        *kept = fresh;
        return;
    }
    for (key, new) in fresh {
        let old = kept.remove(&key).unwrap_or_default();
        let merged = PodDetails {
            gpu_type: new.gpu_type.or(old.gpu_type),
            gpu_count: new.gpu_count.or(old.gpu_count),
            cost_per_hr: new.cost_per_hr.or(old.cost_per_hr),
            maintenance: new.maintenance.or(old.maintenance),
        };
        kept.insert(key, merged);
    }
}

/// Re-apply the last details to a fresh listing between queries, the way `enrich` merges
/// them: fill the GPU type, $/h and maintenance window the cheap list call leaves out (a
/// value the listing does report wins), and take the details' GPU count when it has one.
/// A pod the details never covered (created since) is left as listed.
pub fn overlay_details(pods: &mut [Pod], details: &HashMap<String, PodDetails>) {
    for p in pods.iter_mut() {
        let Some(d) = details.get(&cache_key(&p.provider, &p.id)) else { continue };
        if p.gpu_type.is_none() {
            p.gpu_type = d.gpu_type.clone();
        }
        p.gpu_count = d.gpu_count.or(p.gpu_count);
        if p.cost_per_hr.is_none() {
            p.cost_per_hr = d.cost_per_hr;
        }
        if p.maintenance.is_none() {
            p.maintenance = d.maintenance.clone();
        }
    }
}

/// Split what was typed at `/` into the core selector's arguments: plain tokens are
/// targets (names, ids, ranges `a..b`, comma lists); `-x`/`--exclude TOKEN` or `!TOKEN`
/// leave pods out; `--on PROVIDER` and `--gpus N` narrow (both also as `--flag=value`).
pub fn parse_select_input(input: &str) -> Result<SelectArgs, String> {
    let mut args = SelectArgs::default();
    let mut words = input.split_whitespace();
    while let Some(w) = words.next() {
        let (flag, inline) = match w.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (w, None),
        };
        let takes_value = matches!(flag, "-x" | "--exclude" | "--on" | "--gpus");
        let value = if takes_value {
            match inline.or_else(|| words.next().map(String::from)) {
                Some(v) => v,
                None => return Err(format!("{flag} needs a value")),
            }
        } else {
            String::new()
        };
        match flag {
            "-x" | "--exclude" => args.exclude.push(value),
            "--on" => args.on = Some(value),
            "--gpus" => {
                args.gpus = Some(value.parse().map_err(|_| format!("--gpus wants a number of GPUs (got `{value}`)"))?)
            }
            "--all" => args.all = true,
            _ if w.len() > 1 && w.starts_with('!') => args.exclude.push(w[1..].to_string()),
            _ if w.starts_with('-') => {
                return Err(format!(
                    "unknown option `{w}` — type names, ids or ranges (apple..delta), -x/!NAME to leave one out, --on PROVIDER, --gpus N"
                ))
            }
            _ => args.targets.push(w.to_string()),
        }
    }
    Ok(args)
}

/// What a mark is keyed by: the pod's `provider:id` ([`cache_key`], as the health cache,
/// the details and the deep-check claims are). Ids are only unique per provider — a Vast
/// instance and a Hetzner server can share a number — so a bare id would mark (and hand
/// to terminate/restart) a pod the operator never picked.
pub fn mark_key(pod: &Pod) -> String {
    cache_key(&pod.provider, &pod.id)
}

/// The listed pods that are marked, in row order — what a marked-set action acts on, counts
/// and names.
pub fn marked_pods<'a>(pods: &'a [Pod], marked: &HashSet<String>) -> Vec<&'a Pod> {
    pods.iter().filter(|p| marked.contains(&mark_key(p))).collect()
}

/// What a restart does to a set of pods' disks, for one label: the worst of them — a wipe
/// (`Yes`) over a maybe (`Unknown`) over a kept disk (`No`). An empty set is `Unknown`: never
/// "keeps". Pure.
pub fn worst_reset(resets: impl IntoIterator<Item = ContainerReset>) -> ContainerReset {
    let rank = |r: ContainerReset| match r {
        ContainerReset::No => 0,
        ContainerReset::Unknown => 1,
        ContainerReset::Yes => 2,
    };
    resets.into_iter().max_by_key(|r| rank(*r)).unwrap_or(ContainerReset::Unknown)
}

/// The most pods a lifecycle action (terminate/restart/stop) on a marked set confirms with
/// just their count. Marks used to take a keypress per pod; `/` now marks a cohort in one
/// short range (`/apple..zulu`), so the bar has to follow what is marked, not how: a
/// bigger set — or every listed pod, however few — asks for `ALL`, the whole-fleet bar.
pub const SET_COUNT_CONFIRM_MAX: usize = 5;

/// The token a marked-set confirm asks to be typed, for `marked` of the `listed` pods: the
/// count (which the operator chose), or `ALL` for a lifecycle action
/// ([`Action::marked_set_only`]) on more than [`SET_COUNT_CONFIRM_MAX`] pods or on the whole
/// listed fleet. The others (backup/setup/run/set-branch) keep the count.
pub fn marked_set_token(action: Action, marked: usize, listed: usize) -> String {
    if action.marked_set_only() && (marked > SET_COUNT_CONFIRM_MAX || marked >= listed) {
        "ALL".into()
    } else {
        marked.to_string()
    }
}

/// `names` for a confirm, at most `max` of them, then how many more (`… and 12 more`): the
/// confirm popup has a fixed size and the token prompt must stay on screen below it.
pub fn names_preview(names: &[String], max: usize) -> String {
    match names.len().checked_sub(max) {
        Some(more) if more > 0 => format!("{}, … and {more} more", names[..max].join(", ")),
        _ => names.join(", "),
    }
}

/// A confirm's summary of what is about to happen ([`Action::what`]): `alpha: print the torch
/// version (read-only)` for one pod; for several, the names once, compactly (at most `max` of
/// them, [`names_preview`]) — `3 pods: alpha, bravo, charlie` — then what, rather than the
/// action again per pod. Pure.
pub fn action_summary(action: Action, names: &[String], arg: Option<&str>, max: usize) -> Vec<String> {
    let what = action.what(arg);
    match names {
        [] => vec![what],
        [one] => vec![format!("{one}: {what}")],
        _ => vec![format!("{} pods: {}", names.len(), names_preview(names, max)), what],
    }
}

/// What `/` marks: the selection typed in the core selector syntax ([`parse_select_input`]),
/// parsed and resolved by `arena_core::selector` exactly as the CLI does — so a typo (a
/// name matching no pod, a reversed range, a misspelt `--exclude`) is an error and nothing
/// is marked. Returns the marked pods' [`mark_key`]s and the status line.
///
/// Marks feed the marked-set actions, terminate and restart among them, so two things the
/// CLI's read-only commands accept are refused here: `all` (whole-fleet actions are `A`,
/// which offers only the safe ones), and a selection naming no pod (`--on`/`--gpus`
/// alone start from the whole fleet). A selection that names every pod some other way
/// (`first..last`) is marked, but terminate/restart on it ask for `ALL`
/// ([`marked_set_token`]).
pub fn select_marks(input: &str, naming: &Naming, pods: &[Pod]) -> Result<(HashSet<String>, String), String> {
    let one_line = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let args = parse_select_input(input)?;
    let sel = Selector::parse(&args, naming).map_err(|e| one_line(&e.0))?;
    if sel.all {
        return Err("`all` isn't taken here — marks feed terminate/restart; name pods or a range \
                    (A acts on the whole fleet with the safe actions)"
            .into());
    }
    if sel.is_unscoped() {
        return Err("name pods, ids or a range (apple..delta) — -x, --on and --gpus only narrow a named set".into());
    }
    let picked = sel.resolve(naming, pods).map_err(|e| one_line(&e.0))?;
    let keys: HashSet<String> = picked.iter().map(|&i| mark_key(&pods[i])).collect();
    let n = picked.len();
    Ok((keys, format!("marked {n} pod{}: {}", if n == 1 { "" } else { "s" }, sel.describe())))
}

/// How a pod name is shown: full (`arena8-apple`) or short (`apple`). Stripping the
/// `{prefix}-` is purely cosmetic — the canonical name is still used for provider calls.
pub fn display_name(full: &str, prefix: &str, short: bool) -> String {
    if short {
        full.strip_prefix(&format!("{prefix}-")).unwrap_or(full).to_string()
    } else {
        full.to_string()
    }
}

/// Shorten an autocommit branch to just its iteration label: an
/// `autocommit-{prefix}-w1d2-apple` becomes `w1d2` (the machine name is already in the
/// NAME column). Any other branch (`main`, a feature branch, …) is returned unchanged.
pub fn short_branch(branch: &str, prefix: &str) -> String {
    match branch.strip_prefix(&format!("autocommit-{prefix}-")) {
        // rest is "w1d2-apple" -> take the part before the trailing "-<machine>".
        Some(rest) => rest.rsplit_once('-').map(|(label, _)| label.to_string()).unwrap_or_else(|| rest.to_string()),
        None => branch.to_string(),
    }
}

/// The actions a user can trigger against the selected pod from the dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Restart,
    Stop,
    Terminate,
    Backup,
    Setup,
    /// Health check: import torch and print its version (read-only).
    Test,
    /// Run an arbitrary shell command (the operator types it first).
    Run,
    /// Gently switch the ARENA checkout to a branch (the operator types it first).
    SetBranch,
    // The CLI's own commands, run as `arena` in the background (`runs.rs`), their output in the
    // runs panel — the dashboard's way to them, not a second copy.
    /// `pods start`: a stopped pod back, set up again.
    Start,
    /// `pods rename <pod> <new>` (the operator types the new name first).
    Rename,
    /// `claude assign <pod> <account>`: move it to another Claude Code account — a number or email
    /// from `arena claude accounts`, `none`, or `spread` (typed first) — and push that token.
    Claude,
    /// `pods jobs`: its background jobs (read-only).
    Jobs,
    /// `pods logs`: its newest job's log (read-only).
    Logs,
    /// `pods pull <pod>`: copy its home into `LOCAL_BACKUP_DIR` now — the backups screen's "copy
    /// home now" (`backups.rs`); in no menu of its own.
    Pull,
    // Its OpenRouter key's, offered when a provisioning key is configured (`keys …`).
    /// `keys limit <pod> <amount>`: raise or set its cap (the operator types the amount first).
    KeyLimit,
    /// `keys pause`: its AI calls fail until it's resumed.
    KeyPause,
    /// `keys resume`: a paused key back on. In the per-pod menu it shares pause's row and key:
    /// the row says whichever the key's state calls for ([`Action::for_key`]).
    KeyResume,
    /// `keys rotate --copy`: a new key, copied onto the pod; the old one stops working.
    KeyRotate,
}

impl Action {
    /// The action-menu entries, in display order (harmless first, destructive last —
    /// so the default-highlighted top option is the read-only `test`), with their keys.
    pub const MENU: &'static [(char, Action)] = &[
        ('e', Action::Test),
        ('w', Action::Jobs),
        ('l', Action::Logs),
        ('x', Action::Run),
        ('g', Action::SetBranch),
        ('m', Action::Rename),
        ('c', Action::Claude),
        ('b', Action::Backup),
        ('p', Action::Setup),
        ('u', Action::Start),
        ('r', Action::Restart),
        ('s', Action::Stop),
        ('t', Action::Terminate),
    ];

    /// The rows for a pod's OpenRouter key, before `backup` in both menus: `a` (so `a a` from the
    /// list raises the cursor pod's cap) and `o` (on/off).
    const AI_ROWS: &'static [(char, Action)] = &[('a', Action::KeyLimit), ('o', Action::KeyPause), ('n', Action::KeyRotate)];

    /// The per-pod menu: [`Self::MENU`], with the AI key's rows when `ai` (a provisioning key is
    /// configured — fixed for the dashboard's life, so rows never move under the cursor; whether
    /// this pod has a key is the choice's to check, [`crate`]'s `ai_key_support`). The `o` row is
    /// pause's here: [`Self::for_key`] makes it resume for a paused key.
    pub fn pod_menu(ai: bool) -> Vec<(char, Action)> {
        Self::with_ai_rows(Self::MENU.iter().copied(), if ai { Self::AI_ROWS } else { &[] })
    }

    /// `rows` with `ai` put in before `backup`.
    fn with_ai_rows(rows: impl Iterator<Item = (char, Action)>, ai: &[(char, Action)]) -> Vec<(char, Action)> {
        let mut out = Vec::new();
        for row in rows {
            if row.1 == Action::Backup {
                out.extend_from_slice(ai);
            }
            out.push(row);
        }
        out
    }

    /// The multi-pod menu's actions (and keys): all of the per-pod menu's, the lifecycle ones
    /// (`restart`, `stop`, `terminate`) only for a marked set — never for "the whole fleet" in
    /// one keystroke ([`Self::marked_set_only`]). So are the AI key's (`ai`: a provisioning key is
    /// configured): cap, pause and resume — each a row, as a set can hold both — never rotate.
    pub fn fleet_menu(has_marked: bool, ai: bool) -> Vec<(char, Action)> {
        const SET_AI: &[(char, Action)] = &[('a', Action::KeyLimit), ('o', Action::KeyPause), ('O', Action::KeyResume)];
        let rows = Self::MENU.iter().copied().filter(|(_, a)| !a.cli() && (!a.marked_set_only() || has_marked));
        Self::with_ai_rows(rows, if ai && has_marked { SET_AI } else { &[] })
    }

    /// One of the CLI's commands, run as `arena` (`runs.rs`).
    pub fn cli(&self) -> bool {
        matches!(self, Action::Start | Action::Rename | Action::Claude | Action::Jobs | Action::Logs | Action::Pull) || self.ai_key()
    }

    /// One of the AI key's (`arena keys …`).
    pub fn ai_key(&self) -> bool {
        matches!(self, Action::KeyLimit | Action::KeyPause | Action::KeyResume | Action::KeyRotate)
    }

    /// Pause or resume as a key that's off (`disabled`) calls for — the per-pod menu's `o` row;
    /// any other action as it is.
    pub fn for_key(self, disabled: bool) -> Action {
        match self {
            Action::KeyPause | Action::KeyResume if disabled => Action::KeyResume,
            Action::KeyPause | Action::KeyResume => Action::KeyPause,
            a => a,
        }
    }

    /// Whether a multi-pod run is offered only for a marked set, and a big one (or every listed
    /// pod) confirmed with `ALL` ([`marked_set_token`]): the actions one pod's confirm wants its
    /// name typed back for. Each can lose the participants' work — terminate the pod; restart,
    /// and a stop on RunPod, its container disk — so none is a one-keystroke whole-fleet action
    /// (stop was per-pod only; a marked set now gets it on the same terms as restart).
    pub fn marked_set_only(&self) -> bool {
        matches!(self, Action::Restart | Action::Stop | Action::Terminate)
    }

    /// A short description shown beside the action in the menu. `restart`: what a restart does
    /// to the container disk of the pod(s) the menu is for — the backend's own answer
    /// (`Provider::restart_resets_container`; a marked set: [`worst_reset`] of its pods). It
    /// said "WIPES" for every pod, a Hetzner VM (which keeps its disk) and RunPod v1 (seen
    /// keeping it, not known to) included. Wording only: the confirm's gate is the same for all.
    pub fn desc(&self, restart: ContainerReset) -> &'static str {
        match self {
            Action::Restart => match restart {
                ContainerReset::Yes => "restart — WIPES the container disk",
                ContainerReset::Unknown => "restart — may wipe the container disk",
                ContainerReset::No => "restart — keeps the disk",
            },
            Action::Stop => "stop the pod",
            Action::Terminate => "terminate — irreversible",
            Action::Backup => "commit + push the tree",
            Action::Setup => "provision / re-point git",
            Action::Test => "torch version (read-only)",
            Action::Run => "run a shell command",
            Action::SetBranch => "switch branch",
            Action::Start => "start a stopped pod (+ setup)",
            Action::Rename => "rename (no restart)",
            Action::Claude => "Claude Code: move it to another account",
            Action::Jobs => "its background jobs (read-only)",
            Action::Logs => "its newest job's log (read-only)",
            Action::Pull => "copy its home here (rsync)",
            Action::KeyLimit => "AI key: raise or set its cap",
            Action::KeyPause => "AI key: pause (calls fail until resumed)",
            Action::KeyResume => "AI key: resume",
            Action::KeyRotate => "AI key: a new one, copied onto it",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Action::Restart => "restart",
            Action::Stop => "stop",
            Action::Terminate => "terminate",
            Action::Backup => "backup",
            Action::Setup => "setup",
            Action::Test => "test",
            Action::Run => "run",
            Action::SetBranch => "set-branch",
            Action::Start => "start",
            Action::Rename => "rename",
            Action::Claude => "claude",
            Action::Jobs => "jobs",
            Action::Logs => "logs",
            Action::Pull => "pull",
            Action::KeyLimit => "ai cap",
            Action::KeyPause => "ai pause",
            Action::KeyResume => "ai resume",
            Action::KeyRotate => "ai rotate",
        }
    }

    /// What the busy modal says while it runs, before the pod's name: `backing up alpha…`.
    /// It was the label plus `ing` — `backuping`, `stoping`, `terminateing`.
    pub fn doing(&self) -> &'static str {
        match self {
            Action::Restart => "restarting",
            Action::Stop => "stopping",
            Action::Terminate => "terminating",
            Action::Backup => "backing up",
            Action::Setup => "setting up",
            Action::Test => "testing",
            Action::Run => "running the command on",
            Action::SetBranch => "switching the branch on",
            Action::Start => "starting",
            Action::Rename => "renaming",
            Action::Claude => "moving the Claude Code account of",
            Action::Jobs => "reading the jobs on",
            Action::Logs => "reading the log on",
            Action::Pull => "copying the home of",
            Action::KeyLimit => "setting the AI key's cap on",
            Action::KeyPause => "pausing the AI key on",
            Action::KeyResume => "resuming the AI key on",
            Action::KeyRotate => "rotating the AI key on",
        }
    }

    /// The `arena` command words it runs, for the runs panel and its footer notice.
    pub fn command(&self) -> String {
        match self {
            Action::KeyLimit => "keys limit".into(),
            Action::KeyPause => "keys pause".into(),
            Action::KeyResume => "keys resume".into(),
            Action::KeyRotate => "keys rotate".into(),
            Action::Claude => "claude assign".into(),
            a => format!("pods {}", a.label()),
        }
    }

    /// Lifecycle actions change/destroy a running machine, so they require the user
    /// to type the pod's exact name back before they can be applied. Backup/setup
    /// only touch the in-pod git tree, so a single `y` confirm is enough.
    pub fn requires_typed_name(&self) -> bool {
        matches!(self, Action::Restart | Action::Stop | Action::Terminate)
    }

    /// Whether this action destroys something that can't be got back: terminate (the pod),
    /// and restart — on RunPod a restart resets the container to its image, wiping its
    /// disk (live finding 2026-10-07). Drives the red border and keeps both off the
    /// whole-fleet menu.
    pub fn is_destructive(&self) -> bool {
        matches!(self, Action::Terminate | Action::Restart | Action::KeyRotate)
    }

    /// Whether to render the action in red in the menu — destructive *or* disruptive
    /// (stop takes the pod down). Distinct from [`Self::is_destructive`] so stop is red
    /// without claiming to be irreversible.
    pub fn is_risky(&self) -> bool {
        matches!(self, Action::Terminate | Action::Restart | Action::Stop | Action::KeyRotate)
    }

    /// The warning in the confirm modal, if any. `wipes_disk`: whether the pod's provider
    /// resets the container disk on restart / stop→start (`Provider::
    /// restart_wipes_container_disk`; Hetzner's VM keeps it) — pass `true` when unknown. It
    /// alone decides whether a restart warns; `reset` (`Provider::restart_resets_container`)
    /// only words it: a restart known to reset the container (RunPod v2) "WIPES" it, one that
    /// may (RunPod v1, Vast, an unknown backend) "MAY WIPE" it — the same warning either way.
    pub fn warning(&self, wipes_disk: bool, reset: ContainerReset) -> Option<&'static str> {
        match self {
            Action::Terminate => Some("⚠ IRREVERSIBLE"),
            Action::Restart if wipes_disk && reset == ContainerReset::Yes => Some(
                "⚠ WIPES THE CONTAINER DISK: the pod is reset to its image — everything outside a \
                 /workspace volume is lost (participants' files, ~/.name, git remote, keys). Back it \
                 up first; run setup (p) afterwards.",
            ),
            Action::Restart if wipes_disk => Some(
                "⚠ MAY WIPE THE CONTAINER DISK: this backend's restart can reset the pod to its image — \
                 count everything outside a /workspace volume as lost (participants' files, ~/.name, git \
                 remote, keys). Back it up first; run setup (p) afterwards.",
            ),
            Action::Restart => Some("hard reset: running processes are killed; the disk is kept."),
            Action::Stop if wipes_disk => Some(
                "⚠ a stopped RunPod pod keeps no data: its container disk is discarded and it starts \
                 again as a fresh image (only a /workspace volume survives).",
            ),
            Action::Start => Some("it bills again once it's up; a RunPod/Vast pod comes back as a fresh image, and is set up again."),
            Action::KeyPause => Some("its AI calls fail until the key is resumed (this menu's `o` again, or `arena keys resume`)."),
            Action::Claude => Some("shells already open on it keep the old token until they start again."),
            Action::KeyRotate => Some(
                "⚠ the old key stops working at once. The new one starts at no spend under the default cap, is saved in \
                 ./keys (where the dashboard was started) and copied onto the pod; shells already open keep the old \
                 one until they start again.",
            ),
            _ => None,
        }
    }

    /// Actions that need a free-text argument typed first (the command / the branch),
    /// collected in an input modal before they run.
    pub fn needs_input(&self) -> bool {
        matches!(self, Action::Run | Action::SetBranch | Action::Rename | Action::Claude | Action::KeyLimit)
    }

    /// What the action does, in words an operator reads at a glance — the confirm's summary,
    /// never the generated command (an `ssh -p … -o … -i …` line nobody reads: live, it pushed
    /// the remote command off the popup). `arg` is what was typed for `run` (shown verbatim:
    /// the operator wrote it) or `set-branch`; `None` (or blank) before it is.
    pub fn what(&self, arg: Option<&str>) -> String {
        let arg = arg.map(str::trim).filter(|a| !a.is_empty());
        match self {
            Action::Test => "print the torch version (read-only)".into(),
            Action::Run => arg.map_or_else(|| "run a shell command".into(), String::from),
            Action::SetBranch => match arg {
                Some(b) => format!("switch to branch {b} (fast-forward only)"),
                None => "switch branch (fast-forward only)".into(),
            },
            Action::Backup => "commit + push the current branch".into(),
            Action::Setup => "provision (deploy key, SSH keys, repo, tools)".into(),
            Action::Restart => "restart in place (a hard reset)".into(),
            Action::Stop => "stop (it can be started again)".into(),
            Action::Terminate => "terminate (delete)".into(),
            Action::Start => "start it again (`arena pods start`: waits for it, sets it up, syncs the proxy)".into(),
            Action::Rename => match arg {
                Some(n) => format!("rename to {n} (`arena pods rename`: ~/.name, the proxy, its AI key)"),
                None => "rename (`arena pods rename`)".into(),
            },
            Action::Claude => match arg {
                Some(a) if a.eq_ignore_ascii_case("spread") => {
                    "put it on the least-loaded Claude Code account and push that token (`arena claude assign --spread`)".into()
                }
                Some(a) if a.eq_ignore_ascii_case("none") => "take its Claude Code token off (`arena claude assign … none`)".into(),
                Some(a) => format!("put it on Claude Code account {a} and push that token (`arena claude assign`)"),
                None => "move it to another Claude Code account (`arena claude assign`)".into(),
            },
            Action::Jobs => "list its background jobs (read-only)".into(),
            Action::Logs => "show its newest job's log (read-only)".into(),
            Action::Pull => "copy its home into LOCAL_BACKUP_DIR now (`arena pods pull`: today's snapshot and the all-files tier)".into(),
            Action::KeyLimit => match arg {
                Some(a) => format!("set its AI key's cap: {a} (`arena keys limit`)"),
                None => "raise or set its AI key's cap (`arena keys limit`)".into(),
            },
            Action::KeyPause => "pause its AI key: OpenRouter refuses its calls until it's resumed (`arena keys pause`)".into(),
            Action::KeyResume => "resume its AI key (`arena keys resume`)".into(),
            Action::KeyRotate => "replace its AI key with a new one and copy that onto the pod (`arena keys rotate --copy`)".into(),
        }
    }

    /// The prompt shown in the input modal for [`Self::needs_input`] actions.
    pub fn input_prompt(&self) -> &'static str {
        match self {
            Action::Run => "shell command to run on the pod(s):",
            Action::SetBranch => "branch to switch to (fetch + checkout + fast-forward):",
            Action::Rename => "new name — a free MACHINE_NAME_LIST name (bare names get the prefix):",
            Action::Claude => "Claude Code account — its number or email (`arena claude accounts`), none, or spread:",
            Action::KeyLimit => "the cap in USD — `10` sets it, `+5` raises it by $5:",
            _ => "",
        }
    }
}

/// A pending confirmation for an action against a specific pod.
#[derive(Debug, Clone)]
pub struct Confirm {
    pub action: Action,
    pub pod_name: String,
    pub pod_id: String,
    /// What the user has typed so far (only used for typed-name confirmations).
    pub typed: String,
    /// The full generated command(s) (test/backup/setup) — not shown unless asked for (`v`,
    /// [`Self::full`]): the confirm says what the action does ([`action_summary`]).
    pub preview: Option<String>,
    /// Whether this pod's restart/stop resets its container disk — selects the warning
    /// ([`Action::warning`]). Defaults to `true`: assuming a wipe only adds a warning.
    pub wipes_disk: bool,
    /// What a restart is known to do to this pod's container disk — words the restart's
    /// warning, never decides it. Defaults to `Unknown`: never claimed.
    pub reset: ContainerReset,
    /// The full command is shown under the summary (`v` toggles it).
    pub full: bool,
    /// Said under the summary: an AI key's state now (`ai key now: $0.42 of $5.00 …`).
    pub note: Option<String>,
    /// The AI key's pick: the pod's name and its key as the confirm shows it — what `y` runs with
    /// (`aikey::start`), never a later read (review codex r3 #1).
    pub ai: Vec<(String, arena_core::openrouter::KeyInfo)>,
}

impl Confirm {
    pub fn new(action: Action, pod_name: String, pod_id: String, preview: Option<String>) -> Self {
        Self {
            action,
            pod_name,
            pod_id,
            typed: String::new(),
            preview,
            wipes_disk: true,
            reset: ContainerReset::Unknown,
            full: false,
            note: None,
            ai: Vec::new(),
        }
    }

    /// The warning line for this confirmation, if any.
    pub fn warning(&self) -> Option<&'static str> {
        self.action.warning(self.wipes_disk, self.reset)
    }

    /// Is the confirmation satisfied enough to apply? Typed-name actions need an exact
    /// match of the pod name; others are satisfied as soon as the user hits the
    /// confirm key (handled in `main.rs`).
    pub fn is_satisfied(&self) -> bool {
        if self.action.requires_typed_name() {
            self.typed == self.pod_name
        } else {
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on(name: &str, ip: Option<&str>, port: Option<u16>) -> Pod {
        Pod { name: name.into(), ssh_ip: ip.map(String::from), ssh_port: port, ..Default::default() }
    }

    /// Each endpoint is timed once a round at most, then not again for RTT_EVERY; a pod
    /// without one is never timed; a pod that moved is timed afresh, never shown its old host's.
    #[test]
    fn rtts_time_each_endpoint_once_a_minute_and_follow_a_move() {
        let t0 = std::time::Instant::now();
        let pods = vec![on("a", Some("1.2.3.4"), Some(22001)), on("b", Some("1.2.3.4"), Some(22001)), on("c", None, None), on("d", Some(" "), Some(22))];
        let mut r = Rtts::default();
        assert_eq!(r.due(&pods, t0), [("1.2.3.4".to_string(), 22001)], "shared endpoint once; none without one");
        assert_eq!(r.of(&pods[0]), None, "not timed yet");
        r.record(vec![(("1.2.3.4".into(), 22001), Some(Duration::from_millis(23)))], t0);
        assert_eq!((r.of(&pods[0]), r.of(&pods[1]), r.of(&pods[2])), (Some(Duration::from_millis(23)), Some(Duration::from_millis(23)), None));
        assert!(r.due(&pods, t0 + Duration::from_secs(59)).is_empty());
        assert_eq!(r.due(&pods, t0 + RTT_EVERY).len(), 1);
        // No answer: unknown, and timed again a minute later like any other.
        r.record(vec![(("1.2.3.4".into(), 22001), None)], t0 + RTT_EVERY);
        assert_eq!(r.of(&pods[0]), None);
        // `a` restarted onto another port: timed now, never shown its old port's.
        let moved = vec![on("a", Some("1.2.3.4"), Some(23810))];
        assert_eq!(r.due(&moved, t0 + RTT_EVERY), [("1.2.3.4".to_string(), 23810)]);
        r.record(vec![(("1.2.3.4".into(), 23810), Some(Duration::from_millis(30)))], t0 + RTT_EVERY);
        assert_eq!(r.of(&moved[0]), Some(Duration::from_millis(30)));
        // A listing that missed a pod doesn't forget its timing (no re-dial when it's back)…
        assert!(r.due(&pods, t0 + RTT_EVERY + Duration::from_secs(1)).is_empty());
        // …only age does.
        r.record(Vec::new(), t0 + RTT_EVERY + RTT_FORGET);
        assert!(r.timed.is_empty());
    }

    #[test]
    fn rtt_cells_by_threshold() {
        let ms = |n: u64| rtt_cell(Some(Duration::from_millis(n)));
        assert_eq!(rtt_cell(None), ("-".to_string(), Tone::Dim));
        assert_eq!(ms(0), ("0ms".to_string(), Tone::Good));
        assert_eq!(ms(23), ("23ms".to_string(), Tone::Good));
        assert_eq!(ms(59), ("59ms".to_string(), Tone::Good));
        assert_eq!(ms(60), ("60ms".to_string(), Tone::Warn));
        assert_eq!(ms(95), ("95ms".to_string(), Tone::Warn));
        assert_eq!(ms(120), ("120ms".to_string(), Tone::Bad));
        assert_eq!(ms(999), ("999ms".to_string(), Tone::Bad));
        assert_eq!(ms(1500), (">1s".to_string(), Tone::Bad));
        assert!(["999ms", ">1s", "23ms"].iter().all(|l| l.chars().count() <= 5), "fits the 5-wide column");
    }
    use arena_core::metrics::{parse_nvidia_smi, PodMetrics};

    fn pod(name: &str, cost: Option<f64>) -> Pod {
        Pod {
            id: format!("id-{name}"),
            name: name.into(),
            provider: "runpod".into(),
            status: "RUNNING".into(),
            gpu_type: Some("RTX 4090".into()),
            cost_per_hr: cost,
            ssh_ip: Some("1.2.3.4".into()),
            ssh_port: Some(22001),
            ..Default::default()
        }
    }

    #[test]
    fn history_caps_and_pads_missing() {
        let mut h = History::default();
        for _ in 0..(HISTORY_LEN + 5) {
            h.push(Some(50), None, Some(10)); // temp missing -> recorded as 0
        }
        assert_eq!(h.util.len(), HISTORY_LEN);
        assert_eq!(h.temp.len(), HISTORY_LEN);
        assert_eq!(h.mem.len(), HISTORY_LEN);
        assert_eq!(*h.temp.back().unwrap(), 0);
        assert_eq!(*h.util.back().unwrap(), 50);
        assert_eq!(*h.mem.back().unwrap(), 10);
    }

    #[test]
    fn spark_pads_and_scales() {
        // Empty -> all spaces.
        assert_eq!(spark(&[], 4), "    ");
        // Left-padded when shorter than width.
        let s = spark(&[100], 3);
        assert_eq!(s.chars().count(), 3);
        assert!(s.starts_with("  "));
        assert!(s.ends_with('█')); // 100 -> full bar
        // 0 -> lowest bar.
        assert_eq!(spark(&[0], 1), "▁");
    }

    #[test]
    fn summary_aggregates_across_fleet() {
        let pods = vec![pod("arena8-apple", Some(0.40)), pod("arena8-luna", Some(0.69))];
        let mut metrics = HashMap::new();
        metrics.insert(
            "arena8-apple".to_string(),
            PodMetrics { gpus: parse_nvidia_smi("RTX 3090, 100, 8000, 16000, 60\n"), ..Default::default() },
        );
        metrics.insert(
            "arena8-luna".to_string(),
            PodMetrics { gpus: parse_nvidia_smi("RTX 3090, 0, 1000, 16000, 40\n"), ..Default::default() },
        );
        let s = summarize(&pods, &metrics);
        assert_eq!(s.pods, 2);
        assert_eq!(s.reporting, 2);
        assert_eq!(s.total_gpus, 2);
        assert_eq!(s.mean_util, Some(50)); // (100 + 0) / 2 GPUs
        assert_eq!((s.mem_used_mb, s.mem_total_mb), (9000, 32000));
    }

    /// The summary bar's burn is the core `FleetCost`, worded as `pods list`'s footer:
    /// billing pods only, Hetzner's € kept apart from $ (the bar used to add them up), the
    /// unpriced called out, the same per day — and a provider that failed to list named.
    #[test]
    fn summary_text_shows_the_core_fleet_cost() {
        let mut exited = pod("arena8-zebra", Some(0.13));
        exited.status = "EXITED".into();
        let mut starting = pod("arena8-yak", Some(0.20));
        starting.status = "STARTING".into();
        let mut hetzner = pod("arena8-cpu", Some(0.0056));
        hetzner.provider = "hetzner".into();
        hetzner.status = "off".into(); // a powered-off server still bills
        let unpriced = pod("arena8-new", None);
        let pods = [pod("arena8-apple", Some(0.40)), exited, starting, hetzner, unpriced];
        let s = summarize(&pods, &HashMap::new());
        let cost = fleet::fleet_cost(&pods);
        assert_eq!(
            summary_text(&s, &cost, &[], &[], None),
            " 5 pods · 0 GPUs · mean util - · fleet: $0.60/h across 4 billing pod(s) + €0.006/h hetzner (1 unpriced) \
             ≈ $14.40/day + €0.13/day"
        );
        // A provider that failed to list leads the bar (its tail is cut on narrow terminals).
        let partial = summary_text(&s, &cost, &["vast".to_string(), "hetzner".to_string()], &[], None);
        assert!(
            partial.starts_with(" ⚠ vast, hetzner failed to list — its pods (and their cost) are missing · 5 pods · "),
            "{partial}"
        );
        assert!(partial.ends_with(" ≈ $14.40/day + €0.13/day"), "{partial}");
        assert_eq!(partial_notice(&[]), None);
        // Nothing billing: no per-day figure.
        let idle = summary_text(&FleetSummary::default(), &fleet::fleet_cost(&[]), &[], &[], None);
        assert_eq!(idle, " 0 pods · 0 GPUs · mean util - · fleet: $0.00/h across 0 billing pod(s)");
    }

    /// One pod with one GPU reads `1 pod · 1 GPU` — the bar said `1 pods · 1 GPUs`.
    #[test]
    fn summary_counts_are_pluralised() {
        let pods = [pod("arena8-apple", Some(0.40))];
        let mut metrics = HashMap::new();
        metrics.insert("arena8-apple".to_string(), PodMetrics { gpus: parse_nvidia_smi("RTX 3090, 30, 8000, 16000, 60\n"), ..Default::default() });
        let cost = fleet::fleet_cost(&pods);
        let one = summary_text(&summarize(&pods, &metrics), &cost, &[], &[], None);
        assert!(one.starts_with(" 1 pod · 1 GPU · mean util 30% · "), "{one}");
        let two = [pod("arena8-apple", Some(0.40)), pod("arena8-bloom", Some(0.40))];
        let both = summary_text(&summarize(&two, &HashMap::new()), &fleet::fleet_cost(&two), &[], &[], None);
        assert!(both.starts_with(" 2 pods · 0 GPUs · "), "{both}");
    }

    /// Prepaid accounts as read live now at `rate`/h (no listing): `(account, USD, rate)`.
    fn balances(accounts: &[(balance::Account, f64, f64)]) -> Vec<AccountView> {
        let fetched: Vec<balance::Fetched> = accounts
            .iter()
            .map(|&(account, usd, rate)| balance::Fetched {
                account,
                key_fp: "fp".into(),
                live: Some(Ok(balance::Live { usd, provider_burn_per_hr: Some(rate), spend_limit_per_hr: None, under_balance: None, reduced: false, owed: None })),
                billed: None,
            })
            .collect();
        let policy = balance::BalancePolicy::default();
        balance::views(&balance::BalanceState::default(), &fetched, &balance::Burns::new(), &policy, balance::Mode::Inline, 1_791_460_800)
    }

    /// Balances that need nothing go at the end of the bar, after the burn.
    #[test]
    fn summary_appends_ok_balance() {
        use balance::Account;
        let pods = [pod("arena8-apple", Some(0.40)), pod("arena8-bloom", Some(0.10))];
        let s = summarize(&pods, &HashMap::new());
        let cost = fleet::fleet_cost(&pods);
        let ok = balances(&[(Account::Runpod, 500.0, 0.5), (Account::Vast, 90.0, 0.0)]);
        let text = summary_text(&s, &cost, &[], &ok, None);
        assert!(text.starts_with(" 2 pods · ") && text.ends_with("  ·  balance: runpod $500.00 ≈41.6d · vast $90.00 no burn"), "{text}");
        assert_eq!(balance_tone(&ok), None);
        // No prepaid account: nothing about a balance.
        assert!(!summary_text(&s, &cost, &[], &[], None).contains("balance"));
    }

    /// A low or critical balance leads the bar — right after a failed-listing notice, ahead
    /// of the counts, so a narrow terminal can't cut it off — said once, and toned.
    #[test]
    fn summary_puts_low_balance_first() {
        use balance::Account;
        let pods = [pod("arena8-apple", Some(0.40))];
        let s = summarize(&pods, &HashMap::new());
        let cost = fleet::fleet_cost(&pods);
        let low = balances(&[(Account::Runpod, 30.0, 0.5), (Account::Vast, 90.0, 0.0)]); // 60h
        let text = summary_text(&s, &cost, &["hetzner".to_string()], &low, None);
        assert!(
            text.starts_with(" ⚠ hetzner failed to list — its pods (and their cost) are missing · ⚠ runpod $30.00 ≈2.5d · 1 pod · 0 GPUs · "),
            "{text}"
        );
        assert!(text.ends_with("  ·  balance: vast $90.00 no burn"), "the ok one stays at the end: {text}");
        assert_eq!(text.matches("runpod").count(), 1, "said once: {text}");
        assert_eq!(balance_tone(&low), Some(Tone::Warn));
        let critical = balances(&[(Account::Runpod, 3.0, 0.5)]); // 6h
        assert!(summary_text(&s, &cost, &[], &critical, None).starts_with(" ‼ runpod $3.00 ≈6.0h · 1 pod · "));
        assert_eq!(balance_tone(&critical), Some(Tone::Bad));
    }

    /// OpenRouter in the bar: its warning right after the balance's (or first), its credit
    /// and the cohort's spend today at the end; nothing at all without a provisioning key.
    #[test]
    fn summary_carries_openrouter_after_the_balance() {
        let pods = [pod("arena8-apple", Some(0.40))];
        let s = summarize(&pods, &HashMap::new());
        let cost = fleet::fleet_cost(&pods);
        let calm = CohortUsage { keys: 2, today: Some(1.5), credit_left: Some(422.09), headroom: 8.0, ..Default::default() };
        let text = summary_text(&s, &cost, &[], &[], Some(&calm));
        assert!(text.starts_with(" 1 pod · "), "{text}");
        assert!(text.ends_with("  ·  openrouter $422.09 left · AI today $1.50"), "{text}");
        assert_eq!(ai_tone(Some(&calm)), None);
        let capped = |name: &str, pod: bool| arena_core::openrouter::CappedKey { name: name.into(), pod };
        let short = CohortUsage { credit_left: Some(5.0), capped: vec![capped("arena8-gone", false), capped("arena8-apple", true)], ..calm.clone() };
        let text = summary_text(&s, &cost, &["vast".to_string()], &[], Some(&short));
        assert!(
            text.starts_with(" ⚠ vast failed to list — its pods (and their cost) are missing · ⚠ openrouter $5.00 left, the keys' caps allow $8.00 more · ⚠ 2 AI keys at cap: arena8-gone (no pod), arena8-apple · 1 pod · "),
            "{text}"
        );
        assert_eq!(ai_tone(Some(&short)), Some(Tone::Warn));
        assert_eq!(ai_tone(Some(&CohortUsage { credit_left: Some(0.0), ..calm })), Some(Tone::Bad));
        assert_eq!(ai_tone(None), None);
        assert!(!summary_text(&s, &cost, &[], &[], None).contains("openrouter"));
    }

    fn pod_on(provider: &str, name: &str, cost: Option<f64>) -> Pod {
        Pod { provider: provider.into(), ..pod(name, cost) }
    }

    fn fleet_of(pods: usize, total_gpus: usize, mean_util: Option<u32>) -> FleetSummary {
        FleetSummary { pods, total_gpus, mean_util, ..Default::default() }
    }

    /// The top line with room for every word, no tag.
    fn top(s: &FleetSummary, pods: &[Pod], partial: &[&str], balance: &[AccountView], ai: Option<&CohortUsage>) -> TopLine {
        let partial: Vec<String> = partial.iter().map(|p| p.to_string()).collect();
        top_parts(s, pods, &partial, balance, ai, None, 400)
    }

    /// The colours of the parts reading `text`, in order.
    fn tones(line: &TopLine, text: &str) -> Vec<Option<Tone>> {
        line.parts.iter().filter(|p| p.text == text).map(|p| p.tone).collect()
    }

    /// A calm fleet: the fleet in short, then a group per provider with pods — count and name in
    /// green, balance and runway, burn; Hetzner's € with no balance — then OpenRouter's, a dim `│`
    /// between groups and no mark anywhere. An account with no pods and nothing wrong (Vast here,
    /// Shadeform's wallet unfunded on purpose) is the info panel's.
    #[test]
    fn the_top_line_groups_a_calm_fleet_by_provider() {
        use balance::Account;
        let pods = [pod("arena8-a", Some(0.40)), pod("arena8-b", Some(0.40)), pod("arena8-c", Some(0.40)), pod_on("hetzner", "arena8-cpu", Some(0.0056))];
        let ok = balances(&[(Account::Runpod, 120.30, 1.20), (Account::Vast, 90.0, 0.0), (Account::Shadeform, 0.0, 0.0)]);
        let calm = CohortUsage { keys: 2, today: Some(12.4), credit_left: Some(422.09), headroom: 8.0, ..Default::default() };
        let line = top(&fleet_of(4, 3, Some(37)), &pods, &[], &ok, Some(&calm));
        assert_eq!(line.text(), " 4 pods, 3 GPUs 37% │ 3 RunPod $120.30 ≈4.1d · $1.20/h │ 1 Hetzner €0.006/h │ OpenRouter $12.40 today · $422.09 left");
        for name in ["3", "RunPod", "1", "Hetzner", "OpenRouter"] {
            assert_eq!(tones(&line, name), [Some(Tone::Good)], "{name}");
        }
        assert_eq!(tones(&line, " │ "), [Some(Tone::Dim); 3]);
        assert!(line.parts.iter().all(|p| matches!(p.tone, None | Some(Tone::Good | Tone::Dim))), "{line:?}");
        assert_eq!(line.tag, None);
        // No OpenRouter key, no GPU read yet: neither said.
        assert_eq!(top(&fleet_of(4, 0, None), &pods, &[], &[], None).text(), " 4 pods, 0 GPUs │ 3 RunPod $1.20/h │ 1 Hetzner €0.006/h");
        assert_eq!(top(&FleetSummary::default(), &[], &[], &ok, None).text(), " 0 pods, 0 GPUs");
    }

    /// A low account: its group's mark and name in yellow, and its balance and runway (the
    /// warning: never cut) too, the burn plain; critical in red; an account with no pods there
    /// shows all the same. No reading while it burns: a `?`, neither green nor a warning.
    #[test]
    fn a_low_or_critical_account_marks_its_group() {
        use balance::Account;
        let pods = [pod("arena8-a", Some(0.40)), pod("arena8-b", Some(0.40)), pod("arena8-c", Some(0.40))];
        let s = fleet_of(3, 3, Some(37));
        let low = balances(&[(Account::Runpod, 30.0, 0.5), (Account::Vast, 3.0, 0.5)]); // 60h, 6h
        let line = top(&s, &pods, &[], &low, None);
        assert_eq!(line.text(), " 3 pods, 3 GPUs 37% │ ⚠ 3 RunPod $30.00 ≈2.5d · $1.20/h │ ⚠ Vast $3.00 ≈6.0h");
        assert_eq!(tones(&line, WARN_MARK), [Some(Tone::Warn), Some(Tone::Bad)]);
        for (text, tone) in [("3", Tone::Warn), ("RunPod", Tone::Warn), ("$30.00", Tone::Warn), ("≈2.5d", Tone::Warn), ("Vast", Tone::Bad), ("$3.00", Tone::Bad), ("≈6.0h", Tone::Bad)] {
            assert_eq!(tones(&line, text), [Some(tone)], "{text}");
        }
        assert_eq!(tones(&line, "$1.20/h"), [None], "the burn isn't the warning");
        // RunPod's account under its minimum: said so, beside the amount.
        let mut under = balances(&[(Account::Runpod, 9.0, 0.01)]);
        (under[0].under_balance, under[0].level) = (Some(true), Level::Critical);
        assert_eq!(top(&s, &pods, &[], &under, None).text(), " 3 pods, 3 GPUs 37% │ ⚠ 3 RunPod $9.00 under its minimum · $1.20/h");
        let policy = balance::BalancePolicy::default();
        let unread = balance::views(&balance::BalanceState::default(), &[balance::Fetched::nothing(Account::Runpod, "fp")], &balance::Burns::new(), &policy, balance::Mode::Inline, 1_791_460_800);
        let line = top(&s, &pods, &[], &unread, None);
        assert_eq!(line.text(), " 3 pods, 3 GPUs 37% │ 3 RunPod ? · $1.20/h");
        assert_eq!(tones(&line, "RunPod"), [None]);
    }

    /// A provider that failed to list: its group in red, `⚠ Vast: list failed` — its pods and
    /// their cost are missing — with its balance as read; with no account, just that.
    #[test]
    fn a_failed_listing_marks_its_provider_in_red() {
        use balance::Account;
        let pods = [pod("arena8-a", Some(0.40))];
        let fine = balances(&[(Account::Vast, 90.0, 0.0)]);
        let line = top(&fleet_of(1, 1, Some(5)), &pods, &["hetzner", "vast"], &fine, None);
        assert_eq!(line.text(), " 1 pod, 1 GPU 5% │ 1 RunPod $0.40/h │ ⚠ Vast: list failed · $90.00 no burn │ ⚠ Hetzner: list failed");
        assert_eq!(tones(&line, WARN_MARK), [Some(Tone::Bad); 2]);
        assert_eq!(tones(&line, "list failed"), [Some(Tone::Bad); 2]);
        assert_eq!((tones(&line, "Vast"), tones(&line, "Hetzner")), (vec![Some(Tone::Bad)], vec![Some(Tone::Bad)]));
        assert_eq!(tones(&line, "$90.00"), [None], "a balance that needs nothing stays plain");
    }

    /// OpenRouter's group: credit under what the keys' caps allow (yellow), keys at their cap
    /// (named, `(a → raise cap)` when a row carries one), credit gone (red), `n/a` after a failed
    /// read; nothing to say, or no provisioning key: no group.
    #[test]
    fn openrouter_warnings_mark_its_group() {
        let s = fleet_of(1, 1, None);
        let pods = [pod("arena8-apple", Some(0.40))];
        let group = |c: &CohortUsage| {
            let line = top(&s, &pods, &[], &[], Some(c));
            (line.text().rsplit(" │ ").next().unwrap().to_string(), tones(&line, "OpenRouter")[0])
        };
        let calm = CohortUsage { keys: 2, today: Some(1.5), credit_left: Some(422.09), headroom: 8.0, ..Default::default() };
        assert_eq!(group(&calm), ("OpenRouter $1.50 today · $422.09 left".into(), Some(Tone::Good)));
        let capped = |name: &str, pod: bool| arena_core::openrouter::CappedKey { name: name.into(), pod };
        let short = CohortUsage { credit_left: Some(5.0), capped: vec![capped("arena8-gone", false)], uncapped: 1, failed: true, ..calm.clone() };
        assert_eq!(
            group(&short),
            ("⚠ OpenRouter $5.00 left < caps' $8.00 · key at cap: arena8-gone (no pod) · $1.50 today · 1 uncapped · n/a".into(), Some(Tone::Warn))
        );
        let raisable = CohortUsage { capped: vec![capped("arena8-apple", true), capped("arena8-gone", false)], ..calm.clone() };
        assert_eq!(group(&raisable).0, "⚠ OpenRouter 2 keys at cap: arena8-apple, arena8-gone (no pod) (a → raise cap) · $1.50 today · $422.09 left");
        let gone = CohortUsage { credit_left: Some(0.0), ..calm.clone() };
        assert_eq!(group(&gone), ("⚠ OpenRouter $0.00 left, calls fail · $1.50 today".into(), Some(Tone::Bad)));
        assert_eq!(group(&CohortUsage { failed: true, ..Default::default() }), ("OpenRouter n/a".into(), None));
        assert!(!top(&s, &pods, &[], &[], Some(&CohortUsage::default())).text().contains("OpenRouter"));
        assert!(!top(&s, &pods, &[], &[], None).text().contains("OpenRouter"));
    }

    /// Each provider's burn is `fleet_cost` over its pods alone, in its currency: Hetzner's €
    /// (a powered-off server still bills), unpriced pods counted — the figure is then a floor —,
    /// a stopped pod counted but not billing. The shares add up to the fleet's.
    #[test]
    fn each_provider_burns_in_its_currency_and_counts_its_unpriced() {
        let mut off = pod_on("hetzner", "arena8-cpu2", Some(0.0056));
        off.status = "off".into();
        let mut exited = pod_on("vast", "arena8-v", Some(0.20));
        exited.status = "EXITED".into();
        let pods = [pod_on("shadeform", "arena8-s", Some(1.0)), pod("arena8-a", Some(0.40)), pod_on("hetzner", "arena8-cpu", Some(0.0056)), exited, pod("arena8-new", None), off];
        let shares = by_provider(&pods);
        assert_eq!(shares.iter().map(|p| (p.provider.as_str(), p.pods)).collect::<Vec<_>>(), [("runpod", 2), ("vast", 1), ("hetzner", 2), ("shadeform", 1)]);
        let total = fleet::fleet_cost(&pods);
        let sum = |f: fn(&FleetCost) -> f64| shares.iter().map(|p| f(&p.cost)).sum::<f64>();
        assert!((sum(|c| c.usd_per_hr) - total.usd_per_hr).abs() < 1e-9 && (sum(|c| c.eur_per_hr) - total.eur_per_hr).abs() < 1e-9);
        assert_eq!(shares.iter().map(|p| (p.cost.billing, p.cost.unpriced)).fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1)), (total.billing, total.unpriced));
        assert_eq!(
            top(&fleet_of(6, 2, None), &pods, &[], &[], None).text(),
            " 6 pods, 2 GPUs │ 2 RunPod $0.40/h (1 unpriced) │ 1 Vast │ 2 Hetzner €0.011/h │ 1 Shadeform $1.00/h"
        );
    }

    /// A cohort's fleet at 160, 120, 100 and 80 columns: the least needed words go first, each
    /// kind in every group at once; the warnings (a low account, a key at its cap) and every
    /// group's name stay; the tag only while everything fits beside it. With a third warning
    /// (unreachable pods) it still fits 80 columns; narrower, past every cut, the warnings go
    /// first and the groups that don't fit are counted.
    #[test]
    fn the_top_line_fits_its_width_and_keeps_the_warnings() {
        use balance::Account;
        let mut pods: Vec<Pod> = (0..42).map(|i| pod(&format!("devtest-r{i}"), Some(0.11))).collect();
        pods.extend((0..9).map(|i| pod_on("hetzner", &format!("devtest-h{i}"), Some(0.1367))));
        let mut s = fleet_of(51, 48, Some(37));
        let accounts = balances(&[(Account::Runpod, 520.30, 4.62), (Account::Vast, 40.0, 1.0)]); // ≈112h, 40h
        let capped = arena_core::openrouter::CappedKey { name: "devtest-demo05".into(), pod: true };
        let ai = CohortUsage { keys: 51, today: Some(12.4), credit_left: Some(422.09), headroom: 200.0, capped: vec![capped], ..Default::default() };
        let tag = "vast · ~/sandbox/config.env";
        let at = |s: &FleetSummary, w: usize| top_parts(s, &pods, &[], &accounts, Some(&ai), Some(tag), w);
        let whole = at(&s, 240);
        assert_eq!(
            (whole.text().as_str(), whole.tag.as_deref()),
            (" 51 pods, 48 GPUs 37% │ 42 RunPod $520.30 ≈4.6d · $4.62/h │ ⚠ Vast $40.00 ≈40h │ 9 Hetzner €1.23/h │ ⚠ OpenRouter key at cap: devtest-demo05 (a → raise cap) · $12.40 today · $422.09 left", Some(tag))
        );
        for (w, want) in [
            (160, " 51 pods │ 42 RunPod $520.30 · $4.62/h │ ⚠ Vast $40.00 ≈40h │ 9 Hetzner €1.23/h │ ⚠ OpenRouter key at cap: devtest-demo05 (a → raise cap) · $422.09 left"),
            (120, " 51 pods │ 42 RunPod $520.30 · $4.62/h │ ⚠ Vast $40.00 ≈40h │ 9 Hetzner €1.23/h │ ⚠ OpenRouter key at cap · $422.09 left"),
            (100, " 51 pods │ 42 RunPod │ ⚠ Vast $40.00 ≈40h │ 9 Hetzner │ ⚠ OpenRouter key at cap"),
            (80, " 51 pods │ 42 RunPod │ ⚠ Vast $40.00 ≈40h │ 9 Hetzner │ ⚠ OpenRouter key at cap"),
        ] {
            let line = at(&s, w);
            assert_eq!(line.text(), want, "{w}");
            assert!(cells(&line.text()) <= w && line.tag.is_none(), "{w}");
        }
        s.unreachable = 2;
        for w in [80, 100, 120, 160] {
            let text = at(&s, w).text();
            for warning in ["2 unreachable", "⚠ Vast $40.00 ≈40h", "⚠ OpenRouter key at cap"] {
                assert!(text.chars().take(w).collect::<String>().contains(warning), "{w}: {warning:?} in {text}");
            }
        }
        assert_eq!(at(&s, 80).text(), " 2 unreachable │ RunPod │ ⚠ Vast $40.00 ≈40h │ Hetzner │ ⚠ OpenRouter key at cap");
        // Narrower, past every cut: the warnings first, the rest counted.
        let line = at(&s, 72);
        assert_eq!(line.text(), " 2 unreachable │ ⚠ Vast $40.00 ≈40h │ ⚠ OpenRouter key at cap │ +2 more");
        assert_eq!(tones(&line, "+2 more"), [Some(Tone::Dim)], "only calm names left out");
        let line = at(&s, 60);
        assert_eq!(line.text(), " 2 unreachable │ ⚠ Vast $40.00 ≈40h │ +3 more");
        assert_eq!(tones(&line, "+3 more"), [Some(Tone::Warn)], "a warning among them");
    }

    /// More warnings than 80 columns hold whole (codex's case): two unreachable pods, RunPod's
    /// account under its minimum, Vast's critical, Hetzner's listing failed. Past every cut the
    /// red ones go first, in short (`$9.00 < min`), and what still doesn't fit is counted at the
    /// end in the worst colour among it — `+1 more` in yellow, `+2 more` in red; narrower than
    /// a group and its count, `⚠ 4 warnings` alone.
    #[test]
    fn many_warnings_go_worst_first_and_the_rest_are_counted() {
        use balance::Account;
        let pods = [pod("arena8-a", Some(0.40)), pod("arena8-b", Some(0.40)), pod("arena8-c", Some(0.40))];
        let s = FleetSummary { unreachable: 2, ..fleet_of(3, 3, Some(37)) };
        let mut accounts = balances(&[(Account::Runpod, 9.0, 0.01), (Account::Vast, 3.0, 0.5)]);
        (accounts[0].under_balance, accounts[0].level) = (Some(true), Level::Critical);
        let partial = ["hetzner".to_string()];
        let at = |w: usize| top_parts(&s, &pods, &partial, &accounts, None, None, w);
        assert_eq!(
            at(400).text(),
            " 3 pods, 3 GPUs 37% · 2 unreachable │ ⚠ 3 RunPod $9.00 under its minimum · $1.20/h │ ⚠ Vast $3.00 ≈6.0h │ ⚠ Hetzner: list failed"
        );
        let line = at(80);
        assert_eq!(line.text(), " ⚠ RunPod $9.00 < min │ ⚠ Vast $3.00 ≈6.0h │ ⚠ Hetzner: list failed │ +1 more");
        assert_eq!(tones(&line, "+1 more"), [Some(Tone::Warn)]);
        let line = at(56);
        assert_eq!(line.text(), " ⚠ RunPod $9.00 < min │ ⚠ Vast $3.00 ≈6.0h │ +2 more");
        assert_eq!(tones(&line, "+2 more"), [Some(Tone::Bad)], "a red one among them");
        assert_eq!(at(40).text(), " ⚠ RunPod $9.00 < min │ +3 more");
        // Narrower than one group and the count: the count of warnings alone, in the worst colour.
        let line = at(20);
        assert_eq!((line.text().as_str(), tones(&line, "⚠ 4 warnings")[0]), (" ⚠ 4 warnings", Some(Tone::Bad)));
        let calm = top_parts(&fleet_of(3, 3, None), &pods, &[], &[], None, None, 5);
        assert_eq!(calm.text(), " RunPod", "nothing wrong: the edge cuts the calm names");
    }

    /// The footer's short status: the first warning's words (yellow) and how many more; else `ok`
    /// and the API requests of the last minute; a failed listing, red; work under way, cyan.
    /// The whole line stays for the info panel.
    #[test]
    fn the_refresh_status_is_short_in_the_footer_and_whole_in_the_panel() {
        let ok = RefreshStatus::of("3 reporting · RunPod API 13/min · 47/60 left this minute".into(), &[], Some(13));
        assert_eq!((ok.short.as_str(), ok.tone), ("ok · API 13/min", Some(Tone::Good)));
        assert_eq!(ok.full, "3 reporting · RunPod API 13/min · 47/60 left this minute");
        assert_eq!(RefreshStatus::of("0 reporting".into(), &[], None).short, "ok");
        let warned = RefreshStatus::of("x".into(), &["RunPod rate limit: listing again in 12 s", "warning: can't read the proxy config /etc/x (denied)"], Some(2));
        assert_eq!((warned.short.as_str(), warned.tone), ("warn: RunPod rate limit +1", Some(Tone::Warn)));
        for (w, want) in [
            ("RunPod rate limit: listing again in 12 s (the minute window is spent)", "RunPod rate limit"),
            ("pod details incomplete: HTTP 500", "pod details incomplete"),
            ("pod details timed out after 20s", "pod details timed out after…"),
            ("warning: can't read the proxy config /etc/nginx/streams-enabled/arena.conf (denied) — PROXY shows `?`", "can't read the proxy config…"),
            ("no health cache: no state dir", "no health cache"),
            ("ssh config not rewritten: permission denied", "ssh config not rewritten"),
        ] {
            assert_eq!(warning_label(w), want, "{w}");
        }
        let failed = RefreshStatus::list_error("list error: runpod: HTTP 401".into());
        assert_eq!((failed.short.as_str(), failed.tone), ("list error", Some(Tone::Bad)));
        let listed = RefreshStatus::listed("3 pods");
        assert_eq!((listed.short.as_str(), listed.full.as_str()), ("reading 3 pods…", "listed 3 pods — reading them…"));
    }

    #[test]
    fn summary_counts_unreachable() {
        let pods = vec![pod("arena8-apple", None)];
        let mut metrics = HashMap::new();
        metrics.insert(
            "arena8-apple".to_string(),
            PodMetrics { error: Some("connection refused".into()), ..Default::default() },
        );
        let s = summarize(&pods, &metrics);
        assert_eq!(s.unreachable, 1);
        assert_eq!(s.reporting, 0);
        assert_eq!(s.mean_util, None);
    }

    /// The restart label says what the backend says (live: v1 RunPod kept the disk, v2 wiped
    /// it, a Hetzner VM keeps it): "WIPES" only where a reset is known, "may wipe" where it
    /// isn't, "keeps the disk" where it's kept — and every other action's label is the same
    /// whatever the answer. Pure.
    #[test]
    fn the_restart_label_follows_the_backends_answer() {
        use ContainerReset::{No, Unknown, Yes};
        for (reset, want) in [
            (Yes, "restart — WIPES the container disk"),
            (Unknown, "restart — may wipe the container disk"),
            (No, "restart — keeps the disk"),
        ] {
            assert_eq!(Action::Restart.desc(reset), want, "{reset:?}");
        }
        for (_, a) in Action::MENU.iter().filter(|(_, a)| *a != Action::Restart) {
            assert!([Yes, Unknown, No].iter().all(|r| a.desc(*r) == a.desc(Yes)), "{a:?}");
        }
        // A marked set: the worst of its pods; nothing known is never "keeps".
        let cases: &[(&[ContainerReset], ContainerReset)] = &[
            (&[No, No], No),
            (&[No, Unknown], Unknown),
            (&[Unknown, No, Yes], Yes),
            (&[Yes, No], Yes),
            (&[Unknown], Unknown),
            (&[], Unknown),
        ];
        for (set, want) in cases {
            assert_eq!(worst_reset(set.iter().copied()), *want, "{set:?}");
        }
        // Wording only: whatever the answer, a restart wants the pod's name typed back, and a
        // pod whose provider may wipe is warned.
        for reset in [Yes, Unknown, No] {
            let c = Confirm { reset, ..Confirm::new(Action::Restart, "devtest-a".into(), "1".into(), None) };
            assert!(!c.is_satisfied() && c.warning().is_some_and(|w| w.starts_with('⚠')), "{reset:?}");
        }
    }

    #[test]
    fn lifecycle_actions_need_typed_name() {
        let mut c = Confirm::new(Action::Terminate, "arena8-apple".into(), "id".into(), None);
        assert!(!c.is_satisfied());
        c.typed = "arena8-appl".into();
        assert!(!c.is_satisfied()); // partial doesn't count
        c.typed = "arena8-apple".into();
        assert!(c.is_satisfied());
    }

    #[test]
    fn safe_actions_are_satisfied_immediately() {
        let c = Confirm::new(Action::Backup, "arena8-apple".into(), "id".into(), Some("git ...".into()));
        assert!(c.is_satisfied());
        assert!(!Action::Backup.requires_typed_name());
        assert!(Action::Terminate.is_destructive());
        assert!(!Action::Stop.is_destructive());
        assert!(Action::Backup.warning(true, ContainerReset::Yes).is_none());
    }

    /// Restart wipes a RunPod pod's container disk (live finding), so it's destructive like
    /// terminate: typed name, red, the wipe spelled out, and never offered for the whole
    /// fleet in one keystroke — only for a marked set.
    #[test]
    fn restart_is_destructive_and_says_it_wipes_the_disk() {
        let mut c = Confirm::new(Action::Restart, "arena8-apple".into(), "id".into(), None);
        assert!(Action::Restart.requires_typed_name() && Action::Restart.is_destructive() && Action::Restart.is_risky());
        assert!(!c.is_satisfied());
        c.typed = "arena8-apple".into();
        assert!(c.is_satisfied());
        // Unknown provider → warned as a possible wipe; a verified reset (RunPod v2) "WIPES";
        // Hetzner's reset keeps the disk.
        assert!(c.warning().unwrap().contains("MAY WIPE THE CONTAINER DISK"));
        c.reset = ContainerReset::Yes;
        assert!(c.warning().unwrap().contains("WIPES THE CONTAINER DISK"));
        c.wipes_disk = false;
        let kept = c.warning().unwrap();
        assert!(kept.contains("disk is kept") && !kept.contains("WIPE"), "{kept}");
        assert!(Action::Stop.warning(true, ContainerReset::Unknown).unwrap().contains("keeps no data"));
        assert!(Action::Stop.warning(false, ContainerReset::No).is_none());
        assert!(Action::Restart.desc(ContainerReset::Yes).contains("WIPES"));
        let fleet = |marked| Action::fleet_menu(marked, true).into_iter().map(|(_, a)| a).collect::<Vec<_>>();
        assert!(!fleet(false).contains(&Action::Restart) && !fleet(false).contains(&Action::Terminate));
        assert!(fleet(true).contains(&Action::Restart) && fleet(true).contains(&Action::Terminate));
        assert!(fleet(false).contains(&Action::Setup));
    }

    /// The confirm's summary for every action: a plain line per pod (the operator's own command
    /// verbatim for `run`), the names once for several — never an `ssh … -o … -i …` line.
    #[test]
    fn every_action_has_a_short_summary() {
        let one = ["tuiprobe".to_string()];
        // (action, typed argument, the summary for one pod)
        let cases = [
            (Action::Test, None, "tuiprobe: print the torch version (read-only)"),
            (Action::Run, Some("nvidia-smi -L; hostname"), "tuiprobe: nvidia-smi -L; hostname"),
            (Action::Run, None, "tuiprobe: run a shell command"),
            (Action::SetBranch, Some(" w3d2 "), "tuiprobe: switch to branch w3d2 (fast-forward only)"),
            (Action::SetBranch, Some(""), "tuiprobe: switch branch (fast-forward only)"),
            (Action::Backup, None, "tuiprobe: commit + push the current branch"),
            (Action::Setup, None, "tuiprobe: provision (deploy key, SSH keys, repo, tools)"),
            (Action::Restart, None, "tuiprobe: restart in place (a hard reset)"),
            (Action::Stop, None, "tuiprobe: stop (it can be started again)"),
            (Action::Terminate, None, "tuiprobe: terminate (delete)"),
            (Action::Start, None, "tuiprobe: start it again (`arena pods start`: waits for it, sets it up, syncs the proxy)"),
            (Action::Rename, Some("bravo"), "tuiprobe: rename to bravo (`arena pods rename`: ~/.name, the proxy, its AI key)"),
            (Action::Claude, Some("2"), "tuiprobe: put it on Claude Code account 2 and push that token (`arena claude assign`)"),
            (Action::Jobs, None, "tuiprobe: list its background jobs (read-only)"),
            (Action::Logs, None, "tuiprobe: show its newest job's log (read-only)"),
            (Action::KeyLimit, Some("+5"), "tuiprobe: set its AI key's cap: +5 (`arena keys limit`)"),
            (Action::KeyPause, None, "tuiprobe: pause its AI key: OpenRouter refuses its calls until it's resumed (`arena keys pause`)"),
            (Action::KeyResume, None, "tuiprobe: resume its AI key (`arena keys resume`)"),
            (Action::KeyRotate, None, "tuiprobe: replace its AI key with a new one and copy that onto the pod (`arena keys rotate --copy`)"),
        ];
        for (action, arg, want) in cases {
            assert_eq!(action_summary(action, &one, arg, 10), [want], "{action:?}");
        }
        for (_, action) in Action::pod_menu(true).iter().chain(&Action::fleet_menu(true, true)) {
            assert!(cases.iter().any(|(a, ..)| a == action), "{action:?} has no summary case");
            assert!(!action.what(None).contains("ssh"), "{action:?}");
        }
        // Several pods: the names once, compactly, then what.
        let three: Vec<String> = ["alpha", "bravo", "charlie"].map(String::from).into();
        assert_eq!(action_summary(Action::Backup, &three, None, 10), ["3 pods: alpha, bravo, charlie", "commit + push the current branch"]);
        let many: Vec<String> = (0..12).map(|i| format!("m{i:02}")).collect();
        assert_eq!(action_summary(Action::Test, &many, None, 2), ["12 pods: m00, m01, … and 10 more", "print the torch version (read-only)"]);
        assert_eq!(action_summary(Action::Test, &[], None, 2), ["print the torch version (read-only)"]);
    }

    /// Each menu's keys are its own, one action each; the AI key's rows sit before `backup` and
    /// only with a provisioning key configured, so no row moves under the cursor for a pod; the
    /// per-pod `o` row is pause's or resume's as the key is on or off; rotate is per pod only, red.
    #[test]
    fn menu_keys_map_to_actions_and_the_ai_rows_come_with_a_provisioning_key() {
        let keys = |m: &[(char, Action)]| m.iter().map(|(k, _)| *k).collect::<String>();
        assert_eq!(keys(&Action::pod_menu(false)), "ewlxgmcbpurst");
        assert_eq!(keys(&Action::pod_menu(true)), "ewlxgmcaonbpurst");
        assert_eq!(keys(&Action::fleet_menu(true, false)), "exgbprst");
        assert_eq!(keys(&Action::fleet_menu(true, true)), "exgaoObprst");
        assert_eq!(keys(&Action::fleet_menu(false, true)), "exgbp", "the whole fleet (`A`): never the AI key's rows");
        for menu in [Action::pod_menu(true), Action::fleet_menu(true, true)] {
            let mut k: Vec<char> = menu.iter().map(|(k, _)| *k).collect();
            k.sort();
            k.dedup();
            assert_eq!(k.len(), menu.len(), "a key twice: {menu:?}");
        }
        assert!(!Action::fleet_menu(true, true).iter().any(|(_, a)| *a == Action::KeyRotate));
        assert_eq!(Action::KeyPause.for_key(true), Action::KeyResume);
        assert_eq!(Action::KeyResume.for_key(false), Action::KeyPause);
        assert_eq!(Action::KeyLimit.for_key(true), Action::KeyLimit);
        assert!(Action::KeyRotate.is_risky() && Action::KeyRotate.is_destructive() && !Action::KeyRotate.requires_typed_name());
        assert!(Action::KeyLimit.needs_input() && Action::KeyLimit.cli() && Action::KeyPause.ai_key() && !Action::Rename.ai_key());
        assert_eq!((Action::KeyLimit.command(), Action::Start.command()), ("keys limit".to_string(), "pods start".to_string()));
    }

    #[test]
    fn display_name_strips_prefix_only_when_short() {
        assert_eq!(display_name("arena8-apple", "arena8", true), "apple");
        assert_eq!(display_name("arena8-apple", "arena8", false), "arena8-apple");
        // a name without the prefix is left as-is
        assert_eq!(display_name("apple", "arena8", true), "apple");
    }

    #[test]
    fn short_branch_extracts_iteration_label() {
        assert_eq!(short_branch("autocommit-arena8-w1d2-apple", "arena8"), "w1d2");
        assert_eq!(short_branch("autocommit-arena8-w0d1-autumn", "arena8"), "w0d1");
        // non-autocommit branches are untouched
        assert_eq!(short_branch("main", "arena8"), "main");
        assert_eq!(short_branch("feature/foo", "arena8"), "feature/foo");
    }

    // -- The core snapshot on the dashboard --------------------------------------------

    use arena_core::health::Check;
    use arena_core::snapshot::Issue;

    const NOW: u64 = 1_791_460_800; // 2026-10-08T12:00:00Z

    fn fleet_cfg() -> arena_core::Config {
        arena_core::Config::parse(
            "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(\n \"alpha\"\n \"bravo\"\n \"charlie\"\n \"delta\"\n \"echo\"\n)\n",
        )
    }

    /// `devtest-<name>` on `provider` with id `id`, RUNNING, at `endpoint` if given.
    fn at(name: &str, provider: &str, id: &str, endpoint: Option<(&str, u16)>) -> Pod {
        Pod {
            id: id.into(),
            name: format!("devtest-{name}"),
            provider: provider.into(),
            status: "RUNNING".into(),
            ssh_ip: endpoint.map(|(ip, _)| ip.into()),
            ssh_port: endpoint.map(|(_, port)| port),
            ..Default::default()
        }
    }

    /// A deep-check verdict for `p`, with one check of that status per name in `failing`.
    fn verdict(p: &Pod, status: Status, failing: &[&str]) -> PodHealth {
        PodHealth {
            id: p.id.clone(),
            name: p.name.clone(),
            provider: p.provider.clone(),
            status,
            checks: failing.iter().map(|n| Check { name: n.to_string(), status, detail: "x".into() }).collect(),
            facts: None,
            host: None,
        }
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

    #[test]
    fn health_cell_table() {
        let p = at("alpha", "runpod", "rp1", None);
        let rec = |status, ago: u64| HealthRecord::from_health(&verdict(&p, status, &[]), NOW - ago);
        // (cached record, a check running, text, tone): five cells at most, `HLTH` over them.
        let cases = [
            (None, false, "-", Tone::Dim),
            (None, true, "…", Tone::Busy),
            (Some(rec(Status::Pass, 720)), false, "✓12m", Tone::Good),
            (Some(rec(Status::Warn, 59)), false, "!59s", Tone::Warn),
            (Some(rec(Status::Fail, 7_300)), false, "✗2h", Tone::Bad),
            (Some(rec(Status::Fail, 3 * 86_400)), false, "✗3d", Tone::Bad),
            (Some(rec(Status::Skip, 3_600)), false, "·1h", Tone::Dim),
            (Some(rec(Status::Fail, 400 * 86_400)), false, "✗400d", Tone::Bad),
            // A pass fades past 26h (a daily cron's drift kept); a warning doesn't.
            (Some(rec(Status::Pass, 26 * 3_600)), false, "✓1d", Tone::Good),
            (Some(rec(Status::Pass, 26 * 3_600 + 1)), false, "✓1d", Tone::Dim),
            (Some(rec(Status::Warn, 2 * 86_400)), false, "!2d", Tone::Warn),
            // A re-check in flight says so over the old verdict.
            (Some(rec(Status::Fail, 60)), true, "…", Tone::Busy),
        ];
        for (r, checking, text, tone) in cases {
            assert_eq!(health_cell(r.as_ref(), checking, NOW), (text.to_string(), tone), "{r:?} {checking}");
            assert!(ratatui::text::Span::raw(text).width() <= 5, "{text}");
        }
        // A verdict newer than the snapshot's clock (it landed after the refresh) is 0s old.
        assert_eq!(health_cell(Some(&rec(Status::Pass, 0)), false, NOW - 5).0, "✓0s");
    }

    #[test]
    fn proxy_cell_table() {
        let cfg = fleet_cfg();
        let naming = Naming::from_config(&cfg);
        let pods = [
            at("alpha", "runpod", "rp1", Some(("10.0.0.1", 22001))),
            at("bravo", "runpod", "rp2", Some(("10.0.0.2", 22002))), // moved since the proxy wrote
            at("charlie", "runpod", "rp3", None),
        ];
        let text = proxy_file();
        let read = dashboard_snapshot(&pods, &[], Some(&text), &HealthCache::new(), &naming, NOW);
        let unread = dashboard_snapshot(&pods, &[], None, &HealthCache::new(), &naming, NOW);
        // (row, text, tone): the port at every width, a stale one marked (and yellow).
        let cases = [
            (&read.pods[0], "9500", Tone::Good),
            (&read.pods[1], "9501!", Tone::Warn),
            (&read.pods[2], "-", Tone::Dim),
            (&unread.pods[0], "?", Tone::Dim),
        ];
        for (row, text, tone) in cases {
            assert_eq!(proxy_cell(row), (text.to_string(), tone), "{}", row.pod.name);
        }
        assert_eq!(proxy_detail(&read.pods[0]), ":9500 (live)");
        assert_eq!(proxy_detail(&read.pods[1]), ":9501 stale (`arena proxy apply` re-points it)");
        assert_eq!(proxy_detail(&read.pods[2]), "- (no forward for this name)");
        assert_eq!(proxy_detail(&unread.pods[0]), "? (remote/unreadable proxy config)");
    }

    /// The badge shows exactly when `pods list`'s MAINT column says something.
    #[test]
    fn maintenance_badge_follows_the_pods_list_column() {
        let mut p = at("alpha", "runpod", "rp1", None);
        let cases = [
            (None, false),
            (Some(Maintenance::default()), false), // nothing in it
            (Some(Maintenance { note: Some("  ".into()), ..Default::default() }), false),
            (Some(Maintenance { note: Some("GPU swap".into()), ..Default::default() }), true),
            (Some(Maintenance { start: Some("2026-10-09T02:00:00Z".into()), ..Default::default() }), true),
        ];
        for (m, want) in cases {
            p.maintenance = m.clone();
            assert_eq!(has_maintenance(&p), want, "{m:?}");
        }
    }

    #[test]
    fn dashboard_rows_are_the_core_snapshot_ordered_by_name() {
        let cfg = fleet_cfg();
        let naming = Naming::from_config(&cfg);
        let alpha = at("alpha", "vast", "77", Some(("10.0.0.1", 22001)));
        let bravo = at("bravo", "hetzner", "88", None);
        let charlie = at("charlie", "runpod", "rp3", None);
        let mut cache = HealthCache::new();
        cache.merge(&[verdict(&alpha, Status::Warn, &["load"])], NOW - 600);
        let text = proxy_file();
        let snap = dashboard_snapshot(&[charlie, alpha, bravo], &["runpod".into()], Some(&text), &cache, &naming, NOW);
        // The builder groups by provider (hetzner, runpod, vast); the dashboard reads by name.
        let names: Vec<&str> = snap.pods.iter().map(|p| p.pod.name.as_str()).collect();
        assert_eq!(names, ["devtest-alpha", "devtest-bravo", "devtest-charlie"]);
        // Everything else is the builder's: health joined by provider:id, proxy, list entry, cost.
        let a = &snap.pods[0];
        assert_eq!(a.health.as_ref().map(|h| (h.status, h.issues.clone())), Some((Status::Warn, vec![Issue::BusyHost])));
        assert_eq!((a.proxy, a.proxy_port, a.list_entry.as_deref()), (ProxyState::Live, Some(9500), Some("alpha")));
        assert_eq!((snap.cost.billing, snap.partial.as_slice(), snap.generated_at), (3, &["runpod".to_string()][..], NOW));
    }

    /// The pure half of a background deep check: `d` claims pods (a second `d` doesn't
    /// double up), the results land on the rows at once, every claimed pod is released —
    /// even one whose check produced nothing — and the session's verdicts survive the next
    /// refresh re-reading an older cache file, until something newer is recorded.
    #[test]
    fn deep_check_results_fold_into_the_rows_and_outlive_a_refresh() {
        let cfg = fleet_cfg();
        let naming = Naming::from_config(&cfg);
        let alpha = at("alpha", "runpod", "rp1", Some(("10.0.0.1", 22001)));
        let bravo = at("bravo", "runpod", "rp2", Some(("10.0.0.2", 22002)));
        let charlie = at("charlie", "vast", "77", None);
        let pods = [alpha.clone(), bravo.clone(), charlie.clone()];
        let mut file = HealthCache::new();
        file.merge(&[verdict(&alpha, Status::Pass, &[])], NOW - 3_600);

        let mut deep = DeepChecks::default();
        let mut snap = dashboard_snapshot(&pods, &[], None, &deep.layer(file.clone()), &naming, NOW);
        let claimed = deep.begin(vec![alpha.clone(), bravo.clone()]);
        assert_eq!(claimed, [alpha.clone(), bravo.clone()]);
        assert!(deep.is_running(&alpha) && deep.is_running(&bravo) && !deep.is_running(&charlie));
        let second = deep.begin(vec![bravo.clone(), charlie.clone()]);
        assert_eq!(second, std::slice::from_ref(&charlie), "bravo is already being checked");
        assert_eq!(deep.running(), 3);

        // The first check lands: alpha fails on its GPU; bravo got no result at all.
        let line = deep.finish(&claimed, &[verdict(&alpha, Status::Fail, &["cuda"])], NOW + 100, &mut snap);
        assert_eq!(line, "deep check: 0 pass, 0 warn, 1 fail — failed: devtest-alpha");
        assert!(!deep.is_running(&alpha) && !deep.is_running(&bravo) && deep.is_running(&charlie));
        let h = snap.pods[0].health.as_ref().unwrap();
        assert_eq!((h.status, h.checked_at, h.issues.as_slice()), (Status::Fail, NOW + 100, &[Issue::GpuError][..]));
        assert_eq!(health_cell(Some(h), false, NOW + 100).0, "✗0s");
        assert!(snap.pods[1].health.is_none());

        // The next refresh re-reads the file (still the old pass): the newer verdict stays…
        assert_eq!(deep.layer(file.clone()).get(&alpha).unwrap().status, Status::Fail);
        // …until a later `pods test --deep` records something newer.
        file.merge(&[verdict(&alpha, Status::Pass, &[])], NOW + 200);
        assert_eq!(deep.layer(file).get(&alpha).unwrap().status, Status::Pass);

        // All passing: no "failed" tail; nothing left running.
        let done = deep.finish(&second, &[verdict(&charlie, Status::Pass, &[])], NOW + 300, &mut snap);
        assert_eq!(done, "deep check: 1 pass, 0 warn, 0 fail");
        assert_eq!(deep.running(), 0);
    }

    #[test]
    fn details_due_table() {
        let s = Duration::from_secs;
        // (since the last query, `r` pressed, run it now?)
        let cases = [
            (None, false, true), // never ran: the first refresh fills the details
            (None, true, true),
            (Some(s(5)), false, false),
            (Some(s(5)), true, false), // `r` again within the minimum gap: no
            (Some(DETAILS_MIN_GAP), true, true),
            (Some(s(59)), false, false),
            (Some(DETAILS_EVERY), false, true),
        ];
        for (since, forced, want) in cases {
            assert_eq!(details_due(since, forced), want, "{since:?} forced={forced}");
        }
    }

    /// The footer says which provider is rate-limited, when it is listed again and why, and
    /// how old its rows are.
    #[test]
    fn listing_note_table() {
        use arena_core::ratelimit::Hold;
        let s = Duration::from_secs;
        let hold = |secs: u64, why: &str| Some(Hold { api: "RunPod REST v2".into(), wait: s(secs), why: why.into() });
        let t = |provider: &str, hold: Option<Hold>, age: Option<Duration>| Throttle { provider: provider.into(), hold, rows_age: age };
        let cases = [
            (vec![], None),
            (
                vec![t("runpod", hold(12, "the minute window is spent"), Some(s(40)))],
                Some("RunPod rate limit: listing again in 12 s (the minute window is spent) · its rows are 40 s old"),
            ),
            (
                vec![t("runpod", hold(1800, "the hour window is spent"), None)],
                Some("RunPod rate limit: listing again in 30 min (the hour window is spent) · no rows yet"),
            ),
            (
                vec![t("vast", None, Some(s(5))), t("runpod", hold(3, "the day window is low: 900 of 86400 left"), Some(s(5)))],
                Some("Vast rate limit: listing again next refresh · its rows are 5 s old · RunPod rate limit: listing again in 3 s (the day window is low: 900 of 86400 left) · its rows are 5 s old"),
            ),
        ];
        for (throttled, want) in cases {
            assert_eq!(listing_note(&throttled).as_deref(), want, "{throttled:?}");
        }
    }

    /// A rate-limited provider keeps its last good rows; any other failure stays a failure.
    #[test]
    fn carry_over_keeps_a_throttled_providers_rows() {
        use arena_core::proxy::ProviderListing;
        let pod = |name: &str| Pod { name: name.into(), ..Default::default() };
        let listing = |entries: Vec<(&str, std::result::Result<Vec<Pod>, &str>)>| Listing {
            providers: entries.into_iter().map(|(p, r)| ProviderListing { provider: p.into(), pods: r.map_err(String::from) }).collect(),
        };
        let prev = listing(vec![("runpod", Ok(vec![pod("devtest-apple")])), ("vast", Ok(vec![pod("devtest-bravo")]))]);
        let mut fresh = listing(vec![("runpod", Err("list pods HTTP 429")), ("vast", Err("vast list HTTP 502"))]);
        carry_over(Some(&prev), &mut fresh, &["runpod".into()]);
        assert_eq!(fresh.providers[0].pods.as_ref().map(|p| p[0].name.as_str()), Ok("devtest-apple"));
        assert!(fresh.providers[1].pods.is_err(), "a 502 isn't a throttle");
        // Nothing to fall back on: the failure stays.
        let mut fresh = listing(vec![("runpod", Err("429"))]);
        carry_over(None, &mut fresh, &["runpod".into()]);
        assert!(fresh.providers[0].pods.is_err());
        let mut fresh = listing(vec![("runpod", Err("429"))]);
        carry_over(Some(&listing(vec![("runpod", Err("429"))])), &mut fresh, &["runpod".into()]);
        assert!(fresh.providers[0].pods.is_err());
    }

    /// The rows a throttle falls back on are each provider's last good listing, not the last
    /// refresh's: RunPod lists, fails with a 502, then is rate-limited — its rows come back
    /// (review r2), while Vast lists as usual throughout.
    #[test]
    fn a_throttle_after_another_failure_still_has_the_last_good_rows() {
        use arena_core::proxy::ProviderListing;
        let pod = |name: &str| Pod { name: name.into(), ..Default::default() };
        let listing = |entries: Vec<(&str, std::result::Result<Vec<Pod>, &str>)>| Listing {
            providers: entries.into_iter().map(|(p, r)| ProviderListing { provider: p.into(), pods: r.map_err(String::from) }).collect(),
        };
        fn names(l: &Listing, i: usize) -> std::result::Result<Vec<String>, String> {
            l.providers[i].pods.clone().map(|p| p.into_iter().map(|p| p.name).collect())
        }
        let refreshes = [
            listing(vec![("runpod", Ok(vec![pod("devtest-apple")])), ("vast", Ok(vec![pod("devtest-bravo")]))]),
            listing(vec![("runpod", Err("list pods HTTP 502")), ("vast", Ok(vec![pod("devtest-bravo"), pod("devtest-cherry")]))]),
            listing(vec![("runpod", Err("list pods HTTP 429")), ("vast", Ok(vec![pod("devtest-cherry")]))]),
        ];
        let runpod = ["runpod".to_string()];
        let mut good: Option<Listing> = None;
        let mut shown = Vec::new();
        for (i, fresh) in refreshes.iter().enumerate() {
            let mut s = fresh.clone();
            carry_over(good.as_ref(), &mut s, if i == 2 { &runpod } else { &[] });
            good = Some(remember_good(good.as_ref(), fresh));
            shown.push(s);
        }
        assert!(shown[1].providers[0].pods.is_err(), "a 502 is shown as a failure");
        assert_eq!(names(&shown[2], 0), Ok(vec!["devtest-apple".to_string()]), "the throttle falls back on the last good rows");
        assert_eq!(names(&shown[2], 1), Ok(vec!["devtest-cherry".to_string()]), "Vast as listed");
        // A provider gone from the config is forgotten.
        let only_vast = listing(vec![("vast", Ok(vec![]))]);
        assert_eq!(remember_good(good.as_ref(), &only_vast).providers.len(), 1);
    }

    #[test]
    fn details_are_reapplied_between_queries_as_enrich_merges_them() {
        let window = Maintenance { start: Some("2026-10-09T02:00:00Z".into()), end: None, note: Some("host upgrade".into()) };
        let enriched = |id: &str, ty: &str, n: u32, cost: f64, m: Option<Maintenance>| Pod {
            id: id.into(),
            provider: "runpod".into(),
            gpu_type: Some(ty.into()),
            gpu_count: Some(n),
            cost_per_hr: Some(cost),
            maintenance: m,
            ..Default::default()
        };
        let details = capture_details(&[
            enriched("rp1", "RTX A4000", 2, 0.17, Some(window.clone())),
            enriched("rp2", "RTX A4000", 2, 0.20, None),
        ]);
        let listed = |provider: &str, id: &str| Pod { id: id.into(), provider: provider.into(), ..Default::default() };
        let mut fresh = vec![
            listed("runpod", "rp1"), // the cheap list: no machine details at all
            Pod { gpu_type: Some("RTX 3090".into()), gpu_count: Some(1), cost_per_hr: Some(0.30), ..listed("runpod", "rp2") },
            listed("vast", "rp1"), // same id, another provider: not the same pod
            listed("runpod", "rp9"), // created since the last query
        ];
        overlay_details(&mut fresh, &details);
        let row = |p: &Pod| (p.gpu_type.clone(), p.gpu_count, p.cost_per_hr, p.maintenance.clone());
        assert_eq!(row(&fresh[0]), (Some("RTX A4000".into()), Some(2), Some(0.17), Some(window)));
        // What the listing reports wins, except the count (the details' is the machine's).
        assert_eq!(row(&fresh[1]), (Some("RTX 3090".into()), Some(2), Some(0.30), None));
        assert_eq!(fresh[2], listed("vast", "rp1"));
        assert_eq!(fresh[3], listed("runpod", "rp9"));
    }

    #[test]
    fn select_input_parsing_table() {
        let ok = |input: &str| parse_select_input(input).unwrap();
        assert_eq!(ok("alpha..charlie  echo").targets, ["alpha..charlie", "echo"]);
        assert_eq!(ok("alpha,bravo").targets, ["alpha,bravo"], "comma lists are the core's to split");
        let a = ok("alpha..echo -x bravo !delta --exclude=charlie --on runpod --gpus 2");
        assert_eq!(a.targets, ["alpha..echo"]);
        assert_eq!(a.exclude, ["bravo", "delta", "charlie"]);
        assert_eq!((a.on.as_deref(), a.gpus, a.all), (Some("runpod"), Some(2), false));
        assert!(ok("--all").all);
        assert_eq!(ok("   "), SelectArgs::default());
        for (bad, why) in [
            ("--gpus two", "wants a number"),
            ("alpha -x", "-x needs a value"),
            ("--on", "--on needs a value"),
            ("--force alpha", "unknown option `--force`"),
            ("-v", "unknown option"),
        ] {
            let err = parse_select_input(bad).unwrap_err();
            assert!(err.contains(why), "{bad}: {err}");
        }
    }

    /// `/` resolves exactly as the CLI's selector does (ranges by MACHINE_NAME_LIST order,
    /// names, ids, exclusions, filters), and a typo marks nothing: the core's error, on
    /// one line for the status bar. `all` and filter-only selections are refused, since
    /// marks feed terminate/restart.
    #[test]
    fn selector_marks_resolve_like_the_cli_and_a_typo_marks_nothing() {
        let cfg = fleet_cfg();
        let naming = Naming::from_config(&cfg);
        let pods = vec![
            at("alpha", "runpod", "rp1", None),
            at("bravo", "runpod", "rp2", None),
            at("charlie", "vast", "77", None),
            at("delta", "runpod", "rp4", None),
            at("echo", "hetzner", "88", None),
        ];
        // Marks are `provider:id` keys; the names they mark, in row order.
        let ids = |input: &str| {
            let (keys, _) = select_marks(input, &naming, &pods).unwrap_or_else(|e| panic!("{input}: {e}"));
            marked_pods(&pods, &keys).iter().map(|p| p.id.clone()).collect::<Vec<_>>()
        };
        assert_eq!(ids("alpha..delta"), ["rp1", "rp2", "77", "rp4"]);
        assert_eq!(ids("alpha..echo -x charlie"), ["rp1", "rp2", "rp4", "88"]);
        assert_eq!(ids("alpha..echo !charlie,delta"), ["rp1", "rp2", "88"]);
        assert_eq!(ids("alpha..echo --on runpod"), ["rp1", "rp2", "rp4"]);
        assert_eq!(ids("devtest-bravo 88"), ["rp2", "88"], "a full name and an id");
        let (keys, _) = select_marks("echo", &naming, &pods).unwrap();
        assert_eq!(keys, HashSet::from(["hetzner:88".to_string()]));
        let (_, msg) = select_marks("alpha..charlie", &naming, &pods).unwrap();
        assert_eq!(msg, "marked 3 pods: alpha..charlie");
        let (_, msg) = select_marks("echo", &naming, &pods).unwrap();
        assert_eq!(msg, "marked 1 pod: echo");

        for (input, needle) in [
            ("alpah", "alpah"),
            ("alpha..zulu", "`zulu` is not a MACHINE_NAME_LIST name"),
            ("delta..alpha", "reversed"),
            ("alpha..echo -x brovo", "brovo"),
            ("alpha --on aws", "not a provider"),
            ("all", "`all` isn't taken here"),
            ("--all", "`all` isn't taken here"),
            ("--on runpod", "name pods, ids or a range"),
            ("", "name pods, ids or a range"),
        ] {
            let err = select_marks(input, &naming, &pods).unwrap_err();
            assert!(err.contains(needle), "{input:?}: {err}");
            assert!(!err.contains('\n'), "{input:?}: one line for the status bar: {err}");
        }
    }

    /// Ids are unique only per provider: a Vast instance and a Hetzner server sharing a
    /// number are two pods, and `/` marking one must not hand the other to terminate.
    #[test]
    fn a_mark_is_one_pod_even_when_another_provider_reuses_its_id() {
        let cfg = fleet_cfg();
        let naming = Naming::from_config(&cfg);
        let pods = vec![
            at("alpha", "vast", "88", None),
            at("bravo", "hetzner", "88", None),
            at("charlie", "runpod", "rp3", None),
        ];
        let (keys, msg) = select_marks("alpha", &naming, &pods).unwrap();
        assert_eq!(msg, "marked 1 pod: alpha");
        let marked: Vec<&str> = marked_pods(&pods, &keys).iter().map(|p| p.name.as_str()).collect();
        assert_eq!(marked, ["devtest-alpha"], "the hetzner server with the same id stays unmarked");
        assert_eq!(mark_key(&pods[1]), "hetzner:88");
    }

    /// The marked-set menu offers stop (it used to be per-pod only) on the terms restart has:
    /// only for a marked set — never the whole fleet in one keystroke, so `A` has none of the
    /// lifecycle actions — and with the same token ([`marked_set_token`]).
    #[test]
    fn stop_is_offered_for_a_marked_set_like_restart() {
        let fleet = |marked| Action::fleet_menu(marked, false).into_iter().collect::<Vec<_>>();
        assert!(fleet(true).contains(&('s', Action::Stop)), "{:?}", fleet(true));
        for lifecycle in [Action::Stop, Action::Restart, Action::Terminate] {
            assert!(!fleet(false).iter().any(|(_, a)| *a == lifecycle), "{lifecycle:?} on the whole fleet: {:?}", fleet(false));
            assert!(lifecycle.marked_set_only());
        }
        assert_eq!(fleet(true).len(), Action::MENU.iter().filter(|(_, a)| !a.cli()).count(), "a marked set gets every per-pod action but the CLI's");
        assert!(fleet(true).iter().all(|(_, a)| !a.cli()), "start/rename/jobs/logs: the cursor pod's menu only");
        assert!(!Action::Backup.marked_set_only() && !Action::Test.marked_set_only());
    }

    /// Terminate/restart/stop on a marked set ask for the count only for a small, partial set;
    /// a big one — or the whole listed fleet, however it was marked (`/first..last`, a comma
    /// list, a space per pod) — asks for `ALL`, like `A`. Safe actions keep the count.
    #[test]
    fn a_destructive_marked_set_that_is_big_or_the_whole_fleet_asks_for_all() {
        // (action, marked, listed, token)
        let cases = [
            (Action::Stop, 3, 30, "3"),
            (Action::Stop, SET_COUNT_CONFIRM_MAX + 1, 30, "ALL"),
            (Action::Stop, 2, 2, "ALL"),
            (Action::Terminate, 3, 30, "3"),
            (Action::Terminate, SET_COUNT_CONFIRM_MAX, 30, "5"),
            (Action::Terminate, SET_COUNT_CONFIRM_MAX + 1, 30, "ALL"),
            (Action::Terminate, 30, 30, "ALL"), // `/m00..m29`
            (Action::Restart, 2, 2, "ALL"),     // a small fleet, all of it
            (Action::Restart, 1, 2, "1"),
            (Action::Restart, 29, 30, "ALL"),
            (Action::Setup, 30, 30, "30"),
            (Action::Backup, 12, 30, "12"),
        ];
        for (action, marked, listed, want) in cases {
            assert_eq!(marked_set_token(action, marked, listed), want, "{action:?} {marked}/{listed}");
        }
    }

    #[test]
    fn names_preview_is_bounded() {
        let names: Vec<String> = (0..14).map(|i| format!("m{i:02}")).collect();
        assert_eq!(names_preview(&names[..3], 10), "m00, m01, m02");
        assert_eq!(names_preview(&names[..10], 10), names[..10].join(", "));
        assert_eq!(names_preview(&names, 10), format!("{}, … and 4 more", names[..10].join(", ")));
        assert_eq!(names_preview(&[], 10), "");
    }

    /// The busy modal's words are English: `backing up alpha…`, never the label plus `ing`.
    #[test]
    fn the_busy_words_are_words() {
        assert_eq!(Action::Backup.doing(), "backing up");
        assert_eq!(Action::Stop.doing(), "stopping");
        assert_eq!(Action::Terminate.doing(), "terminating");
        let ai = [Action::KeyLimit, Action::KeyPause, Action::KeyResume, Action::KeyRotate];
        for a in Action::MENU.iter().map(|(_, a)| *a).chain(ai) {
            let w = a.doing();
            assert!(!w.contains("eing") && !w.contains("uping") && !w.contains("oping") && !w.contains('-'), "{a:?}: {w}");
        }
    }

    /// A complete details answer replaces what was kept; an incomplete one (an error after
    /// some pods were filled) adds what it filled, field by field, over the last good one.
    #[test]
    fn merge_details_table() {
        let window = Maintenance { note: Some("host upgrade".into()), ..Default::default() };
        let d = |ty: Option<&str>, cost: Option<f64>, m: Option<&Maintenance>| PodDetails {
            gpu_type: ty.map(String::from),
            gpu_count: ty.map(|_| 1),
            cost_per_hr: cost,
            maintenance: m.cloned(),
        };
        let kept0 = || {
            HashMap::from([
                ("runpod:rp1".to_string(), d(Some("RTX A4000"), Some(0.17), Some(&window))),
                ("runpod:rp2".to_string(), d(Some("RTX 3090"), Some(0.22), None)),
            ])
        };
        // This query filled rp1's price only (another field errored) and nothing for rp2.
        let fresh = || {
            HashMap::from([
                ("runpod:rp1".to_string(), d(None, Some(0.19), None)),
                ("runpod:rp2".to_string(), d(None, None, None)),
                ("runpod:rp3".to_string(), d(Some("RTX A5000"), Some(0.30), None)),
            ])
        };
        let mut partial = kept0();
        merge_details(&mut partial, fresh(), false);
        assert_eq!(partial["runpod:rp1"], d(Some("RTX A4000"), Some(0.19), Some(&window)), "new price, kept rest");
        assert_eq!(partial["runpod:rp2"], d(Some("RTX 3090"), Some(0.22), None), "nothing filled: last good kept");
        assert_eq!(partial["runpod:rp3"], d(Some("RTX A5000"), Some(0.30), None), "a new pod's fill is used");
        // Nothing kept yet (the first query erred part-way): what it filled shows.
        let mut first = HashMap::new();
        merge_details(&mut first, fresh(), false);
        assert_eq!(first["runpod:rp1"], d(None, Some(0.19), None));
        // Complete: exactly the answer — a window that ended is gone.
        let mut complete = kept0();
        merge_details(&mut complete, fresh(), true);
        assert_eq!(complete, fresh());
    }
}
