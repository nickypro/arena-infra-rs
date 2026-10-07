//! Multi-option placement (PLAN 2.A): `--gpu A4000,4000Ada,3090 --cloud community,secure
//! --max-price 0.5` → an ordered list of concrete create options, then fill a set of machine
//! names by trying those options **one create at a time**.
//!
//! Built here rather than adopted from dstack/SkyPilot (PLAN 2.0: neither honours our CUDA-13
//! floor, both take over the pod) — the ideas are theirs: expand options, filter by price,
//! order cheapest or as listed, and block an option for the rest of a round once it reports
//! no capacity (SkyPilot's `blocked_resources`).
//!
//! Two halves, kept apart so both are table/scenario-testable:
//! - **Planning** ([`Request`], [`PriceBook`], [`plan_options`]) is pure: expand gpu × cloud,
//!   attach prices, drop what `--max-price` rules out, order. `arena offers`, the create/up
//!   dry-runs and the confirm prompt all show this one plan, so what the operator agreed to
//!   is exactly what runs.
//! - **Executing** ([`place`]) is generic over `&dyn Provider`. Per name, options are tried
//!   in order and a success moves on to the next name, so a name is never created twice; a
//!   capacity error blocks that option for the rest of the round, so later names don't each
//!   burn a create on a pool that just said "empty".
//!
//! Stock is a tie-break hint only, never a filter: RunPod's flag is coarse (1-GPU, and on the
//! GraphQL catalog not even per tier), and trying to create is the truth.

use std::future::Future;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Error, ProviderErrorKind, Result};
use crate::fleet::fmt_money;
use crate::gpu;
use crate::metrics::normalize_gpu_name;
use crate::pod::{Pod, PodSpec};
use crate::provider::runpod::GpuType;
use crate::provider::Provider;
use crate::retry::{retrying, RetryPolicy};
use crate::table::{self, Align};

/// The order options are tried in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Order {
    /// Lowest $/h per pod first; unpriced options last. A price tie is broken by the stock
    /// hint, then by the listed order.
    #[default]
    Cheapest,
    /// Exactly as listed: GPU-major (every cloud of the first GPU, then the next GPU) — the
    /// same "exhaust the preferred GPU before downgrading" walk as `plan.rs`.
    Listed,
}

impl FromStr for Order {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "cheapest" => Ok(Self::Cheapest),
            "listed" => Ok(Self::Listed),
            other => Err(format!("unknown order `{other}` (expected `cheapest` or `listed`)")),
        }
    }
}

impl std::fmt::Display for Order {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Cheapest => "cheapest",
            Self::Listed => "listed",
        })
    }
}

/// Where an option's price came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PriceSource {
    /// RunPod's live catalog (GraphQL `gpuTypes`, or REST v2 `/catalog/gpus`).
    Live,
    /// The local [`gpu::PRESETS`] (no live price for this GPU) — shown with `~`.
    Estimate,
}

/// Split a comma-list flag (`A4000, 3090,`) into trimmed, non-empty tokens.
pub fn split_list(raw: &str) -> Vec<String> {
    raw.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
}

fn dedup(items: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for i in items {
        if !out.contains(&i) {
            out.push(i);
        }
    }
    out
}

/// What the operator asked for, flags resolved: the GPU and cloud lists in their order.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// Provider GPU type strings (short names resolved via [`gpu::resolve`]), deduped.
    pub gpus: Vec<String>,
    /// Cloud tiers, uppercased, deduped.
    pub clouds: Vec<String>,
    /// GPUs per pod — prices are per GPU, the cap is per pod.
    pub gpu_count: u32,
    /// `--max-price`: $/h per pod.
    pub max_price: Option<f64>,
    pub order: Order,
}

impl Request {
    /// Build from the `--gpu`/`--cloud` flag values (comma lists) over `base`, the config
    /// spec with every other override applied: an absent flag means "what config says".
    /// A flag that lists nothing (`--gpu ,`) or a non-positive/non-finite cap is refused —
    /// before anything is listed or created.
    pub fn from_flags(
        gpu: Option<&str>,
        cloud: Option<&str>,
        base: &PodSpec,
        max_price: Option<f64>,
        order: Order,
    ) -> Result<Self> {
        let gpus = match gpu {
            None => vec![base.gpu_type.clone()],
            Some(raw) => dedup(split_list(raw).iter().map(|g| gpu::resolve(g)).collect()),
        };
        if gpus.is_empty() {
            return Err(Error::Config("--gpu lists no GPU type".into()));
        }
        let clouds = match cloud {
            None => vec![base.cloud_type.trim().to_uppercase()],
            Some(raw) => dedup(split_list(raw).iter().map(|c| c.to_uppercase()).collect()),
        };
        if clouds.is_empty() {
            return Err(Error::Config("--cloud lists no cloud tier".into()));
        }
        if let Some(p) = max_price {
            if !(p.is_finite() && p > 0.0) {
                return Err(Error::Config(format!("--max-price must be a positive $/h (got {p})")));
            }
        }
        Ok(Self { gpus, clouds, gpu_count: base.gpu_count, max_price, order })
    }

    /// One GPU, one cloud, no price cap: `create`/`up` keep today's single-spec path for this
    /// (same output, same `--keep-trying`), so nothing changes for an operator who doesn't
    /// use the new flags.
    pub fn is_single(&self) -> bool {
        self.gpus.len() == 1 && self.clouds.len() == 1 && self.max_price.is_none()
    }

    /// Checks that need the target provider. On RunPod every tier must be COMMUNITY or
    /// SECURE — prices are quoted per tier, and v2 creates on SECURE when the tier isn't one
    /// it knows. A GPU provider needs a GPU type (config's may be unset).
    pub fn validate_for(&self, provider: &str) -> Result<()> {
        if provider == "runpod" {
            if let Some(bad) = self.clouds.iter().find(|c| !matches!(c.as_str(), "COMMUNITY" | "SECURE")) {
                return Err(Error::Config(format!("--cloud `{bad}`: RunPod tiers are COMMUNITY and SECURE")));
            }
        }
        if provider != "hetzner" && self.gpus.iter().any(|g| g.trim().is_empty()) {
            return Err(Error::Config("no GPU type: pass --gpu or set GPU_TYPE".into()));
        }
        Ok(())
    }
}

/// One price lookup's answer.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Quote {
    /// $/h per GPU; `None` = unknown (not offered on this tier, or no quote at all).
    pub price_per_gpu: Option<f64>,
    pub source: Option<PriceSource>,
    /// Stock hint ("High"/"Medium"/"Low"/"None").
    pub stock: Option<String>,
}

/// The prices the planner can see, fetched once before planning (read-only) so the order
/// shown in the dry-run/confirm is the order that runs.
#[derive(Debug, Clone, Default)]
pub struct PriceBook {
    /// Live catalogs, each tagged with the tier its *stock* describes: REST v2 reports stock
    /// per tier (`Some("COMMUNITY")`), GraphQL's 1-GPU stock isn't per tier (`None`).
    catalogs: Vec<(Option<String>, Vec<GpuType>)>,
    /// Back unpriced GPUs with the [`gpu::PRESETS`] estimates. Only for RunPod: the presets
    /// *are* RunPod prices, so on another provider they'd be fiction.
    estimates: bool,
}

impl PriceBook {
    /// RunPod: `catalogs` may be empty (no key / fetch failed) — then every known GPU is
    /// priced from the presets, marked as an estimate.
    pub fn runpod(catalogs: Vec<(Option<String>, Vec<GpuType>)>) -> Self {
        Self { catalogs, estimates: true }
    }

    /// No quote before create: Vast's marketplace prices per offer at create time, Hetzner
    /// creates CPU VMs. Every option is unpriced.
    pub fn unpriced() -> Self {
        Self::default()
    }

    /// The price and stock hint for `gpu` on `cloud` (`None` = a provider without tiers).
    ///
    /// A live row that quotes either tier is trusted as a pair — its `None` tier means "not
    /// offered there", and is *not* back-filled with a preset (same rule as `arena gpus`).
    /// Only a GPU with no live price at all falls back to the preset estimate.
    pub fn quote(&self, gpu: &str, cloud: Option<&str>) -> Quote {
        let secure = cloud.is_some_and(|c| c.eq_ignore_ascii_case("SECURE"));
        let tier = |community: Option<f64>, secure_p: Option<f64>| if secure { secure_p } else { community };
        let rows = || self.catalogs.iter().flat_map(|(tag, rows)| rows.iter().map(move |r| (tag, r)));
        let live = rows().map(|(_, r)| r).find(|r| r.id == gpu && (r.community_price.is_some() || r.secure_price.is_some()));
        let preset = if self.estimates { gpu::find(gpu) } else { None };
        let (price, source) = match (live, preset) {
            (Some(r), _) => (tier(r.community_price, r.secure_price), Some(PriceSource::Live)),
            (None, Some(g)) => (tier(Some(g.community), Some(g.secure)), Some(PriceSource::Estimate)),
            (None, None) => (None, None),
        };
        let price_per_gpu = price.filter(|p| p.is_finite() && *p > 0.0);
        // Stock: the catalog for this very tier first, else a tier-agnostic one.
        let tagged = |want: Option<&str>| {
            rows().find_map(|(tag, r)| {
                let same = match (tag.as_deref(), want) {
                    (Some(t), Some(w)) => t.eq_ignore_ascii_case(w),
                    (None, None) => true,
                    _ => false,
                };
                (same && r.id == gpu).then(|| r.stock_status.clone()).flatten()
            })
        };
        let stock = cloud.and_then(|c| tagged(Some(c))).or_else(|| tagged(None));
        Quote { price_per_gpu, source, stock }
    }
}

/// One concrete thing to try creating.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlacementOption {
    /// Position in the listed (GPU-major) order, 0-based — the final tie-break.
    pub listed: usize,
    /// The provider GPU type string sent on create.
    pub gpu: String,
    /// `1×RTX A4000` (or `CPU VM` on Hetzner) — how the option reads in tables.
    pub label: String,
    /// The tier sent on create (RunPod); `None` on a provider without tiers.
    pub cloud: Option<String>,
    pub gpu_count: u32,
    /// $/h per GPU; `None` = unknown.
    pub price_per_gpu: Option<f64>,
    /// $/h per pod (`price_per_gpu × gpu_count`) — what `--max-price` caps.
    pub price_per_pod: Option<f64>,
    pub price_source: Option<PriceSource>,
    pub stock: Option<String>,
}

impl PlacementOption {
    /// `1×RTX A4000 COMMUNITY` — progress lines and the summary.
    pub fn describe(&self) -> String {
        match &self.cloud {
            Some(c) => format!("{} {c}", self.label),
            None => self.label.clone(),
        }
    }

    /// `$0.34/h`, `~$0.34/h` for an estimate, or `price unknown`.
    pub fn price_label(&self) -> String {
        match self.price_per_pod {
            Some(p) => format!("{}{}/h", self.tilde(), fmt_money("$", p)),
            None => "price unknown".to_string(),
        }
    }

    fn tilde(&self) -> &'static str {
        if self.price_source == Some(PriceSource::Estimate) {
            "~"
        } else {
            ""
        }
    }
}

/// An option `--max-price` ruled out, and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dropped {
    #[serde(flatten)]
    pub option: PlacementOption,
    pub reason: String,
}

/// The ordered options — what `arena offers` prints (`--json`: this struct) and what
/// [`place`] walks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionPlan {
    pub provider: String,
    pub gpu_count: u32,
    pub order: Order,
    pub max_price: Option<f64>,
    /// What placement will try, first to last.
    pub options: Vec<PlacementOption>,
    /// Ruled out by `--max-price`, never tried.
    pub dropped: Vec<Dropped>,
    /// Things the operator should know (tiers collapsed, estimates used, …).
    pub notes: Vec<String>,
}

/// Prices compared in tenths of a cent: `0.17 × 3` is `0.51000000000000001` in f64 and must
/// still fit under a `0.51` cap (and tie with a `0.51` option).
fn milli(p: f64) -> i64 {
    (p * 1000.0).round() as i64
}

/// Tie-break rank of a stock hint: known stock beats no report, which beats a reported
/// "None". Unrecognised strings count as no report.
fn stock_rank(stock: Option<&str>) -> u8 {
    match stock.map(str::to_ascii_lowercase).as_deref() {
        Some("high") => 0,
        Some("medium") => 1,
        Some("low") => 2,
        Some("none") => 4,
        _ => 3,
    }
}

/// The short GPU name for a label: the preset's, else the provider string minus vendor.
fn gpu_short(api: &str) -> String {
    gpu::find(api).map(|g| g.label.to_string()).unwrap_or_else(|| normalize_gpu_name(api))
}

/// Expand, price, filter and order the options for `provider`. Pure.
///
/// Cloud tiers only exist on RunPod: elsewhere the cloud list collapses to one option per
/// GPU (with a note when more than one tier was asked for). Hetzner creates CPU VMs from
/// `HETZNER_SERVER_TYPE`, so the GPU list collapses too — one option.
///
/// With a cap, an option is kept only when its per-pod price is known and within it; an
/// unknown price can't be checked, so it's dropped (and reported) rather than risked.
/// Without a cap nothing is dropped.
pub fn plan_options(req: &Request, provider: &str, prices: &PriceBook) -> OptionPlan {
    let mut notes = Vec::new();
    let tiered = provider == "runpod";
    let cpu_only = provider == "hetzner";
    let clouds: Vec<Option<String>> = if tiered {
        req.clouds.iter().cloned().map(Some).collect()
    } else {
        if req.clouds.len() > 1 {
            notes.push(format!(
                "{provider} has no cloud tiers — --cloud {} collapses to one option per GPU",
                req.clouds.join(",").to_lowercase()
            ));
        }
        vec![None]
    };
    let gpus: Vec<&String> = if cpu_only {
        if req.gpus.len() > 1 {
            notes.push("hetzner creates CPU VMs (HETZNER_SERVER_TYPE) — --gpu is ignored, so there is one option".into());
        }
        req.gpus.iter().take(1).collect()
    } else {
        req.gpus.iter().collect()
    };

    let mut all = Vec::new();
    for g in gpus {
        for c in &clouds {
            let q = prices.quote(g, c.as_deref());
            all.push(PlacementOption {
                listed: all.len(),
                gpu: g.clone(),
                label: if cpu_only { "CPU VM".to_string() } else { format!("{}×{}", req.gpu_count, gpu_short(g)) },
                cloud: c.clone(),
                gpu_count: req.gpu_count,
                price_per_gpu: q.price_per_gpu,
                price_per_pod: q.price_per_gpu.map(|p| p * f64::from(req.gpu_count.max(1))),
                price_source: q.source,
                stock: q.stock,
            });
        }
    }

    let (mut options, mut dropped) = (Vec::new(), Vec::new());
    for o in all {
        let reason = match (req.max_price, o.price_per_pod) {
            (None, _) => None,
            (Some(cap), Some(p)) if milli(p) > milli(cap) => {
                Some(format!("{} > cap {}/h per pod", o.price_label(), fmt_money("$", cap)))
            }
            (Some(_), Some(_)) => None,
            (Some(_), None) => Some("no known price, so --max-price can't be checked".to_string()),
        };
        match reason {
            Some(reason) => dropped.push(Dropped { option: o, reason }),
            None => options.push(o),
        }
    }
    if req.order == Order::Cheapest {
        options.sort_by_key(|o| {
            (o.price_per_pod.is_none(), o.price_per_pod.map(milli).unwrap_or(0), stock_rank(o.stock.as_deref()), o.listed)
        });
    }
    if options.iter().chain(dropped.iter().map(|d| &d.option)).any(|o| o.price_source == Some(PriceSource::Estimate)) {
        notes.push(format!(
            "~ = preset estimate (no live price for that GPU){}",
            if req.max_price.is_some() { "; --max-price is checked against it" } else { "" }
        ));
    }
    OptionPlan {
        provider: provider.to_string(),
        gpu_count: req.gpu_count,
        order: req.order,
        max_price: req.max_price,
        options,
        dropped,
        notes,
    }
}

/// The option table (`arena offers`, create/up dry-run and confirm), then what the cap
/// dropped and any notes. Pure, snapshot-tested.
pub fn render_plan(plan: &OptionPlan) -> String {
    use Align::{Left, Right};
    let mut out = String::new();
    if plan.options.is_empty() {
        out.push_str("(no option to try)\n");
    } else {
        let price = |o: &PlacementOption| match o.price_per_pod {
            Some(p) => format!("{}{}", o.tilde(), fmt_money("$", p)),
            None => "-".to_string(),
        };
        let source = |o: &PlacementOption| match o.price_source {
            Some(PriceSource::Live) => "live",
            Some(PriceSource::Estimate) => "estimate",
            None => "-",
        };
        let rows: Vec<Vec<String>> = plan
            .options
            .iter()
            .enumerate()
            .map(|(i, o)| {
                vec![
                    (i + 1).to_string(),
                    o.label.clone(),
                    o.cloud.clone().unwrap_or_else(|| "-".into()),
                    price(o),
                    source(o).to_string(),
                    o.stock.clone().unwrap_or_else(|| "-".into()),
                ]
            })
            .collect();
        out.push_str(&table::render(
            &["#", "OPTION", "CLOUD", "$/H/POD", "PRICE", "STOCK"],
            &[Right, Left, Left, Right, Left, Left],
            &rows,
        ));
    }
    if !plan.dropped.is_empty() {
        out.push_str("not tried (--max-price):\n");
        for d in &plan.dropped {
            out.push_str(&format!("  {}: {}\n", d.option.describe(), d.reason));
        }
    }
    for n in &plan.notes {
        out.push_str(&format!("note: {n}\n"));
    }
    out
}

/// What one create attempt came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Created { pod_id: String },
    /// No capacity on this option: blocked for the rest of the round.
    Capacity(String),
    /// This account may not use this option (RunPod v2's 403 on create: no access to the
    /// pool): skipped for the rest of the run — waiting won't grant access.
    Denied(String),
    /// Credentials rejected: the run aborts.
    Auth(String),
    /// Anything else: the run stops, as a single-option create does.
    Failed(String),
}

/// One create attempt in a name's log.
#[derive(Debug, Clone, PartialEq)]
pub struct Attempt {
    /// Index into [`OptionPlan::options`].
    pub option: usize,
    pub round: u32,
    /// Since the run started (tokio's clock, so paused-time tests see exact values).
    pub at: Duration,
    pub outcome: Outcome,
}

/// Everything that happened to one name — the summary table's row, and what a later
/// `up --check` (2.B) builds on.
#[derive(Debug, Clone, PartialEq)]
pub struct NameLog {
    pub name: String,
    /// Rounds in which this name's turn came up (0 = a stop came first).
    pub turns: u32,
    pub attempts: Vec<Attempt>,
    /// The option it was created on.
    pub placed: Option<usize>,
    /// Dropped between rounds without a create (it appeared on the fleet, or the top-up
    /// target was met), and why.
    pub skipped: Option<String>,
}

/// Why a run ended.
#[derive(Debug)]
pub enum End {
    /// Nothing left to create.
    Filled,
    /// One round (no retry window) left names unplaced: every option ran out of capacity.
    Exhausted,
    /// The retry window closed with names unplaced.
    WindowElapsed,
    /// Ctrl+C between rounds; what was made is kept.
    Interrupted,
    /// Credentials rejected creating `name` — or every option was refused (`Denied`), which
    /// reads as an access problem with the key rather than with any one pool.
    Aborted { name: String, error: Error },
    /// A non-capacity error creating `name` (or, with `name: None`, the re-list before a
    /// retry round failed — no round runs without knowing what exists).
    Failed { name: Option<String>, error: Error },
}

/// A finished run: per-name logs (in the order given), the pods made, and why it stopped.
#[derive(Debug)]
pub struct Placement {
    pub log: Vec<NameLog>,
    pub created: Vec<Pod>,
    pub rounds: u32,
    pub end: End,
}

/// The retry window (`--retry-mins` / `--retry-secs`). A zero window = one round.
#[derive(Debug, Clone, Copy)]
pub struct Rounds {
    pub window: Duration,
    pub every: Duration,
}

/// The `-n`/`-a` bound: the provider-scoped total (`provider` pods named `{prefix}-…`) a
/// top-up aims for. A retry round only fills what is still short of it.
#[derive(Debug, Clone)]
pub struct TopUp {
    pub target: usize,
    pub provider: String,
    pub prefix: String,
}

/// Before a retry round: which unplaced names still need a create, and which drop out (with
/// why). A name that now exists on the fleet drops — creating it would duplicate it. A
/// top-up is also cut to what the target still lacks, so pods made meanwhile by someone else
/// count. Pure.
pub fn still_needed(pending: &[String], fleet: &[Pod], topup: Option<&TopUp>) -> (Vec<String>, Vec<(String, String)>) {
    let mut keep = Vec::new();
    let mut dropped = Vec::new();
    for n in pending {
        if fleet.iter().any(|p| &p.name == n) {
            dropped.push((n.clone(), "it exists now".to_string()));
        } else {
            keep.push(n.clone());
        }
    }
    if let Some(t) = topup {
        let pre = format!("{}-", t.prefix);
        let have = fleet.iter().filter(|p| p.provider == t.provider && p.name.starts_with(&pre)).count();
        let room = t.target.saturating_sub(have);
        if keep.len() > room {
            for n in keep.split_off(room) {
                dropped.push((n, format!("target {} reached ({have} on {})", t.target, t.provider)));
            }
        }
    }
    (keep, dropped)
}

/// The spec for creating `name` on `option`: `base` (config + other overrides) with the
/// option's GPU, count and tier.
pub fn spec_for(base: &PodSpec, name: &str, option: &PlacementOption) -> PodSpec {
    let mut spec = base.clone();
    spec.name = name.to_string();
    spec.env.push(("MACHINE_NAME".into(), name.to_string()));
    spec.gpu_type = option.gpu.clone();
    spec.gpu_count = option.gpu_count;
    if let Some(c) = &option.cloud {
        spec.cloud_type = c.clone();
    }
    spec
}

/// A progress event from [`place`], for the caller to print.
#[derive(Debug)]
pub enum Progress<'a> {
    Attempt {
        name: &'a str,
        option: &'a PlacementOption,
        outcome: &'a Outcome,
        /// After a capacity error: another option is still open for this name this round.
        next: bool,
    },
    Waiting { round: u32, unplaced: usize, every: Duration },
    Skipped { name: &'a str, why: &'a str },
}

impl Progress<'_> {
    /// A pod was made — the line that belongs on stdout.
    pub fn is_created(&self) -> bool {
        matches!(self, Progress::Attempt { outcome: Outcome::Created { .. }, .. })
    }

    pub fn line(&self) -> String {
        match self {
            Progress::Attempt { name, option, outcome, next } => {
                let on = option.describe();
                match outcome {
                    Outcome::Created { pod_id } => {
                        format!("[created] {name} id={pod_id} on {on} ({})", option.price_label())
                    }
                    Outcome::Capacity(_) => format!(
                        "[no capacity] {name} on {on}{}",
                        if *next { " — trying the next option" } else { " — no option left this round" }
                    ),
                    Outcome::Denied(e) => format!(
                        "[no access] {name} on {on}: {e}{}",
                        if *next { " — skipping this option, trying the next" } else { " — skipping this option" }
                    ),
                    Outcome::Auth(e) => format!("[auth failed] {name} on {on}: {e}"),
                    Outcome::Failed(e) => format!("[failed] {name} on {on}: {e}"),
                }
            }
            Progress::Waiting { round, unplaced, every } => format!(
                "round {round}: {unplaced} name(s) not placed (no capacity); retrying in {}s (Ctrl+C to stop)…",
                every.as_secs()
            ),
            Progress::Skipped { name, why } => format!("skip {name} — {why}"),
        }
    }
}

/// Fill `names` from `plan.options`. Per name: try the options in order, **one create at a
/// time**; the first success places it (never a second create for that name). A capacity
/// error blocks the option for the rest of the round; a `Denied` one (no access to that
/// pool) for the rest of the run, and once *every* option is denied the run aborts — that's
/// the key, not the pools. Auth aborts the run; any other error stops it (the single-option
/// create does the same — a bad request isn't something the next option fixes reliably,
/// and stopping keeps a misconfiguration from fanning out). Transient errors
/// (429/5xx/connect) are retried with backoff inside one attempt.
///
/// With a retry window, unplaced names get another round every `rounds.every` while that
/// round would still start inside the window; capacity blocks are cleared each round, and
/// the fleet is re-listed first ([`still_needed`]) — a failed list ends the run rather than
/// risk a duplicate. `interrupt` makes a fresh "stop" future per wait (Ctrl+C in the CLI): it
/// ends the run between rounds, keeping what was made. The option order is the confirmed
/// plan's — fixed for the run.
pub async fn place<W, F>(
    provider: &dyn Provider,
    base: &PodSpec,
    names: &[String],
    plan: &OptionPlan,
    rounds: Rounds,
    topup: Option<&TopUp>,
    mut interrupt: W,
    on_progress: &mut (dyn FnMut(&Progress) + Send),
) -> Placement
where
    W: FnMut() -> F,
    F: Future<Output = ()>,
{
    let policy = RetryPolicy::default();
    let start = tokio::time::Instant::now();
    let deadline = start + rounds.window;
    let mut log: Vec<NameLog> =
        names.iter().map(|n| NameLog { name: n.clone(), turns: 0, attempts: Vec::new(), placed: None, skipped: None }).collect();
    let mut created = Vec::new();
    let mut pending: Vec<usize> = (0..log.len()).collect();
    // Options this account can't use: unlike capacity blocks, never cleared.
    let mut denied = vec![false; plan.options.len()];
    let mut round = 0u32;
    let end = 'run: loop {
        if pending.is_empty() {
            break End::Filled;
        }
        round += 1;
        let mut blocked = vec![false; plan.options.len()];
        let mut unplaced = Vec::new();
        for &i in &pending {
            log[i].turns += 1;
            let name = log[i].name.clone();
            let mut placed = false;
            for (k, option) in plan.options.iter().enumerate() {
                if blocked[k] || denied[k] {
                    continue;
                }
                let spec = spec_for(base, &name, option);
                let result = retrying(&policy, || provider.create_pod(&spec)).await;
                let at = start.elapsed();
                let (outcome, stop) = match result {
                    Ok(pod) => {
                        let outcome = Outcome::Created { pod_id: pod.id.clone() };
                        created.push(pod);
                        placed = true;
                        (outcome, None)
                    }
                    Err(e) => match e.kind() {
                        Some(ProviderErrorKind::Capacity) => {
                            blocked[k] = true;
                            (Outcome::Capacity(e.to_string()), None)
                        }
                        Some(ProviderErrorKind::Denied) => {
                            denied[k] = true;
                            let outcome = Outcome::Denied(e.to_string());
                            if denied.iter().all(|d| *d) {
                                let error = Error::Provider {
                                    kind: ProviderErrorKind::Denied,
                                    message: format!(
                                        "every option was refused (no access) — the API key may lack \
                                         permission to create pods, or the account can't use any of these \
                                         pools; last: {e}"
                                    ),
                                };
                                (outcome, Some(End::Aborted { name: name.clone(), error }))
                            } else {
                                (outcome, None)
                            }
                        }
                        Some(ProviderErrorKind::Auth) => {
                            (Outcome::Auth(e.to_string()), Some(End::Aborted { name: name.clone(), error: e }))
                        }
                        _ => (Outcome::Failed(e.to_string()), Some(End::Failed { name: Some(name.clone()), error: e })),
                    },
                };
                let next = (k + 1..plan.options.len()).any(|j| !blocked[j] && !denied[j]);
                on_progress(&Progress::Attempt { name: &name, option, outcome: &outcome, next });
                log[i].attempts.push(Attempt { option: k, round, at, outcome });
                if placed {
                    log[i].placed = Some(k);
                    break;
                }
                if let Some(end) = stop {
                    break 'run end;
                }
            }
            if !placed {
                unplaced.push(i);
            }
        }
        pending = unplaced;
        if pending.is_empty() {
            break End::Filled;
        }
        // Nothing to try (a cap ruled every option out) can't get better by waiting.
        if rounds.window.is_zero() || plan.options.is_empty() {
            break End::Exhausted;
        }
        // A round starts only inside the window. The next one would start `every` from now,
        // so if that's past the deadline, end here — not sleep just to give up, nor create
        // (bill) up to `every` after the window the operator agreed to. (Decided before the
        // sleep, so timer slack can't flip it.)
        let now = tokio::time::Instant::now();
        if now >= deadline || now + rounds.every > deadline {
            break End::WindowElapsed;
        }
        on_progress(&Progress::Waiting { round, unplaced: pending.len(), every: rounds.every });
        tokio::select! {
            _ = tokio::time::sleep(rounds.every) => {}
            _ = interrupt() => break End::Interrupted,
        }
        match retrying(&policy, || provider.list_pods()).await {
            Ok(fleet) => {
                let names: Vec<String> = pending.iter().map(|&i| log[i].name.clone()).collect();
                let (keep, dropped) = still_needed(&names, &fleet, topup);
                for (name, why) in dropped {
                    on_progress(&Progress::Skipped { name: &name, why: &why });
                    if let Some(l) = log.iter_mut().find(|l| l.name == name) {
                        l.skipped = Some(why);
                    }
                }
                pending.retain(|&i| keep.contains(&log[i].name));
            }
            Err(e) => {
                let error = Error::provider(format!(
                    "listing pods before retry round {} (refusing to create — a failed list could duplicate pods): {e}",
                    round + 1
                ));
                break End::Failed { name: None, error };
            }
        }
    };
    Placement { log, created, rounds: round, end }
}

/// The end-of-run table: each name → where it was created, or why not. Pure.
pub fn render_summary(plan: &OptionPlan, run: &Placement) -> String {
    let tried = |l: &NameLog| -> String {
        // Group by option, first-tried first: `1×RTX A4000 COMMUNITY: capacity ×2`.
        let mut seen: Vec<(usize, &'static str, usize)> = Vec::new();
        for a in &l.attempts {
            let what = match a.outcome {
                Outcome::Created { .. } => continue,
                Outcome::Capacity(_) => "capacity",
                Outcome::Denied(_) => "no access",
                Outcome::Auth(_) => "auth failed",
                Outcome::Failed(_) => "failed",
            };
            match seen.iter_mut().find(|(o, w, _)| *o == a.option && *w == what) {
                Some(s) => s.2 += 1,
                None => seen.push((a.option, what, 1)),
            }
        }
        seen.iter()
            .map(|(o, what, n)| {
                let times = if *n > 1 { format!(" ×{n}") } else { String::new() };
                format!("{}: {what}{times}", plan.options[*o].describe())
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    let any_denied = run.log.iter().flat_map(|l| &l.attempts).any(|a| matches!(a.outcome, Outcome::Denied(_)));
    let rows: Vec<Vec<String>> = run
        .log
        .iter()
        .map(|l| {
            let result = match (l.placed, &l.skipped) {
                (Some(k), _) => {
                    let o = &plan.options[k];
                    let before = tried(l);
                    let after = if before.is_empty() { String::new() } else { format!(" (after {before})") };
                    format!("created on {} ({}){after}", o.describe(), o.price_label())
                }
                (None, Some(why)) => format!("skipped ({why})"),
                (None, None) if l.turns == 0 => "not attempted (run stopped)".to_string(),
                (None, None) if l.attempts.is_empty() && any_denied => {
                    "not placed (every option was out of capacity or refused before its turn)".to_string()
                }
                (None, None) if l.attempts.is_empty() => {
                    "not placed (every option ran out of capacity before its turn)".to_string()
                }
                (None, None) => format!("not placed (tried: {})", tried(l)),
            };
            vec![l.name.clone(), result]
        })
        .collect();
    table::render(&["NAME", "PLACEMENT"], &[Align::Left, Align::Left], &rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;

    const A4000: &str = "NVIDIA RTX A4000";
    const R3090: &str = "NVIDIA GeForce RTX 3090";
    const ADA: &str = "NVIDIA RTX 4000 Ada Generation";

    fn base() -> PodSpec {
        PodSpec {
            name: String::new(),
            image: "img:1".into(),
            gpu_type: A4000.into(),
            gpu_count: 1,
            cloud_type: "COMMUNITY".into(),
            disk_gb: 50,
            volume_gb: 0,
            ports: "22/tcp".into(),
            env: Vec::new(),
            docker_args: None,
            allowed_cuda: Vec::new(),
        }
    }

    fn live(id: &str, comm: Option<f64>, sec: Option<f64>, stock: Option<&str>) -> GpuType {
        GpuType {
            id: id.into(),
            display_name: String::new(),
            memory_gb: 16,
            community_price: comm,
            secure_price: sec,
            stock_status: stock.map(String::from),
        }
    }

    fn req(gpus: &[&str], clouds: &[&str], count: u32, cap: Option<f64>, order: Order) -> Request {
        Request {
            gpus: gpus.iter().map(|s| s.to_string()).collect(),
            clouds: clouds.iter().map(|s| s.to_string()).collect(),
            gpu_count: count,
            max_price: cap,
            order,
        }
    }

    /// `(gpu label, cloud)` in plan order — what the ordering tests compare.
    fn order_of(plan: &OptionPlan) -> Vec<(String, Option<String>)> {
        plan.options.iter().map(|o| (o.label.clone(), o.cloud.clone())).collect()
    }

    fn opt(label: &str, cloud: Option<&str>) -> (String, Option<String>) {
        (label.to_string(), cloud.map(String::from))
    }

    #[test]
    fn flags_resolve_dedupe_and_default_from_config() {
        let b = base();
        let r = Request::from_flags(Some("A4000, 4000Ada ,3090,a4000,"), Some("community,SECURE"), &b, None, Order::Cheapest)
            .unwrap();
        assert_eq!(r.gpus, [A4000, ADA, R3090]);
        assert_eq!(r.clouds, ["COMMUNITY", "SECURE"]);
        assert!(!r.is_single());
        // Absent flags = config's GPU and tier; that alone is the single (legacy) path…
        let single = Request::from_flags(None, None, &b, None, Order::Cheapest).unwrap();
        assert_eq!((single.gpus.clone(), single.clouds.clone()), (vec![A4000.to_string()], vec!["COMMUNITY".to_string()]));
        assert!(single.is_single());
        assert!(Request::from_flags(Some("A4000"), Some("community"), &b, None, Order::Listed).unwrap().is_single());
        // …but a price cap needs prices, so even one option goes through placement.
        assert!(!Request::from_flags(Some("A4000"), None, &b, Some(0.3), Order::Cheapest).unwrap().is_single());
        for (gpu, cloud, cap) in [(Some(" , "), None, None), (None, Some(","), None), (None, None, Some(0.0)), (None, None, Some(f64::NAN))] {
            assert!(Request::from_flags(gpu, cloud, &b, cap, Order::Cheapest).is_err(), "{gpu:?} {cloud:?} {cap:?}");
        }
        assert_eq!("Listed".parse::<Order>().unwrap(), Order::Listed);
        assert!("fastest".parse::<Order>().is_err());
    }

    #[test]
    fn validate_rejects_unknown_runpod_tiers_and_missing_gpu() {
        let r = req(&[A4000], &["COMMUNITY", "ALL"], 1, None, Order::Cheapest);
        assert!(r.validate_for("runpod").unwrap_err().to_string().contains("`ALL`"));
        assert!(r.validate_for("vast").is_ok()); // no tiers there: collapsed, not sent
        let none = req(&[""], &["COMMUNITY"], 1, None, Order::Cheapest);
        assert!(none.validate_for("runpod").unwrap_err().to_string().contains("GPU_TYPE"));
        assert!(none.validate_for("hetzner").is_ok());
    }

    #[test]
    fn quote_prefers_live_then_estimate_and_takes_stock_per_tier() {
        let book = PriceBook::runpod(vec![
            (Some("COMMUNITY".into()), vec![live(A4000, Some(0.17), Some(0.25), Some("Low")), live("NVIDIA H100 80GB HBM3", None, Some(2.69), Some("None"))]),
            (Some("SECURE".into()), vec![live(A4000, Some(0.17), Some(0.25), Some("High"))]),
        ]);
        let q = book.quote(A4000, Some("SECURE"));
        assert_eq!(q, Quote { price_per_gpu: Some(0.25), source: Some(PriceSource::Live), stock: Some("High".into()) });
        assert_eq!(book.quote(A4000, Some("COMMUNITY")).stock.as_deref(), Some("Low"));
        // A live secure-only GPU is not back-filled with the preset community price.
        let h100 = book.quote("NVIDIA H100 80GB HBM3", Some("COMMUNITY"));
        assert_eq!((h100.price_per_gpu, h100.source), (None, Some(PriceSource::Live)));
        // No live price at all: the preset estimate, marked as such.
        let est = book.quote(R3090, Some("COMMUNITY"));
        assert_eq!((est.price_per_gpu, est.source, est.stock), (Some(0.22), Some(PriceSource::Estimate), None));
        assert_eq!(book.quote("NVIDIA Mystery", Some("COMMUNITY")), Quote::default());
        // GraphQL's stock isn't per tier: it describes both.
        let v1 = PriceBook::runpod(vec![(None, vec![live(A4000, Some(0.17), Some(0.25), Some("Medium"))])]);
        assert_eq!(v1.quote(A4000, Some("SECURE")).stock.as_deref(), Some("Medium"));
        // Off RunPod nothing is priced — the presets are RunPod's prices.
        assert_eq!(PriceBook::unpriced().quote(A4000, None), Quote::default());
    }

    /// Two A4000 tiers + a 3090 + a 4000 Ada, with 3090 tied with the 4000 Ada.
    fn book() -> PriceBook {
        PriceBook::runpod(vec![(
            None,
            vec![
                live(A4000, Some(0.17), Some(0.25), None),
                live(ADA, Some(0.22), Some(0.32), None),
                live(R3090, Some(0.22), Some(0.43), None),
            ],
        )])
    }

    #[test]
    fn cheapest_orders_by_price_and_ties_keep_the_listed_order() {
        let plan = plan_options(&req(&[R3090, ADA, A4000], &["COMMUNITY", "SECURE"], 1, None, Order::Cheapest), "runpod", &book());
        assert_eq!(
            order_of(&plan),
            [
                opt("1×RTX A4000", Some("COMMUNITY")),   // 0.17
                opt("1×RTX 3090", Some("COMMUNITY")),    // 0.22, listed before the Ada
                opt("1×RTX 4000 Ada", Some("COMMUNITY")), // 0.22
                opt("1×RTX A4000", Some("SECURE")),      // 0.25
                opt("1×RTX 4000 Ada", Some("SECURE")),   // 0.32
                opt("1×RTX 3090", Some("SECURE")),       // 0.43
            ]
        );
        // Listed the other way round, the tie flips with it.
        let flipped = plan_options(&req(&[ADA, R3090], &["COMMUNITY"], 1, None, Order::Cheapest), "runpod", &book());
        assert_eq!(order_of(&flipped), [opt("1×RTX 4000 Ada", Some("COMMUNITY")), opt("1×RTX 3090", Some("COMMUNITY"))]);
    }

    #[test]
    fn listed_order_is_gpu_major_and_ignores_price() {
        let plan = plan_options(&req(&[R3090, A4000], &["SECURE", "COMMUNITY"], 1, None, Order::Listed), "runpod", &book());
        assert_eq!(
            order_of(&plan),
            [
                opt("1×RTX 3090", Some("SECURE")),
                opt("1×RTX 3090", Some("COMMUNITY")),
                opt("1×RTX A4000", Some("SECURE")),
                opt("1×RTX A4000", Some("COMMUNITY")),
            ]
        );
        assert_eq!(plan.options.iter().map(|o| o.listed).collect::<Vec<_>>(), [0, 1, 2, 3]);
    }

    #[test]
    fn stock_only_breaks_price_ties_and_never_filters() {
        let book = PriceBook::runpod(vec![(
            None,
            vec![
                live(A4000, Some(0.30), None, Some("High")),
                live(R3090, Some(0.22), None, Some("None")), // cheapest but "no stock": still first, still kept
                live(ADA, Some(0.30), None, Some("Low")),
                live("NVIDIA RTX A5000", Some(0.30), None, None),
            ],
        )]);
        let plan = plan_options(
            &req(&[ADA, "NVIDIA RTX A5000", R3090, A4000], &["COMMUNITY"], 1, None, Order::Cheapest),
            "runpod",
            &book,
        );
        let labels: Vec<String> = plan.options.iter().map(|o| o.label.clone()).collect();
        // 0.22 first despite "None"; then the 0.30 tie by stock: High, Low, unreported.
        assert_eq!(labels, ["1×RTX 3090", "1×RTX A4000", "1×RTX 4000 Ada", "1×RTX A5000"]);
        assert!(plan.dropped.is_empty());
    }

    #[test]
    fn price_cap_is_per_pod_and_drops_unknown_prices() {
        // 2 GPUs per pod: A4000 0.34 fits a 0.40 cap, 3090 0.44 doesn't, the unknown can't be checked.
        let r = req(&[A4000, R3090, "NVIDIA Mystery"], &["COMMUNITY"], 2, Some(0.40), Order::Cheapest);
        let plan = plan_options(&r, "runpod", &book());
        assert_eq!(order_of(&plan), [opt("2×RTX A4000", Some("COMMUNITY"))]);
        assert_eq!(plan.options[0].price_per_pod.map(milli), Some(340));
        let why: Vec<(&str, &str)> = plan.dropped.iter().map(|d| (d.option.label.as_str(), d.reason.as_str())).collect();
        assert_eq!(
            why,
            [
                ("2×RTX 3090", "$0.44/h > cap $0.40/h per pod"),
                ("2×Mystery", "no known price, so --max-price can't be checked"),
            ]
        );
        // Without a cap nothing is dropped; the unpriced option goes last.
        let open = plan_options(&req(&["NVIDIA Mystery", R3090, A4000], &["COMMUNITY"], 2, None, Order::Cheapest), "runpod", &book());
        assert!(open.dropped.is_empty());
        assert_eq!(open.options.last().unwrap().label, "2×Mystery");
        // Exactly at the cap is within it, float noise included (0.17 × 3 = 0.51000000000000001).
        let at = plan_options(&req(&[A4000], &["COMMUNITY"], 3, Some(0.51), Order::Cheapest), "runpod", &book());
        assert_eq!(at.options.len(), 1, "{:?}", at.dropped);
    }

    #[test]
    fn estimates_are_marked_and_count_against_the_cap() {
        // No live catalog: every known GPU priced from the presets.
        let plan = plan_options(&req(&[A4000, "NVIDIA A40"], &["COMMUNITY"], 1, Some(0.30), Order::Cheapest), "runpod", &PriceBook::runpod(vec![]));
        assert_eq!(plan.options.len(), 1);
        assert_eq!(plan.options[0].price_source, Some(PriceSource::Estimate));
        assert_eq!(plan.options[0].price_label(), "~$0.17/h");
        assert_eq!(plan.dropped[0].reason, "~$0.39/h > cap $0.30/h per pod");
        assert!(plan.notes.iter().any(|n| n.contains("checked against it")), "{:?}", plan.notes);
    }

    #[test]
    fn tiers_collapse_off_runpod_and_gpus_collapse_on_hetzner() {
        let r = req(&[A4000, R3090], &["COMMUNITY", "SECURE"], 1, None, Order::Cheapest);
        let vast = plan_options(&r, "vast", &PriceBook::unpriced());
        assert_eq!(order_of(&vast), [opt("1×RTX A4000", None), opt("1×RTX 3090", None)]); // unpriced: listed order
        assert!(vast.notes[0].contains("vast has no cloud tiers"), "{:?}", vast.notes);
        let hz = plan_options(&r, "hetzner", &PriceBook::unpriced());
        assert_eq!(order_of(&hz), [opt("CPU VM", None)]);
        assert_eq!(hz.notes.len(), 2, "{:?}", hz.notes);
        // A cap leaves nothing to try on an unpriced provider — reported, not risked.
        let capped = plan_options(&req(&[A4000], &["COMMUNITY"], 1, Some(1.0), Order::Cheapest), "vast", &PriceBook::unpriced());
        assert!(capped.options.is_empty());
        assert_eq!(capped.dropped.len(), 1);
    }

    #[test]
    fn plan_table_and_json_snapshot() {
        let r = req(&[A4000, "NVIDIA A40"], &["COMMUNITY", "SECURE"], 1, Some(0.40), Order::Cheapest);
        let catalog = vec![live(A4000, Some(0.17), Some(0.25), Some("Low"))];
        let plan = plan_options(&r, "runpod", &PriceBook::runpod(vec![(None, catalog)]));
        let want = "\
#  OPTION       CLOUD      $/H/POD  PRICE     STOCK
1  1×RTX A4000  COMMUNITY    $0.17  live      Low
2  1×RTX A4000  SECURE       $0.25  live      Low
3  1×A40        COMMUNITY   ~$0.39  estimate  -
not tried (--max-price):
  1×A40 SECURE: ~$0.47/h > cap $0.40/h per pod
note: ~ = preset estimate (no live price for that GPU); --max-price is checked against it
";
        let out = render_plan(&plan);
        assert_eq!(out, want, "\n--- got ---\n{out}");
        let v = serde_json::to_value(&plan).unwrap();
        assert_eq!(v["order"], "cheapest");
        assert_eq!(v["options"][0]["price_source"], "live");
        assert_eq!(v["options"][2]["price_source"], "estimate");
        assert_eq!(v["dropped"][0]["cloud"], "SECURE"); // the option's fields, flattened
        assert!(v["dropped"][0]["reason"].as_str().unwrap().contains("cap"));
        let back: OptionPlan = serde_json::from_value(v).unwrap();
        assert_eq!(back, plan);
    }

    // ---- executor -------------------------------------------------------------------

    #[derive(Debug, Clone, Copy)]
    enum Reply {
        Ok,
        Capacity,
        /// RunPod v2's create 403: no access to this pool.
        Denied,
        Auth,
        Bad,
    }

    /// A provider whose create answers per (gpu, cloud) from a script (then `fallback`), and
    /// which records every create and lists what it made (plus `fleet`).
    struct Fake {
        script: Mutex<HashMap<(String, String), VecDeque<Reply>>>,
        fallback: Reply,
        calls: Mutex<Vec<(String, String, String, u32)>>,
        fleet: Mutex<Vec<Pod>>,
        list_fails: bool,
    }

    impl Fake {
        fn new(fallback: Reply) -> Self {
            Fake { script: Default::default(), fallback, calls: Default::default(), fleet: Default::default(), list_fails: false }
        }
        fn script(self, gpu: &str, cloud: &str, replies: &[Reply]) -> Self {
            self.script.lock().unwrap().insert((gpu.into(), cloud.into()), replies.iter().copied().collect());
            self
        }
        /// `(name, gpu, cloud)` per create call.
        fn calls(&self) -> Vec<(String, String, String)> {
            self.calls.lock().unwrap().iter().map(|(n, g, c, _)| (n.clone(), g.clone(), c.clone())).collect()
        }
    }

    #[async_trait]
    impl Provider for Fake {
        fn name(&self) -> &'static str {
            "runpod"
        }
        fn describe(&self, _spec: &PodSpec) -> String {
            String::new()
        }
        async fn list_pods(&self) -> Result<Vec<Pod>> {
            if self.list_fails {
                return Err(Error::provider("list HTTP 400: nope"));
            }
            Ok(self.fleet.lock().unwrap().clone())
        }
        async fn create_pod(&self, spec: &PodSpec) -> Result<Pod> {
            self.calls.lock().unwrap().push((spec.name.clone(), spec.gpu_type.clone(), spec.cloud_type.clone(), spec.gpu_count));
            let key = (spec.gpu_type.clone(), spec.cloud_type.clone());
            let reply = self.script.lock().unwrap().get_mut(&key).and_then(VecDeque::pop_front).unwrap_or(self.fallback);
            match reply {
                Reply::Ok => {
                    let mut fleet = self.fleet.lock().unwrap();
                    let pod = Pod {
                        id: format!("id{}", fleet.len() + 1),
                        name: spec.name.clone(),
                        provider: "runpod".into(),
                        status: "RUNNING".into(),
                        ..Default::default()
                    };
                    fleet.push(pod.clone());
                    Ok(pod)
                }
                Reply::Capacity => Err(Error::capacity("create pod HTTP 500: There are no instances currently available")),
                Reply::Denied => Err(Error::Provider {
                    kind: ProviderErrorKind::Denied,
                    message: "create pod HTTP 403 Forbidden: your account cannot access the requested pool".into(),
                }),
                Reply::Auth => Err(Error::Provider { kind: ProviderErrorKind::Auth, message: "create pod HTTP 401".into() }),
                Reply::Bad => Err(Error::provider("create pod HTTP 400: bad image")),
            }
        }
        async fn stop_pod(&self, _id: &str) -> Result<()> {
            Ok(())
        }
        async fn restart_pod(&self, _id: &str) -> Result<()> {
            Ok(())
        }
        async fn terminate_pod(&self, _id: &str) -> Result<()> {
            Ok(())
        }
    }

    const ONE_ROUND: Rounds = Rounds { window: Duration::ZERO, every: Duration::from_secs(60) };

    /// A4000 COMMUNITY (0.17) → 3090 COMMUNITY (0.22) → A4000 SECURE (0.25), cheapest first.
    fn three() -> OptionPlan {
        plan_options(&req(&[A4000, R3090], &["COMMUNITY", "SECURE"], 1, Some(0.30), Order::Cheapest), "runpod", &book())
    }

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    fn call(name: &str, gpu: &str, cloud: &str) -> (String, String, String) {
        (name.into(), gpu.into(), cloud.into())
    }

    async fn run(fake: &Fake, plan: &OptionPlan, n: &[&str], rounds: Rounds, topup: Option<&TopUp>) -> (Placement, Vec<String>) {
        let mut lines = Vec::new();
        let mut sink = |p: &Progress| lines.push(p.line());
        let out = place(fake, &base(), &names(n), plan, rounds, topup, std::future::pending::<()>, &mut sink).await;
        (out, lines)
    }

    #[tokio::test]
    async fn first_option_dry_falls_through_to_the_second() {
        let fake = Fake::new(Reply::Ok).script(A4000, "COMMUNITY", &[Reply::Capacity]);
        let plan = three();
        let (out, lines) = run(&fake, &plan, &["arena8-apple"], ONE_ROUND, None).await;
        assert!(matches!(out.end, End::Filled), "{:?}", out.end);
        assert_eq!(fake.calls(), [call("arena8-apple", A4000, "COMMUNITY"), call("arena8-apple", R3090, "COMMUNITY")]);
        assert_eq!(out.log[0].placed, Some(1));
        assert_eq!(out.created.len(), 1);
        assert_eq!(lines[0], "[no capacity] arena8-apple on 1×RTX A4000 COMMUNITY — trying the next option");
        assert_eq!(lines[1], "[created] arena8-apple id=id1 on 1×RTX 3090 COMMUNITY ($0.22/h)");
    }

    #[tokio::test]
    async fn a_dry_option_is_skipped_by_later_names_in_the_round() {
        // A4000 COMMUNITY is dry: apple discovers it; bloom must not spend a create on it.
        let fake = Fake::new(Reply::Ok).script(A4000, "COMMUNITY", &[Reply::Capacity, Reply::Ok]);
        let (out, _) = run(&fake, &three(), &["arena8-apple", "arena8-bloom"], ONE_ROUND, None).await;
        assert!(matches!(out.end, End::Filled));
        assert_eq!(
            fake.calls(),
            [
                call("arena8-apple", A4000, "COMMUNITY"),
                call("arena8-apple", R3090, "COMMUNITY"),
                call("arena8-bloom", R3090, "COMMUNITY"),
            ]
        );
    }

    #[tokio::test]
    async fn every_option_dry_ends_the_round_without_more_calls() {
        let fake = Fake::new(Reply::Capacity);
        let plan = three();
        let (out, lines) = run(&fake, &plan, &["arena8-apple", "arena8-bloom"], ONE_ROUND, None).await;
        assert!(matches!(out.end, End::Exhausted), "{:?}", out.end);
        assert_eq!(fake.calls().len(), 3, "each option tried once, by the first name only");
        assert!(out.created.is_empty());
        assert!(lines[2].ends_with("— no option left this round"), "{lines:?}");
        let summary = render_summary(&plan, &out);
        assert!(summary.contains("arena8-apple  not placed (tried: 1×RTX A4000 COMMUNITY: capacity; 1×RTX 3090 COMMUNITY: capacity; 1×RTX A4000 SECURE: capacity)"), "{summary}");
        assert!(summary.contains("arena8-bloom  not placed (every option ran out of capacity before its turn)"), "{summary}");
    }

    #[tokio::test]
    async fn stops_at_the_target_and_never_creates_a_name_twice() {
        let fake = Fake::new(Reply::Ok);
        let (out, _) = run(&fake, &three(), &["arena8-apple", "arena8-bloom", "arena8-cider"], ONE_ROUND, None).await;
        assert!(matches!(out.end, End::Filled));
        // Plenty of capacity: one create per name, all on the cheapest option, nothing extra.
        assert_eq!(
            fake.calls(),
            [
                call("arena8-apple", A4000, "COMMUNITY"),
                call("arena8-bloom", A4000, "COMMUNITY"),
                call("arena8-cider", A4000, "COMMUNITY"),
            ]
        );
        assert_eq!(out.rounds, 1);
    }

    #[tokio::test]
    async fn auth_aborts_the_whole_run_keeping_what_was_made() {
        let fake = Fake::new(Reply::Ok).script(A4000, "COMMUNITY", &[Reply::Ok, Reply::Auth]);
        let plan = three();
        let (out, _) = run(&fake, &plan, &["arena8-apple", "arena8-bloom", "arena8-cider"], ONE_ROUND, None).await;
        match &out.end {
            End::Aborted { name, error } => {
                assert_eq!(name, "arena8-bloom");
                assert_eq!(error.kind(), Some(ProviderErrorKind::Auth));
            }
            other => panic!("expected abort, got {other:?}"),
        }
        assert_eq!(fake.calls().len(), 2, "no next option, no next name after an auth failure");
        assert_eq!(out.created.len(), 1);
        assert!(render_summary(&plan, &out).contains("arena8-cider  not attempted (run stopped)"));
    }

    /// Review fix: RunPod v2 documents a create 403 as "no access to the requested pool — skip
    /// this candidate, keep going". One restricted pool must not abort a multi-option run.
    #[tokio::test(start_paused = true)]
    async fn a_pool_without_access_is_skipped_for_the_whole_run() {
        // A4000 COMMUNITY is refused; 3090 COMMUNITY is dry in round 1, then fills.
        let fake = Fake::new(Reply::Ok)
            .script(A4000, "COMMUNITY", &[Reply::Denied])
            .script(R3090, "COMMUNITY", &[Reply::Capacity])
            .script(A4000, "SECURE", &[Reply::Capacity]);
        let plan = three();
        let rounds = Rounds { window: Duration::from_secs(600), every: Duration::from_secs(60) };
        let (out, lines) = run(&fake, &plan, &["arena8-apple", "arena8-bloom"], rounds, None).await;
        assert!(matches!(out.end, End::Filled), "{:?}", out.end);
        assert_eq!(
            fake.calls(),
            [
                call("arena8-apple", A4000, "COMMUNITY"), // 403: skipped from now on
                call("arena8-apple", R3090, "COMMUNITY"), // dry
                call("arena8-apple", A4000, "SECURE"),    // dry — round 1 is over for everyone
                // Round 2: capacity blocks are cleared, the refused pool stays skipped.
                call("arena8-apple", R3090, "COMMUNITY"),
                call("arena8-bloom", R3090, "COMMUNITY"),
            ]
        );
        assert!(lines[0].starts_with("[no access] arena8-apple on 1×RTX A4000 COMMUNITY:"), "{lines:?}");
        assert!(lines[0].ends_with("— skipping this option, trying the next"), "{lines:?}");
        let summary = render_summary(&plan, &out);
        assert!(summary.contains("(after 1×RTX A4000 COMMUNITY: no access; 1×RTX 3090 COMMUNITY: capacity;"), "{summary}");
    }

    #[tokio::test]
    async fn every_option_refused_aborts_as_an_access_problem() {
        // A key that may not create at all 403s everywhere: once no option is left that
        // wasn't refused, stop — later names would only collect the same 403s.
        let fake = Fake::new(Reply::Denied);
        let plan = three();
        let rounds = Rounds { window: Duration::from_secs(600), every: Duration::from_secs(60) };
        let (out, lines) = run(&fake, &plan, &["arena8-apple", "arena8-bloom"], rounds, None).await;
        match &out.end {
            End::Aborted { name, error } => {
                assert_eq!(name, "arena8-apple");
                assert_eq!(error.kind(), Some(ProviderErrorKind::Denied));
                assert!(error.to_string().contains("every option was refused"), "{error}");
            }
            other => panic!("expected abort, got {other:?}"),
        }
        assert_eq!(fake.calls().len(), 3, "each option once, then stop");
        assert!(lines[2].ends_with("— skipping this option"), "{lines:?}");
        assert!(render_summary(&plan, &out).contains("arena8-bloom  not attempted (run stopped)"));

        // A refusal for a later name (after others were placed) still only skips that option.
        let fake = Fake::new(Reply::Ok).script(A4000, "COMMUNITY", &[Reply::Ok, Reply::Denied]);
        let (out, _) = run(&fake, &plan, &["arena8-apple", "arena8-bloom", "arena8-cider"], ONE_ROUND, None).await;
        assert!(matches!(out.end, End::Filled), "{:?}", out.end);
        assert_eq!(fake.calls()[2..], [call("arena8-bloom", R3090, "COMMUNITY"), call("arena8-cider", R3090, "COMMUNITY")]);
    }

    #[tokio::test]
    async fn another_error_stops_like_a_single_create() {
        let fake = Fake::new(Reply::Ok).script(A4000, "COMMUNITY", &[Reply::Bad]);
        let (out, _) = run(&fake, &three(), &["arena8-apple", "arena8-bloom"], ONE_ROUND, None).await;
        assert!(matches!(&out.end, End::Failed { name: Some(n), .. } if n == "arena8-apple"), "{:?}", out.end);
        assert_eq!(fake.calls().len(), 1);
    }

    #[tokio::test]
    async fn no_option_to_try_ends_at_once_even_with_a_retry_window() {
        let empty = plan_options(&req(&[A4000], &["COMMUNITY"], 1, Some(0.01), Order::Cheapest), "runpod", &book());
        assert!(empty.options.is_empty());
        let fake = Fake::new(Reply::Ok);
        let rounds = Rounds { window: Duration::from_secs(3600), every: Duration::from_secs(60) };
        let (out, _) = run(&fake, &empty, &["arena8-apple"], rounds, None).await;
        assert!(matches!(out.end, End::Exhausted), "{:?}", out.end);
        assert!(fake.calls().is_empty());
        assert_eq!(out.rounds, 1);
    }

    #[tokio::test]
    async fn one_option_behaves_like_the_single_create_path() {
        // The single-spec path: name after name until capacity runs out, then stop.
        let plan = plan_options(&req(&[A4000], &["COMMUNITY"], 1, Some(1.0), Order::Cheapest), "runpod", &book());
        let fake = Fake::new(Reply::Ok).script(A4000, "COMMUNITY", &[Reply::Ok, Reply::Capacity]);
        let (out, _) = run(&fake, &plan, &["arena8-apple", "arena8-bloom", "arena8-cider"], ONE_ROUND, None).await;
        assert!(matches!(out.end, End::Exhausted));
        assert_eq!(fake.calls().len(), 2);
        assert_eq!(out.created.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_retry_round_clears_blocks_and_fills_later() {
        // Round 1: every option dry. Round 2 (60s later): the cheapest has room again.
        let fake = Fake::new(Reply::Ok)
            .script(A4000, "COMMUNITY", &[Reply::Capacity])
            .script(R3090, "COMMUNITY", &[Reply::Capacity])
            .script(A4000, "SECURE", &[Reply::Capacity]);
        let rounds = Rounds { window: Duration::from_secs(300), every: Duration::from_secs(60) };
        let plan = three();
        let (out, lines) = run(&fake, &plan, &["arena8-apple", "arena8-bloom"], rounds, None).await;
        assert!(matches!(out.end, End::Filled), "{:?}", out.end);
        assert_eq!(out.rounds, 2);
        assert_eq!(&fake.calls()[3..], [call("arena8-apple", A4000, "COMMUNITY"), call("arena8-bloom", A4000, "COMMUNITY")]);
        assert!(lines.contains(&"round 1: 2 name(s) not placed (no capacity); retrying in 60s (Ctrl+C to stop)…".to_string()));
        let apple = &out.log[0];
        assert_eq!(apple.attempts.iter().map(|a| (a.round, a.at)).collect::<Vec<_>>(), [
            (1, Duration::ZERO),
            (1, Duration::ZERO),
            (1, Duration::ZERO),
            (2, Duration::from_secs(60)),
        ]);
        let summary = render_summary(&plan, &out);
        assert!(summary.contains("arena8-apple  created on 1×RTX A4000 COMMUNITY ($0.17/h) (after 1×RTX A4000 COMMUNITY: capacity; 1×RTX 3090 COMMUNITY: capacity; 1×RTX A4000 SECURE: capacity)"), "{summary}");
    }

    #[tokio::test(start_paused = true)]
    async fn the_window_closes_with_names_unplaced() {
        let fake = Fake::new(Reply::Capacity);
        let rounds = Rounds { window: Duration::from_secs(150), every: Duration::from_secs(60) };
        let (out, _) = run(&fake, &three(), &["arena8-apple"], rounds, None).await;
        assert!(matches!(out.end, End::WindowElapsed), "{:?}", out.end);
        // t=0, 60, 120 — a fourth round would start at t=180, after the 150s window.
        assert_eq!(out.rounds, 3);
        assert_eq!(fake.calls().len(), 9);
        let last = out.log[0].attempts.iter().map(|a| a.at).max().unwrap();
        assert_eq!(last, Duration::from_secs(120));
        assert!(render_summary(&three(), &out).contains("capacity ×3"));
    }

    /// Review fix: no round (no create, no bill) after the window closes — even when the
    /// interval is longer than the window, and with no pointless sleep before giving up.
    #[tokio::test(start_paused = true)]
    async fn no_round_starts_after_the_window() {
        for (window, every, want_rounds) in [
            (60, 600, 1),  // --retry-mins 1 --retry-secs 600: the 2nd round would be at t=600
            (120, 60, 3),  // t=0, 60, 120: a round exactly at the deadline is still inside
            (119, 60, 2),  // t=0, 60: t=120 would be past it
            (600, 60, 11), // t=0..600
        ] {
            let fake = Fake::new(Reply::Capacity);
            let rounds = Rounds { window: Duration::from_secs(window), every: Duration::from_secs(every) };
            let started = tokio::time::Instant::now();
            let (out, _) = run(&fake, &three(), &["arena8-apple"], rounds, None).await;
            assert!(matches!(out.end, End::WindowElapsed), "{window}/{every}: {:?}", out.end);
            assert_eq!(out.rounds, want_rounds, "{window}/{every}");
            assert!(out.log[0].attempts.iter().all(|a| a.at <= Duration::from_secs(window)), "{window}/{every}");
            // Gave up right after the last round, not after one more sleep.
            assert_eq!(started.elapsed(), Duration::from_secs(every * (want_rounds as u64 - 1)), "{window}/{every}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_retry_round_skips_names_that_appeared_and_respects_the_topup_target() {
        // Round 1 is dry. Before round 2, someone else has made `arena8-bloom` and two more
        // cohort pods on runpod: the target (3) is met, so round 2 creates nothing.
        let fake = Fake::new(Reply::Capacity);
        let rounds = Rounds { window: Duration::from_secs(600), every: Duration::from_secs(60) };
        let topup = TopUp { target: 3, provider: "runpod".into(), prefix: "arena8".into() };
        let plan = three();
        let mut lines = Vec::new();
        let mut sink = |p: &Progress| lines.push(p.line());
        let n = names(&["arena8-apple", "arena8-bloom", "arena8-cider"]);
        let race = async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let mut fleet = fake.fleet.lock().unwrap();
            for name in ["arena8-bloom", "arena8-yak", "arena8-zebra"] {
                fleet.push(Pod { id: name.into(), name: name.into(), provider: "runpod".into(), ..Default::default() });
            }
        };
        let b = base();
        let (out, ()) = tokio::join!(place(&fake, &b, &n, &plan, rounds, Some(&topup), std::future::pending::<()>, &mut sink), race);
        assert!(matches!(out.end, End::Filled), "{:?}", out.end);
        assert_eq!(fake.calls().len(), 3, "round 2 never ran a create");
        assert_eq!(out.log[1].skipped.as_deref(), Some("it exists now"));
        assert_eq!(out.log[0].skipped.as_deref(), Some("target 3 reached (3 on runpod)"));
        assert_eq!(out.log[2].skipped.as_deref(), Some("target 3 reached (3 on runpod)"));
        assert!(lines.iter().any(|l| l == "skip arena8-bloom — it exists now"), "{lines:?}");
        assert!(render_summary(&plan, &out).contains("arena8-cider  skipped (target 3 reached (3 on runpod))"));
    }

    #[test]
    fn still_needed_is_pure_and_keeps_the_given_order() {
        let pod = |n: &str, p: &str| Pod { name: n.into(), provider: p.into(), ..Default::default() };
        let pending = names(&["arena8-apple", "arena8-bloom", "arena8-cider"]);
        let fleet = vec![pod("arena8-bloom", "vast"), pod("arena8-old", "runpod"), pod("other-x", "runpod")];
        // Explicit names: only the one that now exists (on any provider) drops.
        let (keep, dropped) = still_needed(&pending, &fleet, None);
        assert_eq!(keep, ["arena8-apple", "arena8-cider"]);
        assert_eq!(dropped, [("arena8-bloom".to_string(), "it exists now".to_string())]);
        // Top-up to 2 runpod pods: one cohort pod exists there (other prefixes/providers don't count).
        let t = TopUp { target: 2, provider: "runpod".into(), prefix: "arena8".into() };
        let (keep, dropped) = still_needed(&pending, &fleet, Some(&t));
        assert_eq!(keep, ["arena8-apple"]);
        assert_eq!(dropped[1], ("arena8-cider".to_string(), "target 2 reached (1 on runpod)".to_string()));
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_relist_ends_the_run_instead_of_risking_a_duplicate() {
        let mut fake = Fake::new(Reply::Capacity);
        fake.list_fails = true;
        let rounds = Rounds { window: Duration::from_secs(600), every: Duration::from_secs(60) };
        let (out, _) = run(&fake, &three(), &["arena8-apple", "arena8-bloom"], rounds, None).await;
        match &out.end {
            End::Failed { name: None, error } => assert!(error.to_string().contains("refusing to create"), "{error}"),
            other => panic!("expected a failed relist, got {other:?}"),
        }
        assert_eq!(fake.calls().len(), 3, "only round 1 ran");
        // bloom's turn came (every option already blocked) — that's not "not attempted".
        let summary = render_summary(&three(), &out);
        assert!(summary.contains("arena8-apple  not placed (tried: "), "{summary}");
        assert!(summary.contains("arena8-bloom  not placed (every option ran out of capacity before its turn)"), "{summary}");
    }

    #[tokio::test(start_paused = true)]
    async fn an_interrupt_between_rounds_keeps_what_was_made() {
        let fake = Fake::new(Reply::Ok).script(A4000, "COMMUNITY", &[Reply::Ok, Reply::Capacity]).script(R3090, "COMMUNITY", &[Reply::Capacity]).script(A4000, "SECURE", &[Reply::Capacity]);
        let rounds = Rounds { window: Duration::from_secs(600), every: Duration::from_secs(60) };
        let mut sink = |_: &Progress| {};
        let n = names(&["arena8-apple", "arena8-bloom"]);
        let out = place(&fake, &base(), &n, &three(), rounds, None, || async {}, &mut sink).await;
        assert!(matches!(out.end, End::Interrupted), "{:?}", out.end);
        assert_eq!(out.created.len(), 1);
        assert_eq!(out.rounds, 1);
    }

    #[test]
    fn spec_for_applies_the_option_over_the_base() {
        let plan = three();
        let o = &plan.options[2]; // A4000 SECURE
        let s = spec_for(&base(), "arena8-apple", o);
        assert_eq!((s.name.as_str(), s.gpu_type.as_str(), s.cloud_type.as_str(), s.gpu_count), ("arena8-apple", A4000, "SECURE", 1));
        assert_eq!(s.env, [("MACHINE_NAME".to_string(), "arena8-apple".to_string())]);
        assert_eq!((s.image.as_str(), s.disk_gb), ("img:1", 50)); // the rest is the base's
        // No tier (Vast): the base's tier stays.
        let vast = plan_options(&req(&[R3090], &["COMMUNITY"], 2, None, Order::Cheapest), "vast", &PriceBook::unpriced());
        let s = spec_for(&base(), "arena8-apple", &vast.options[0]);
        assert_eq!((s.gpu_type.as_str(), s.cloud_type.as_str(), s.gpu_count), (R3090, "COMMUNITY", 2));
    }
}
