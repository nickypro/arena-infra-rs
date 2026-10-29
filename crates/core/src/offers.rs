//! `arena offers`: what `create`/`up` would try on each configured provider, in one table — and
//! `arena offers <provider>` / `all`, everything a provider sells: RunPod's GPU types on every
//! tier, the GPUs on Vast's market now per host class, Hetzner's server types.
//!
//! It used to take two commands to answer "what can I get, and for how much": `gpus` (RunPod's
//! catalog only) and `offers` (the plan, one provider at a time), with Hetzner in neither. Pure:
//! the CLI fetches — through the very planner `create`/`up` run ([`crate::placement`]), so the
//! table is what they would try, in their order — and this lays it out, snapshot-tested.

use serde::Serialize;

use crate::fleet::fmt_money;
use crate::gpu::{self, GpuRow};
use crate::near::{Near, Region};
use crate::placement::{milli, Dropped, OptionPlan, Order, PlacementOption, PriceSource};
use crate::provider::hetzner::ServerType;
use crate::provider::runpod::GpuType;
use crate::table::{self, Align};

/// A backend `create`/`up` never pick unless told: Shadeform, overflow created on only when
/// `--provider shadeform` names it ([`crate::balance::Account::opt_in`]). `offers` leaves it
/// out the same way.
fn opt_in(provider: &str) -> bool {
    crate::balance::Account::from_name(provider).is_some_and(|a| a.opt_in())
}

/// The providers `offers` shows, in the fleet's order (runpod, vast, hetzner, shadeform): the
/// one `--provider` names on the command line; else every provider whose key the config has
/// (`has_key(KEY)`, [`crate::teardown::PROVIDER_KEYS`]) but the opt-in ones — or, with none,
/// RunPod alone (`create`/`up`'s default, priced from the presets without a key). `create`/`up`
/// use one provider per run (`--provider`, default `runpod`): there's no order between
/// providers, only within one. `Err`: a provider arena doesn't know. Pure.
pub fn providers(named: Option<&str>, has_key: impl Fn(&str) -> bool) -> Result<Vec<&'static str>, String> {
    let known = crate::teardown::PROVIDER_KEYS;
    if let Some(name) = named {
        return match known.iter().find(|(p, _)| *p == name) {
            Some((p, _)) => Ok(vec![*p]),
            None => {
                let names: Vec<&str> = known.iter().map(|(p, _)| *p).collect();
                Err(format!("unknown provider `{name}` (known: {})", names.join(", ")))
            }
        };
    }
    let keyed: Vec<&'static str> = known.iter().filter(|(p, key)| !opt_in(p) && has_key(key)).map(|(p, _)| *p).collect();
    Ok(if keyed.is_empty() { vec!["runpod"] } else { keyed })
}

/// The opt-in providers with a key that `offers` leaves out when no `--provider` is named — said
/// (as `skipped`), so the table doesn't read as all there is. Pure.
pub fn opt_in_left_out(named: Option<&str>, has_key: impl Fn(&str) -> bool) -> Vec<Skipped> {
    if named.is_some() {
        return Vec::new();
    }
    let keyed = crate::teardown::PROVIDER_KEYS.iter().filter(|(p, key)| opt_in(p) && has_key(key));
    keyed
        .map(|(p, _)| Skipped { provider: p.to_string(), why: format!("opt-in overflow, never `up`'s default — `--provider {p}` shows it") })
        .collect()
}

/// VRAM per GPU of `gpu` (a provider type string, as placement sends it), in GB: RunPod's live
/// catalog's (`catalog`, may be empty), else the preset's. `None` for a GPU neither lists.
pub fn vram_gb(gpu: &str, catalog: &[GpuType]) -> Option<u32> {
    catalog
        .iter()
        .find(|t| t.id == gpu && t.memory_gb > 0)
        .map(|t| t.memory_gb)
        .or_else(|| gpu::find(gpu).map(|g| g.vram_gb))
        .or_else(|| gpu::unlisted_vram(gpu))
}

/// One option in the table: what placement would try ([`PlacementOption`], flattened in the
/// JSON), with its GPU's VRAM.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Offer {
    #[serde(flatten)]
    pub option: PlacementOption,
    /// VRAM per GPU, GB ([`vram_gb`]); `None` on a CPU VM or for a GPU nobody lists.
    pub vram_gb: Option<u32>,
}

/// One provider's part of `arena offers`: the plan `create`/`up` would walk there, as
/// [`OptionPlan`] has it (`options` in the order they'd be tried), each option with its VRAM,
/// and on Hetzner the server type a create asks for — with its size and € price, which
/// placement doesn't see (Hetzner quotes nothing to it, so `--max-price` can't check it).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderOffers {
    pub provider: String,
    pub gpu_count: u32,
    pub order: Order,
    pub max_price: Option<f64>,
    pub options: Vec<Offer>,
    pub dropped: Vec<Dropped>,
    pub notes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_type: Option<ServerType>,
    /// Near the proxy first ([`OptionPlan::near`]); `None`: off, or not this provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub near: Option<Near>,
}

impl ProviderOffers {
    /// `plan`, each option's VRAM looked up in `catalog` ([`vram_gb`]) — none on Hetzner, whose
    /// one option is the CPU VM `server_type` describes.
    pub fn new(plan: OptionPlan, catalog: &[GpuType], server_type: Option<ServerType>) -> Self {
        let cpu = plan.provider == "hetzner";
        let options = plan
            .options
            .into_iter()
            .map(|option| Offer { vram_gb: if cpu { None } else { vram_gb(&option.gpu, catalog) }, option })
            .collect();
        Self {
            provider: plan.provider,
            gpu_count: plan.gpu_count,
            order: plan.order,
            max_price: plan.max_price,
            options,
            dropped: plan.dropped,
            notes: plan.notes,
            server_type,
            near: plan.near,
        }
    }

    /// How an option reads in the table: Hetzner's server type (`cx23`) for its CPU VM, else
    /// placement's own label (`1×RTX A4000`).
    fn label(&self, o: &PlacementOption) -> String {
        match &self.server_type {
            Some(t) if self.provider == "hetzner" => t.name.clone(),
            _ => o.label.clone(),
        }
    }
}

/// A provider `offers` doesn't show, and why. Skipped: a flag that doesn't apply there (`--gpu`
/// on Hetzner, `--server-type` anywhere else — as `create` refuses it, only not an error when
/// no `--provider` was named), or opt-in Shadeform. Failed: planning it as `create` would
/// failed (a `--gpu` typo, a server type Hetzner doesn't sell).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Skipped {
    pub provider: String,
    pub why: String,
}

/// `arena offers` (`--json` prints this): each provider's options, the providers left out on
/// purpose, and the ones that failed (the command then exits non-zero, after showing the rest).
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Offers {
    pub providers: Vec<ProviderOffers>,
    pub skipped: Vec<Skipped>,
    pub failed: Vec<Skipped>,
}

/// `4` for 4.0 GB, `0.5` for half a GB.
fn gb(x: f64) -> String {
    if x.fract() == 0.0 {
        format!("{x:.0}")
    } else {
        format!("{x}")
    }
}

/// A server type's size: `2 vCPU, 4 GB RAM` (`, arm` for an Arm type, as `type_label` says it).
fn cpu_size(t: &ServerType) -> String {
    let mut parts = Vec::new();
    if let Some(c) = t.cores {
        parts.push(format!("{c} vCPU"));
    }
    if let Some(m) = t.memory_gb {
        parts.push(format!("{} GB RAM", gb(m)));
    }
    if t.architecture.as_deref() == Some("arm") {
        parts.push("arm".into());
    }
    if parts.is_empty() {
        "-".into()
    } else {
        parts.join(", ")
    }
}

/// `note` with its provider in front, unless it already starts with it (`vast prices are …`).
fn tagged(provider: &str, note: &str) -> String {
    if note.starts_with(provider) {
        note.to_string()
    } else {
        format!("{provider}: {note}")
    }
}

/// The `offers` table: one row per option, providers in [`providers`] order and each one's
/// options in the order `create`/`up` would try them (`#`); then the providers that failed, and
/// one line saying what `-v` (`verbose`) adds: what `--max-price` dropped, the notes and the
/// providers left out on purpose. How to read the table is `offers --help`'s (it used to be
/// four lines under every table). Prices carry their currency: a
/// Hetzner VM's `€` is Hetzner's own, never converted (as `pods list` shows it), and its source
/// is `list` — read for the table, not a price placement has. With near the proxy on for any
/// provider, a WHERE column says which options are tried only in the region (`Europe`) and
/// which anywhere (`any`; `-` for a provider near doesn't apply to). Pure.
pub fn render(offers: &Offers, verbose: bool) -> String {
    use Align::{Left, Right};
    let near = offers.providers.iter().any(|p| p.near.is_some());
    let mut rows: Vec<Vec<String>> = Vec::new();
    for p in &offers.providers {
        if p.options.is_empty() {
            rows.push(vec![p.provider.clone(), "-".into(), "(no option left to try)".into()]);
        }
        for (i, Offer { option: o, vram_gb }) in p.options.iter().enumerate() {
            let size = match (&p.server_type, vram_gb) {
                (Some(t), _) if p.provider == "hetzner" => cpu_size(t),
                (_, Some(v)) if o.gpu_count > 1 => format!("{}×{v} GB VRAM", o.gpu_count),
                (_, Some(v)) => format!("{v} GB VRAM"),
                _ => "-".into(),
            };
            // A Hetzner VM's price is its list price, read for this table: placement prices
            // nothing there (`--max-price` can't use it), so it's `list`, never `live`.
            let (price, source) = match (o.price_per_pod, o.price_source, &p.server_type) {
                (Some(usd), Some(PriceSource::Estimate), _) => (format!("~{}/h", fmt_money("$", usd)), "estimate"),
                (Some(usd), Some(PriceSource::Live), _) => (format!("{}/h", fmt_money("$", usd)), "live"),
                (Some(usd), None, _) => (format!("{}/h", fmt_money("$", usd)), "-"),
                (None, _, Some(ServerType { eur_per_hour: Some(eur), .. })) => (format!("{}/h", fmt_money("€", *eur)), "list"),
                (None, _, _) => ("-".into(), "-"),
            };
            let mut row = vec![p.provider.clone(), (i + 1).to_string(), p.label(o), o.cloud.clone().unwrap_or_else(|| "-".into())];
            if near {
                let at = if p.near.is_some() { o.near.map_or("any", Region::label) } else { "-" };
                row.push(at.to_string());
            }
            row.extend([size, price, source.into(), o.stock.clone().unwrap_or_else(|| "-".into())]);
            rows.push(row);
        }
    }
    let mut out = if rows.is_empty() {
        "(no provider to show)\n".to_string()
    } else if near {
        table::render(
            &["PROVIDER", "#", "OPTION", "CLOUD", "WHERE", "SIZE", "PER POD", "SOURCE", "STOCK"],
            &[Left, Right, Left, Left, Left, Left, Right, Left, Left],
            &rows,
        )
    } else {
        table::render(
            &["PROVIDER", "#", "OPTION", "CLOUD", "SIZE", "PER POD", "SOURCE", "STOCK"],
            &[Left, Right, Left, Left, Left, Right, Left, Left],
            &rows,
        )
    };
    let dropped: Vec<String> = offers
        .providers
        .iter()
        .flat_map(|p| {
            p.dropped.iter().map(move |d| {
                let what = match &d.option.cloud {
                    Some(c) => format!("{} {c}", p.label(&d.option)),
                    None => p.label(&d.option),
                };
                format!("  {}: {what}: {}", p.provider, d.reason)
            })
        })
        .collect();
    let mut notes: Vec<String> = Vec::new();
    for p in &offers.providers {
        notes.extend(p.notes.iter().map(|n| tagged(&p.provider, n)));
        if let Some(ServerType { name, locations, eur_per_hour, .. }) = &p.server_type {
            if let (Some(first), Some(_)) = (locations.first(), eur_per_hour) {
                let walk = match locations.as_slice() {
                    [_] => format!("is created in {first}"),
                    many => format!("is tried in {} in that order", many.join(", ")),
                };
                notes.push(format!(
                    "hetzner: {name} {walk}; `list` is its gross list price in {first}, for reference \
                     (Hetzner bills in EUR, never converted)"
                ));
            }
        }
    }
    let failed: String = offers.failed.iter().map(|s| format!("failed: {}: {}\n", s.provider, s.why)).collect();
    if verbose {
        if !dropped.is_empty() {
            out.push_str("not tried (--max-price):\n");
            dropped.iter().for_each(|l| out.push_str(&format!("{l}\n")));
        }
        notes.iter().for_each(|n| out.push_str(&format!("note: {n}\n")));
        for s in &offers.skipped {
            out.push_str(&format!("skipped: {}: {}\n", s.provider, s.why));
        }
        out.push_str(&failed);
        return out;
    }
    out.push_str(&failed);
    let mut more: Vec<String> = Vec::new();
    let count = |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    if !notes.is_empty() {
        more.push(count(notes.len(), "note", "notes"));
    }
    if !dropped.is_empty() {
        more.push(format!("{} over --max-price", dropped.len()));
    }
    if !offers.skipped.is_empty() {
        let names: Vec<&str> = offers.skipped.iter().map(|s| s.provider.as_str()).collect();
        more.push(format!("{} skipped", names.join(", ")));
    }
    let more = if more.is_empty() { String::new() } else { format!(" · -v: {}", more.join(", ")) };
    out.push_str(&format!("# = the order `up` tries{more} · everything on sale: `arena offers all`\n"));
    out
}

/// Whose stock RunPod's STOCK column shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stock {
    /// REST v2, both tiers read: `community/secure`.
    BothTiers,
    /// REST v2, SECURE's catalog couldn't be read: the community figure alone.
    CommunityOnly,
    /// GraphQL (`RUNPOD_API=v1`): a 1-GPU pod's, either tier.
    OneGpu,
}

/// RunPod's GPU catalog as `gpus` shows it (`--json`: `rows`), and `offers runpod` reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct RunpodCatalog {
    /// Every row, the ones its create API rejects (`creatable: false`) too.
    pub rows: Vec<GpuRow>,
    /// From RunPod's live catalog; else the local presets.
    pub live: bool,
    /// Whether `creatable` could be checked (v2's catalog is what its create takes; v1's enum
    /// may not have been readable).
    pub creatable_known: bool,
    pub stock: Stock,
    /// The presets only: why, and how to get the live list ([`NO_KEY`], [`NOT_READ`],
    /// [`GPUS_HINT`]).
    pub hint: &'static str,
}

/// Why the presets: there's no `RUNPOD_API_KEY`.
pub const NO_KEY: &str = "set RUNPOD_API_KEY for RunPod's full live list";
/// Why the presets: RunPod's catalog didn't answer (the warning says why).
pub const NOT_READ: &str = "RunPod's live list couldn't be read";
/// `arena gpus`' own words, whatever the reason, so its output stays as it was.
pub const GPUS_HINT: &str = "run with --provider runpod + a key for the full live list";

impl RunpodCatalog {
    /// The local presets (RunPod's prices, estimated), saying `hint`.
    pub fn presets(hint: &'static str) -> Self {
        Self { rows: gpu::rows_from_presets(), live: false, creatable_known: false, stock: Stock::OneGpu, hint }
    }
}

/// RunPod's catalog the way `arena gpus` printed it: the table (minus what the create API
/// rejects, counted under it), then how to read it. Pure.
pub fn render_runpod_catalog(c: &RunpodCatalog) -> String {
    if !c.live {
        return format!(
            "{}\n(local presets, ~ = rough estimate — {}.)\n\
             Pass the API name, label, or alias (e.g. `A4000`, `3090`, \"A100 SXM\") to --gpu.\n",
            gpu::render_gpu_table(&c.rows),
            c.hint
        );
    }
    let (shown, hidden): (Vec<_>, Vec<_>) = c.rows.iter().cloned().partition(|r| r.creatable != Some(false));
    let mut out = gpu::render_gpu_table(&shown);
    out.push_str(&format!(
        "\n{} GPU types {}. Pass the GPU name (or the API name, or a short alias like A4000 / 3090) to --gpu. \
         Prices are RunPod's live $/hr per GPU (~ = preset estimate, - = not offered); STOCK is RunPod's stock hint{}.\n",
        shown.len(),
        if c.creatable_known { "(live from RunPod, creatable via the create API)" } else { "(live from RunPod)" },
        match c.stock {
            Stock::BothTiers => " as community/secure (over hosts at or above the CUDA floor, if one is set)",
            Stock::CommunityOnly => " on the community cloud (secure's couldn't be read)",
            Stock::OneGpu => " for a 1-GPU pod (either tier)",
        }
    ));
    if !hidden.is_empty() {
        out.push_str(&format!(
            "({} more in RunPod's catalog are hidden — listed but rejected by the create API, so --gpu can't use them; see --json.)\n",
            hidden.len()
        ));
    }
    out
}

/// Hetzner's server-type families in the order the catalogue lists them, each with the line
/// that heads its group: sorted by price, cx, cpx, cax and ccx interleaved and the list was
/// hard to read (the human, 2026-10-29: "grouped by type").
const HETZNER_FAMILIES: &[(&str, &str)] = &[
    ("cx", "cx — shared x86, cost-optimised"),
    ("cpx", "cpx — shared AMD: the newer cpx?2 first, then the older cpx?1"),
    ("cax", "cax — shared Arm"),
    ("ccx", "ccx — dedicated x86"),
];

/// A server type's family: its name's leading letters (`cpx` for `cpx42`).
fn family(name: &str) -> &str {
    let end = name.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(name.len());
    &name[..end]
}

/// Hetzner's server types in catalogue order ([`HETZNER_FAMILIES`], then any other family by
/// name): within a family the newer generation first — the last digit, `cpx42` before `cpx41`
/// — then by size, vCPUs then RAM, which for Hetzner's names is their natural order (cx23,
/// cx33, cx43). What `offers hetzner` lists, in its table and `--json`; the plan's own options
/// keep their try order. Pure.
pub fn order_server_types(types: &mut [ServerType]) {
    let rank = |t: &ServerType| {
        let fam = family(&t.name);
        let known = HETZNER_FAMILIES.iter().position(|(f, _)| *f == fam).unwrap_or(HETZNER_FAMILIES.len());
        let generation = t.name.chars().last().and_then(|c| c.to_digit(10)).unwrap_or(0);
        (known, fam.to_string(), std::cmp::Reverse(generation), t.cores.unwrap_or(0), mb(t.memory_gb.unwrap_or(0.0)), t.name.clone())
    };
    types.sort_by_key(rank);
}

/// GB to whole MB, for ordering.
fn mb(gb: f64) -> u64 {
    (gb * 1000.0).round().max(0.0) as u64
}

/// Hetzner's server types for `offers hetzner`, in [`order_server_types`] order and grouped by
/// family, a line heading each group: size, € per hour where it's cheapest, where it's sold.
/// `configured` (`HETZNER_SERVER_TYPE`, what `create`/`up` use) is starred — or, when it's on
/// sale nowhere now (create looks it up by this exact name), said. Pure.
pub fn render_server_types(types: &[ServerType], configured: &str) -> String {
    use Align::{Left, Right};
    let mut types = types.to_vec();
    order_server_types(&mut types);
    let rows: Vec<Vec<String>> = types
        .iter()
        .map(|t| {
            let kind = [t.cpu_type.clone(), t.architecture.clone()];
            vec![
                if t.name == configured { format!("{}*", t.name) } else { t.name.clone() },
                t.cores.map_or("-".into(), |c| c.to_string()),
                kind.into_iter().flatten().collect::<Vec<_>>().join(" "),
                t.memory_gb.map_or("-".into(), |m| format!("{} GB", gb(m))),
                t.disk_gb.map_or("-".into(), |d| format!("{d} GB")),
                t.eur_per_hour.map_or("-".into(), |e| fmt_money("€", e)),
                t.locations.join(", "),
            ]
        })
        .collect();
    let table = table::render(&["TYPE", "VCPU", "KIND", "RAM", "DISK", "€/H", "LOCATIONS"], &[Left, Right, Left, Right, Right, Right, Left], &rows);
    let mut lines = table.lines();
    let mut out = format!("{}\n", lines.next().unwrap_or_default());
    let mut last: Option<&str> = None;
    for (t, line) in types.iter().zip(lines) {
        let fam = family(&t.name);
        if last != Some(fam) {
            if last.is_some() {
                out.push('\n');
            }
            let head = HETZNER_FAMILIES.iter().find(|(f, _)| *f == fam).map_or_else(|| fam.to_string(), |(_, h)| h.to_string());
            out.push_str(&format!("{head}\n"));
            last = Some(fam);
        }
        out.push_str(&format!("{line}\n"));
    }
    out.push_str(&format!("\n{} types · * = HETZNER_SERVER_TYPE · --server-type <TYPE> picks another · €/h in the first location\n", types.len()));
    if !types.iter().any(|t| t.name == configured) {
        out.push_str(&format!(
            "HETZNER_SERVER_TYPE `{configured}` isn't on sale anywhere now — `up --provider hetzner` would be refused.\n"
        ));
    }
    out
}

/// One row of a provider's catalogue (`arena offers runpod` / `vast`): a GPU type on one cloud
/// tier — RunPod's `COMMUNITY`/`SECURE`, Vast's host classes (`DATACENTER`/`VERIFIED`/
/// `UNVERIFIED`, [`crate::provider::vast::Host`]).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CatalogRow {
    /// What `--gpu` takes for it (RunPod's id; Vast's name for the card).
    pub gpu: String,
    /// The provider's short name for it.
    pub name: String,
    pub vram_gb: Option<u32>,
    /// The tier; `None`: not known (a type the catalog doesn't list) or sold on no tier now.
    pub cloud: Option<String>,
    /// $/h per GPU; `None`: unknown.
    pub price_per_gpu: Option<f64>,
    /// `live`: RunPod's catalog, Vast's cheapest offer now; `estimate`: arena's presets;
    /// `pod`: what a pod of this type on the account bills (an unlisted type's only price).
    pub price_source: Option<String>,
    /// The stock hint (RunPod's); `None` on Vast, where the row *is* an offer.
    pub stock: Option<String>,
    /// Where the row's offer is (Vast: the cheapest one's country).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// The position `create`/`up` with the config and flags would try it at (1 = first, as
    /// `#` in `arena offers`); `None`: they wouldn't.
    pub try_order: Option<usize>,
    /// What's special about it, in a word or two: `unlisted` (not in the public catalog; seen
    /// on the account's pods or listed in config), `v1: not creatable`, `not sold now`,
    /// `--cloud unverified` (the configured create wouldn't rent there).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One provider's catalogue, as `arena offers <provider> --json` prints it (in `catalogs`).
/// `rows` for RunPod and Vast, `server_types` for Hetzner, both empty for a provider without a
/// list (Shadeform), `notes` saying why or how it was read.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Catalog {
    pub provider: String,
    /// GPUs per pod the prices and stock are for (`--gpus`, config `NUM_GPUS`).
    pub gpu_count: u32,
    pub rows: Vec<CatalogRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_types: Option<Vec<ServerType>>,
    pub notes: Vec<String>,
}

/// `arena offers <provider>|all --json`: each catalogue, in the fleet's order; the providers
/// left out (Shadeform unnamed; a flag that doesn't apply) and the ones whose list failed (the
/// command then exits non-zero, after printing the rest).
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Catalogs {
    pub catalogs: Vec<Catalog>,
    pub skipped: Vec<Skipped>,
    pub failed: Vec<Skipped>,
}

/// What `offers <provider>` keeps: the `--gpu` types (exact ids/names), `--cloud` tiers and
/// `--max-price` cap per pod of `gpu_count` GPUs — each `None` when not given.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RowFilter {
    pub gpus: Option<Vec<String>>,
    pub clouds: Option<Vec<String>>,
    pub max_price: Option<f64>,
    pub gpu_count: u32,
}

impl RowFilter {
    /// Whether `row` passes: its GPU listed (when `--gpu` is), its tier listed (a row of no
    /// known tier passes: it may be sold on any), its price per pod within the cap (an
    /// unknown price can't be checked: out, as `create` drops it).
    fn keeps(&self, row: &CatalogRow, same_gpu: impl Fn(&str, &str) -> bool) -> bool {
        let gpu = self.gpus.as_ref().is_none_or(|g| g.iter().any(|g| same_gpu(g, &row.gpu)));
        let cloud = match (&self.clouds, &row.cloud) {
            (Some(want), Some(c)) => want.iter().any(|w| w.eq_ignore_ascii_case(c)),
            _ => true,
        };
        let n = f64::from(self.gpu_count.max(1));
        let price = self.max_price.is_none_or(|cap| row.price_per_gpu.is_some_and(|p| milli(p * n) <= milli(cap)));
        gpu && cloud && price
    }
}

/// A GPU type RunPod's public catalog lacks that arena knows of ([`gpu::UNLISTED`], the
/// account's pods, config `RUNPOD_EXTRA_GPU_TYPES`), with what a pod of it bills per GPU when a
/// pod on the account says (`cost` over its GPU count; never a guess).
#[derive(Debug, Clone, PartialEq)]
pub struct Unlisted {
    pub id: String,
    pub pod_price_per_gpu: Option<f64>,
}

/// `arena offers runpod`'s rows: every GPU type in `cat` once per tier that sells it (RunPod's
/// catalog lists every type for every tier, priced where it's sold), a type sold on neither
/// once, marked; then `unlisted`'s types the catalog lacks. `plan` marks what `create`/`up`
/// would try ([`CatalogRow::try_order`]). Then `filter`. Ordered as the catalog (VRAM, name),
/// COMMUNITY before SECURE, the unlisted last. Pure.
pub fn runpod_rows(cat: &RunpodCatalog, unlisted: &[Unlisted], plan: Option<&OptionPlan>, filter: &RowFilter) -> Vec<CatalogRow> {
    let order = |gpu: &str, cloud: Option<&str>| {
        plan.and_then(|p| p.options.iter().position(|o| o.gpu == gpu && o.cloud.as_deref() == cloud)).map(|i| i + 1)
    };
    let source = |r: &GpuRow| match r.price_source {
        Some(gpu::Source::Presets) => Some("estimate".to_string()),
        Some(gpu::Source::Live) => Some("live".to_string()),
        None => None,
    };
    let mut rows = Vec::new();
    for r in &cat.rows {
        let v1 = (r.creatable == Some(false)).then(|| "v1: not creatable".to_string());
        let tiers = [("COMMUNITY", r.community_price, r.stock_status.clone()), ("SECURE", r.secure_price, r.secure_stock_status.clone())];
        let mut sold = false;
        for (tier, price, stock) in tiers {
            let Some(price) = price else { continue };
            sold = true;
            // GraphQL's stock is one figure for either tier; v2's COMMUNITY figure is the row's own.
            let stock = match (cat.stock, tier) {
                (Stock::OneGpu, _) | (_, "COMMUNITY") => r.stock_status.clone(),
                _ => stock,
            };
            rows.push(CatalogRow {
                gpu: r.id.clone(),
                name: r.display_name.clone(),
                vram_gb: Some(r.memory_gb).filter(|m| *m > 0),
                cloud: Some(tier.to_string()),
                price_per_gpu: Some(price),
                price_source: source(r),
                stock,
                location: None,
                try_order: order(&r.id, Some(tier)),
                note: v1.clone(),
            });
        }
        if !sold {
            rows.push(CatalogRow {
                gpu: r.id.clone(),
                name: r.display_name.clone(),
                vram_gb: Some(r.memory_gb).filter(|m| *m > 0),
                cloud: None,
                price_per_gpu: None,
                price_source: None,
                stock: None,
                location: None,
                try_order: None,
                note: Some(v1.unwrap_or_else(|| "not sold now".into())),
            });
        }
    }
    for u in unlisted {
        if cat.rows.iter().any(|r| r.id.eq_ignore_ascii_case(&u.id)) || rows.iter().any(|r: &CatalogRow| r.gpu == u.id) {
            continue;
        }
        let tried = plan.and_then(|p| p.options.iter().position(|o| o.gpu == u.id)).map(|i| i + 1);
        rows.push(CatalogRow {
            gpu: u.id.clone(),
            name: crate::placement::mig_label(&u.id).unwrap_or_else(|| crate::metrics::normalize_gpu_name(&u.id)),
            vram_gb: gpu::unlisted_vram(&u.id),
            cloud: None,
            price_per_gpu: u.pod_price_per_gpu,
            price_source: u.pod_price_per_gpu.map(|_| "pod".to_string()),
            stock: None,
            location: None,
            try_order: tried,
            note: Some("unlisted".into()),
        });
    }
    rows.retain(|r| filter.keeps(r, |a, b| a.eq_ignore_ascii_case(b)));
    rows
}

/// `arena offers vast`'s rows from the market's cheapest offer per card and host class
/// ([`crate::provider::vast::market_rows`]), prices per GPU. `default`: the tier a create
/// rents on as configured — a row it wouldn't rent says `--cloud unverified`. `plan` marks
/// the row each option would rent: the cheapest of the classes its tier admits. Only options
/// tried anywhere: a near option rents in its region, which these worldwide rows don't show
/// (review: codex). Pure.
pub fn vast_rows(
    market: &[crate::provider::vast::MarketRow],
    gpu_count: u32,
    default: crate::provider::vast::Host,
    plan: Option<&OptionPlan>,
    filter: &RowFilter,
) -> Vec<CatalogRow> {
    use crate::provider::vast::{tier, vast_gpu_name, Host};
    let n = f64::from(gpu_count.max(1));
    let same = |a: &str, b: &str| {
        let key = |s: &str| vast_gpu_name(s).to_ascii_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>();
        key(a) == key(b)
    };
    let mut rows: Vec<CatalogRow> = market
        .iter()
        .map(|m| CatalogRow {
            gpu: m.gpu.clone(),
            name: m.gpu.clone(),
            vram_gb: m.offer.vram_gb,
            cloud: Some(m.host.key().to_string()),
            price_per_gpu: Some(m.offer.price / n),
            price_source: Some("live".into()),
            stock: None,
            location: m.offer.geolocation.clone(),
            try_order: None,
            note: (!default.admits(m.host)).then(|| "--cloud unverified".to_string()),
        })
        .collect();
    for (i, o) in plan.map(|p| p.options.as_slice()).unwrap_or_default().iter().enumerate() {
        let Ok(t) = tier(o.cloud.as_deref().unwrap_or(""), default != Host::Unverified) else { continue };
        if o.near.is_some() {
            continue;
        }
        let pick = rows
            .iter_mut()
            .filter(|r| same(&o.gpu, &r.gpu) && Host::ALL.iter().any(|h| Some(h.key()) == r.cloud.as_deref() && t.admits(*h)))
            .min_by(|a, b| a.price_per_gpu.unwrap_or(f64::MAX).total_cmp(&b.price_per_gpu.unwrap_or(f64::MAX)));
        if let Some(r) = pick.filter(|r| r.try_order.is_none()) {
            r.try_order = Some(i + 1);
        }
    }
    rows.retain(|r| filter.keeps(r, same));
    rows
}

/// A catalogue's rows as a table: `#` (the try order, blank for what `up` wouldn't try), the
/// GPU, VRAM, tier, $/h per GPU (`~` an estimate, `?` unknown), stock, where (when any row
/// says), the note, and the `--gpu` name when it isn't the GPU's own. Pure.
pub fn render_rows(rows: &[CatalogRow]) -> String {
    use Align::{Left, Right};
    let located = rows.iter().any(|r| r.location.is_some());
    let stocked = rows.iter().any(|r| r.stock.is_some());
    let noted = rows.iter().any(|r| r.note.is_some());
    let named = rows.iter().any(|r| r.gpu != r.name);
    let mut headers = vec!["#", "GPU", "VRAM", "CLOUD", "PER GPU"];
    let mut align = vec![Right, Left, Right, Left, Right];
    let mut extra = |h: &'static str, on: bool| {
        if on {
            headers.push(h);
            align.push(Left);
        }
    };
    extra("STOCK", stocked);
    extra("WHERE", located);
    extra("NOTE", noted);
    extra("--gpu", named);
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            let price = match (r.price_per_gpu, r.price_source.as_deref()) {
                (Some(p), Some("estimate")) => format!("~{}/h", fmt_money("$", p)),
                (Some(p), _) => format!("{}/h", fmt_money("$", p)),
                (None, _) => "?".into(),
            };
            let mut row = vec![
                r.try_order.map_or(String::new(), |i| i.to_string()),
                r.name.clone(),
                r.vram_gb.map_or("-".into(), |v| format!("{v}G")),
                r.cloud.clone().unwrap_or_else(|| "-".into()),
                price,
            ];
            if stocked {
                row.push(r.stock.clone().unwrap_or_else(|| "-".into()));
            }
            if located {
                row.push(r.location.clone().unwrap_or_else(|| "-".into()));
            }
            if noted {
                row.push(r.note.clone().unwrap_or_default());
            }
            if named {
                row.push(r.gpu.clone());
            }
            row
        })
        .collect();
    if cells.is_empty() {
        return "(nothing matches)\n".into();
    }
    table::render(&headers, &align, &cells)
}

/// A catalogue as `arena offers <provider>` prints it: a heading, the table (Hetzner's server
/// types grouped, [`render_server_types`]), then its notes on one line each. Pure.
pub fn render_catalog(c: &Catalog, configured_server_type: &str) -> String {
    let what = match c.provider.as_str() {
        "runpod" => "GPU types on every tier".to_string(),
        "vast" => "GPUs on the market now, the cheapest offer per host class".to_string(),
        "hetzner" => "server types".to_string(),
        _ => "no list".to_string(),
    };
    let per = if c.gpu_count > 1 && c.provider != "hetzner" { format!(" (for {}-GPU pods)", c.gpu_count) } else { String::new() };
    let mut out = format!("{} — {what}{per}\n", c.provider);
    match (&c.server_types, c.rows.is_empty()) {
        (Some(types), _) => out.push_str(&render_server_types(types, configured_server_type)),
        (None, false) => out.push_str(&render_rows(&c.rows)),
        (None, true) if c.provider == "runpod" || c.provider == "vast" => out.push_str("(nothing matches)\n"),
        (None, true) => {}
    }
    for n in &c.notes {
        out.push_str(&format!("{n}\n"));
    }
    out
}

/// What `offers` says for a provider with no list to show: Shadeform prices a GPU per search,
/// at the cheapest offer of the moment.
pub fn no_catalog(provider: &str) -> String {
    format!(
        "{provider}: no fixed list — `arena offers --provider {provider} --gpu H100,A100` prices each GPU at its \
         cheapest offer now"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::placement::{plan_options, PriceBook, Request};
    use crate::pod::PodSpec;

    fn keys(set: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |k| set.contains(&k)
    }

    /// Which providers: a named one alone (Shadeform too — that's how it's asked for); else
    /// every keyed one in the fleet's order, never Shadeform, and RunPod when none has a key.
    #[test]
    fn providers_are_the_named_one_else_every_keyed_default_one() {
        let all = &["RUNPOD_API_KEY", "VAST_API_KEY", "HETZNER_API_KEY", "SHADEFORM_API_KEY"];
        // (`--provider`, the keys set, what's shown)
        type Case = (Option<&'static str>, &'static [&'static str], Result<Vec<&'static str>, String>);
        let cases: [Case; 7] = [
            (None, all, Ok(vec!["runpod", "vast", "hetzner"])),
            (None, &["HETZNER_API_KEY", "RUNPOD_API_KEY"], Ok(vec!["runpod", "hetzner"])),
            (None, &["SHADEFORM_API_KEY"], Ok(vec!["runpod"])),
            (None, &[], Ok(vec!["runpod"])),
            (Some("shadeform"), all, Ok(vec!["shadeform"])),
            (Some("vast"), &[], Ok(vec!["vast"])),
            (Some("lambda"), all, Err("unknown provider `lambda` (known: runpod, vast, hetzner, shadeform)".into())),
        ];
        for (named, set, want) in cases {
            assert_eq!(providers(named, keys(set)), want, "{named:?} {set:?}");
        }
        // Shadeform with a key but not named: said, not silently missing.
        let left = opt_in_left_out(None, keys(all));
        assert_eq!(left.len(), 1);
        assert_eq!((left[0].provider.as_str(), left[0].why.as_str()), ("shadeform", "opt-in overflow, never `up`'s default — `--provider shadeform` shows it"));
        assert!(opt_in_left_out(Some("runpod"), keys(all)).is_empty() && opt_in_left_out(None, keys(&["RUNPOD_API_KEY"])).is_empty());
    }

    #[test]
    fn vram_comes_from_the_live_catalog_then_the_presets() {
        let live = |id: &str, gb: u32| GpuType { id: id.into(), memory_gb: gb, ..Default::default() };
        let catalog = vec![live("NVIDIA B300 SXM6 AC MIG 1g.34gb", 34), live("NVIDIA RTX A4000", 0)];
        assert_eq!(vram_gb("NVIDIA B300 SXM6 AC MIG 1g.34gb", &catalog), Some(34));
        assert_eq!(vram_gb("NVIDIA RTX A4000", &catalog), Some(16), "a 0 GB placeholder falls back to the preset");
        assert_eq!(vram_gb("NVIDIA GeForce RTX 3090", &[]), Some(24));
        assert_eq!(vram_gb("NVIDIA Mystery", &catalog), None);
    }

    fn cx23() -> ServerType {
        ServerType {
            name: "cx23".into(),
            cores: Some(2),
            cpu_type: Some("shared".into()),
            architecture: Some("x86".into()),
            memory_gb: Some(4.0),
            disk_gb: Some(40),
            eur_per_hour: Some(0.0104),
            locations: vec!["nbg1".into(), "fsn1".into(), "hel1".into()],
        }
    }

    /// Plans as the CLI makes them: placement's own planner over a price book.
    fn plan(provider: &str, gpus: &str, clouds: &str, gpu_count: u32, cap: Option<f64>, book: &PriceBook) -> OptionPlan {
        let base = PodSpec { gpu_type: "NVIDIA RTX A4000".into(), cloud_type: "COMMUNITY".into(), gpu_count, ..PodSpec::from_config(&crate::config::Config::parse("")) };
        let req = Request::from_flags(Some(gpus), Some(clouds), &base, cap, Order::Cheapest).unwrap();
        plan_options(&req, provider, book)
    }

    /// The whole table: every provider's options in the order placement tries them, numbered
    /// per provider; VRAM for GPUs, vCPU/RAM for Hetzner's VM, its € price as Hetzner's own.
    /// Under it the failures, and one line counting what `-v` shows: what the cap dropped, the
    /// notes (tagged by provider), what was skipped.
    #[test]
    fn the_table_is_every_providers_plan_in_try_order() {
        let runpod = plan("runpod", "A4000,3090", "community,secure", 1, Some(0.40), &PriceBook::runpod(Vec::new()));
        let vast = plan("vast", "3090", "verified", 2, None, &PriceBook::offers(vec![("NVIDIA GeForce RTX 3090".into(), 0.36)], 2));
        let hetzner = plan("hetzner", "A4000", "community", 1, None, &PriceBook::unpriced());
        let offers = Offers {
            providers: vec![
                ProviderOffers::new(runpod, &[], None),
                ProviderOffers::new(vast, &[], None),
                ProviderOffers::new(hetzner, &[], Some(cx23())),
            ],
            skipped: vec![Skipped { provider: "shadeform".into(), why: "not configured".into() }],
            failed: vec![Skipped { provider: "lambda".into(), why: "no such thing".into() }],
        };
        let table = "PROVIDER  #  OPTION       CLOUD      SIZE               PER POD  SOURCE    STOCK\n\
             runpod    1  1×RTX A4000  COMMUNITY  16 GB VRAM        ~$0.17/h  estimate  -\n\
             runpod    2  1×RTX 3090   COMMUNITY  24 GB VRAM        ~$0.22/h  estimate  -\n\
             runpod    3  1×RTX A4000  SECURE     16 GB VRAM        ~$0.32/h  estimate  -\n\
             vast      1  2×RTX 3090   VERIFIED   2×24 GB VRAM       $0.36/h  live      -\n\
             hetzner   1  cx23         -          2 vCPU, 4 GB RAM  €0.010/h  list      -\n";
        assert_eq!(
            render(&offers, false),
            format!(
                "{table}failed: lambda: no such thing\n\
                 # = the order `up` tries · -v: 2 notes, 1 over --max-price, shadeform skipped · everything on sale: `arena offers all`\n"
            )
        );
        assert_eq!(
            render(&offers, true),
            format!(
                "{table}not tried (--max-price):\n  \
                   runpod: 1×RTX 3090 SECURE: ~$0.43/h > cap $0.40/h per pod\n\
                 note: runpod: ~ = preset estimate (no live price for that GPU); --max-price is checked against it\n\
                 note: hetzner: cx23 is tried in nbg1, fsn1, hel1 in that order; `list` is its gross list price in nbg1, for reference (Hetzner bills in EUR, never converted)\n\
                 skipped: shadeform: not configured\n\
                 failed: lambda: no such thing\n"
            )
        );
        // Nothing more to say: just the line.
        let plain = Offers { providers: vec![ProviderOffers::new(plan("vast", "3090", "verified", 1, None, &PriceBook::offers(vec![("NVIDIA GeForce RTX 3090".into(), 0.36)], 1)), &[], None)], ..Offers::default() };
        assert!(render(&plain, false).ends_with("\n# = the order `up` tries · everything on sale: `arena offers all`\n"), "{}", render(&plain, false));
    }

    /// Near the proxy: a WHERE column — `Europe` for an option tried only there, `any` for the
    /// rest, `-` on a provider near doesn't reach (Hetzner picks its nearest itself) — and
    /// `--json` carries the plan's `near` and each near option's. Off: the table as before.
    #[test]
    fn near_options_say_where() {
        let eu = Near { region: Region::Europe, max_extra_pct: 25.0 };
        let base = PodSpec { gpu_type: "NVIDIA RTX A4000".into(), cloud_type: "SECURE".into(), ..PodSpec::from_config(&crate::config::Config::parse("")) };
        let req = Request::from_flags(Some("A4000"), Some("secure"), &base, None, Order::Cheapest).unwrap().with_near(Some(eu), "runpod", &base);
        let runpod = plan_options(&req, "runpod", &PriceBook::runpod(Vec::new()));
        let hetzner = plan("hetzner", "A4000", "community", 1, None, &PriceBook::unpriced());
        let offers = Offers {
            providers: vec![ProviderOffers::new(runpod.clone(), &[], None), ProviderOffers::new(hetzner.clone(), &[], Some(cx23()))],
            ..Offers::default()
        };
        let out = render(&offers, true);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[..4],
            [
                "PROVIDER  #  OPTION       CLOUD   WHERE   SIZE               PER POD  SOURCE    STOCK",
                "runpod    1  1×RTX A4000  SECURE  Europe  16 GB VRAM        ~$0.32/h  estimate  -",
                "runpod    2  1×RTX A4000  SECURE  any     16 GB VRAM        ~$0.32/h  estimate  -",
                "hetzner   1  cx23         -       -       2 vCPU, 4 GB RAM  €0.010/h  list      -",
            ],
            "{out}"
        );
        assert!(out.contains("note: runpod: near the proxy (NEAR=europe): Europe first"), "{out}");
        let v = serde_json::to_value(&offers).unwrap();
        assert_eq!(v["providers"][0]["near"], serde_json::json!({"region": "europe", "max_extra_pct": 25.0}));
        assert_eq!((v["providers"][0]["options"][0]["near"].as_str(), v["providers"][0]["options"][1].get("near")), (Some("europe"), None));
        assert!(v["providers"][1].get("near").is_none());
        // Every field of a near plan is carried too.
        let plan_json = serde_json::to_value(&runpod).unwrap();
        for (k, val) in plan_json.as_object().unwrap().iter().filter(|(k, _)| *k != "options") {
            assert_eq!(&v["providers"][0][k], val, "{k}");
        }
        // A provider the cap left nothing in keeps its short row under the WHERE header (review:
        // claude asked — `table::render` takes short rows, nothing shifts).
        let capped = plan("hetzner", "A4000", "community", 1, Some(1.0), &PriceBook::unpriced());
        let both = Offers {
            providers: vec![ProviderOffers::new(runpod.clone(), &[], None), ProviderOffers::new(capped, &[], Some(cx23()))],
            ..Offers::default()
        };
        let out = render(&both, true);
        assert!(out.lines().any(|l| l.split_whitespace().collect::<Vec<_>>() == ["hetzner", "-", "(no", "option", "left", "to", "try)"]), "{out}");
        // Each cell under its header (by characters: `×` is two bytes).
        let header = out.lines().next().unwrap();
        let row: Vec<char> = out.lines().nth(1).unwrap().chars().collect();
        let cell = |col: &str, n: usize| {
            let at = header[..header.find(col).unwrap()].chars().count();
            row[at..at + n].iter().collect::<String>()
        };
        assert_eq!((cell("WHERE", 6), cell("SIZE", 10)), ("Europe".to_string(), "16 GB VRAM".to_string()), "{out}");
        // Off: no column.
        let off = render(&Offers { providers: vec![ProviderOffers::new(hetzner, &[], Some(cx23()))], ..Offers::default() }, true);
        assert!(!off.contains("WHERE"), "{off}");
    }

    /// A provider whose every option the cap dropped still has a row, saying so; Hetzner's VM
    /// is dropped by any cap (placement can't price it), named by its server type.
    #[test]
    fn a_provider_with_nothing_left_still_shows() {
        let hetzner = plan("hetzner", "A4000", "community", 1, Some(1.0), &PriceBook::unpriced());
        let offers = Offers { providers: vec![ProviderOffers::new(hetzner, &[], Some(cx23()))], ..Offers::default() };
        let out = render(&offers, true);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[1..4],
            [
                "hetzner   -  (no option left to try)",
                "not tried (--max-price):",
                "  hetzner: cx23: no known price, so --max-price can't be checked"
            ],
            "{out}"
        );
        assert_eq!(render(&Offers::default(), true).lines().next(), Some("(no provider to show)"));
        // A pinned location (HETZNER_LOCATION=nbg1) is one place, not an order.
        let one = plan("hetzner", "A4000", "community", 1, None, &PriceBook::unpriced());
        let pinned = ServerType { locations: vec!["nbg1".into()], ..cx23() };
        let out = render(&Offers { providers: vec![ProviderOffers::new(one, &[], Some(pinned))], ..Offers::default() }, true);
        assert!(out.contains("note: hetzner: cx23 is created in nbg1; `list` is its gross list price in nbg1,"), "{out}");
    }

    /// `--json`: each provider's plan as [`OptionPlan`] serializes it — so a script reading
    /// `options[].price_per_pod` reads what placement sees — plus each option's VRAM, and the
    /// server type on Hetzner.
    #[test]
    fn the_json_is_the_plan_plus_vram_and_the_server_type() {
        let runpod = plan("runpod", "A4000", "community", 1, None, &PriceBook::runpod(Vec::new()));
        let hetzner = plan("hetzner", "A4000", "community", 1, None, &PriceBook::unpriced());
        let offers = Offers {
            providers: vec![ProviderOffers::new(runpod.clone(), &[], None), ProviderOffers::new(hetzner, &[], Some(cx23()))],
            ..Offers::default()
        };
        let v = serde_json::to_value(&offers).unwrap();
        let first = &v["providers"][0];
        let plan_json = serde_json::to_value(&runpod).unwrap();
        // Every field of the plan is carried (a new one can't silently go missing here).
        for (k, v) in plan_json.as_object().unwrap().iter().filter(|(k, _)| *k != "options") {
            assert_eq!(&first[k], v, "{k}");
        }
        let mut option = plan_json["options"][0].clone();
        option["vram_gb"] = 16.into();
        assert_eq!(first["options"][0], option);
        assert!(first.get("server_type").is_none());
        let hz = &v["providers"][1];
        assert_eq!((hz["options"][0]["vram_gb"].clone(), hz["options"][0]["price_per_pod"].clone()), (serde_json::Value::Null, serde_json::Value::Null));
        assert_eq!(hz["server_type"]["eur_per_hour"], 0.0104);
        assert_eq!(hz["server_type"]["locations"][0], "nbg1");
        assert_eq!((v["skipped"].clone(), v["failed"].clone()), (serde_json::json!([]), serde_json::json!([])));
    }

    fn row(id: &str, name: &str, mem: u32, creatable: Option<bool>) -> GpuRow {
        GpuRow {
            id: id.into(),
            display_name: name.into(),
            memory_gb: mem,
            community_price: Some(0.17),
            secure_price: Some(0.25),
            stock_status: Some("High".into()),
            secure_stock_status: Some("Low".into()),
            creatable,
            source: gpu::Source::Live,
            price_source: Some(gpu::Source::Live),
        }
    }

    /// RunPod's catalog reads as `arena gpus` printed it: the create API's rejects hidden and
    /// counted, the stock's meaning per API; the presets say how to get the live list.
    #[test]
    fn the_runpod_catalog_reads_as_gpus_did() {
        let live = RunpodCatalog {
            rows: vec![row("NVIDIA RTX A4000", "RTX A4000", 16, Some(true)), row("NVIDIA PRO 6000 MIG", "PRO 6000 MIG 24GB", 24, Some(false))],
            live: true,
            creatable_known: true,
            stock: Stock::BothTiers,
            hint: "",
        };
        assert_eq!(
            render_runpod_catalog(&live),
            "GPU        VRAM  $/HR COMM  $/HR SEC  STOCK     API NAME (--gpu)\n\
             RTX A4000   16G      $0.17     $0.25  High/Low  NVIDIA RTX A4000\n\
             \n1 GPU types (live from RunPod, creatable via the create API). Pass the GPU name (or the API name, or a short \
             alias like A4000 / 3090) to --gpu. Prices are RunPod's live $/hr per GPU (~ = preset estimate, - = not offered); \
             STOCK is RunPod's stock hint as community/secure (over hosts at or above the CUDA floor, if one is set).\n\
             (1 more in RunPod's catalog are hidden — listed but rejected by the create API, so --gpu can't use them; see --json.)\n"
        );
        let v1 = RunpodCatalog { creatable_known: false, stock: Stock::OneGpu, ..live.clone() };
        let out = render_runpod_catalog(&RunpodCatalog { rows: vec![row("NVIDIA RTX A4000", "RTX A4000", 16, None)], ..v1 });
        assert!(out.contains("1 GPU types (live from RunPod). ") && out.contains("stock hint for a 1-GPU pod (either tier).\n") && !out.contains("hidden"), "{out}");
        let out = render_runpod_catalog(&RunpodCatalog::presets(NO_KEY));
        assert!(out.starts_with("GPU ") && out.contains("~$0.17") && out.contains("— set RUNPOD_API_KEY for RunPod's full live list.)"), "{out}");
        // `gpus`' own footer, word for word as it always printed it.
        let out = render_runpod_catalog(&RunpodCatalog::presets(GPUS_HINT));
        assert!(out.ends_with(
            "\n(local presets, ~ = rough estimate — run with --provider runpod + a key for the full live list.)\n\
             Pass the API name, label, or alias (e.g. `A4000`, `3090`, \"A100 SXM\") to --gpu.\n"
        ), "{out}");
    }

    #[test]
    fn server_types_show_size_price_and_where_grouped_with_the_configured_one_starred() {
        let arm = ServerType { name: "cax11".into(), architecture: Some("arm".into()), eur_per_hour: Some(0.0112), ..cx23() };
        let ded = ServerType { name: "ccx13".into(), cpu_type: Some("dedicated".into()), memory_gb: Some(8.0), disk_gb: Some(80), eur_per_hour: Some(0.0817), locations: vec!["ash".into()], ..cx23() };
        let cx33 = ServerType { name: "cx33".into(), cores: Some(4), memory_gb: Some(8.0), eur_per_hour: Some(0.016), ..cx23() };
        assert_eq!(
            render_server_types(&[ded, cx33, arm, cx23()], "cx23"),
            "TYPE   VCPU  KIND            RAM   DISK     €/H  LOCATIONS\n\
             cx — shared x86, cost-optimised\n\
             cx23*     2  shared x86     4 GB  40 GB  €0.010  nbg1, fsn1, hel1\n\
             cx33      4  shared x86     8 GB  40 GB  €0.016  nbg1, fsn1, hel1\n\
             \ncax — shared Arm\n\
             cax11     2  shared arm     4 GB  40 GB  €0.011  nbg1, fsn1, hel1\n\
             \nccx — dedicated x86\n\
             ccx13     2  dedicated x86  8 GB  80 GB  €0.082  ash\n\
             \n4 types · * = HETZNER_SERVER_TYPE · --server-type <TYPE> picks another · €/h in the first location\n"
        );
        let retired = render_server_types(&[cx23()], "cpx41");
        assert!(retired.ends_with("HETZNER_SERVER_TYPE `cpx41` isn't on sale anywhere now — `up --provider hetzner` would be refused.\n"), "{retired}");
        assert!(!retired.contains("cx23*"));
    }

    /// The catalogue's order, whatever order Hetzner (or its prices) gave: cx, cpx — the newer
    /// cpx?2 before the older cpx?1 — cax, ccx, then a family arena doesn't know; within each
    /// by size (vCPU, then RAM), which is the names' own order.
    #[test]
    fn server_types_are_grouped_by_family_then_sized() {
        let t = |name: &str, cores: u32, ram: f64| ServerType { name: name.into(), cores: Some(cores), memory_gb: Some(ram), ..ServerType::default() };
        let mut shuffled = vec![
            t("ccx23", 4, 16.0),
            t("cpx21", 3, 4.0),
            t("cx53", 16, 32.0),
            t("cax11", 2, 4.0),
            t("cpx02", 1, 1.0),
            t("zz1", 1, 1.0),
            t("cpx42", 8, 16.0),
            t("cx23", 2, 4.0),
            t("cpx11", 2, 2.0),
            t("ccx13", 2, 8.0),
            t("cpx12", 1, 2.0),
            t("cax21", 4, 8.0),
            t("cx33", 4, 8.0),
        ];
        order_server_types(&mut shuffled);
        let names: Vec<&str> = shuffled.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            ["cx23", "cx33", "cx53", "cpx02", "cpx12", "cpx42", "cpx11", "cpx21", "cax11", "cax21", "ccx13", "ccx23", "zz1"]
        );
    }

    /// A live v2 catalog as `offers runpod` reads it: a secure-only B300 and MIG slice.
    fn live_catalog() -> RunpodCatalog {
        let mut a4000 = row("NVIDIA RTX A4000", "RTX A4000", 16, Some(true));
        a4000.stock_status = Some("None".into());
        let mig = GpuRow {
            community_price: None,
            secure_price: Some(0.69),
            ..row("NVIDIA RTX PRO 6000 Blackwell Server Edition MIG 1g.24gb", "PRO 6000 MIG 24GB", 24, Some(true))
        };
        let b300 = GpuRow { community_price: Some(6.94), secure_price: Some(8.99), ..row("NVIDIA B300 SXM6 AC", "B300", 288, Some(true)) };
        let gone = GpuRow { community_price: None, secure_price: None, ..row("NVIDIA Old", "Old", 8, Some(true)) };
        RunpodCatalog { rows: vec![gone, a4000, mig, b300], live: true, creatable_known: true, stock: Stock::BothTiers, hint: "" }
    }

    /// Every GPU once per tier that sells it, with that tier's price and stock — so a
    /// secure-only MIG slice or B300 shows, never hidden by the configured COMMUNITY tier; a
    /// type no tier sells now stays, marked; the unlisted types arena knows come last, marked,
    /// priced only by a pod's own bill. `#` marks what `up` would try, in its order.
    #[test]
    fn runpod_rows_list_every_type_on_every_tier_and_mark_what_up_would_try() {
        let book = PriceBook::runpod(Vec::new());
        let req = Request::from_flags(Some("A4000"), Some("secure,community"), &PodSpec::from_config(&crate::config::Config::parse("")), None, Order::Listed).unwrap();
        let plan = plan_options(&req, "runpod", &book);
        let mig = "NVIDIA B300 SXM6 AC MIG 1g.34gb";
        let unlisted = [Unlisted { id: mig.into(), pod_price_per_gpu: Some(0.5) }, Unlisted { id: "NVIDIA RTX A4000".into(), pod_price_per_gpu: None }];
        let all = RowFilter { gpu_count: 1, ..RowFilter::default() };
        let rows = runpod_rows(&live_catalog(), &unlisted, Some(&plan), &all);
        let brief: Vec<(&str, Option<&str>, Option<f64>, Option<usize>, Option<&str>)> =
            rows.iter().map(|r| (r.name.as_str(), r.cloud.as_deref(), r.price_per_gpu, r.try_order, r.note.as_deref())).collect();
        assert_eq!(
            brief,
            [
                ("Old", None, None, None, Some("not sold now")),
                ("RTX A4000", Some("COMMUNITY"), Some(0.17), Some(2), None),
                ("RTX A4000", Some("SECURE"), Some(0.25), Some(1), None),
                ("PRO 6000 MIG 24GB", Some("SECURE"), Some(0.69), None, None),
                ("B300", Some("COMMUNITY"), Some(6.94), None, None),
                ("B300", Some("SECURE"), Some(8.99), None, None),
                ("B300 MIG 34GB", None, Some(0.5), None, Some("unlisted")),
            ]
        );
        assert_eq!((rows[2].stock.as_deref(), rows[1].stock.as_deref()), (Some("Low"), Some("None")), "each tier's own stock");
        assert_eq!((rows[6].vram_gb, rows[6].price_source.as_deref()), (Some(34), Some("pod")));
        // Filters: --gpu, --cloud (a row of unknown tier stays), --max-price per pod (unknown
        // price: out, as create drops it).
        let f = |gpus: Option<&[&str]>, clouds: Option<&[&str]>, cap: Option<f64>, n: u32| RowFilter {
            gpus: gpus.map(|g| g.iter().map(|s| s.to_string()).collect()),
            clouds: clouds.map(|c| c.iter().map(|s| s.to_string()).collect()),
            max_price: cap,
            gpu_count: n,
        };
        let names = |f: &RowFilter| runpod_rows(&live_catalog(), &unlisted, None, f).iter().map(|r| format!("{}/{}", r.name, r.cloud.as_deref().unwrap_or("-"))).collect::<Vec<_>>();
        assert_eq!(names(&f(Some(&["NVIDIA B300 SXM6 AC", mig]), None, None, 1)), ["B300/COMMUNITY", "B300/SECURE", "B300 MIG 34GB/-"]);
        assert_eq!(names(&f(None, Some(&["SECURE"]), Some(0.70), 1)), ["RTX A4000/SECURE", "PRO 6000 MIG 24GB/SECURE", "B300 MIG 34GB/-"]);
        assert_eq!(names(&f(None, None, Some(0.50), 2)), ["RTX A4000/COMMUNITY", "RTX A4000/SECURE"], "the cap is per pod");
        // The table: `#`, tiers, the unlisted marked and unpriced ones `?`, the --gpu name.
        let out = render_rows(&runpod_rows(&live_catalog(), &[Unlisted { id: mig.into(), pod_price_per_gpu: None }], Some(&plan), &all));
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "#  GPU                VRAM  CLOUD      PER GPU  STOCK  NOTE          --gpu");
        assert_eq!(lines[3], "1  RTX A4000           16G  SECURE     $0.25/h  Low                  NVIDIA RTX A4000");
        assert_eq!(lines[7], "   B300 MIG 34GB       34G  -                ?  -      unlisted      NVIDIA B300 SXM6 AC MIG 1g.34gb");
        // v1's create enum hides nothing here: the row says it.
        let mut v1 = live_catalog();
        v1.rows[1].creatable = Some(false);
        assert_eq!(runpod_rows(&v1, &[], None, &all)[1].note.as_deref(), Some("v1: not creatable"));
    }

    /// Vast's market per card and host class: prices per GPU, where the cheapest offer is, the
    /// unverified ones marked as needing `--cloud unverified` when the default is verified;
    /// `#` on the row each option would rent — the cheapest its tier admits.
    #[test]
    fn vast_rows_show_each_class_and_what_up_would_rent() {
        use crate::provider::vast::{Host, MarketRow, Offer};
        let offer = |price: f64, host: Host, at: &str| Offer {
            id: 1,
            gpu_name: "RTX A4000".into(),
            num_gpus: 2,
            disk_space: 100.0,
            price,
            cuda_max_good: Some(13.0),
            geolocation: Some(at.into()),
            host,
            vram_gb: Some(16),
        };
        let market = vec![
            MarketRow { gpu: "RTX A4000".into(), host: Host::Datacenter, offer: offer(0.40, Host::Datacenter, "Norway, NO") },
            MarketRow { gpu: "RTX A4000".into(), host: Host::Verified, offer: offer(0.30, Host::Verified, "Spain, ES") },
            MarketRow { gpu: "RTX A4000".into(), host: Host::Unverified, offer: offer(0.20, Host::Unverified, "Texas, US") },
        ];
        let base = PodSpec { gpu_type: "NVIDIA RTX A4000".into(), gpu_count: 2, ..PodSpec::from_config(&crate::config::Config::parse("")) };
        let req = Request::from_flags(None, Some("datacenter,verified"), &base, None, Order::Listed).unwrap();
        let plan = plan_options(&req, "vast", &PriceBook::unpriced());
        let rows = vast_rows(&market, 2, Host::Verified, Some(&plan), &RowFilter { gpu_count: 2, ..RowFilter::default() });
        let brief: Vec<(&str, Option<f64>, Option<usize>, Option<&str>)> =
            rows.iter().map(|r| (r.cloud.as_deref().unwrap(), r.price_per_gpu, r.try_order, r.note.as_deref())).collect();
        assert_eq!(
            brief,
            [("DATACENTER", Some(0.20), Some(1), None), ("VERIFIED", Some(0.15), Some(2), None), ("UNVERIFIED", Some(0.10), None, Some("--cloud unverified"))]
        );
        assert_eq!(rows[1].location.as_deref(), Some("Spain, ES"));
        // A near option rents in its region, which these rows don't show: it marks none.
        let mut near = plan.clone();
        near.options[0].near = Some(Region::Europe);
        let marked: Vec<Option<usize>> = vast_rows(&market, 2, Host::Verified, Some(&near), &RowFilter { gpu_count: 2, ..RowFilter::default() }).iter().map(|r| r.try_order).collect();
        assert_eq!(marked, [None, Some(2), None]);
        let out = render_rows(&rows);
        assert_eq!(out.lines().next(), Some("#  GPU        VRAM  CLOUD       PER GPU  WHERE       NOTE"), "{out}");
        // --gpu by RunPod's id or Vast's name; --cloud by class.
        let f = RowFilter { gpus: Some(vec!["NVIDIA RTX A4000".into()]), clouds: Some(vec!["UNVERIFIED".into()]), gpu_count: 2, ..RowFilter::default() };
        assert_eq!(vast_rows(&market, 2, Host::Unverified, None, &f).len(), 1);
        assert!(vast_rows(&market, 2, Host::Unverified, None, &f)[0].note.is_none(), "allowed: no note");
        let f = RowFilter { gpus: Some(vec!["3090".into()]), gpu_count: 2, ..RowFilter::default() };
        assert!(vast_rows(&market, 2, Host::Verified, None, &f).is_empty());
    }

    /// A catalogue's text: heading, table, notes; Hetzner's grouped; `--json` as documented.
    #[test]
    fn catalogs_render_and_serialize() {
        let c = Catalog { provider: "hetzner".into(), gpu_count: 1, server_types: Some(vec![cx23()]), notes: vec!["n".into()], ..Catalog::default() };
        let out = render_catalog(&c, "cx23");
        assert!(out.starts_with("hetzner — server types\nTYPE ") && out.contains("\ncx — shared x86, cost-optimised\ncx23*") && out.ends_with("\nn\n"), "{out}");
        let empty = Catalog { provider: "runpod".into(), gpu_count: 2, ..Catalog::default() };
        assert_eq!(render_catalog(&empty, ""), "runpod — GPU types on every tier (for 2-GPU pods)\n(nothing matches)\n");
        let sf = Catalog { provider: "shadeform".into(), gpu_count: 1, notes: vec![no_catalog("shadeform")], ..Catalog::default() };
        assert_eq!(render_catalog(&sf, ""), format!("shadeform — no list\n{}\n", no_catalog("shadeform")));
        let v = serde_json::to_value(Catalogs { catalogs: vec![c], ..Catalogs::default() }).unwrap();
        assert_eq!(v["catalogs"][0]["server_types"][0]["name"], "cx23");
        assert_eq!((v["catalogs"][0]["rows"].clone(), v["skipped"].clone(), v["failed"].clone()), (serde_json::json!([]), serde_json::json!([]), serde_json::json!([])));
        let row = CatalogRow {
            gpu: "g".into(),
            name: "G".into(),
            vram_gb: None,
            cloud: None,
            price_per_gpu: None,
            price_source: None,
            stock: None,
            location: None,
            try_order: None,
            note: None,
        };
        assert_eq!(
            serde_json::to_value(row).unwrap(),
            serde_json::json!({"gpu": "g", "name": "G", "vram_gb": null, "cloud": null, "price_per_gpu": null, "price_source": null, "stock": null, "try_order": null})
        );
    }

}
