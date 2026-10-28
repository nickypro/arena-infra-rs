//! `arena offers`: what `create`/`up` would try on each configured provider, in one table — and
//! with `--all`, every type each provider sells (RunPod's GPU catalog, which `arena gpus`
//! listed, and Hetzner's server types).
//!
//! It used to take two commands to answer "what can I get, and for how much": `gpus` (RunPod's
//! catalog only) and `offers` (the plan, one provider at a time), with Hetzner in neither. Pure:
//! the CLI fetches — through the very planner `create`/`up` run ([`crate::placement`]), so the
//! table is what they would try, in their order — and this lays it out, snapshot-tested.

use serde::Serialize;

use crate::fleet::fmt_money;
use crate::gpu::{self, GpuRow};
use crate::placement::{Dropped, OptionPlan, Order, PlacementOption, PriceSource};
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
    catalog.iter().find(|t| t.id == gpu && t.memory_gb > 0).map(|t| t.memory_gb).or_else(|| gpu::find(gpu).map(|g| g.vram_gb))
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

/// A provider `offers` left out, and why: a flag that doesn't apply there (`--gpu` on Hetzner,
/// `--server-type` anywhere else) — as `create` refuses it, only not an error when no
/// `--provider` was named.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Skipped {
    pub provider: String,
    pub why: String,
}

/// `arena offers` (`--json` prints this): each provider's options, and the providers left out.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Offers {
    pub providers: Vec<ProviderOffers>,
    pub skipped: Vec<Skipped>,
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
/// options in the order `create`/`up` would try them (`#`); then what `--max-price` dropped,
/// the notes, the providers left out, and how to read it. Prices carry their currency: a
/// Hetzner VM's `€` is Hetzner's own, never converted (as `pods list` shows it), and its source
/// is `list` — read for the table, not a price placement has. Pure.
pub fn render(offers: &Offers) -> String {
    use Align::{Left, Right};
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
            let (price, source) = match (o.price_per_pod, &p.server_type) {
                (Some(usd), _) if o.price_source == Some(PriceSource::Estimate) => (format!("~{}/h", fmt_money("$", usd)), "estimate"),
                (Some(usd), _) => (format!("{}/h", fmt_money("$", usd)), if o.price_source.is_some() { "live" } else { "-" }),
                (None, Some(ServerType { eur_per_hour: Some(eur), .. })) => (format!("{}/h", fmt_money("€", *eur)), "list"),
                (None, _) => ("-".into(), "-"),
            };
            rows.push(vec![
                p.provider.clone(),
                (i + 1).to_string(),
                p.label(o),
                o.cloud.clone().unwrap_or_else(|| "-".into()),
                size,
                price,
                source.into(),
                o.stock.clone().unwrap_or_else(|| "-".into()),
            ]);
        }
    }
    let mut out = if rows.is_empty() {
        "(no provider to show)\n".to_string()
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
    if !dropped.is_empty() {
        out.push_str("not tried (--max-price):\n");
        dropped.iter().for_each(|l| out.push_str(&format!("{l}\n")));
    }
    for p in &offers.providers {
        for n in &p.notes {
            out.push_str(&format!("note: {}\n", tagged(&p.provider, n)));
        }
        if let Some(ServerType { name, locations, eur_per_hour, .. }) = &p.server_type {
            if let (Some(first), Some(_)) = (locations.first(), eur_per_hour) {
                let walk = match locations.as_slice() {
                    [_] => format!("is created in {first}"),
                    many => format!("is tried in {} in that order", many.join(", ")),
                };
                out.push_str(&format!(
                    "note: hetzner: {name} {walk}; `list` is its gross list price in {first}, for reference \
                     (Hetzner bills in EUR, never converted)\n"
                ));
            }
        }
    }
    for s in &offers.skipped {
        out.push_str(&format!("skipped: {}: {}\n", s.provider, s.why));
    }
    out.push_str(
        "\n`pods create`/`up` create on one provider: --provider, default runpod. # is the order they'd try \
         its options in, one create at a time per name; a pool with no capacity is skipped for the rest of \
         that round. STOCK is a hint only — creating is the real test. `offers --all` lists every type.\n",
    );
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

/// RunPod's GPU catalog as `offers --all` and `gpus` show it (`--json`: `rows`).
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
}

/// RunPod's catalog the way `arena gpus` printed it: the table (minus what the create API
/// rejects, counted under it), then how to read it. Pure.
pub fn render_runpod_catalog(c: &RunpodCatalog) -> String {
    if !c.live {
        return format!(
            "{}\n(local presets, ~ = rough estimate — set RUNPOD_API_KEY for RunPod's full live list.)\n\
             Pass the API name, label, or alias (e.g. `A4000`, `3090`, \"A100 SXM\") to --gpu.\n",
            gpu::render_gpu_table(&c.rows)
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

/// Hetzner's server types for `offers --all`: size, € per hour where it's cheapest, where it's
/// sold. `configured` (`HETZNER_SERVER_TYPE`, what `create`/`up` use) is starred. Pure.
pub fn render_server_types(types: &[ServerType], configured: &str) -> String {
    use Align::{Left, Right};
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
    let mut out = table::render(&["TYPE", "VCPU", "KIND", "RAM", "DISK", "€/H", "LOCATIONS"], &[Left, Right, Left, Right, Right, Right, Left], &rows);
    out.push_str(&format!(
        "\n{} Hetzner server types on sale. Pass one to --server-type with --provider hetzner (* = HETZNER_SERVER_TYPE, \
         what create/up use). €/h is gross, in the first (cheapest) location listed; Hetzner bills in EUR.\n",
        types.len()
    ));
    out
}

/// `arena offers --all --json`: each catalog read, by provider. `runpod` is the array `arena
/// gpus --json` prints.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Catalogs {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runpod: Option<Vec<GpuRow>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hetzner: Option<Vec<ServerType>>,
    /// What isn't a catalog (a marketplace), or couldn't be read.
    pub notes: Vec<String>,
}

/// What `offers --all` says for a provider with no fixed list to show: Vast and Shadeform price
/// a GPU per search, at the cheapest offer of the moment.
pub fn no_catalog(provider: &str) -> String {
    format!(
        "{provider}: a marketplace with no fixed list — `arena offers --provider {provider} --gpu A4000,3090` prices \
         each GPU at its cheapest offer now"
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
    /// per provider; VRAM for GPUs, vCPU/RAM for Hetzner's VM, its € price as Hetzner's own;
    /// what the cap dropped, the notes (tagged by provider), what was skipped and the footer.
    #[test]
    fn the_table_is_every_providers_plan_in_try_order() {
        let runpod = plan("runpod", "A4000,3090", "community,secure", 1, Some(0.40), &PriceBook::runpod(Vec::new()));
        let vast = plan("vast", "3090", "community", 2, None, &PriceBook::offers(vec![("NVIDIA GeForce RTX 3090".into(), 0.36)], 2));
        let hetzner = plan("hetzner", "A4000", "community", 1, None, &PriceBook::unpriced());
        let offers = Offers {
            providers: vec![
                ProviderOffers::new(runpod, &[], None),
                ProviderOffers::new(vast, &[], None),
                ProviderOffers::new(hetzner, &[], Some(cx23())),
            ],
            skipped: vec![Skipped { provider: "shadeform".into(), why: "not configured".into() }],
        };
        assert_eq!(
            render(&offers),
            "PROVIDER  #  OPTION       CLOUD      SIZE               PER POD  SOURCE    STOCK\n\
             runpod    1  1×RTX A4000  COMMUNITY  16 GB VRAM        ~$0.17/h  estimate  -\n\
             runpod    2  1×RTX 3090   COMMUNITY  24 GB VRAM        ~$0.22/h  estimate  -\n\
             runpod    3  1×RTX A4000  SECURE     16 GB VRAM        ~$0.32/h  estimate  -\n\
             vast      1  2×RTX 3090   -          2×24 GB VRAM       $0.36/h  live      -\n\
             hetzner   1  cx23         -          2 vCPU, 4 GB RAM  €0.010/h  list      -\n\
             not tried (--max-price):\n  \
               runpod: 1×RTX 3090 SECURE: ~$0.43/h > cap $0.40/h per pod\n\
             note: runpod: ~ = preset estimate (no live price for that GPU); --max-price is checked against it\n\
             note: hetzner: cx23 is tried in nbg1, fsn1, hel1 in that order; `list` is its gross list price in nbg1, for reference (Hetzner bills in EUR, never converted)\n\
             skipped: shadeform: not configured\n\
             \n`pods create`/`up` create on one provider: --provider, default runpod. # is the order they'd try its \
             options in, one create at a time per name; a pool with no capacity is skipped for the rest of that round. \
             STOCK is a hint only — creating is the real test. `offers --all` lists every type.\n"
        );
    }

    /// A provider whose every option the cap dropped still has a row, saying so; Hetzner's VM
    /// is dropped by any cap (placement can't price it), named by its server type.
    #[test]
    fn a_provider_with_nothing_left_still_shows() {
        let hetzner = plan("hetzner", "A4000", "community", 1, Some(1.0), &PriceBook::unpriced());
        let offers = Offers { providers: vec![ProviderOffers::new(hetzner, &[], Some(cx23()))], skipped: Vec::new() };
        let out = render(&offers);
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
        assert_eq!(render(&Offers::default()).lines().next(), Some("(no provider to show)"));
        // A pinned location (HETZNER_LOCATION=nbg1) is one place, not an order.
        let one = plan("hetzner", "A4000", "community", 1, None, &PriceBook::unpriced());
        let pinned = ServerType { locations: vec!["nbg1".into()], ..cx23() };
        let out = render(&Offers { providers: vec![ProviderOffers::new(one, &[], Some(pinned))], skipped: Vec::new() });
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
            skipped: vec![],
        };
        let v = serde_json::to_value(&offers).unwrap();
        let first = &v["providers"][0];
        let plan_json = serde_json::to_value(&runpod).unwrap();
        for k in ["provider", "gpu_count", "order", "max_price", "dropped", "notes"] {
            assert_eq!(first[k], plan_json[k], "{k}");
        }
        let mut option = plan_json["options"][0].clone();
        option["vram_gb"] = 16.into();
        assert_eq!(first["options"][0], option);
        assert!(first.get("server_type").is_none());
        let hz = &v["providers"][1];
        assert_eq!((hz["options"][0]["vram_gb"].clone(), hz["options"][0]["price_per_pod"].clone()), (serde_json::Value::Null, serde_json::Value::Null));
        assert_eq!(hz["server_type"]["eur_per_hour"], 0.0104);
        assert_eq!(hz["server_type"]["locations"][0], "nbg1");
        assert_eq!(v["skipped"], serde_json::json!([]));
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
        let presets = RunpodCatalog { rows: gpu::rows_from_presets(), live: false, creatable_known: false, stock: Stock::OneGpu };
        let out = render_runpod_catalog(&presets);
        assert!(out.starts_with("GPU ") && out.contains("~$0.17") && out.contains("set RUNPOD_API_KEY for RunPod's full live list"), "{out}");
    }

    #[test]
    fn server_types_show_size_price_and_where_with_the_configured_one_starred() {
        let arm = ServerType { name: "cax11".into(), architecture: Some("arm".into()), eur_per_hour: Some(0.0112), ..cx23() };
        let ded = ServerType { name: "ccx13".into(), cpu_type: Some("dedicated".into()), memory_gb: Some(8.0), disk_gb: Some(80), eur_per_hour: Some(0.0817), locations: vec!["ash".into()], ..cx23() };
        assert_eq!(
            render_server_types(&[cx23(), arm, ded], "cx23"),
            "TYPE   VCPU  KIND            RAM   DISK     €/H  LOCATIONS\n\
             cx23*     2  shared x86     4 GB  40 GB  €0.010  nbg1, fsn1, hel1\n\
             cax11     2  shared arm     4 GB  40 GB  €0.011  nbg1, fsn1, hel1\n\
             ccx13     2  dedicated x86  8 GB  80 GB  €0.082  ash\n\
             \n3 Hetzner server types on sale. Pass one to --server-type with --provider hetzner (* = HETZNER_SERVER_TYPE, \
             what create/up use). €/h is gross, in the first (cheapest) location listed; Hetzner bills in EUR.\n"
        );
    }
}
