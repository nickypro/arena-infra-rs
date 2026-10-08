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
//! how often the provider's slower details query may run, and what `/` marks.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use arena_core::fleet::{self, FleetCost};
use arena_core::health::{render_summary, PodHealth, Status};
use arena_core::metrics::PodMetrics;
use arena_core::pod::Maintenance;
use arena_core::selector::{Naming, SelectArgs, Selector};
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
    if util_n > 0 {
        s.mean_util = Some((util_sum / util_n) as u32);
    }
    s
}

/// The summary bar's warning when a provider failed to list: its pods are missing from the
/// rows and their cost from the fleet total, so the numbers after it are a floor.
pub fn partial_notice(partial: &[String]) -> Option<String> {
    (!partial.is_empty())
        .then(|| format!(" ⚠ {} failed to list — its pods (and their cost) are missing ·", partial.join(", ")))
}

/// The summary bar: when a provider failed to list, [`partial_notice`] **first** — the bar
/// is one unwrapped line and its tail is cut on anything narrower than ~160 columns, so a
/// warning at the end would be invisible exactly when it matters —, then pod/GPU/util
/// counts from the probes, then the fleet's burn exactly as `pods list`'s footer words it
/// ([`fleet::fleet_footer`]: billing pods only, `$` and Hetzner's `€` kept apart, unpriced
/// pods called out), and the same per day.
pub fn summary_text(s: &FleetSummary, cost: &FleetCost, partial: &[String]) -> String {
    let util = s.mean_util.map(|u| format!("{u}%")).unwrap_or_else(|| "-".into());
    let mut out = partial_notice(partial).unwrap_or_default();
    out.push_str(&format!(" {} pods · {} GPUs · mean util {util} · {}", s.pods, s.total_gpus, fleet::fleet_footer(cost)));
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
    out
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

/// The HEALTH cell: `checking` while a deep check of this pod runs, else the last cached
/// verdict and its age (`pass 12m`, `fail 2h` — `Status::label` + `snapshot::fmt_age`,
/// as `arena snapshot` words them; the worst issue is left to the detail pane, it doesn't
/// fit a column), or `-` for a pod never checked. `now` is the snapshot's clock.
pub fn health_cell(h: Option<&HealthRecord>, checking: bool, now: u64) -> (String, Tone) {
    if checking {
        return ("checking".into(), Tone::Busy);
    }
    let Some(h) = h else { return ("-".into(), Tone::Dim) };
    let tone = match h.status {
        Status::Pass => Tone::Good,
        Status::Warn => Tone::Warn,
        Status::Fail => Tone::Bad,
        Status::Skip => Tone::Dim,
    };
    (format!("{} {}", h.status.label(), fmt_age(now.saturating_sub(h.checked_at))), tone)
}

/// The PROXY cell: [`snapshot::proxy_label`] (`:9500`, `:9500 stale`, `-`, `?`), or with
/// `compact` (a narrow terminal) just the port — the tone still tells live from stale.
pub fn proxy_cell(p: &SnapshotPod, compact: bool) -> (String, Tone) {
    let tone = match p.proxy {
        ProxyState::Live => Tone::Good,
        ProxyState::Stale => Tone::Warn,
        ProxyState::None | ProxyState::Unknown => Tone::Dim,
    };
    let text = match (compact, p.proxy_port) {
        (true, Some(port)) if matches!(p.proxy, ProxyState::Live | ProxyState::Stale) => format!(":{port}"),
        _ => snapshot::proxy_label(p),
    };
    (text, tone)
}

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

/// The most pods a destructive action (terminate/restart) on a marked set confirms with
/// just their count. Marks used to take a keypress per pod; `/` now marks a cohort in one
/// short range (`/apple..zulu`), so the bar has to follow what is marked, not how: a
/// bigger set — or every listed pod, however few — asks for `ALL`, the whole-fleet bar.
pub const SET_COUNT_CONFIRM_MAX: usize = 5;

/// The token a marked-set confirm asks to be typed, for `marked` of the `listed` pods: the
/// count (which the operator chose), or `ALL` for a destructive action on more than
/// [`SET_COUNT_CONFIRM_MAX`] pods or on the whole listed fleet. Non-destructive actions
/// (backup/setup/run/set-branch) keep the count.
pub fn marked_set_token(action: Action, marked: usize, listed: usize) -> String {
    if action.is_destructive() && (marked > SET_COUNT_CONFIRM_MAX || marked >= listed) {
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

/// One provider choice in the add-pod form, with whether its API key is configured.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderOpt {
    pub name: String,
    pub available: bool,
}

/// The editable fields of the add-pod form, in display order. Which ones apply depends
/// on the chosen provider (no cloud type except RunPod; no GPU fields on Hetzner).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NpField {
    Provider,
    CloudType,
    GpuType,
    GpuCount,
    Disk,
    Volume,
    Pods,
}

/// State of the interactive add-pod form: `↑↓` moves between [`NpField`]s, `←→` changes
/// the selected field's value. All pure — `main` owns the provider build / listing.
#[derive(Debug, Clone)]
pub struct NewPodForm {
    pub providers: Vec<ProviderOpt>,
    pub provider_idx: usize,
    pub cloud_types: Vec<String>,
    pub cloud_idx: usize,
    pub gpu_types: Vec<String>,
    pub gpu_idx: usize,
    pub gpu_count: u32,
    /// Container disk size in GB (adjusted in 50GB steps) — the main storage knob.
    pub disk_gb: u32,
    /// Persistent volume size in GB (0 = none, the default); 100GB steps.
    pub volume_gb: u32,
    pub count: usize,
    /// Free machine names available on the selected provider (for the count cap).
    pub free: Vec<String>,
    /// Index into [`Self::fields`] of the currently-selected field.
    pub field: usize,
}

fn wrap(idx: usize, delta: i32, len: usize) -> usize {
    if len == 0 {
        0
    } else {
        (idx as i32 + delta).rem_euclid(len as i32) as usize
    }
}

impl NewPodForm {
    pub fn provider_name(&self) -> &str {
        self.providers.get(self.provider_idx).map(|p| p.name.as_str()).unwrap_or("runpod")
    }

    fn is_runpod(&self) -> bool {
        self.provider_name() == "runpod"
    }

    /// GPU providers take GPU type/count; Hetzner is CPU-only.
    fn is_gpu(&self) -> bool {
        self.provider_name() != "hetzner"
    }

    /// The fields that apply to the current provider, in display order.
    pub fn fields(&self) -> Vec<NpField> {
        let mut f = vec![NpField::Provider];
        if self.is_runpod() {
            f.push(NpField::CloudType);
        }
        if self.is_gpu() {
            f.push(NpField::GpuType);
            f.push(NpField::GpuCount);
            f.push(NpField::Disk);
            f.push(NpField::Volume);
        }
        f.push(NpField::Pods);
        f
    }

    pub fn selected(&self) -> NpField {
        let f = self.fields();
        f[self.field.min(f.len() - 1)]
    }

    pub fn move_field(&mut self, delta: i32) {
        let n = self.fields().len();
        self.field = wrap(self.field, delta, n);
    }

    /// Change the selected field's value by `delta` (−1 left / +1 right). Returns true
    /// if the *provider* changed, since the caller must then re-list free names.
    pub fn change(&mut self, delta: i32) -> bool {
        match self.selected() {
            NpField::Provider => {
                self.cycle_provider(delta);
                // The field set may have shrunk (e.g. → Hetzner); keep `field` valid.
                let n = self.fields().len();
                self.field = self.field.min(n - 1);
                return true;
            }
            NpField::CloudType => self.cloud_idx = wrap(self.cloud_idx, delta, self.cloud_types.len()),
            NpField::GpuType => self.gpu_idx = wrap(self.gpu_idx, delta, self.gpu_types.len()),
            NpField::GpuCount => self.gpu_count = (self.gpu_count as i32 + delta).clamp(1, 8) as u32,
            // Container disk in 50GB steps, 20–2000GB.
            NpField::Disk => self.disk_gb = (self.disk_gb as i32 + delta * 50).clamp(20, 2000) as u32,
            // Volume in 100GB steps, 0 (none) up to 2000GB.
            NpField::Volume => self.volume_gb = (self.volume_gb as i32 + delta * 100).clamp(0, 2000) as u32,
            NpField::Pods => {
                let max = self.free.len().max(1) as i32;
                self.count = (self.count as i32 + delta).clamp(1, max) as usize;
            }
        }
        false
    }

    /// Step to the next available provider in `delta`'s direction, skipping any whose
    /// API key isn't configured (so you can't select an unusable provider).
    fn cycle_provider(&mut self, delta: i32) {
        let n = self.providers.len() as i32;
        if n == 0 {
            return;
        }
        let step = if delta < 0 { -1 } else { 1 };
        let mut idx = self.provider_idx as i32;
        for _ in 0..n {
            idx = (idx + step).rem_euclid(n);
            if self.providers[idx as usize].available {
                self.provider_idx = idx as usize;
                return;
            }
        }
    }

    /// The selected cloud type, only meaningful for RunPod.
    pub fn cloud_type(&self) -> Option<&str> {
        if self.is_runpod() {
            self.cloud_types.get(self.cloud_idx).map(String::as_str)
        } else {
            None
        }
    }

    pub fn gpu_type(&self) -> &str {
        self.gpu_types.get(self.gpu_idx).map(String::as_str).unwrap_or("")
    }

    /// The names that would be created for the current count.
    pub fn planned_names(&self) -> Vec<String> {
        self.free.iter().take(self.count).cloned().collect()
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
}

impl Action {
    /// The action-menu entries, in display order (harmless first, destructive last —
    /// so the default-highlighted top option is the read-only `test`), with their keys.
    pub const MENU: &'static [(char, Action)] = &[
        ('e', Action::Test),
        ('x', Action::Run),
        ('g', Action::SetBranch),
        ('b', Action::Backup),
        ('p', Action::Setup),
        ('r', Action::Restart),
        ('s', Action::Stop),
        ('t', Action::Terminate),
    ];

    pub fn from_key(c: char) -> Option<Action> {
        Self::MENU.iter().find(|(k, _)| *k == c).map(|(_, a)| *a)
    }

    /// The multi-pod menu's actions (and keys): everything except the per-pod-only
    /// `stop`; the destructive ones (`terminate`, `restart`) are offered only for a marked
    /// set — never for "the whole fleet" in one keystroke.
    pub fn fleet_menu(has_marked: bool) -> Vec<(char, Action)> {
        Self::MENU
            .iter()
            .copied()
            .filter(|(_, a)| !matches!(a, Action::Stop) && (!a.is_destructive() || has_marked))
            .collect()
    }

    /// A short description shown beside the action in the menu.
    pub fn desc(&self) -> &'static str {
        match self {
            Action::Restart => "restart — WIPES the container disk",
            Action::Stop => "stop the pod",
            Action::Terminate => "terminate — irreversible",
            Action::Backup => "commit + push the tree",
            Action::Setup => "provision / re-point git",
            Action::Test => "torch version (read-only)",
            Action::Run => "run a shell command",
            Action::SetBranch => "switch branch",
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
        matches!(self, Action::Terminate | Action::Restart)
    }

    /// Whether to render the action in red in the menu — destructive *or* disruptive
    /// (stop takes the pod down). Distinct from [`Self::is_destructive`] so stop is red
    /// without claiming to be irreversible.
    pub fn is_risky(&self) -> bool {
        matches!(self, Action::Terminate | Action::Restart | Action::Stop)
    }

    /// The warning in the confirm modal, if any. `wipes_disk`: whether the pod's provider
    /// resets the container disk on restart / stop→start (`Provider::
    /// restart_wipes_container_disk`; Hetzner's VM keeps it) — pass `true` when unknown.
    pub fn warning(&self, wipes_disk: bool) -> Option<&'static str> {
        match self {
            Action::Terminate => Some("⚠ IRREVERSIBLE"),
            Action::Restart if wipes_disk => Some(
                "⚠ WIPES THE CONTAINER DISK: the pod is reset to its image — everything outside a \
                 /workspace volume is lost (participants' files, ~/.name, git remote, keys). Back it \
                 up first; run setup (p) afterwards.",
            ),
            Action::Restart => Some("hard reset: running processes are killed; the disk is kept."),
            Action::Stop if wipes_disk => Some(
                "⚠ a stopped RunPod pod keeps no data: its container disk is discarded and it starts \
                 again as a fresh image (only a /workspace volume survives).",
            ),
            _ => None,
        }
    }

    /// Actions that need a free-text argument typed first (the command / the branch),
    /// collected in an input modal before they run.
    pub fn needs_input(&self) -> bool {
        matches!(self, Action::Run | Action::SetBranch)
    }

    /// The prompt shown in the input modal for [`Self::needs_input`] actions.
    pub fn input_prompt(&self) -> &'static str {
        match self {
            Action::Run => "shell command to run on the pod(s):",
            Action::SetBranch => "branch to switch to (fetch + checkout + ff-pull):",
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
    /// The exact command(s) that will run, shown for backup/setup so the operator
    /// sees precisely what's about to execute.
    pub preview: Option<String>,
    /// Whether this pod's restart/stop resets its container disk — selects the warning
    /// ([`Action::warning`]). Defaults to `true`: assuming a wipe only adds a warning.
    pub wipes_disk: bool,
}

impl Confirm {
    pub fn new(action: Action, pod_name: String, pod_id: String, preview: Option<String>) -> Self {
        Self { action, pod_name, pod_id, typed: String::new(), preview, wipes_disk: true }
    }

    /// The warning line for this confirmation, if any.
    pub fn warning(&self) -> Option<&'static str> {
        self.action.warning(self.wipes_disk)
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
            summary_text(&s, &cost, &[]),
            " 5 pods · 0 GPUs · mean util - · fleet: $0.60/h across 4 billing pod(s) + €0.006/h hetzner (1 unpriced) \
             ≈ $14.40/day + €0.13/day"
        );
        // A provider that failed to list leads the bar (its tail is cut on narrow terminals).
        let partial = summary_text(&s, &cost, &["vast".to_string(), "hetzner".to_string()]);
        assert!(
            partial.starts_with(" ⚠ vast, hetzner failed to list — its pods (and their cost) are missing · 5 pods · "),
            "{partial}"
        );
        assert!(partial.ends_with(" ≈ $14.40/day + €0.13/day"), "{partial}");
        assert_eq!(partial_notice(&[]), None);
        // Nothing billing: no per-day figure.
        let idle = summary_text(&FleetSummary::default(), &fleet::fleet_cost(&[]), &[]);
        assert_eq!(idle, " 0 pods · 0 GPUs · mean util - · fleet: $0.00/h across 0 billing pod(s)");
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
        assert!(Action::Backup.warning(true).is_none());
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
        // Unknown provider → assume the wipe; Hetzner's reset keeps the disk.
        assert!(c.warning().unwrap().contains("WIPES THE CONTAINER DISK"));
        c.wipes_disk = false;
        let kept = c.warning().unwrap();
        assert!(kept.contains("disk is kept") && !kept.contains("WIPES"), "{kept}");
        assert!(Action::Stop.warning(true).unwrap().contains("keeps no data"));
        assert!(Action::Stop.warning(false).is_none());
        assert!(Action::Restart.desc().contains("WIPES"));
        let fleet = |marked| Action::fleet_menu(marked).into_iter().map(|(_, a)| a).collect::<Vec<_>>();
        assert!(!fleet(false).contains(&Action::Restart) && !fleet(false).contains(&Action::Terminate));
        assert!(fleet(true).contains(&Action::Restart) && fleet(true).contains(&Action::Terminate));
        assert!(!fleet(true).contains(&Action::Stop));
        assert!(fleet(false).contains(&Action::Setup));
    }

    #[test]
    fn menu_keys_map_to_actions() {
        assert_eq!(Action::from_key('t'), Some(Action::Terminate));
        assert_eq!(Action::from_key('z'), None);
    }

    #[test]
    fn display_name_strips_prefix_only_when_short() {
        assert_eq!(display_name("arena8-apple", "arena8", true), "apple");
        assert_eq!(display_name("arena8-apple", "arena8", false), "arena8-apple");
        // a name without the prefix is left as-is
        assert_eq!(display_name("apple", "arena8", true), "apple");
    }

    fn form() -> NewPodForm {
        NewPodForm {
            providers: vec![
                ProviderOpt { name: "runpod".into(), available: true },
                ProviderOpt { name: "vast".into(), available: false },
                ProviderOpt { name: "hetzner".into(), available: true },
            ],
            provider_idx: 0,
            cloud_types: vec!["COMMUNITY".into(), "SECURE".into()],
            cloud_idx: 0,
            gpu_types: vec!["RTX 3090".into(), "RTX 4090".into()],
            gpu_idx: 0,
            gpu_count: 1,
            disk_gb: 100,
            volume_gb: 0,
            count: 1,
            free: vec!["arena8-apple".into(), "arena8-autumn".into()],
            field: 0,
        }
    }

    #[test]
    fn fields_depend_on_provider() {
        let f = form();
        assert_eq!(
            f.fields(),
            vec![
                NpField::Provider,
                NpField::CloudType,
                NpField::GpuType,
                NpField::GpuCount,
                NpField::Disk,
                NpField::Volume,
                NpField::Pods
            ]
        );
    }

    #[test]
    fn provider_cycle_skips_unavailable_and_reshapes_fields() {
        let mut f = form();
        // runpod -> (skip vast, no key) -> hetzner
        assert!(f.change(1)); // provider changed
        assert_eq!(f.provider_name(), "hetzner");
        // hetzner is CPU-only: no cloud/gpu fields
        assert_eq!(f.fields(), vec![NpField::Provider, NpField::Pods]);
        assert_eq!(f.cloud_type(), None);
    }

    #[test]
    fn change_clamps_gpu_count_and_pods() {
        let mut f = form();
        f.field = 3; // GpuCount
        for _ in 0..20 {
            f.change(1);
        }
        assert_eq!(f.gpu_count, 8); // capped
        f.field = 4; // Disk (50GB steps)
        f.change(1);
        assert_eq!(f.disk_gb, 150);
        f.field = 5; // Volume (100GB steps)
        f.change(1);
        f.change(1);
        assert_eq!(f.volume_gb, 200);
        f.field = 6; // Pods
        for _ in 0..20 {
            f.change(1);
        }
        assert_eq!(f.count, 2); // capped at free.len()
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
        // (cached record, a check running, text, tone)
        let cases = [
            (None, false, "-", Tone::Dim),
            (None, true, "checking", Tone::Busy),
            (Some(rec(Status::Pass, 720)), false, "pass 12m", Tone::Good),
            (Some(rec(Status::Warn, 59)), false, "warn 59s", Tone::Warn),
            (Some(rec(Status::Fail, 7_300)), false, "fail 2h", Tone::Bad),
            (Some(rec(Status::Fail, 3 * 86_400)), false, "fail 3d", Tone::Bad),
            // A re-check in flight says so over the old verdict.
            (Some(rec(Status::Fail, 60)), true, "checking", Tone::Busy),
        ];
        for (r, checking, text, tone) in cases {
            assert_eq!(health_cell(r.as_ref(), checking, NOW), (text.to_string(), tone), "{r:?} {checking}");
        }
        // A verdict newer than the snapshot's clock (it landed after the refresh) is 0s old.
        assert_eq!(health_cell(Some(&rec(Status::Pass, 0)), false, NOW - 5).0, "pass 0s");
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
        // (row, compact, text, tone)
        let cases = [
            (&read.pods[0], false, ":9500", Tone::Good),
            (&read.pods[0], true, ":9500", Tone::Good),
            (&read.pods[1], false, ":9501 stale", Tone::Warn),
            (&read.pods[1], true, ":9501", Tone::Warn), // narrow: the colour says stale
            (&read.pods[2], false, "-", Tone::Dim),
            (&read.pods[2], true, "-", Tone::Dim),
            (&unread.pods[0], false, "?", Tone::Dim),
            (&unread.pods[0], true, "?", Tone::Dim),
        ];
        for (row, compact, text, tone) in cases {
            assert_eq!(proxy_cell(row, compact), (text.to_string(), tone), "{} compact={compact}", row.pod.name);
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
        assert_eq!(second, [charlie.clone()], "bravo is already being checked");
        assert_eq!(deep.running(), 3);

        // The first check lands: alpha fails on its GPU; bravo got no result at all.
        let line = deep.finish(&claimed, &[verdict(&alpha, Status::Fail, &["cuda"])], NOW + 100, &mut snap);
        assert_eq!(line, "deep check: 0 pass, 0 warn, 1 fail — failed: devtest-alpha");
        assert!(!deep.is_running(&alpha) && !deep.is_running(&bravo) && deep.is_running(&charlie));
        let h = snap.pods[0].health.as_ref().unwrap();
        assert_eq!((h.status, h.checked_at, h.issues.as_slice()), (Status::Fail, NOW + 100, &[Issue::GpuError][..]));
        assert_eq!(health_cell(Some(h), false, NOW + 100).0, "fail 0s");
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

    /// Terminate/restart on a marked set ask for the count only for a small, partial set;
    /// a big one — or the whole listed fleet, however it was marked (`/first..last`, a comma
    /// list, a space per pod) — asks for `ALL`, like `A`. Safe actions keep the count.
    #[test]
    fn a_destructive_marked_set_that_is_big_or_the_whole_fleet_asks_for_all() {
        // (action, marked, listed, token)
        let cases = [
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
