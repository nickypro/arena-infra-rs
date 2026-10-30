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
//!
//! **Near the proxy** ([`crate::near`], `NEAR` / `NEAR_MAX_EXTRA_PCT`): on RunPod and Vast an
//! option can also be tried in the proxy's region first. The plan then holds a near twin of
//! each option it's worth it for — cheapest-first: every near option within the margin over
//! the cheapest acceptable option, then everything anywhere; as listed: each option near, then
//! anywhere. A near RunPod option that reports no capacity is out for the rest of the run:
//! its anywhere twin covers the same data centres, so a cohort spends one create on a full
//! region, not one per name and round (RunPod's API limits are per key, shared by every
//! dashboard). A near Vast option, a market search's answer, is blocked for the round.

use std::future::Future;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Error, ProviderErrorKind, Result};
use crate::fleet::fmt_money;
use crate::gpu;
use crate::metrics::normalize_gpu_name;
use crate::near::{self, Near, Region};
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
    /// RunPod's live catalog (GraphQL `gpuTypes`, or REST v2 `/catalog/gpus`), or Vast's
    /// cheapest matching offer at plan time.
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
    /// Placement near the proxy ([`Self::with_near`]); `None` = off, or nothing it could do.
    pub near: Option<Near>,
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
        Ok(Self { gpus, clouds, gpu_count: base.gpu_count, max_price, order, near: None })
    }

    /// With placement near the proxy (`near`: config's [`Near::from_config`]), where it can
    /// do something for a create of `base` on `provider`: Vast, or RunPod with SECURE among
    /// the tiers ([`near::reaches`]) — and not when `base` already says where on RunPod
    /// (`--api-json` / `CREATE_EXTRA_JSON` with data centres, a network volume — which lives
    /// in one — or countries, [`near::runpod_pins_location`]: the operator's pin wins, and a
    /// near option would only repeat or contradict it). Otherwise near stays off, so a plain
    /// community create keeps the single-spec path. Pure.
    pub fn with_near(mut self, near: Option<Near>, provider: &str, base: &PodSpec) -> Self {
        let pinned = provider == "runpod" && base.api_extra.as_ref().is_some_and(near::runpod_pins_location);
        self.near = near.filter(|_| near::reaches(provider, &self.clouds) && !pinned);
        self
    }

    /// One GPU, one cloud, no price cap, nothing near to try first: `create`/`up` keep today's
    /// single-spec path for this (same output, same `--keep-trying`), so nothing changes for an
    /// operator who doesn't use the new flags.
    pub fn is_single(&self) -> bool {
        self.gpus.len() == 1 && self.clouds.len() == 1 && self.max_price.is_none() && self.near.is_none()
    }

    /// Checks that need the target provider. On RunPod every tier must be COMMUNITY or
    /// SECURE — prices are quoted per tier, and v2 creates on SECURE when the tier isn't one
    /// it knows; on Vast one of its host classes, or a RunPod tier (its default,
    /// [`crate::provider::vast::tier`]). A GPU provider needs a GPU type (config's may be unset).
    pub fn validate_for(&self, provider: &str) -> Result<()> {
        if provider == "runpod" {
            if let Some(bad) = self.clouds.iter().find(|c| !matches!(c.as_str(), "COMMUNITY" | "SECURE")) {
                return Err(Error::Config(format!("--cloud `{bad}`: RunPod tiers are COMMUNITY and SECURE")));
            }
        }
        if provider == "vast" {
            for c in &self.clouds {
                crate::provider::vast::tier(c, true)?;
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
    /// Marketplace quotes (Vast, Shadeform): `(gpu, $/h per GPU)` from the cheapest matching
    /// offer when the plan was made. A GPU with no offer then isn't here — unpriced.
    offers: Vec<MarketPrice>,
    /// The same, among offers near the proxy only ([`Self::with_near_offers`]).
    near_offers: Vec<MarketPrice>,
    /// Whether the market was searched near the proxy at all — without it (no key) a GPU
    /// missing from `near_offers` says nothing about the region.
    near_searched: bool,
    /// GPUs whose near search failed ([`Self::with_near_failed`]): no offer for them says
    /// nothing about the region either. With the tier searched ([`Self::for_cloud`]).
    near_failed: Vec<(String, Option<String>)>,
}

/// A marketplace's quote for one GPU: `$/h` per GPU, and the tier it was searched on (Vast's
/// host classes, [`crate::provider::vast::Host`]) — `None`: any option's tier.
#[derive(Debug, Clone, PartialEq)]
struct MarketPrice {
    gpu: String,
    cloud: Option<String>,
    per_gpu: f64,
}

impl MarketPrice {
    /// Whether this quote prices `gpu` on `cloud`.
    fn prices(&self, gpu: &str, cloud: Option<&str>) -> bool {
        self.gpu == gpu && self.cloud.as_deref().is_none_or(|t| cloud.is_some_and(|c| c.eq_ignore_ascii_case(t)))
    }
}

impl PriceBook {
    /// RunPod: `catalogs` may be empty (no key / fetch failed) — then every known GPU is
    /// priced from the presets, marked as an estimate.
    pub fn runpod(catalogs: Vec<(Option<String>, Vec<GpuType>)>) -> Self {
        Self { catalogs, estimates: true, ..Self::default() }
    }

    /// No quote before create (Hetzner creates CPU VMs; Vast without an API key to search
    /// with). Every option is unpriced.
    pub fn unpriced() -> Self {
        Self::default()
    }

    /// Vast: `per_pod` = each GPU option's cheapest matching offer right now, $/h for the
    /// whole pod (the search asks for exactly `gpu_count` GPUs, and the offer's price
    /// includes the disk asked for: `provider::vast::Offer::price`). Kept per GPU like the catalog prices, so `plan_options`
    /// prices and caps it the same way. Live, but a snapshot: each create searches again and
    /// rents the cheapest offer *then* — never above `--max-price` ([`PodSpec::max_price`]).
    pub fn offers(per_pod: Vec<(String, f64)>, gpu_count: u32) -> Self {
        Self { offers: per_gpu(per_pod, gpu_count), ..Self::default() }
    }

    /// The book's marketplace quotes as searched on one tier (`cloud`, e.g. Vast's `VERIFIED`):
    /// they price only options of that tier. Quotes already tagged keep their tier.
    pub fn for_cloud(mut self, cloud: &str) -> Self {
        for p in self.offers.iter_mut().chain(self.near_offers.iter_mut()) {
            p.cloud.get_or_insert_with(|| cloud.to_string());
        }
        for (_, c) in self.near_failed.iter_mut() {
            c.get_or_insert_with(|| cloud.to_string());
        }
        self
    }

    /// Both books' marketplace quotes (one per tier searched, [`Self::for_cloud`]). Catalogs
    /// and estimates are `self`'s.
    pub fn merge(mut self, other: PriceBook) -> Self {
        self.offers.extend(other.offers);
        self.near_offers.extend(other.near_offers);
        self.near_searched |= other.near_searched;
        self.near_failed.extend(other.near_failed);
        self
    }

    /// Vast, near the proxy: each GPU option's cheapest matching offer in the region, $/h per
    /// pod, as [`Self::offers`] takes them. What a near option is priced (and capped) at.
    pub fn with_near_offers(mut self, per_pod: Vec<(String, f64)>, gpu_count: u32) -> Self {
        self.near_offers = per_gpu(per_pod, gpu_count);
        self.near_searched = true;
        self
    }

    /// Vast, near the proxy: the GPUs whose region search failed (an HTTP error, a timeout) —
    /// tried anywhere only, and the plan says the search failed rather than that the region
    /// has no matching offer (review: codex, claude).
    pub fn with_near_failed(mut self, gpus: Vec<String>) -> Self {
        self.near_failed = gpus.into_iter().map(|g| (g, None)).collect();
        self
    }

    /// Vast: the cheapest near offer's $/h per GPU for `gpu` on `cloud`; `None` = none (or not
    /// searched).
    fn near_offer(&self, gpu: &str, cloud: Option<&str>) -> Option<f64> {
        self.near_offers.iter().find(|p| p.prices(gpu, cloud)).map(|p| p.per_gpu).filter(|p| p.is_finite() && *p > 0.0)
    }

    /// Whether the near search for `gpu` on `cloud` failed.
    fn near_search_failed(&self, gpu: &str, cloud: Option<&str>) -> bool {
        self.near_failed.iter().any(|(g, t)| g == gpu && t.as_deref().is_none_or(|t| cloud.is_some_and(|c| c.eq_ignore_ascii_case(t))))
    }

    /// RunPod: what the live catalog for `cloud` says about `gpu` in `region` — from REST
    /// v2's per-data-centre listing ([`GpuType::data_centers`]): the best stock among the
    /// data centres a near create pins ([`Region::runpod_dcs`]) that list it with any stock.
    /// One listed at `None`, or only in a data centre the create doesn't pin, can't be had
    /// near: [`NearStock::NotOffered`], so no create is spent on it (RunPod's API limits are
    /// per key). Only a catalog for this very tier knows; GraphQL's, or none at all, is
    /// [`NearStock::Unknown`]. The catalog is read for a 1-GPU pod: a data centre with room
    /// for more has room for one, so one it doesn't list can't host a bigger pod either; one
    /// it lists may lack room for more — a create that finds out, once per run.
    fn runpod_near(&self, gpu: &str, cloud: &str, region: Region) -> NearStock {
        let tiered = self.catalogs.iter().filter(|(tag, _)| tag.as_deref().is_some_and(|t| t.eq_ignore_ascii_case(cloud)));
        let Some(row) = tiered.flat_map(|(_, rows)| rows).find(|r| r.id == gpu) else { return NearStock::Unknown };
        let pinned = |dc: &str| region.runpod_dcs().iter().any(|p| p.eq_ignore_ascii_case(dc.trim()));
        let near = row.data_centers.iter().filter(|(dc, stock)| pinned(dc) && stock_rank(Some(stock)) < stock_rank(Some("None")));
        match near.map(|(_, stock)| stock.as_str()).min_by_key(|s| stock_rank(Some(s))) {
            Some(best) => NearStock::Offered(Some(best.to_string()).filter(|s| !s.is_empty())),
            None => NearStock::NotOffered,
        }
    }

    /// RunPod data centres the live catalogs place in `region` that the region's table
    /// ([`Region::runpod_dcs`]) doesn't list — so a near create doesn't pin them: one RunPod
    /// added since. Sorted, once each. For a note that says the table wants updating.
    fn unpinned_dcs(&self, region: Region) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for (dc, _) in self.catalogs.iter().flat_map(|(_, rows)| rows).flat_map(|r| &r.data_centers) {
            let dc = dc.trim().to_ascii_uppercase();
            if region.has_runpod_dc(&dc) && !region.runpod_dcs().contains(&dc.as_str()) && !out.contains(&dc) {
                out.push(dc);
            }
        }
        out.sort();
        out
    }

    /// The live catalog's display name for `gpu` (`B300 MIG 34GB` for `NVIDIA B300 SXM6 AC MIG
    /// 1g.34gb`); `None` without a catalog that lists it with one.
    pub fn display_name(&self, gpu: &str) -> Option<&str> {
        self.catalogs.iter().flat_map(|(_, rows)| rows).find(|r| r.id == gpu).map(|r| r.display_name.trim()).filter(|n| !n.is_empty())
    }

    /// The price and stock hint for `gpu` on `cloud` (`None` = a provider without tiers).
    ///
    /// A live row that quotes either tier is trusted as a pair — its `None` tier means "not
    /// offered there", and is *not* back-filled with a preset (same rule as `arena gpus`).
    /// Only a GPU with no live price at all falls back to the preset estimate.
    pub fn quote(&self, gpu: &str, cloud: Option<&str>) -> Quote {
        if let Some(p) = self.offers.iter().find(|p| p.prices(gpu, cloud)) {
            let price_per_gpu = Some(p.per_gpu).filter(|p| p.is_finite() && *p > 0.0);
            return Quote { price_per_gpu, source: price_per_gpu.map(|_| PriceSource::Live), stock: None };
        }
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

/// `(gpu, $/h per pod)` quotes as [`MarketPrice`]s per GPU, for any tier.
fn per_gpu(per_pod: Vec<(String, f64)>, gpu_count: u32) -> Vec<MarketPrice> {
    let n = f64::from(gpu_count.max(1));
    per_pod.into_iter().map(|(gpu, p)| MarketPrice { gpu, cloud: None, per_gpu: p / n }).collect()
}

/// What RunPod's catalog says about an option near the proxy ([`PriceBook::runpod_near`]).
#[derive(Debug, Clone, PartialEq)]
enum NearStock {
    /// No per-data-centre answer for this tier (GraphQL, no catalog): try it and see.
    Unknown,
    /// Offered in one of the region's data centres; the best stock there.
    Offered(Option<String>),
    /// Listed, but in none of the region's data centres right now.
    NotOffered,
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
    /// Created only in this region (near the proxy, [`crate::near`]); `None` = anywhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub near: Option<Region>,
    /// The most this option's create may pay, $/h per pod, under `--max-price`: a near Vast
    /// option's margin over the cheapest ([`Near::budget`]) — a marketplace picks the offer,
    /// and so the price, only at create. `None` = just `--max-price`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap: Option<f64>,
}

impl PlacementOption {
    /// `1×RTX A4000 COMMUNITY`, `1×RTX A4000 SECURE in Europe` — progress lines and the summary.
    pub fn describe(&self) -> String {
        let near = self.near.map(|r| format!(" in {}", r.label())).unwrap_or_default();
        match &self.cloud {
            Some(c) => format!("{} {c}{near}", self.label),
            None => format!("{}{near}", self.label),
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
    /// Near the proxy first, when on ([`Request::near`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub near: Option<Near>,
}

/// Prices compared in tenths of a cent: `0.17 × 3` is `0.51000000000000001` in f64 and must
/// still fit under a `0.51` cap (and tie with a `0.51` option). Also how a marketplace
/// backend checks an offer against the cap at create, so plan and create agree at the edge.
pub(crate) fn milli(p: f64) -> i64 {
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

/// The short GPU name for a label: the preset's, else the provider string minus vendor. A
/// MIG slice is named as RunPod's catalog names it (`B300 MIG 34GB`, not its id's `B300 SXM6
/// AC MIG 1g.34gb`) — the name `pods list` shows for the pod (live BPF S6: `up`'s placement,
/// `[created]`, READY and summary lines said the id's): the live catalog's when there is one,
/// else read off the id ([`mig_label`]) — the one-GPU path prices nothing (review G4).
fn gpu_short(api: &str, prices: &PriceBook) -> String {
    let mig = api.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| w.eq_ignore_ascii_case("mig"));
    if mig {
        if let Some(name) = prices.display_name(api).map(normalize_gpu_name).or_else(|| mig_label(api)) {
            return name;
        }
    }
    gpu::find(api).map(|g| g.label.to_string()).unwrap_or_else(|| normalize_gpu_name(api))
}

/// RunPod's name for a MIG slice, from its id alone (`NVIDIA B300 SXM6 AC MIG 1g.34gb` → `B300
/// MIG 34GB`): the shared label code's ([`crate::fleet::mig_label`]), so every surface agrees.
pub(crate) use crate::fleet::mig_label;

/// Expand, price, filter and order the options for `provider`. Pure.
///
/// Cloud tiers exist on RunPod (`COMMUNITY`/`SECURE`) and Vast (its host classes, which the
/// caller has already turned `--cloud` into: [`crate::provider::vast::plan_tiers`]):
/// elsewhere the cloud list collapses to one option per GPU (with a note when more than one
/// tier was asked for). Hetzner creates CPU VMs from
/// `--server-type` / `HETZNER_SERVER_TYPE`, so the GPU list collapses too — one option.
///
/// With a cap, an option is kept only when its per-pod price is known and within it; an
/// unknown price can't be checked, so it's dropped (and reported) rather than risked.
/// Without a cap nothing is dropped.
///
/// With [`Request::near`], options near the proxy come first ([`near_first`]).
pub fn plan_options(req: &Request, provider: &str, prices: &PriceBook) -> OptionPlan {
    let mut notes = Vec::new();
    let tiered = matches!(provider, "runpod" | "vast");
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
            // Only a configured GPU list gets here: `create`/`up` refuse `--gpu` on Hetzner.
            notes.push("hetzner creates CPU VMs (--server-type / HETZNER_SERVER_TYPE) — the GPU list doesn't apply, so there is one option".into());
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
                label: if cpu_only { "CPU VM".to_string() } else { format!("{}×{}", req.gpu_count, gpu_short(g, prices)) },
                cloud: c.clone(),
                gpu_count: req.gpu_count,
                price_per_gpu: q.price_per_gpu,
                price_per_pod: q.price_per_gpu.map(|p| p * f64::from(req.gpu_count.max(1))),
                price_source: q.source,
                stock: q.stock,
                near: None,
                cap: None,
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
        sort_cheapest(&mut options);
    }
    let near = req.near.filter(|_| near::reaches(provider, &req.clouds));
    if let Some(n) = near {
        options = near_first(options, n, req, provider, prices, &mut notes);
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
        near,
    }
}

/// Cheapest first: lowest $/h per pod, unpriced last; a price tie broken by the stock hint,
/// then by the listed order.
fn sort_cheapest(options: &mut [PlacementOption]) {
    options.sort_by_key(|o| (o.price_per_pod.is_none(), o.price_per_pod.map(milli).unwrap_or(0), stock_rank(o.stock.as_deref()), o.listed));
}

/// Add the near twins of `anywhere` (the options as planned: capped, ordered) and order the
/// lot. Pure.
///
/// Which options get a near twin, and at what price:
/// - **RunPod**: SECURE ones (COMMUNITY hosts have no data centre, [`crate::near`]) that the
///   tier's live catalog doesn't show missing from every data centre in the region
///   ([`PriceBook::runpod_near`] — one create saved per option per run when it's plainly not
///   there; no catalog, no say: tried). The price is the option's own (per GPU and tier).
/// - **Vast**: those with a matching offer in the region ([`PriceBook::with_near_offers`]),
///   priced at the cheapest such offer, and never above `--max-price`.
///
/// How much more a near option may cost, by `req.order`:
/// - cheapest first: up to [`Near::max_extra_pct`] over the cheapest acceptable option — the
///   one that would otherwise be tried first. Those near options come first (cheapest first
///   among them), then every option anywhere, as before.
/// - as listed: up to the margin over the same option anywhere, and each option is tried near
///   just before it's tried anywhere — the GPU order the operator gave still comes first.
///
/// A Vast near option carries its margin as its own price cap ([`PlacementOption::cap`]): its
/// create searches the market again, and must not rent dearer than the plan allowed.
fn near_first(
    anywhere: Vec<PlacementOption>,
    n: Near,
    req: &Request,
    provider: &str,
    prices: &PriceBook,
    notes: &mut Vec<String>,
) -> Vec<PlacementOption> {
    let region = n.region.label();
    // A marketplace not searched near the proxy (no key: nothing was searched at all) can't
    // say the region lacks anything — no near option, and no claim about the region.
    if provider != "runpod" && !prices.near_searched {
        notes.push(format!("near the proxy (NEAR={}): {region} not searched — every option anywhere", n.region.key()));
        return anywhere;
    }
    let cheapest = anywhere.iter().filter_map(|o| o.price_per_pod).min_by(f64::total_cmp);
    let mut community = false;
    let mut why_not = Vec::new();
    let mut twins: Vec<Option<PlacementOption>> = Vec::new();
    for o in &anywhere {
        let (price_per_gpu, source, stock) = match provider {
            "runpod" => {
                if !o.cloud.as_deref().is_some_and(near::runpod_tier_placeable) {
                    community = true;
                    twins.push(None);
                    continue;
                }
                let cloud = o.cloud.as_deref().unwrap_or("");
                match prices.runpod_near(&o.gpu, cloud, n.region) {
                    NearStock::NotOffered => {
                        why_not.push(format!("{}: RunPod's catalog lists it in no {region} data centre right now", o.describe()));
                        twins.push(None);
                        continue;
                    }
                    NearStock::Offered(stock) => (o.price_per_gpu, o.price_source, stock),
                    NearStock::Unknown => (o.price_per_gpu, o.price_source, None),
                }
            }
            _ => match prices.near_offer(&o.gpu, o.cloud.as_deref()) {
                Some(p) => (Some(p), Some(PriceSource::Live), None),
                None if prices.near_search_failed(&o.gpu, o.cloud.as_deref()) => {
                    why_not.push(format!("{}: searching {region} failed", o.describe()));
                    twins.push(None);
                    continue;
                }
                None => {
                    why_not.push(format!("{}: no matching offer in {region} right now", o.describe()));
                    twins.push(None);
                    continue;
                }
            },
        };
        let price_per_pod = price_per_gpu.map(|p| p * f64::from(req.gpu_count.max(1)));
        // The margin is over what would otherwise be tried: the cheapest option, or (as
        // listed) this same option anywhere. Unknown when that has no price.
        let base = match req.order {
            Order::Cheapest => cheapest,
            Order::Listed => o.price_per_pod,
        };
        let budget = base.map(|b| n.budget(b));
        let twin = PlacementOption { near: Some(n.region), price_per_gpu, price_per_pod, price_source: source, stock, ..o.clone() };
        // Only a near option whose price can be weighed goes ahead of others. Cheapest first,
        // an unpriced one (a GPU no catalog or preset prices) would jump the queue of priced
        // options, at any price (review: codex). And a marketplace's near create rents what the
        // region offers then: without a price anywhere there's no margin to cap it at (review:
        // codex, claude). As listed, a RunPod twin costs exactly its anywhere option: fine.
        let weighable = match req.order {
            Order::Cheapest => price_per_pod.is_some() && budget.is_some(),
            Order::Listed => provider == "runpod" || budget.is_some(),
        };
        if !weighable {
            why_not.push(format!("{}: no price anywhere to weigh it against", twin.describe()));
            twins.push(None);
            continue;
        }
        let over = |limit: f64| price_per_pod.is_some_and(|p| milli(p) > milli(limit));
        if let Some(cap) = req.max_price.filter(|c| over(*c)) {
            why_not.push(format!("{}: {} > cap {}/h per pod", twin.describe(), twin.price_label(), fmt_money("$", cap)));
            twins.push(None);
            continue;
        }
        if let (Some(_), Some(base)) = (budget.filter(|b| over(*b)), base) {
            why_not.push(format!(
                "{}: {} is more than +{}% over {}/h",
                twin.describe(),
                twin.price_label(),
                near::fmt_pct(n.max_extra_pct),
                fmt_money("$", base)
            ));
            twins.push(None);
            continue;
        }
        // Only a marketplace's price is chosen at create: cap it at what the plan allowed.
        let cap = if provider == "runpod" { None } else { lower_cap(budget, req.max_price) };
        twins.push(Some(PlacementOption { cap, ..twin }));
    }
    let planned = twins.iter().flatten().count();
    let margin = match (req.order, cheapest) {
        (Order::Cheapest, Some(c)) => format!(
            "within +{}% of the cheapest ({}/h → up to {}/h)",
            near::fmt_pct(n.max_extra_pct),
            fmt_money("$", c),
            fmt_money("$", n.budget(c))
        ),
        (Order::Cheapest, None) => format!("within +{}% of the cheapest", near::fmt_pct(n.max_extra_pct)),
        (Order::Listed, _) => format!("each within +{}% of the same option anywhere", near::fmt_pct(n.max_extra_pct)),
    };
    if planned > 0 {
        let how = match req.order {
            Order::Cheapest => format!("{region} first, then every option anywhere"),
            Order::Listed => format!("each option in {region} first, then anywhere"),
        };
        notes.push(format!("near the proxy (NEAR={}): {how} — {margin} (NEAR_MAX_EXTRA_PCT)", n.region.key()));
    } else {
        notes.push(format!("near the proxy (NEAR={}): no option to try in {region} — every option anywhere", n.region.key()));
    }
    if community {
        notes.push(format!(
            "near: RunPod's COMMUNITY cloud can't be placed by data centre (its hosts have none) — only SECURE options are tried in {region}"
        ));
    }
    let unpinned = if provider == "runpod" { prices.unpinned_dcs(n.region) } else { Vec::new() };
    if !unpinned.is_empty() {
        notes.push(format!(
            "near: RunPod lists {} in {region}, which arena's table doesn't pin yet (crates/core/src/near.rs) — reached only anywhere",
            unpinned.join(", ")
        ));
    }
    for w in why_not {
        notes.push(format!("near, not tried in {region}: {w}"));
    }
    match req.order {
        Order::Cheapest => {
            let mut first: Vec<PlacementOption> = twins.into_iter().flatten().collect();
            sort_cheapest(&mut first);
            first.extend(anywhere);
            first
        }
        Order::Listed => twins.into_iter().zip(anywhere).flat_map(|(t, o)| t.into_iter().chain(std::iter::once(o))).collect(),
    }
}

/// The option table (`arena offers`, create/up dry-run and confirm), then what the cap
/// dropped and any notes. With near placement on, a WHERE column says which options are
/// tried in the region only (`Europe`) and which anywhere (`any`). Pure, snapshot-tested.
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
        let near = plan.near.is_some();
        let rows: Vec<Vec<String>> = plan
            .options
            .iter()
            .enumerate()
            .map(|(i, o)| {
                let mut row = vec![(i + 1).to_string(), o.label.clone(), o.cloud.clone().unwrap_or_else(|| "-".into())];
                if near {
                    row.push(o.near.map_or("any", Region::label).to_string());
                }
                row.extend([price(o), source(o).to_string(), o.stock.clone().unwrap_or_else(|| "-".into())]);
                row
            })
            .collect();
        let (headers, aligns): (&[&str], &[Align]) = if near {
            (&["#", "OPTION", "CLOUD", "WHERE", "$/H/POD", "PRICE", "STOCK"], &[Right, Left, Left, Left, Right, Left, Left])
        } else {
            (&["#", "OPTION", "CLOUD", "$/H/POD", "PRICE", "STOCK"], &[Right, Left, Left, Right, Left, Left])
        };
        out.push_str(&table::render(headers, aligns, &rows));
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
    /// The API refused this option's request as invalid (RunPod v1's schema 400 — a GPU type
    /// its create enum lacks — or v2's 422 "Unknown GPU type"): skipped for the rest of the
    /// run, like `Denied`. Another option may be fine; this one won't become fine by waiting.
    Rejected(String),
    /// The account is at one of its own limits (Hetzner's `resource_limit_exceeded`: servers,
    /// cores): skipped for the rest of the run, like `Rejected` — a smaller option may still fit,
    /// and waiting won't free the limit.
    Quota(String),
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

/// Whether another retry round — starting `every` from `now` — would still start inside the
/// window that ends at `deadline` (a round exactly at the deadline is inside). Asked
/// *before* the sleep, so a retry never creates (bills) after the window the operator agreed
/// to, nor sleeps just to give up, and timer slack can't flip the answer. Shared by the
/// placement executor ([`place`]) and the CLI's single-option retry loop, so both stop the
/// same way. Pure.
pub fn next_round_in_window(now: tokio::time::Instant, every: Duration, deadline: tokio::time::Instant) -> bool {
    now < deadline && now + every <= deadline
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
/// option's GPU, count and tier — and, for a near option, its region and price cap.
pub fn spec_for(base: &PodSpec, name: &str, option: &PlacementOption) -> PodSpec {
    let mut spec = base.clone();
    spec.name = name.to_string();
    spec.env.push(("MACHINE_NAME".into(), name.to_string()));
    spec.gpu_type = option.gpu.clone();
    spec.gpu_count = option.gpu_count;
    if let Some(c) = &option.cloud {
        spec.cloud_type = c.clone();
    }
    spec.region = option.near;
    spec.max_price = lower_cap(spec.max_price, option.cap);
    spec
}

/// The tighter of two $/h caps (`None` = no cap). Pure.
fn lower_cap(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// A progress event from [`place`], for the caller to print.
#[derive(Debug)]
pub enum Progress<'a> {
    Attempt {
        name: &'a str,
        option: &'a PlacementOption,
        /// Its index in the plan's options.
        index: usize,
        outcome: &'a Outcome,
        /// After a capacity error: another option is still open for this name this round.
        next: bool,
        /// The pod this attempt created (with [`Outcome::Created`]) — so a caller can start on
        /// it at once, while the run goes on placing the other names (`up`'s pipelines).
        pod: Option<&'a Pod>,
    },
    /// Between rounds. `listing_missing`: the last round was skipped ([`Progress::Unlisted`]),
    /// not short of capacity.
    Waiting { round: u32, unplaced: usize, every: Duration, listing_missing: bool },
    Skipped { name: &'a str, why: &'a str },
    /// Round `round` ran no create: the fleet listing before it was missing a backend that may
    /// answer again ([`Relisted::Partial`]).
    Unlisted { round: u32, error: &'a Error },
}

impl Progress<'_> {
    /// A pod was made — the line that belongs on stdout.
    pub fn is_created(&self) -> bool {
        matches!(self, Progress::Attempt { outcome: Outcome::Created { .. }, .. })
    }

    pub fn line(&self) -> String {
        match self {
            Progress::Attempt { name, option, outcome, next, .. } => {
                let on = option.describe();
                match outcome {
                    Outcome::Created { pod_id } => {
                        format!("[created] {name} id={pod_id} on {on} ({})", option.price_label())
                    }
                    // The provider's own words: "no capacity" for a type it doesn't know at all
                    // reads differently (RunPod: a 422 "Unknown GPU type", `[rejected]`).
                    Outcome::Capacity(e) => format!(
                        "[no capacity] {name} on {on}: {e}{}",
                        if *next { " — trying the next option" } else { " — no option left this round" }
                    ),
                    Outcome::Denied(e) => format!(
                        "[no access] {name} on {on}: {e}{}",
                        if *next { " — skipping this option, trying the next" } else { " — skipping this option" }
                    ),
                    Outcome::Rejected(e) => format!(
                        "[rejected] {name} on {on}: {e}{}",
                        if *next { " — skipping this option, trying the next" } else { " — skipping this option" }
                    ),
                    Outcome::Quota(e) => format!(
                        "[over the limit] {name} on {on}: {e}{}",
                        if *next { " — skipping this option, trying the next" } else { " — skipping this option" }
                    ),
                    Outcome::Auth(e) => format!("[auth failed] {name} on {on}: {e}"),
                    Outcome::Failed(e) => format!("[failed] {name} on {on}: {e}"),
                }
            }
            Progress::Waiting { round, unplaced, every, listing_missing } => format!(
                "round {round}: {unplaced} name(s) not placed ({}); retrying in {}s (Ctrl+C to stop)…",
                if *listing_missing { "fleet listing incomplete" } else { "no capacity" },
                every.as_secs()
            ),
            Progress::Skipped { name, why } => format!("skip {name} — {why}"),
            Progress::Unlisted { round, error } => format!("round {round}: nothing created — {error}"),
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
/// the fleet is re-listed first ([`relist`], then [`still_needed`]). No round creates from a
/// listing with any backend missing ([`crate::provider::list_complete`]): one that may
/// recover (throttled, 5xx, stalled) skips that round — the next lists again, inside the same window —
/// and any other failure ends the run rather than risk a duplicate. A round whose listing
/// outlasted the window doesn't run. Finally, once every option is out for good (denied or
/// rejected, in any mix), the run ends at once instead of idling out its window.
/// `interrupt` makes a fresh "stop" future per wait (Ctrl+C in the CLI): it
/// ends the run between rounds — during the sleep or the re-listing — keeping what was made.
/// It is called more than once per round, so its future must resolve at once once a stop has
/// happened (a latch, as the CLI's `Interrupt::wait`): a fresh one-shot listener would miss a
/// press made before it. The option order is the confirmed
/// plan's — fixed for the run — and every create carries its `--max-price`
/// ([`PodSpec::max_price`]) — a near option's own cap when lower — which a marketplace
/// backend enforces on the offer it rents.
///
/// A near RunPod option ([`PlacementOption::near`]) that reports no capacity is out for the
/// rest of the *run*, not just the round: the same option anywhere includes the region's data
/// centres, so nothing is lost but the preference — and a 60-pod `up` spends one create on a
/// full region instead of one per name and round (RunPod limits requests per key). A near Vast
/// option is blocked for the round only: its "no capacity" is one market search's answer.
#[allow(clippy::too_many_arguments)] // the run's independent inputs; a one-use struct for them would add nothing
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
    // Options this account can't use (`Denied`) or the API refuses as invalid (`Rejected`):
    // unlike capacity blocks, never cleared.
    let mut denied = vec![false; plan.options.len()];
    let mut rejected = vec![false; plan.options.len()];
    // Near RunPod options that found no capacity: out for the run (see above).
    let mut near_full = vec![false; plan.options.len()];
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
                if blocked[k] || denied[k] || rejected[k] || near_full[k] {
                    continue;
                }
                // The cap travels with the spec: a marketplace backend (Vast) picks the
                // machine, and so the price, only now — the plan's quote may be gone.
                let spec = PodSpec { max_price: lower_cap(plan.max_price, option.cap), ..spec_for(base, &name, option) };
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
                            // RunPod's full region: out for the run (each create is a POST against
                            // the key's limit). A marketplace's near "no capacity" is a snapshot
                            // a fresh search can change (an offer raced, prices moved): only
                            // for the round, like any other (review: claude).
                            near_full[k] = option.near.is_some() && plan.provider == "runpod";
                            (Outcome::Capacity(e.to_string()), None)
                        }
                        Some(ProviderErrorKind::Denied) => {
                            denied[k] = true;
                            let outcome = Outcome::Denied(e.to_string());
                            // Every option refused access — a near one that was full counts with
                            // them (its anywhere twin was refused): still the key's problem.
                            if denied.iter().zip(&near_full).all(|(d, n)| *d || *n) {
                                let error = Error::Provider {
                                    kind: ProviderErrorKind::Denied,
                                    message: format!(
                                        "every option was refused (no access) — the API key may lack \
                                         permission to create pods, or the account can't use any of these \
                                         pools; last: {e}"
                                    ),
                                };
                                (outcome, Some(End::Aborted { name: name.clone(), error }))
                            } else if refused_all(&denied, &rejected, &near_full) {
                                // Some rejected as invalid, the rest denied: nothing is left to try
                                // ever again, whichever kind of refusal came last — without this the
                                // run idled out its whole window making no create ("no capacity").
                                (outcome, Some(End::Failed { name: Some(name.clone()), error: every_option_refused(&e) }))
                            } else {
                                (outcome, None)
                            }
                        }
                        Some(ProviderErrorKind::Auth) => {
                            (Outcome::Auth(e.to_string()), Some(End::Aborted { name: name.clone(), error: e }))
                        }
                        // The account's own limit: this option (its size) won't fit for the rest
                        // of the run, a smaller one may — never an abort (the key works) and
                        // never waited out (no round frees it).
                        Some(ProviderErrorKind::Quota) => {
                            rejected[k] = true;
                            let outcome = Outcome::Quota(e.to_string());
                            if refused_all(&denied, &rejected, &near_full) {
                                (outcome, Some(End::Failed { name: Some(name.clone()), error: every_option_refused(&e) }))
                            } else {
                                (outcome, None)
                            }
                        }
                        // Invalid for this option only (finding #22: one v1 schema 400 on the
                        // first option left four good ones untried). Once nothing usable is
                        // left, the run ends as failed — a request every option refuses is the
                        // spec's fault, not the pools'.
                        _ if is_rejected(&e) => {
                            rejected[k] = true;
                            let outcome = Outcome::Rejected(e.to_string());
                            if refused_all(&denied, &rejected, &near_full) {
                                (outcome, Some(End::Failed { name: Some(name.clone()), error: every_option_refused(&e) }))
                            } else {
                                (outcome, None)
                            }
                        }
                        _ => (Outcome::Failed(e.to_string()), Some(End::Failed { name: Some(name.clone()), error: e })),
                    },
                };
                let next = (k + 1..plan.options.len()).any(|j| !blocked[j] && !denied[j] && !rejected[j] && !near_full[j]);
                let pod = placed.then(|| created.last()).flatten();
                on_progress(&Progress::Attempt { name: &name, option, index: k, outcome: &outcome, next, pod });
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
        // Wait for the next round. The whole fleet is listed first, every backend: a name on a
        // backend that didn't answer isn't free. A listing that may recover skips that round
        // (nothing created) and the wait starts over, inside the same window.
        let mut listing_missing = false;
        loop {
            // A round starts only inside the window. The next one would start `every` from
            // now, so if that's past the deadline, end here — not sleep just to give up, nor
            // create (bill) up to `every` after the window the operator agreed to. (Decided
            // before the sleep, so timer slack can't flip it.)
            if !next_round_in_window(tokio::time::Instant::now(), rounds.every, deadline) {
                break 'run End::WindowElapsed;
            }
            on_progress(&Progress::Waiting { round, unplaced: pending.len(), every: rounds.every, listing_missing });
            tokio::select! {
                _ = tokio::time::sleep(rounds.every) => {}
                _ = interrupt() => break 'run End::Interrupted,
            }
            // The listing is raced against Ctrl+C too, Ctrl+C first (`biased`): it can take many
            // seconds (retries, a RunPod 429 backoff), and a press then used to let one more
            // round of creates (bills) through after "stopping". A listing reads only, so
            // dropping it half-way is safe.
            let relisted = tokio::select! {
                biased;
                _ = interrupt() => break 'run End::Interrupted,
                r = relist(provider) => r,
            };
            match relisted {
                Relisted::Whole(fleet) => {
                    // The listing (retried with backoff) can outlast the window: no round after it.
                    if tokio::time::Instant::now() > deadline {
                        break 'run End::WindowElapsed;
                    }
                    let names: Vec<String> = pending.iter().map(|&i| log[i].name.clone()).collect();
                    let (keep, dropped) = still_needed(&names, &fleet, topup);
                    for (name, why) in dropped {
                        on_progress(&Progress::Skipped { name: &name, why: &why });
                        if let Some(l) = log.iter_mut().find(|l| l.name == name) {
                            l.skipped = Some(why);
                        }
                    }
                    pending.retain(|&i| keep.contains(&log[i].name));
                    break;
                }
                Relisted::Partial(error) => {
                    round += 1;
                    listing_missing = true;
                    on_progress(&Progress::Unlisted { round, error: &error });
                }
                Relisted::Failed(e) => {
                    let error = Error::provider(format!(
                        "listing pods before retry round {} (refusing to create — a failed list could duplicate pods): {e}",
                        round + 1
                    ));
                    break 'run End::Failed { name: None, error };
                }
            }
        }
    };
    Placement { log, created, rounds: round, end }
}

/// Whether a create error means "this option's request is invalid" — skip the option, try
/// the next ([`Outcome::Rejected`]): RunPod v1's schema 400 ([`ProviderErrorKind::Rejected`])
/// or a config-level refusal such as v2's 422 "Unknown GPU type" ([`Error::Config`]).
fn is_rejected(e: &Error) -> bool {
    matches!(e, Error::Config(_)) || e.kind() == Some(ProviderErrorKind::Rejected)
}

/// Whether every option is out for the rest of the run — each one denied or rejected, in
/// whatever mix and order, or a near option that was full (its anywhere twin, refused too,
/// covers it). (Other capacity blocks don't count: a later round clears them.) Pure.
fn refused_all(denied: &[bool], rejected: &[bool], near_full: &[bool]) -> bool {
    denied.iter().zip(rejected).zip(near_full).all(|((d, r), n)| *d || *r || *n)
}

/// The error a run fails with once [`refused_all`] holds and not every refusal was a `Denied`
/// (that one is an access problem with the key: `End::Aborted`).
fn every_option_refused(last: &Error) -> Error {
    // The account's limit, last: said as that (and kept a `Quota`), not as an invalid request.
    if last.kind() == Some(ProviderErrorKind::Quota) {
        let said = match last {
            Error::Provider { message, .. } => message.clone(),
            other => other.to_string(),
        };
        return Error::Provider { kind: ProviderErrorKind::Quota, message: format!("no option fits the account's limits — last: {said}") };
    }
    Error::provider(format!("every option was refused as an invalid request (or not accessible) — last: {last}"))
}

/// What the fleet listing before a retry round came to ([`relist`]).
#[derive(Debug)]
pub enum Relisted {
    /// Every backend answered: the round may run.
    Whole(Vec<Pod>),
    /// A backend didn't answer, but may yet — every failure throttled, a 5xx or unreachable
    /// (already retried with backoff), or a list that stalled past its bound
    /// ([`crate::provider::LIST_TIMEOUT`]). This round creates nothing; the next lists again.
    Partial(Error),
    /// A failure waiting won't fix (a rejected key, an unreadable answer): the run ends — no
    /// round runs without knowing what exists.
    Failed(Error),
}

/// The whole fleet before a retry round — what [`crate::provider::list_complete`] gives,
/// retried with backoff the same way ([`RetryPolicy::default`]) — shared by the placement
/// executor and the CLI's single-option loop. A partial listing never lets a round create —
/// a name held on the backend that didn't answer would read as free — but one that may
/// recover only skips that round: an outage of an unrelated provider (Vast throttling while
/// placing on RunPod) used to end a `--retry-mins` cohort start for every unplaced name, and
/// an `up --check` replacement after its old pod was already terminated.
///
/// A stalled backend ([`Error::Timeout`] from the listing's bound) counts as one that may
/// recover, like a 504 would, but isn't re-listed in place: each try already took the whole
/// bound, so the next round's listing is its retry.
pub async fn relist(provider: &dyn Provider) -> Relisted {
    let policy = RetryPolicy::default();
    let mut retries = 0u32;
    loop {
        let mut fleet = Vec::new();
        let mut failed: Vec<(String, Error)> = Vec::new();
        for (name, listed) in provider.list_by_provider().await {
            match listed {
                Ok(pods) => fleet.extend(pods),
                Err(e) => failed.push((name, e)),
            }
        }
        if failed.is_empty() {
            return Relisted::Whole(fleet);
        }
        let error = crate::provider::partial_listing_error(&failed);
        // Every failure throttled/5xx: retried with backoff, as `retrying` does.
        if crate::retry::is_retryable(&error) && retries < policy.max_retries {
            retries += 1;
            tokio::time::sleep(policy.backoff(retries)).await;
            continue;
        }
        let may_recover = failed.iter().all(|(_, e)| crate::retry::is_retryable(e) || matches!(e, Error::Timeout { .. }));
        return if may_recover { Relisted::Partial(error) } else { Relisted::Failed(error) };
    }
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
                Outcome::Rejected(_) => "rejected",
                Outcome::Quota(_) => "over the account's limit",
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
    let any_denied =
        run.log.iter().flat_map(|l| &l.attempts).any(|a| matches!(a.outcome, Outcome::Denied(_) | Outcome::Rejected(_) | Outcome::Quota(_)));
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
            min_cuda: None,
            max_price: None,
            api_extra: None,
            avoid_machines: Vec::new(),
            server_type: None,
            vm_image: None,
            region: None,
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
            data_centers: Vec::new(),
        }
    }

    fn req(gpus: &[&str], clouds: &[&str], count: u32, cap: Option<f64>, order: Order) -> Request {
        Request {
            gpus: gpus.iter().map(|s| s.to_string()).collect(),
            clouds: clouds.iter().map(|s| s.to_string()).collect(),
            gpu_count: count,
            max_price: cap,
            order,
            near: None,
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
        assert!(r.validate_for("shadeform").is_ok()); // no tiers there: collapsed, not sent
        assert!(r.validate_for("vast").unwrap_err().to_string().contains("Vast's cloud types are datacenter, verified and unverified"));
        let vast = req(&[A4000], &["COMMUNITY", "DATACENTER", "VERIFIED", "UNVERIFIED"], 1, None, Order::Cheapest);
        assert!(vast.validate_for("vast").is_ok(), "its classes, and RunPod's tiers (its default)");
        let none = req(&[""], &["COMMUNITY"], 1, None, Order::Cheapest);
        assert!(none.validate_for("runpod").unwrap_err().to_string().contains("GPU_TYPE"));
        assert!(none.validate_for("hetzner").is_ok());
    }

    /// Live BPF S6: a MIG slice's option reads as `pods list` reads the pod — the catalog's
    /// display name (`1×B300 MIG 34GB`), not its id (`1×B300 SXM6 AC MIG 1g.34gb`). Only MIG
    /// slices: every other label stays the preset's / the id minus its vendor.
    #[test]
    fn a_mig_option_is_labelled_with_the_catalog_name() {
        const MIG: &str = "NVIDIA B300 SXM6 AC MIG 1g.34gb";
        let named = |id: &str, name: &str| GpuType { display_name: name.into(), ..live(id, Some(0.5), Some(0.5), None) };
        let book = PriceBook::runpod(vec![(
            Some("SECURE".into()),
            vec![named(MIG, "B300 MIG 34GB"), named("NVIDIA H100 80GB HBM3", "H100 SXM"), named("NVIDIA Mystery 9", "Mystery")],
        )]);
        let labels = |book: &PriceBook| -> Vec<String> {
            let r = req(&[MIG, "NVIDIA H100 80GB HBM3", "NVIDIA Mystery 9"], &["SECURE"], 1, None, Order::Listed);
            plan_options(&r, "runpod", book).options.iter().map(|o| o.label.clone()).collect()
        };
        assert_eq!(labels(&book), ["1×B300 MIG 34GB", "1×H100", "1×Mystery 9"]);
        assert_eq!(book.display_name(MIG), Some("B300 MIG 34GB"));
        // The catalog's name wins over the one read off the id ...
        let renamed = PriceBook::runpod(vec![(None, vec![named(MIG, "B300 Slice 34GB")])]);
        assert_eq!(labels(&renamed)[0], "1×B300 Slice 34GB");
        // ... which is used without one (it couldn't be fetched, or the one-GPU path — review
        // G4), or when its row has no name: never a blank label.
        assert_eq!(labels(&PriceBook::runpod(Vec::new())), ["1×B300 MIG 34GB", "1×H100", "1×Mystery 9"]);
        let unnamed = PriceBook::runpod(vec![(None, vec![live(MIG, Some(0.5), Some(0.5), None)])]);
        assert_eq!(labels(&unnamed)[0], "1×B300 MIG 34GB");
    }

    /// Review G4: the one-GPU path has no catalog, so a MIG slice is named off its id — the
    /// same name RunPod's catalog gives it, for every MIG entry of the recorded catalog.
    #[test]
    fn a_mig_label_read_off_the_id_matches_the_catalogs_name() {
        let recorded: serde_json::Value =
            serde_json::from_str(include_str!("provider/fixtures/runpod_v2/catalog_community_cuda13.json")).unwrap();
        let migs: Vec<(&str, &str)> = recorded["body"]["gpus"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|g| Some((g["id"].as_str()?, g["name"].as_str()?)))
            .filter(|(id, _)| id.contains(" MIG "))
            .collect();
        assert!(migs.len() >= 3, "{migs:?}");
        for (id, name) in migs {
            assert_eq!(mig_label(id).as_deref(), Some(name), "{id}");
        }
        for (id, want) in [
            ("NVIDIA A100-SXM4-80GB MIG 1g.10gb", Some("A100 MIG 10GB")),
            ("NVIDIA H100 80GB HBM3 MIG 3g.40gb", Some("H100 MIG 40GB")),
            ("NVIDIA B300 SXM6 AC MIG 1g", None),
            ("NVIDIA B300 SXM6 AC", None),
        ] {
            assert_eq!(mig_label(id).as_deref(), want, "{id}");
        }
        let unpriced = |gpu: &str| {
            let r = req(&[gpu], &["SECURE"], 1, None, Order::Listed);
            plan_options(&r, "runpod", &PriceBook::unpriced()).options[0].label.clone()
        };
        assert_eq!(unpriced("NVIDIA B300 SXM6 AC MIG 1g.34gb"), "1×B300 MIG 34GB");
        assert_eq!(unpriced("NVIDIA RTX A4000"), "1×RTX A4000", "non-MIG labels are as before");
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
    fn tiers_collapse_off_runpod_and_vast_and_gpus_collapse_on_hetzner() {
        let r = req(&[A4000, R3090], &["COMMUNITY", "SECURE"], 1, None, Order::Cheapest);
        let sf = plan_options(&r, "shadeform", &PriceBook::unpriced());
        assert_eq!(order_of(&sf), [opt("1×RTX A4000", None), opt("1×RTX 3090", None)]); // unpriced: listed order
        assert!(sf.notes[0].contains("shadeform has no cloud tiers"), "{:?}", sf.notes);
        // Vast's tiers are its host classes (the CLI turned `--cloud` into them): one option each,
        // priced per tier — a quote searched on one tier prices only that tier's option.
        let tiers = req(&[A4000], &["DATACENTER", "VERIFIED"], 1, None, Order::Cheapest);
        let book = PriceBook::offers(vec![(A4000.into(), 0.30)], 1).for_cloud("DATACENTER").merge(PriceBook::offers(vec![(A4000.into(), 0.20)], 1).for_cloud("VERIFIED"));
        let vast = plan_options(&tiers, "vast", &book);
        assert_eq!(order_of(&vast), [opt("1×RTX A4000", Some("VERIFIED")), opt("1×RTX A4000", Some("DATACENTER"))]);
        assert_eq!(vast.options.iter().map(|o| o.price_per_pod.map(milli)).collect::<Vec<_>>(), [Some(200), Some(300)]);
        assert!(vast.notes.is_empty(), "{:?}", vast.notes);
        let hz = plan_options(&r, "hetzner", &PriceBook::unpriced());
        assert_eq!(order_of(&hz), [opt("CPU VM", None)]);
        assert_eq!(hz.notes.len(), 2, "{:?}", hz.notes);
        // A cap leaves nothing to try on an unpriced provider — reported, not risked.
        let capped = plan_options(&req(&[A4000], &["VERIFIED"], 1, Some(1.0), Order::Cheapest), "vast", &PriceBook::unpriced());
        assert!(capped.options.is_empty());
        assert_eq!(capped.dropped.len(), 1);
    }

    /// Vast: each option priced at its cheapest matching offer (per pod: the offer is for
    /// exactly `gpu_count` GPUs), live — so `--max-price` and `cheapest` work there too.
    #[test]
    fn vast_options_are_priced_from_the_cheapest_offer() {
        // (label, $/h per pod in tenths of a cent) in plan order.
        let priced = |p: &OptionPlan| -> Vec<(String, Option<i64>)> {
            p.options.iter().map(|o| (o.label.clone(), o.price_per_pod.map(milli))).collect()
        };
        let row = |label: &str, m: Option<i64>| (label.to_string(), m);
        let book = PriceBook::offers(vec![(R3090.into(), 0.105_78), (A4000.into(), 0.20)], 1);
        let plan = plan_options(&req(&[A4000, R3090, ADA], &["VERIFIED"], 1, None, Order::Cheapest), "vast", &book);
        assert_eq!(
            priced(&plan),
            [row("1×RTX 3090", Some(106)), row("1×RTX A4000", Some(200)), row("1×RTX 4000 Ada", None)],
            "cheapest first; no offer = unpriced, last"
        );
        assert_eq!(plan.options[0].price_source, Some(PriceSource::Live));
        assert_eq!(plan.options[0].price_label(), "$0.11/h");
        // The cap drops the dearer option and the unpriced one, with reasons.
        let capped = plan_options(&req(&[A4000, R3090, ADA], &["VERIFIED"], 1, Some(0.15), Order::Cheapest), "vast", &book);
        assert_eq!(priced(&capped), [row("1×RTX 3090", Some(106))]);
        let reasons: Vec<&str> = capped.dropped.iter().map(|d| d.reason.as_str()).collect();
        assert_eq!(reasons, ["$0.20/h > cap $0.15/h per pod", "no known price, so --max-price can't be checked"]);
        // A 2-GPU offer's price is the pod's: kept per GPU, multiplied back.
        let two = plan_options(&req(&[R3090], &["VERIFIED"], 2, Some(0.25), Order::Cheapest), "vast", &PriceBook::offers(vec![(R3090.into(), 0.25)], 2));
        assert_eq!(priced(&two), [row("2×RTX 3090", Some(250))]);
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

    // ---- near the proxy ----------------------------------------------------------------

    const EU: Near = Near { region: Region::Europe, max_extra_pct: 25.0 };

    fn near_req(gpus: &[&str], clouds: &[&str], cap: Option<f64>, order: Order, near: Near) -> Request {
        Request { near: Some(near), ..req(gpus, clouds, 1, cap, order) }
    }

    /// A REST v2 row with its per-data-centre stock.
    fn dc_row(id: &str, comm: Option<f64>, sec: Option<f64>, dcs: &[(&str, &str)]) -> GpuType {
        GpuType { data_centers: dcs.iter().map(|(d, s)| (d.to_string(), s.to_string())).collect(), ..live(id, comm, sec, Some("Low")) }
    }

    /// RunPod as the live v2 catalog read on 2026-10-28: A4000 SECURE only in EUR-IS-1 (Low),
    /// the 4000 Ada in EU-RO-1 and the US, the 3090 SECURE only in the US; the COMMUNITY
    /// answer names no data centre for anything.
    fn v2_book() -> PriceBook {
        PriceBook::runpod(vec![
            (
                Some("COMMUNITY".into()),
                vec![dc_row(A4000, Some(0.17), Some(0.25), &[]), dc_row(ADA, Some(0.20), Some(0.28), &[]), dc_row(R3090, Some(0.22), Some(0.43), &[])],
            ),
            (
                Some("SECURE".into()),
                vec![
                    dc_row(A4000, Some(0.17), Some(0.25), &[("EUR-IS-1", "Low")]),
                    dc_row(ADA, Some(0.20), Some(0.28), &[("EU-RO-1", "Low"), ("US-KS-2", "High")]),
                    dc_row(R3090, Some(0.22), Some(0.43), &[("US-TX-3", "High")]),
                ],
            ),
        ])
    }

    /// `(label, cloud, near?)` in plan order.
    fn where_of(plan: &OptionPlan) -> Vec<(String, Option<String>, bool)> {
        plan.options.iter().map(|o| (o.label.clone(), o.cloud.clone(), o.near.is_some())).collect()
    }

    fn at(label: &str, cloud: &str, near: bool) -> (String, Option<String>, bool) {
        (label.to_string(), Some(cloud.to_string()), near)
    }

    /// Who gets near placement at all: Vast, RunPod with SECURE asked for — never a
    /// COMMUNITY-only RunPod create (so a plain community create stays on the single path),
    /// never Hetzner or Shadeform, never beside the operator's own data-centre pin.
    #[test]
    fn near_is_on_only_where_it_can_place_something() {
        let b = base();
        let r = |clouds: &[&str]| req(&[A4000], clouds, 1, None, Order::Cheapest);
        let on = |r: Request, provider: &str, base: &PodSpec| r.with_near(Some(EU), provider, base).near.is_some();
        assert!(on(r(&["SECURE"]), "runpod", &b));
        assert!(on(r(&["COMMUNITY", "SECURE"]), "runpod", &b));
        assert!(!on(r(&["COMMUNITY"]), "runpod", &b));
        assert!(on(r(&["VERIFIED"]), "vast", &b));
        assert!(!on(r(&["SECURE"]), "hetzner", &b));
        assert!(!on(r(&["SECURE"]), "shadeform", &b));
        let pinned = PodSpec { api_extra: serde_json::json!({"dataCenterIds": ["US-TX-3"]}).as_object().cloned(), ..b.clone() };
        assert!(!on(r(&["SECURE"]), "runpod", &pinned), "the operator's own pin wins");
        assert!(on(r(&["VERIFIED"]), "vast", &pinned), "a RunPod field means nothing to Vast");
        let volume = PodSpec { api_extra: serde_json::json!({"mounts": {"network": [{"volumeId": "v1", "path": "/d"}]}}).as_object().cloned(), ..b.clone() };
        assert!(!on(r(&["SECURE"]), "runpod", &volume), "a network volume lives in one data centre");
        assert!(r(&["SECURE"]).with_near(None, "runpod", &b).near.is_none(), "NEAR=any");
        // A near option to try first makes one GPU and one tier a placement, not the single path…
        assert!(!r(&["SECURE"]).with_near(Some(EU), "runpod", &b).is_single());
        // …but nothing changes for a community create.
        assert!(r(&["COMMUNITY"]).with_near(Some(EU), "runpod", &b).is_single());
    }

    #[test]
    fn runpod_near_options_first_within_the_margin_then_everything_anywhere() {
        // SECURE only, cheapest first: $0.25 is the cheapest, so up to $0.3125 is near enough —
        // the A4000 (EUR-IS-1) and the 4000 Ada (EU-RO-1); the 3090 is in no European data
        // centre right now, so it's only tried anywhere.
        let plan = plan_options(&near_req(&[A4000, ADA, R3090], &["SECURE"], None, Order::Cheapest, EU), "runpod", &v2_book());
        assert_eq!(
            where_of(&plan),
            [
                at("1×RTX A4000", "SECURE", true),
                at("1×RTX 4000 Ada", "SECURE", true),
                at("1×RTX A4000", "SECURE", false),
                at("1×RTX 4000 Ada", "SECURE", false),
                at("1×RTX 3090", "SECURE", false),
            ]
        );
        // The same price as anywhere (RunPod prices per GPU and tier); the stock is the
        // region's; no cap of its own.
        let (near, any) = (&plan.options[1], &plan.options[3]);
        assert_eq!((near.price_per_pod, near.stock.as_deref(), near.cap), (any.price_per_pod, Some("Low"), None));
        assert_eq!(plan.near, Some(EU));
        assert_eq!(
            plan.notes,
            [
                "near the proxy (NEAR=europe): Europe first, then every option anywhere — within +25% of the cheapest ($0.25/h → up to $0.31/h) (NEAR_MAX_EXTRA_PCT)",
                "near, not tried in Europe: 1×RTX 3090 SECURE: RunPod's catalog lists it in no Europe data centre right now",
            ]
        );
        // As listed: each option near, then anywhere — the operator's GPU order first.
        let listed = plan_options(&near_req(&[R3090, A4000], &["SECURE"], None, Order::Listed, EU), "runpod", &v2_book());
        assert_eq!(where_of(&listed), [at("1×RTX 3090", "SECURE", false), at("1×RTX A4000", "SECURE", true), at("1×RTX A4000", "SECURE", false)]);
        assert!(listed.notes[0].contains("(NEAR=europe): each option in Europe first, then anywhere — each within +25% of the same option anywhere"), "{:?}", listed.notes);
    }

    /// The margin is over the cheapest acceptable option: with community in the list, a near
    /// SECURE A4000 ($0.25) is +47% over the community one ($0.17) — not near enough at 25%,
    /// near enough at 50%. COMMUNITY options never get a near twin, and the plan says why.
    #[test]
    fn a_near_option_dearer_than_the_margin_is_tried_anywhere_only() {
        let r = |pct: f64| near_req(&[A4000], &["COMMUNITY", "SECURE"], None, Order::Cheapest, Near { max_extra_pct: pct, ..EU });
        let at25 = plan_options(&r(25.0), "runpod", &v2_book());
        assert_eq!(where_of(&at25), [at("1×RTX A4000", "COMMUNITY", false), at("1×RTX A4000", "SECURE", false)]);
        assert_eq!(
            at25.notes,
            [
                "near the proxy (NEAR=europe): no option to try in Europe — every option anywhere",
                "near: RunPod's COMMUNITY cloud can't be placed by data centre (its hosts have none) — only SECURE options are tried in Europe",
                "near, not tried in Europe: 1×RTX A4000 SECURE in Europe: $0.25/h is more than +25% over $0.17/h",
            ]
        );
        let at50 = plan_options(&r(50.0), "runpod", &v2_book());
        assert_eq!(
            where_of(&at50),
            [at("1×RTX A4000", "SECURE", true), at("1×RTX A4000", "COMMUNITY", false), at("1×RTX A4000", "SECURE", false)]
        );
        // `--max-price` still rules: a cap under the near option's price leaves it out.
        let capped = plan_options(&near_req(&[A4000], &["COMMUNITY", "SECURE"], Some(0.2), Order::Cheapest, Near { max_extra_pct: 50.0, ..EU }), "runpod", &v2_book());
        assert_eq!(where_of(&capped), [at("1×RTX A4000", "COMMUNITY", false)]);
    }

    /// Without a per-data-centre answer (no key: presets; GraphQL on v1) nobody can say the
    /// region lacks it: the near option is planned, stock unknown, and creating is the test.
    #[test]
    fn without_a_per_data_centre_catalog_near_is_tried() {
        for book in [PriceBook::runpod(Vec::new()), PriceBook::runpod(vec![(None, vec![live(A4000, Some(0.17), Some(0.25), Some("High"))])])] {
            let plan = plan_options(&near_req(&[A4000], &["SECURE"], None, Order::Cheapest, EU), "runpod", &book);
            assert_eq!(where_of(&plan), [at("1×RTX A4000", "SECURE", true), at("1×RTX A4000", "SECURE", false)]);
            assert_eq!(plan.options[0].stock, None);
        }
    }

    /// Vast prices per offer: a near option is priced at the cheapest offer in the region,
    /// kept only within the margin over the cheapest acceptable offer, and capped at it — the
    /// create searches again and must not rent dearer than the plan allowed.
    #[test]
    fn vast_near_offers_within_the_margin_first_and_capped() {
        let book = PriceBook::offers(vec![(A4000.into(), 0.109), (R3090.into(), 0.10)], 1)
            .with_near_offers(vec![(A4000.into(), 0.1096), (R3090.into(), 0.20)], 1);
        let plan = plan_options(&near_req(&[A4000, R3090, ADA], &["VERIFIED"], None, Order::Cheapest, EU), "vast", &book);
        let got: Vec<(String, bool, Option<i64>, Option<i64>)> =
            plan.options.iter().map(|o| (o.label.clone(), o.near.is_some(), o.price_per_pod.map(milli), o.cap.map(milli))).collect();
        let row = |l: &str, near: bool, p: Option<i64>, cap: Option<i64>| (l.to_string(), near, p, cap);
        assert_eq!(
            got,
            [
                row("1×RTX A4000", true, Some(110), Some(125)), // $0.1096 ≤ $0.10 × 1.25
                row("1×RTX 3090", false, Some(100), None),
                row("1×RTX A4000", false, Some(109), None),
                row("1×RTX 4000 Ada", false, None, None),
            ]
        );
        let why: Vec<&String> = plan.notes.iter().filter(|n| n.starts_with("near, not tried")).collect();
        assert_eq!(
            why,
            [
                "near, not tried in Europe: 1×RTX 3090 VERIFIED in Europe: $0.20/h is more than +25% over $0.10/h",
                "near, not tried in Europe: 1×RTX 4000 Ada VERIFIED: no matching offer in Europe right now",
            ]
        );
        // Under a tighter --max-price, the cap is that.
        let capped = plan_options(&near_req(&[A4000], &["VERIFIED"], Some(0.115), Order::Cheapest, EU), "vast", &book);
        assert_eq!(capped.options[0].cap.map(milli), Some(115));
        // As listed, the margin is over the same card anywhere: the 3090's $0.20 is +100%.
        let listed = plan_options(&near_req(&[R3090, A4000], &["VERIFIED"], None, Order::Listed, EU), "vast", &book);
        let order: Vec<(String, bool)> = listed.options.iter().map(|o| (o.label.clone(), o.near.is_some())).collect();
        assert_eq!(order, [("1×RTX 3090".to_string(), false), ("1×RTX A4000".into(), true), ("1×RTX A4000".into(), false)]);
        assert_eq!(listed.options[1].cap.map(milli), Some(136), "$0.109 × 1.25");
    }

    /// Review (codex, claude): when the search anywhere failed there's no price to weigh a
    /// near Vast offer against, so no margin to cap its create at — it isn't planned (its
    /// create could otherwise rent any price there).
    #[test]
    fn a_vast_near_option_needs_a_price_anywhere_to_cap_it() {
        let book = PriceBook::offers(Vec::new(), 1).with_near_offers(vec![(A4000.into(), 0.11)], 1);
        for order in [Order::Cheapest, Order::Listed] {
            let plan = plan_options(&near_req(&[A4000], &["VERIFIED"], None, order, EU), "vast", &book);
            assert!(plan.options.iter().all(|o| o.near.is_none()), "{order:?}: {:?}", plan.options);
            assert!(plan.notes.iter().any(|n| n == "near, not tried in Europe: 1×RTX A4000 VERIFIED in Europe: no price anywhere to weigh it against"), "{:?}", plan.notes);
        }
        // Every near Vast option that is planned carries a finite cap.
        let priced = PriceBook::offers(vec![(A4000.into(), 0.10)], 1).with_near_offers(vec![(A4000.into(), 0.11)], 1);
        let plan = plan_options(&near_req(&[A4000], &["VERIFIED"], None, Order::Cheapest, EU), "vast", &priced);
        assert!(plan.options.iter().filter(|o| o.near.is_some()).all(|o| o.cap.is_some_and(f64::is_finite)));
    }

    /// Review (claude): the catalog's per-data-centre stock decides — a region listed only at
    /// `None`, or only in a data centre the create doesn't pin (one RunPod added since the
    /// table), costs a create that can't succeed near; and the plan says the table wants it.
    #[test]
    fn a_runpod_region_listed_without_stock_or_unpinned_isnt_tried_near() {
        let book = |dcs: &[(&str, &str)]| PriceBook::runpod(vec![(Some("SECURE".into()), vec![dc_row(A4000, Some(0.17), Some(0.25), dcs)])]);
        let near_of = |b: &PriceBook| plan_options(&near_req(&[A4000], &["SECURE"], None, Order::Cheapest, EU), "runpod", b);
        // (data centres listed, near planned?, its stock)
        for (dcs, planned, stock) in [
            (&[("EUR-IS-1", "Low"), ("US-KS-2", "High")][..], true, Some("Low")),
            (&[("EUR-IS-1", "Low"), ("EU-RO-1", "Medium")], true, Some("Medium")),
            (&[("EUR-IS-1", "None"), ("US-KS-2", "High")], false, None),
            (&[("EU-DE-1", "High")], false, None),
            (&[("US-KS-2", "High")], false, None),
        ] {
            let plan = near_of(&book(dcs));
            assert_eq!(plan.options[0].near.is_some(), planned, "{dcs:?}");
            if planned {
                assert_eq!(plan.options[0].stock.as_deref(), stock, "{dcs:?}");
            }
        }
        let unpinned = near_of(&book(&[("EU-DE-1", "High")]));
        assert!(
            unpinned.notes.iter().any(|n| n.starts_with("near: RunPod lists EU-DE-1 in Europe, which arena's table doesn't pin yet")),
            "{:?}",
            unpinned.notes
        );
        assert!(!near_of(&book(&[("EUR-IS-1", "Low")])).notes.iter().any(|n| n.contains("doesn't pin")));
    }

    /// Review (codex): cheapest first, an unpriced option (a GPU no catalog or preset prices)
    /// gets no near twin — it would jump the queue of priced options at an unknown price —
    /// and stays last, anywhere. As listed its twin is just the same option pinned: kept.
    #[test]
    fn an_unpriced_option_doesnt_jump_the_queue_near() {
        const MYSTERY: &str = "NVIDIA Mystery 9";
        let cheapest = plan_options(&near_req(&[A4000, MYSTERY], &["SECURE"], None, Order::Cheapest, EU), "runpod", &PriceBook::runpod(Vec::new()));
        let got: Vec<(String, bool)> = cheapest.options.iter().map(|o| (o.label.clone(), o.near.is_some())).collect();
        assert_eq!(got, [("1×RTX A4000".to_string(), true), ("1×RTX A4000".into(), false), ("1×Mystery 9".into(), false)]);
        assert!(cheapest.notes.iter().any(|n| n == "near, not tried in Europe: 1×Mystery 9 SECURE in Europe: no price anywhere to weigh it against"), "{:?}", cheapest.notes);
        let listed = plan_options(&near_req(&[MYSTERY, A4000], &["SECURE"], None, Order::Listed, EU), "runpod", &PriceBook::runpod(Vec::new()));
        let got: Vec<(String, bool)> = listed.options.iter().map(|o| (o.label.clone(), o.near.is_some())).collect();
        assert_eq!(got, [("1×Mystery 9".to_string(), true), ("1×Mystery 9".into(), false), ("1×RTX A4000".into(), true), ("1×RTX A4000".into(), false)]);
    }

    /// A data centre listed without a level is listed (stock unknown) — not "None", which
    /// would turn near off on a field RunPod left out (review: claude).
    #[test]
    fn a_data_centre_without_a_level_still_counts() {
        let book = PriceBook::runpod(vec![(Some("SECURE".into()), vec![dc_row(A4000, Some(0.17), Some(0.25), &[("EUR-IS-1", "")])])]);
        let plan = plan_options(&near_req(&[A4000], &["SECURE"], None, Order::Cheapest, EU), "runpod", &book);
        assert_eq!((plan.options[0].near, plan.options[0].stock.as_deref()), (Some(Region::Europe), None));
    }

    /// Near off (`NEAR=any`): the plan is exactly as before — no twin, no column, no note.
    #[test]
    fn near_off_plans_as_before() {
        let r = req(&[A4000, ADA], &["SECURE"], 1, None, Order::Cheapest);
        let off = plan_options(&r, "runpod", &v2_book());
        assert_eq!(off.options.len(), 2);
        assert!(off.options.iter().all(|o| o.near.is_none() && o.cap.is_none()) && off.near.is_none() && off.notes.is_empty());
        assert!(!render_plan(&off).contains("WHERE"));
        let v = serde_json::to_value(&off).unwrap();
        assert!(v.get("near").is_none() && v["options"][0].get("near").is_none() && v["options"][0].get("cap").is_none());
    }

    #[test]
    fn near_plan_table_and_json_snapshot() {
        let plan = plan_options(&near_req(&[A4000, R3090], &["SECURE"], None, Order::Cheapest, EU), "runpod", &v2_book());
        let want = "\
#  OPTION       CLOUD   WHERE   $/H/POD  PRICE  STOCK
1  1×RTX A4000  SECURE  Europe    $0.25  live   Low
2  1×RTX A4000  SECURE  any       $0.25  live   Low
3  1×RTX 3090   SECURE  any       $0.43  live   Low
note: near the proxy (NEAR=europe): Europe first, then every option anywhere — within +25% of the cheapest ($0.25/h → up to $0.31/h) (NEAR_MAX_EXTRA_PCT)
note: near, not tried in Europe: 1×RTX 3090 SECURE: RunPod's catalog lists it in no Europe data centre right now
";
        let out = render_plan(&plan);
        assert_eq!(out, want, "\n--- got ---\n{out}");
        let v = serde_json::to_value(&plan).unwrap();
        assert_eq!(v["near"], serde_json::json!({"region": "europe", "max_extra_pct": 25.0}));
        assert_eq!((v["options"][0]["near"].as_str(), v["options"][1].get("near")), (Some("europe"), None));
        let back: OptionPlan = serde_json::from_value(v).unwrap();
        assert_eq!(back, plan);
        // What a create of each is: the near one pinned to the region.
        let near = spec_for(&base(), "devtest-apple", &plan.options[0]);
        let any = spec_for(&base(), "devtest-apple", &plan.options[1]);
        assert_eq!((near.region, near.cloud_type.as_str(), any.region), (Some(Region::Europe), "SECURE", None));
        assert_eq!(plan.options[0].describe(), "1×RTX A4000 SECURE in Europe");
        assert_eq!(plan.options[1].describe(), "1×RTX A4000 SECURE");
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
        /// RunPod v1's schema 400 (a GPU its create enum lacks), as the backend classifies it.
        Schema,
        /// RunPod v2's 422 "Unknown GPU type" — a config error.
        Unknown,
        /// Hetzner's 403 `resource_limit_exceeded` (live), as its backend reads it.
        Quota,
    }

    /// A provider whose create answers per (gpu, cloud) from a script (then `fallback`), and
    /// which records every create and lists what it made (plus `fleet`).
    struct Fake {
        script: Mutex<HashMap<(String, String), VecDeque<Reply>>>,
        fallback: Reply,
        calls: Mutex<Vec<(String, String, String, u32)>>,
        /// `max_price` per create call.
        caps: Mutex<Vec<Option<f64>>>,
        fleet: Mutex<Vec<Pod>>,
        list_fails: bool,
        /// A second backend (Vast) that fails to list while this one answers: the aggregate
        /// `list_pods` (like the fleet provider's) skips it, `list_by_provider` says so.
        vast_down: bool,
        /// How many more `list_by_provider` calls find Vast throttled (a 429 — retryable).
        vast_throttled: Mutex<u32>,
        /// How many more `list_by_provider` calls find Vast's list stalled past its bound.
        vast_stalled: Mutex<u32>,
    }

    impl Fake {
        fn new(fallback: Reply) -> Self {
            Fake {
                script: Default::default(),
                fallback,
                calls: Default::default(),
                caps: Default::default(),
                fleet: Default::default(),
                list_fails: false,
                vast_down: false,
                vast_throttled: Mutex::new(0),
                vast_stalled: Mutex::new(0),
            }
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
        async fn list_by_provider(&self) -> Vec<(String, Result<Vec<Pod>>)> {
            let mut out = vec![("runpod".to_string(), self.list_pods().await)];
            if self.vast_down {
                out.push(("vast".into(), Err(Error::provider("vast list HTTP 404: {\"error\": \"auth_error\"}"))));
            }
            let throttled = {
                let mut left = self.vast_throttled.lock().unwrap();
                let was = *left;
                *left = was.saturating_sub(1);
                was > 0
            };
            if throttled {
                let e = Error::Provider { kind: ProviderErrorKind::RateLimited, message: "vast list HTTP 429 Too Many Requests".into() };
                out.push(("vast".into(), Err(e)));
            }
            let stalled = {
                let mut left = self.vast_stalled.lock().unwrap();
                let was = *left;
                *left = was.saturating_sub(1);
                was > 0
            };
            if stalled {
                // The real bound, on a list that never answers (the paused clock runs it out).
                let listed = crate::provider::bounded_list("vast", std::future::pending(), crate::provider::LIST_TIMEOUT).await;
                out.push(("vast".into(), listed));
            }
            out
        }
        async fn create_pod(&self, spec: &PodSpec) -> Result<Pod> {
            // A near create is told apart by its tier: `SECURE@europe` (scripts and calls alike).
            let cloud = match spec.region {
                Some(r) => format!("{}@{}", spec.cloud_type, r.key()),
                None => spec.cloud_type.clone(),
            };
            self.calls.lock().unwrap().push((spec.name.clone(), spec.gpu_type.clone(), cloud.clone(), spec.gpu_count));
            self.caps.lock().unwrap().push(spec.max_price);
            let key = (spec.gpu_type.clone(), cloud);
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
                Reply::Schema => Err(Error::provider_http(
                    reqwest::StatusCode::BAD_REQUEST,
                    &serde_json::json!([{"error": "At #/paths/pods/post …, it does not meet the schema requirements of the specification. Suggestion: …",
                                         "problems": ["At /pods/properties/gpuTypeIds/items: value must be one of …"]}]),
                    "create pod",
                )),
                Reply::Unknown => Err(Error::Config("create pod HTTP 422: {\"detail\":\"Unknown GPU type: NVIDIA X\"}".into())),
                Reply::Quota => Err(crate::provider::hetzner::recorded::limit_refusal("hetzner create")),
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
        assert_eq!(lines[0], "[no capacity] arena8-apple on 1×RTX A4000 COMMUNITY: provider error: create pod HTTP 500: There are no instances currently available — trying the next option");
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

    /// Near the proxy, the SECURE A4000 in Europe, then anywhere — as `up` plans it with
    /// `--cloud secure` and NEAR on.
    fn near_two() -> OptionPlan {
        plan_options(&near_req(&[A4000], &["SECURE"], None, Order::Cheapest, EU), "runpod", &v2_book())
    }

    const NEAR_SECURE: &str = "SECURE@europe";

    #[tokio::test]
    async fn near_with_stock_places_there_first() {
        let fake = Fake::new(Reply::Ok);
        let plan = near_two();
        let (out, lines) = run(&fake, &plan, &["devtest-near-apple"], ONE_ROUND, None).await;
        assert!(matches!(out.end, End::Filled), "{:?}", out.end);
        assert_eq!(fake.calls(), [call("devtest-near-apple", A4000, NEAR_SECURE)], "one create, pinned to Europe");
        assert_eq!(out.log[0].placed, Some(0));
        assert_eq!(lines, ["[created] devtest-near-apple id=id1 on 1×RTX A4000 SECURE in Europe ($0.25/h)"]);
        assert!(render_summary(&plan, &out).contains("created on 1×RTX A4000 SECURE in Europe ($0.25/h)"));
    }

    /// RunPod rate-limits per key (shared by every dashboard and cron on it): a full region
    /// must cost one create per run, not one per name. 10 pods with Europe full: 11 creates —
    /// the first name finds Europe full, every name then goes straight to anywhere. Without
    /// near the same run is 10.
    #[tokio::test]
    async fn near_full_falls_through_to_anywhere_and_costs_one_create_per_run() {
        let names: Vec<String> = (0..10).map(|i| format!("devtest-near-{i}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let fake = Fake::new(Reply::Ok).script(A4000, NEAR_SECURE, &[Reply::Capacity, Reply::Ok, Reply::Ok]);
        let plan = near_two();
        let (out, lines) = run(&fake, &plan, &names, ONE_ROUND, None).await;
        assert!(matches!(out.end, End::Filled), "{:?}", out.end);
        let calls = fake.calls();
        assert_eq!(calls.len(), 11, "{calls:?}");
        assert_eq!(calls[0], call("devtest-near-0", A4000, NEAR_SECURE));
        assert!(calls[1..].iter().all(|(_, g, c)| g == A4000 && c == "SECURE"), "the rest anywhere: {calls:?}");
        assert_eq!(out.created.len(), 10);
        assert_eq!(lines[0], "[no capacity] devtest-near-0 on 1×RTX A4000 SECURE in Europe: provider error: create pod HTTP 500: There are no instances currently available — trying the next option");
        assert!(render_summary(&plan, &out).contains("created on 1×RTX A4000 SECURE ($0.25/h) (after 1×RTX A4000 SECURE in Europe: capacity)"));
        // Near off: the same run, 10 creates.
        let off = Fake::new(Reply::Ok);
        let plain = plan_options(&req(&[A4000], &["SECURE"], 1, None, Order::Cheapest), "runpod", &v2_book());
        run(&off, &plain, &names, ONE_ROUND, None).await;
        assert_eq!(off.calls().len(), 10);
    }

    /// A full region stays out for the whole run: retry rounds clear capacity blocks, but a
    /// later round goes straight to anywhere (which still covers Europe).
    #[tokio::test(start_paused = true)]
    async fn a_full_region_is_not_retried_in_later_rounds() {
        let fake = Fake::new(Reply::Ok)
            .script(A4000, NEAR_SECURE, &[Reply::Capacity, Reply::Ok])
            .script(A4000, "SECURE", &[Reply::Capacity, Reply::Ok, Reply::Ok]);
        let rounds = Rounds { window: Duration::from_secs(600), every: Duration::from_secs(60) };
        let (out, lines) = run(&fake, &near_two(), &["devtest-near-apple", "devtest-near-bloom"], rounds, None).await;
        assert!(matches!(out.end, End::Filled), "{:?}", out.end);
        assert_eq!(
            fake.calls(),
            [
                call("devtest-near-apple", A4000, NEAR_SECURE), // full: out for the run
                call("devtest-near-apple", A4000, "SECURE"),    // full this round
                // Round 2: anywhere only.
                call("devtest-near-apple", A4000, "SECURE"),
                call("devtest-near-bloom", A4000, "SECURE"),
            ]
        );
        assert_eq!(lines[1], "[no capacity] devtest-near-apple on 1×RTX A4000 SECURE: provider error: create pod HTTP 500: There are no instances currently available — no option left this round");
    }

    /// Review (claude): a near Vast "no capacity" is one market search's answer — blocked for
    /// the round, tried again the next (a RunPod region stays out for the run).
    #[tokio::test(start_paused = true)]
    async fn a_marketplace_region_is_retried_the_next_round() {
        let book = PriceBook::offers(vec![(A4000.into(), 0.10)], 1).with_near_offers(vec![(A4000.into(), 0.11)], 1);
        let plan = plan_options(&near_req(&[A4000], &["VERIFIED"], None, Order::Cheapest, EU), "vast", &book);
        let fake = Fake::new(Reply::Capacity).script(A4000, "VERIFIED@europe", &[Reply::Capacity, Reply::Ok]);
        let rounds = Rounds { window: Duration::from_secs(600), every: Duration::from_secs(60) };
        let (out, _) = run(&fake, &plan, &["devtest-near-apple"], rounds, None).await;
        assert!(matches!(out.end, End::Filled), "{:?}", out.end);
        let clouds: Vec<String> = fake.calls().into_iter().map(|c| c.2).collect();
        assert_eq!(clouds, ["VERIFIED@europe", "VERIFIED", "VERIFIED@europe"], "round 2 tries Europe again, and gets it");
    }

    /// Without a near search (no Vast key: nothing searched) the plan claims nothing about the
    /// region — review (claude): it said "no matching offer in Europe right now".
    #[test]
    fn an_unsearched_region_is_said_so() {
        let plan = plan_options(&near_req(&[A4000], &["VERIFIED"], None, Order::Cheapest, EU), "vast", &PriceBook::offers(vec![(A4000.into(), 0.10)], 1));
        assert!(plan.options.iter().all(|o| o.near.is_none()));
        assert_eq!(plan.notes, ["near the proxy (NEAR=europe): Europe not searched — every option anywhere"]);
    }

    /// A near Vast option carries its margin as the create's cap (the marketplace picks the
    /// offer, and so the price, only then) — the tighter of it and `--max-price`; its anywhere
    /// twin only `--max-price`.
    #[tokio::test]
    async fn a_near_marketplace_create_is_capped_at_the_margin() {
        let book = PriceBook::offers(vec![(A4000.into(), 0.10)], 1).with_near_offers(vec![(A4000.into(), 0.11)], 1);
        let plan = plan_options(&near_req(&[A4000], &["VERIFIED"], Some(0.30), Order::Cheapest, EU), "vast", &book);
        let fake = Fake::new(Reply::Capacity);
        let (out, _) = run(&fake, &plan, &["devtest-near-apple"], ONE_ROUND, None).await;
        assert!(matches!(out.end, End::Exhausted), "{:?}", out.end);
        let caps: Vec<Option<i64>> = fake.caps.lock().unwrap().iter().map(|c| c.map(milli)).collect();
        assert_eq!(caps, [Some(125), Some(300)], "near: $0.10 × 1.25; anywhere: --max-price");
        assert_eq!(fake.calls()[0].2, "VERIFIED@europe");
    }

    /// The run ends once nothing is left to try ever again: Europe full (out for the run) and
    /// the pool anywhere refused — not idled out to the window's end.
    #[tokio::test(start_paused = true)]
    async fn a_full_region_beside_a_refused_pool_ends_the_run() {
        let fake = Fake::new(Reply::Ok).script(A4000, NEAR_SECURE, &[Reply::Capacity]).script(A4000, "SECURE", &[Reply::Schema]);
        let rounds = Rounds { window: Duration::from_secs(3600), every: Duration::from_secs(60) };
        let (out, _) = run(&fake, &near_two(), &["devtest-near-apple"], rounds, None).await;
        assert!(matches!(out.end, End::Failed { .. }), "{:?}", out.end);
        assert_eq!(fake.calls().len(), 2);
        assert_eq!(out.rounds, 1);
    }

    /// Review (claude): Europe full, then the same pool anywhere refused access — that's the
    /// key's problem, as without near (`End::Aborted`), not a mixed "invalid request" failure.
    #[tokio::test]
    async fn a_full_region_then_no_access_aborts_as_an_access_problem() {
        let fake = Fake::new(Reply::Ok).script(A4000, NEAR_SECURE, &[Reply::Capacity]).script(A4000, "SECURE", &[Reply::Denied]);
        let (out, _) = run(&fake, &near_two(), &["devtest-near-apple", "devtest-near-bloom"], ONE_ROUND, None).await;
        match &out.end {
            End::Aborted { error, .. } => assert!(error.to_string().contains("every option was refused (no access)"), "{error}"),
            other => panic!("expected an access abort, got {other:?}"),
        }
        assert_eq!(fake.calls().len(), 2);
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

    /// Finding #22: a per-option 400 (v1's schema refusal of a GPU its enum lacks, v2's 422
    /// "Unknown GPU type") skips that option — for the rest of the run — instead of stopping
    /// with the other options untried; once every option is refused the run fails.
    #[tokio::test(start_paused = true)]
    async fn an_option_the_api_rejects_is_skipped_not_fatal() {
        for bad in [Reply::Schema, Reply::Unknown] {
            let fake = Fake::new(Reply::Ok).script(A4000, "COMMUNITY", &[bad, bad, bad]);
            let (out, lines) = run(&fake, &three(), &["arena8-apple", "arena8-bloom"], ONE_ROUND, None).await;
            assert!(matches!(out.end, End::Filled), "{bad:?}: {:?}", out.end);
            assert_eq!(
                fake.calls(),
                [
                    call("arena8-apple", A4000, "COMMUNITY"),
                    call("arena8-apple", R3090, "COMMUNITY"),
                    call("arena8-bloom", R3090, "COMMUNITY"), // the rejected option isn't tried again
                ],
                "{bad:?}"
            );
            assert!(lines[0].starts_with("[rejected] arena8-apple on 1×RTX A4000 COMMUNITY: "), "{}", lines[0]);
            assert!(lines[0].ends_with("— skipping this option, trying the next"), "{}", lines[0]);
            let summary = render_summary(&three(), &out);
            assert!(summary.contains("(after 1×RTX A4000 COMMUNITY: rejected)"), "{summary}");
        }
        // Every option refused: the run fails, naming the last refusal — never a hang or a retry.
        let fake = Fake::new(Reply::Schema);
        let rounds = Rounds { window: Duration::from_secs(600), every: Duration::from_secs(60) };
        let (out, _) = run(&fake, &three(), &["arena8-apple"], rounds, None).await;
        match &out.end {
            End::Failed { name: Some(n), error } => {
                assert_eq!(n, "arena8-apple");
                assert!(error.to_string().contains("every option was refused") && error.to_string().contains("problems:"), "{error}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(fake.calls().len(), 3, "each option once");
    }

    /// Review fix: the end-of-options check ran on a `Rejected` refusal only, so with the last
    /// open option refused as `Denied` (the others rejected) the run went on — and idled out
    /// its whole `--retry-mins` window with no create, saying "no capacity" every round.
    #[tokio::test(start_paused = true)]
    async fn rejected_and_denied_options_in_any_order_end_the_run_at_once() {
        let rounds = Rounds { window: Duration::from_secs(600), every: Duration::from_secs(60) };
        for replies in [
            [Reply::Schema, Reply::Denied, Reply::Denied],
            [Reply::Denied, Reply::Schema, Reply::Denied],
            [Reply::Denied, Reply::Denied, Reply::Schema],
            [Reply::Unknown, Reply::Denied, Reply::Unknown],
        ] {
            let fake = Fake::new(Reply::Ok)
                .script(A4000, "COMMUNITY", &[replies[0]])
                .script(R3090, "COMMUNITY", &[replies[1]])
                .script(A4000, "SECURE", &[replies[2]]);
            let started = tokio::time::Instant::now();
            let (out, _) = run(&fake, &three(), &["arena8-apple", "arena8-bloom"], rounds, None).await;
            match &out.end {
                End::Failed { name: Some(n), error } => {
                    assert_eq!(n, "arena8-apple", "{replies:?}");
                    assert!(error.to_string().contains("every option was refused"), "{replies:?}: {error}");
                }
                other => panic!("{replies:?}: {other:?}"),
            }
            assert_eq!((out.rounds, fake.calls().len()), (1, 3), "{replies:?}: each option once, then stop");
            assert_eq!(started.elapsed(), Duration::ZERO, "{replies:?}: no wait for a window nothing can use");
        }
    }

    /// Hetzner's project limit (live 2026-10-29: "shared core limit exceeded", which read as
    /// "authentication failed"): the key works, so it never aborts the run — the option that hit
    /// the limit is out for the run and the next is tried (a smaller one may fit); once every
    /// option has hit it, the run ends at once (no retry round frees a limit), saying it's the
    /// account's limits, still a `Quota`.
    #[tokio::test(start_paused = true)]
    async fn the_accounts_limit_skips_the_option_and_never_aborts_or_waits() {
        let rounds = Rounds { window: Duration::from_secs(600), every: Duration::from_secs(60) };
        let fake = Fake::new(Reply::Ok).script(A4000, "COMMUNITY", &[Reply::Quota]);
        let (out, lines) = run(&fake, &three(), &["arena8-apple", "arena8-bloom"], rounds, None).await;
        assert!(matches!(out.end, End::Filled), "{:?}", out.end);
        assert_eq!(out.created.len(), 2);
        assert_eq!(fake.calls().iter().filter(|(_, g, c)| g == A4000 && c == "COMMUNITY").count(), 1, "out for the run: {:?}", fake.calls());
        assert!(lines.iter().any(|l| l.starts_with("[over the limit] arena8-apple on ") && l.contains("Hetzner refused: the project's resource limit is reached") && l.ends_with("— skipping this option, trying the next")), "{lines:#?}");

        let fake = Fake::new(Reply::Quota);
        let started = tokio::time::Instant::now();
        let (out, _) = run(&fake, &three(), &["arena8-apple", "arena8-bloom"], rounds, None).await;
        match &out.end {
            End::Failed { name: Some(n), error } => {
                assert_eq!(n, "arena8-apple");
                assert_eq!(error.kind(), Some(ProviderErrorKind::Quota), "{error}");
                assert!(error.to_string().starts_with("provider error: no option fits the account's limits — last: hetzner create HTTP 403"), "{error}");
                assert!(!error.to_string().contains("invalid request"), "{error}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!((out.rounds, fake.calls().len()), (1, 3), "each option once, then stop");
        assert_eq!(started.elapsed(), Duration::ZERO, "no wait for a window nothing can use");
        let summary = render_summary(&three(), &out);
        assert!(summary.contains("over the account's limit"), "{summary}");
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

    #[test]
    fn next_round_in_window_table() {
        let t0 = tokio::time::Instant::now();
        let s = Duration::from_secs;
        // (now offset, every, window, another round?)
        for (now, every, window, want) in [
            (0, 60, 120, true),   // t=60 is inside
            (60, 60, 120, true),  // t=120: exactly at the deadline is still inside
            (61, 60, 120, false), // t=121 would be past it
            (0, 600, 60, false),  // interval longer than the window
            (120, 0, 120, false), // already at the deadline: no new round
            (0, 0, 0, false),     // zero window = one round only
        ] {
            assert_eq!(next_round_in_window(t0 + s(now), s(every), t0 + s(window)), want, "{now} {every} {window}");
        }
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

    /// Finding #9: a retry round re-plans from a listing — with one backend not answering,
    /// a name held there would read as free. The round must not run.
    #[tokio::test(start_paused = true)]
    async fn a_relist_missing_a_provider_ends_the_run_instead_of_risking_a_duplicate() {
        let mut fake = Fake::new(Reply::Capacity);
        fake.vast_down = true;
        let rounds = Rounds { window: Duration::from_secs(600), every: Duration::from_secs(60) };
        let (out, _) = run(&fake, &three(), &["arena8-apple"], rounds, None).await;
        match &out.end {
            End::Failed { name: None, error } => {
                let e = error.to_string();
                assert!(e.contains("vast failed to list") && e.contains("refusing to create"), "{e}");
            }
            other => panic!("expected the partial relist to end the run, got {other:?}"),
        }
        assert_eq!(fake.calls().len(), 3, "only round 1 ran");
    }

    /// Review fix: a backend throttled (or 5xx-ing) past the re-list's own retries used to end
    /// the run — every unplaced name of a `--retry-mins` cohort start, or an `up --check`
    /// replacement whose old pod was already gone. It skips that round instead (nothing is
    /// created from the partial listing) and the next round lists again; throttled for the
    /// whole window, the run just ends with it.
    #[tokio::test(start_paused = true)]
    async fn a_relist_missing_a_provider_that_may_recover_skips_the_round() {
        let dry = || {
            Fake::new(Reply::Ok)
                .script(A4000, "COMMUNITY", &[Reply::Capacity])
                .script(R3090, "COMMUNITY", &[Reply::Capacity])
                .script(A4000, "SECURE", &[Reply::Capacity])
        };
        let rounds = Rounds { window: Duration::from_secs(600), every: Duration::from_secs(60) };
        // The first re-list and its 4 retries (1+2+4+8 = 15s of backoff) find Vast throttled.
        let fake = dry();
        *fake.vast_throttled.lock().unwrap() = 5;
        let (out, lines) = run(&fake, &three(), &["arena8-apple"], rounds, None).await;
        assert!(matches!(out.end, End::Filled), "{:?}", out.end);
        assert_eq!(out.rounds, 3, "round 2 was skipped, round 3 placed it");
        assert_eq!(fake.calls().len(), 4, "round 1's three tries, then one create in round 3");
        let placed = out.log[0].attempts.last().unwrap();
        assert_eq!((placed.round, placed.at), (3, Duration::from_secs(60 + 15 + 60)), "{:?}", out.log[0].attempts);
        let skipped = lines.iter().find(|l| l.starts_with("round 2: nothing created — ")).unwrap_or_else(|| panic!("{lines:#?}"));
        assert!(skipped.contains("vast failed to list") && skipped.contains("429"), "{skipped}");
        assert!(lines.contains(&"round 2: 1 name(s) not placed (fleet listing incomplete); retrying in 60s (Ctrl+C to stop)…".to_string()), "{lines:#?}");

        // Throttled for good: no create after round 1, and the window — not the outage — ends it.
        let fake = dry();
        *fake.vast_throttled.lock().unwrap() = u32::MAX;
        let (out, _) = run(&fake, &three(), &["arena8-apple"], rounds, None).await;
        assert!(matches!(out.end, End::WindowElapsed), "{:?}", out.end);
        assert_eq!(fake.calls().len(), 3, "only round 1 created");
    }

    /// Review fix: a backend whose list stalls past its bound (`LIST_TIMEOUT`) isn't a refusal
    /// either — it used to end the run like a rejected key. It skips that round like a
    /// throttled backend, and isn't re-listed in place (each try already took the whole bound):
    /// the next round's listing is its retry.
    #[tokio::test(start_paused = true)]
    async fn a_relist_with_a_stalled_provider_skips_the_round() {
        let fake = Fake::new(Reply::Ok)
            .script(A4000, "COMMUNITY", &[Reply::Capacity])
            .script(R3090, "COMMUNITY", &[Reply::Capacity])
            .script(A4000, "SECURE", &[Reply::Capacity]);
        *fake.vast_stalled.lock().unwrap() = 1;
        let rounds = Rounds { window: Duration::from_secs(600), every: Duration::from_secs(60) };
        let (out, lines) = run(&fake, &three(), &["arena8-apple"], rounds, None).await;
        assert!(matches!(out.end, End::Filled), "{:?}", out.end);
        assert_eq!((out.rounds, fake.calls().len()), (3, 4), "round 2 skipped, round 3 placed it");
        let placed = out.log[0].attempts.last().unwrap();
        assert_eq!((placed.round, placed.at), (3, Duration::from_secs(60 + 60 + 60)), "one 60s stall, not five");
        let skipped = lines.iter().find(|l| l.starts_with("round 2: nothing created — ")).unwrap_or_else(|| panic!("{lines:#?}"));
        assert!(skipped.contains("vast list timed out after 60s"), "{skipped}");
    }

    /// Review fix: a listing slow enough to outlast the window (its retries' backoff) runs no
    /// round after it — a round starts only inside the window.
    #[tokio::test(start_paused = true)]
    async fn no_round_runs_after_a_relist_that_outlasted_the_window() {
        let fake = Fake::new(Reply::Ok).script(A4000, "COMMUNITY", &[Reply::Capacity]).script(R3090, "COMMUNITY", &[Reply::Capacity]).script(A4000, "SECURE", &[Reply::Capacity]);
        // Round 2 may start at t=60 (window 62s); its re-list takes 3s more (two throttled tries).
        *fake.vast_throttled.lock().unwrap() = 2;
        let rounds = Rounds { window: Duration::from_secs(62), every: Duration::from_secs(60) };
        let (out, _) = run(&fake, &three(), &["arena8-apple"], rounds, None).await;
        assert!(matches!(out.end, End::WindowElapsed), "{:?}", out.end);
        assert_eq!(fake.calls().len(), 3, "no create at t=63, past the 62s window");
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

    /// A Ctrl+C during the re-list before a round (slow here: a throttled backend's retries)
    /// ends the run there. It used to be seen only during the sleep, so the round after the
    /// listing still created (billed) — what the dashboard's `[s]` "stops at once" then wasn't.
    #[tokio::test(start_paused = true)]
    async fn an_interrupt_during_the_relist_creates_nothing_more() {
        let fake = Fake::new(Reply::Ok).script(A4000, "COMMUNITY", &[Reply::Capacity]).script(R3090, "COMMUNITY", &[Reply::Capacity]).script(A4000, "SECURE", &[Reply::Capacity]);
        // Round 2 may start at t=60; its re-list takes 3s more (two throttled tries). Ctrl+C at t=61.
        *fake.vast_throttled.lock().unwrap() = 2;
        let start = tokio::time::Instant::now();
        let pressed_at = start + Duration::from_secs(61);
        let rounds = Rounds { window: Duration::from_secs(600), every: Duration::from_secs(60) };
        let mut sink = |_: &Progress| {};
        let n = names(&["arena8-apple"]);
        let out = place(&fake, &base(), &n, &three(), rounds, None, move || tokio::time::sleep_until(pressed_at), &mut sink).await;
        assert!(matches!(out.end, End::Interrupted), "{:?}", out.end);
        assert_eq!(fake.calls().len(), 3, "round 1's three tries only: no create after the press");
        assert!(out.created.is_empty());
        assert_eq!((out.rounds, start.elapsed()), (1, Duration::from_secs(61)), "stopped at the press, mid-listing");
    }

    /// The cap reaches every create: a marketplace backend picks the machine (and price)
    /// only then, and must not rent above what the operator agreed to.
    #[tokio::test]
    async fn every_create_carries_the_plans_cap() {
        let fake = Fake::new(Reply::Ok).script(A4000, "COMMUNITY", &[Reply::Capacity]);
        let (out, _) = run(&fake, &three(), &["arena8-apple", "arena8-bloom"], ONE_ROUND, None).await;
        assert!(matches!(out.end, End::Filled), "{:?}", out.end);
        assert_eq!(*fake.caps.lock().unwrap(), [Some(0.30), Some(0.30), Some(0.30)]);
        let uncapped = plan_options(&req(&[A4000], &["COMMUNITY"], 1, None, Order::Cheapest), "runpod", &book());
        let fake = Fake::new(Reply::Ok);
        run(&fake, &uncapped, &["arena8-apple"], ONE_ROUND, None).await;
        assert_eq!(*fake.caps.lock().unwrap(), [None]);
    }

    #[test]
    fn spec_for_applies_the_option_over_the_base() {
        let plan = three();
        let o = &plan.options[2]; // A4000 SECURE
        let s = spec_for(&base(), "arena8-apple", o);
        assert_eq!((s.name.as_str(), s.gpu_type.as_str(), s.cloud_type.as_str(), s.gpu_count), ("arena8-apple", A4000, "SECURE", 1));
        assert_eq!(s.env, [("MACHINE_NAME".to_string(), "arena8-apple".to_string())]);
        assert_eq!((s.image.as_str(), s.disk_gb), ("img:1", 50)); // the rest is the base's
        // Vast's tier is its host class, which its create reads from the spec.
        let vast = plan_options(&req(&[R3090], &["VERIFIED"], 2, None, Order::Cheapest), "vast", &PriceBook::unpriced());
        let s = spec_for(&base(), "arena8-apple", &vast.options[0]);
        assert_eq!((s.gpu_type.as_str(), s.cloud_type.as_str(), s.gpu_count), (R3090, "VERIFIED", 2));
        // No tier (Shadeform): the base's tier stays.
        let sf = plan_options(&req(&[R3090], &["SECURE"], 2, None, Order::Cheapest), "shadeform", &PriceBook::unpriced());
        let s = spec_for(&base(), "arena8-apple", &sf.options[0]);
        assert_eq!((s.gpu_type.as_str(), s.cloud_type.as_str(), s.gpu_count), (R3090, "COMMUNITY", 2));
    }
}
