//! `arena offers` and its old name `arena gpus`: the I/O around [`arena_core::offers`].
//!
//! Two views. `arena offers`: what `create`/`up` would try on each provider, each provider's
//! rows from the very functions they run — the `--gpu` check ([`checked_gpu`]), the request
//! from the flags and [`plan_placement`] — so the table is what they would try there, in their
//! order: when placement changes how it orders or prices options, `offers` shows it without a
//! change here. `arena offers <provider>|all`: everything a provider sells ([`handle_catalogs`]),
//! with what `up` would pick marked by that same plan. Read-only throughout: catalog, market
//! and price reads, the account's pod list (RunPod: for types its catalog lacks), and on
//! Hetzner one `GET /server_types` plus the location timing a create does.

use anyhow::Result;
use arena_core::offers::{self, Catalog, Catalogs, Offers, ProviderOffers, RowFilter, RunpodCatalog, Skipped, Stock, Unlisted};
use arena_core::provider::hetzner::{HetznerOpts, HetznerProvider, ServerType};
use arena_core::{Config, PodSpec};

use super::{checked_gpu, live_gpu_catalog, money, plan_placement, sizing_flags, spec_with_overrides, v2_catalog, SpecOverrides, PRICE_TIMEOUT};

/// The placement flags `offers` shares with `create`/`up`.
#[derive(Debug, Clone, Default)]
pub(crate) struct Flags {
    pub gpu: Option<String>,
    pub cloud: Option<String>,
    pub max_price: Option<f64>,
    pub gpus: Option<u32>,
    pub server_type: Option<String>,
    pub order: arena_core::placement::Order,
}

/// Whether `create` would refuse these flags on `provider` because they don't apply there
/// ([`sizing_flags`]: `--gpu`/`--gpus` on Hetzner's CPU VMs, `--server-type` on a GPU
/// provider) — not because one is malformed. Pure.
fn misapplied(provider: &str, f: &Flags) -> bool {
    if provider == "hetzner" {
        f.gpu.is_some() || f.gpus.is_some()
    } else {
        f.server_type.is_some()
    }
}

/// The views `arena offers [VIEW]` takes: a provider's catalogue, or every provider's.
pub(crate) const VIEWS: [&str; 5] = ["runpod", "vast", "hetzner", "shadeform", "all"];

/// The providers a catalogue view lists, and the ones it leaves out on purpose: `view`'s
/// provider alone, or with `all` every configured one but opt-in Shadeform (said, as the
/// default view says it). `--provider` (`named`) picks a provider for the default view; with
/// a view it must agree. Pure.
fn view_scope(view: &str, named: Option<&str>, has_key: impl Fn(&str) -> bool + Copy) -> Result<(Vec<&'static str>, Vec<Skipped>)> {
    match (view, named) {
        ("all", Some(n)) => anyhow::bail!("`offers all` lists every provider — drop --provider {n}, or run `offers {n}`"),
        ("all", None) => Ok((offers::providers(None, has_key).map_err(|e| anyhow::anyhow!(e))?, offers::opt_in_left_out(None, has_key))),
        (v, Some(n)) if n != v => anyhow::bail!("`offers {v}` and --provider {n}: name one provider"),
        (v, _) => Ok((offers::providers(Some(v), has_key).map_err(|e| anyhow::anyhow!(e))?, Vec::new())),
    }
}

/// `arena offers [VIEW]`. No view: what `create`/`up` would try on each provider
/// ([`offers::providers`]: the one `--provider` names, else every configured one), `--json`
/// [`Offers`]. A view: that provider's catalogue, or `all` of them ([`handle_catalogs`]).
/// `named`: `--provider` when given on the command line (its default can't tell). `verbose`:
/// the notes under the table. A provider that failed exits non-zero, after the rest is shown.
pub(crate) async fn handle_offers(cfg: &Config, named: Option<&str>, view: Option<&str>, flags: Flags, json: bool, verbose: bool) -> Result<()> {
    let has_key = |k: &str| cfg.get(k).is_some_and(|v| !v.trim().is_empty());
    if let Some(view) = view {
        let (scope, left_out) = view_scope(view, named, has_key)?;
        return handle_catalogs(cfg, &scope, view != "all", left_out, &flags, json).await;
    }
    let scope = offers::providers(named, has_key).map_err(|e| anyhow::anyhow!(e))?;
    let left_out = offers::opt_in_left_out(named, has_key);
    let mut out = collect(&scope, named, &flags, |p, server_type| provider_offers(cfg, p, &flags, server_type)).await?;
    out.skipped.extend(left_out);
    if json {
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        print!("{}", offers::render(&out, verbose));
    }
    if !out.failed.is_empty() {
        let names: Vec<&str> = out.failed.iter().map(|f| f.provider.as_str()).collect();
        anyhow::bail!("couldn't plan {} as create would (see `failed:`)", names.join(", "));
    }
    if out.providers.is_empty() {
        // E.g. `--server-type` with no Hetzner key: every provider in scope was skipped.
        anyhow::bail!("no provider to show: the flags apply to none of {} (see `skipped:`)", scope.join(", "));
    }
    Ok(())
}

/// Each provider of `scope` planned by `plan` (the server type `--server-type` asks for, as
/// [`sizing_flags`] checked it): [`provider_offers`], or a fake in tests. A flag that doesn't
/// apply to a provider ([`misapplied`]) skips it; one whose planning fails — a `--gpu` typo, a
/// server type Hetzner doesn't sell — is listed as failed, and the others still show. With a
/// provider named, either fails the command, as `create` would.
async fn collect<F, Fut>(scope: &[&'static str], named: Option<&str>, flags: &Flags, mut plan: F) -> Result<Offers>
where
    F: FnMut(&'static str, Option<String>) -> Fut,
    Fut: std::future::Future<Output = Result<ProviderOffers>>,
{
    let mut out = Offers::default();
    for &provider in scope {
        let server_type = match sizing_flags(provider, flags.gpu.as_deref(), flags.gpus, flags.server_type.clone()) {
            Ok(t) => t,
            Err(e) if named.is_none() && misapplied(provider, flags) => {
                out.skipped.push(Skipped { provider: provider.into(), why: e.to_string() });
                continue;
            }
            Err(e) => return Err(e),
        };
        match plan(provider, server_type).await {
            Ok(p) => out.providers.push(p),
            Err(e) if named.is_none() => out.failed.push(Skipped { provider: provider.into(), why: format!("{e:#}") }),
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

/// The plan `create`/`up` would make on `provider` with these flags (`gpu`: `--gpu` as
/// [`checked_gpu`] passed it), and the spec it's layered on.
async fn plan_for(cfg: &Config, provider: &str, flags: &Flags, gpu: Option<&str>, server_type: Option<String>) -> Result<(arena_core::placement::OptionPlan, PodSpec)> {
    let base = spec_with_overrides(cfg, &SpecOverrides { gpus: flags.gpus, server_type, ..Default::default() });
    let req = arena_core::placement::Request::from_flags(gpu, flags.cloud.as_deref(), &base, flags.max_price, flags.order)?;
    let plan = plan_placement(cfg, provider, &req, &base).await?;
    // Live catalog/offer prices are remembered for the balance's known-GPU stand-ins.
    money::BalanceCtx::from_config(cfg).remember_gpu_prices(&plan);
    Ok((plan, base))
}

/// One provider's plan, made as `create` makes it, with what the table adds: VRAM, and on
/// Hetzner the server type a create would ask for.
async fn provider_offers(cfg: &Config, provider: &str, flags: &Flags, server_type: Option<String>) -> Result<ProviderOffers> {
    let gpu = checked_gpu(cfg, provider, flags.gpu.clone()).await?;
    let (plan, base) = plan_for(cfg, provider, flags, gpu.as_deref(), server_type).await?;
    if provider == "hetzner" {
        let (server, note) = hetzner_offer(cfg, &base).await?;
        let mut offers = ProviderOffers::new(plan, &[], Some(server));
        offers.notes.extend(note);
        return Ok(offers);
    }
    // VRAM: the presets know the usual cards; RunPod's catalog (cached per run on v2, where
    // the plan just read it) the rest. No key, or no answer: unknown, never an error.
    let unknown = plan.options.iter().any(|o| arena_core::gpu::find(&o.gpu).is_none());
    let catalog = if unknown { live_gpu_catalog(cfg).await.unwrap_or_default() } else { Vec::new() };
    Ok(ProviderOffers::new(plan, &catalog, None))
}

/// The server type `base` would create on Hetzner, read from its catalog
/// ([`HetznerProvider::offer`], bounded by [`PRICE_TIMEOUT`]) — or, when that can't be read,
/// just its name with a note saying why: the row still shows what create would ask for. `Err`:
/// the catalog says create would refuse it (a type Hetzner doesn't have, or sells nowhere
/// `HETZNER_LOCATION` allows) — that's no option to offer.
async fn hetzner_offer(cfg: &Config, base: &PodSpec) -> Result<(ServerType, Option<String>)> {
    let opts = HetznerOpts::from_config(cfg);
    let name = opts.server_type_for(base).to_string();
    let bare = ServerType { name: name.clone(), ..Default::default() };
    let Some(key) = cfg.get("HETZNER_API_KEY").filter(|k| !k.trim().is_empty()) else {
        return Ok((bare, Some(format!("no HETZNER_API_KEY — {name}'s size and price are unknown"))));
    };
    match arena_core::ratelimit::bounded(PRICE_TIMEOUT, HetznerProvider::new(key, opts).offer(base)).await {
        Ok(Ok(t)) => Ok((t, None)),
        Ok(Err(arena_core::Error::Config(why))) => anyhow::bail!("create would refuse {name}: {why}"),
        Ok(Err(e)) => Ok((bare, Some(format!("couldn't read {name} from Hetzner's server types ({e}) — size and price unknown")))),
        Err(_) => Ok((bare, Some(format!("Hetzner didn't answer within {}s — {name}'s size and price are unknown", PRICE_TIMEOUT.as_secs())))),
    }
}

/// `offers <provider>|all`: each provider of `scope`'s whole list, filtered by `--gpu`,
/// `--cloud` and `--max-price` and marked with what `up` would try ([`catalog`]). A flag that
/// doesn't apply to a provider ([`misapplied`]) skips it, or with the provider named (`named`)
/// fails as `create` would; a list that can't be read is a failure — the others still show,
/// then the exit is non-zero. `--json` prints [`Catalogs`].
async fn handle_catalogs(cfg: &Config, scope: &[&'static str], named: bool, left_out: Vec<Skipped>, flags: &Flags, json: bool) -> Result<()> {
    let mut out = Catalogs { skipped: left_out, ..Catalogs::default() };
    let configured = flags.server_type.clone().unwrap_or_else(|| HetznerOpts::from_config(cfg).server_type);
    for &provider in scope {
        let server_type = match super::sizing_flags(provider, flags.gpu.as_deref(), flags.gpus, flags.server_type.clone()) {
            Ok(t) => t,
            Err(e) if !named && misapplied(provider, flags) => {
                out.skipped.push(Skipped { provider: provider.into(), why: e.to_string() });
                continue;
            }
            Err(e) => return Err(e),
        };
        match catalog(cfg, provider, flags, server_type).await {
            Ok(c) => out.catalogs.push(c),
            Err(e) if !named => out.failed.push(Skipped { provider: provider.into(), why: format!("{e:#}") }),
            Err(e) => return Err(e),
        }
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        let text: Vec<String> = out.catalogs.iter().map(|c| offers::render_catalog(c, &configured)).collect();
        print!("{}", text.join("\n"));
        for s in &out.skipped {
            println!("skipped: {}: {}", s.provider, s.why);
        }
        for s in &out.failed {
            println!("failed: {}: {}", s.provider, s.why);
        }
    }
    if !out.failed.is_empty() {
        let names: Vec<&str> = out.failed.iter().map(|f| f.provider.as_str()).collect();
        anyhow::bail!("couldn't list {} (see `failed:`)", names.join(", "));
    }
    Ok(())
}

/// What `offers <provider>` keeps of a catalogue: `--gpu` (`gpus`: as checked), `--cloud`,
/// `--max-price`, per pod of `--gpus` (config `NUM_GPUS`).
fn row_filter(cfg: &Config, flags: &Flags, gpus: Option<&str>) -> RowFilter {
    RowFilter {
        gpus: gpus.map(arena_core::placement::split_list),
        clouds: flags.cloud.as_deref().map(|c| arena_core::placement::split_list(c).iter().map(|c| c.to_uppercase()).collect()),
        max_price: flags.max_price,
        gpu_count: flags.gpus.unwrap_or_else(|| PodSpec::from_config(cfg).gpu_count),
    }
}

/// One provider's catalogue for `offers <provider>`: RunPod's GPU types on every tier with the
/// types its catalog lacks ([`runpod_catalog_view`]), Vast's market ([`vast_catalog`]),
/// Hetzner's server types, and for Shadeform how to price a GPU on it. `server_type`:
/// `--server-type`, checked ([`super::sizing_flags`]).
async fn catalog(cfg: &Config, provider: &'static str, flags: &Flags, server_type: Option<String>) -> Result<Catalog> {
    let gpu_count = flags.gpus.unwrap_or_else(|| PodSpec::from_config(cfg).gpu_count);
    let mut c = Catalog { provider: provider.into(), gpu_count, ..Catalog::default() };
    match provider {
        "runpod" => runpod_catalog_view(cfg, flags, &mut c).await?,
        "vast" => vast_catalog(cfg, flags, &mut c).await?,
        "hetzner" => {
            let opts = HetznerOpts::from_config(cfg);
            let Some(key) = cfg.get("HETZNER_API_KEY").filter(|k| !k.trim().is_empty()) else {
                anyhow::bail!("no HETZNER_API_KEY — Hetzner's server types can't be read");
            };
            let mut types = match arena_core::ratelimit::bounded(PRICE_TIMEOUT, HetznerProvider::new(key, opts).catalog()).await {
                Ok(read) => read.map_err(|e| anyhow::anyhow!("couldn't read the server types ({e})"))?,
                Err(_) => anyhow::bail!("Hetzner didn't answer within {}s", PRICE_TIMEOUT.as_secs()),
            };
            offers::order_server_types(&mut types);
            if let Some(t) = &server_type {
                types.retain(|s| &s.name == t);
            }
            if flags.max_price.is_some() || flags.cloud.is_some() {
                c.notes.push("hetzner: --cloud and --max-price ($) don't apply to its server types (priced in €)".into());
            }
            c.server_types = Some(types);
        }
        other => c.notes.push(offers::no_catalog(other)),
    }
    Ok(c)
}

/// RunPod's catalogue: [`runpod_catalog`]'s rows once per tier, the GPU types the catalog
/// lacks that arena has seen ([`seen_gpu_types`], with a pod's own price), marked with what
/// `up` would try ([`plan_for`]), filtered as asked. Notes: how to read it, and that RunPod's
/// CPU pods aren't here (arena creates GPU pods only: v2's create takes `cpu` instead of
/// `gpu`, and arena always sends `gpu`).
async fn runpod_catalog_view(cfg: &Config, flags: &Flags, c: &mut Catalog) -> Result<()> {
    let gpu = checked_gpu(cfg, "runpod", flags.gpu.clone()).await?;
    let (cat, warning) = runpod_catalog(cfg).await?;
    if let Some(w) = warning {
        eprint!("{w}");
    }
    let (plan, _) = plan_for(cfg, "runpod", flags, gpu.as_deref(), None).await?;
    let seen = seen_gpu_types(cfg).await;
    let unlisted: Vec<Unlisted> = seen.into_iter().map(|(id, pod_price_per_gpu)| Unlisted { id, pod_price_per_gpu }).collect();
    c.rows = offers::runpod_rows(&cat, &unlisted, Some(&plan), &row_filter(cfg, flags, gpu.as_deref()));
    let source = if cat.live { "RunPod's live catalog" } else { "arena's presets (~): set RUNPOD_API_KEY for the live list" };
    c.notes.push(format!(
        "# = what `up` would try, in order · prices: {source} · unlisted = not in RunPod's public catalog, seen on the \
         account's pods or configured (RUNPOD_EXTRA_GPU_TYPES): `--gpu` takes it, RunPod's create decides"
    ));
    c.notes.push("RunPod's CPU pods (cpu3/cpu5 flavours, secure cloud) aren't listed: arena creates GPU pods only".into());
    Ok(())
}

/// The GPU types RunPod's public catalog may lack that arena knows of, by exact id, each with a
/// pod's own price per GPU when one bills: arena's own list ([`arena_core::gpu::UNLISTED`]),
/// config `RUNPOD_EXTRA_GPU_TYPES`, and (on v2, with a key) the types on the account's pods —
/// one pod-list walk, bounded by [`PRICE_TIMEOUT`]; a failed read is just fewer types. The
/// catalog's own types are filtered out by the caller.
async fn seen_gpu_types(cfg: &Config) -> Vec<(String, Option<f64>)> {
    use arena_core::provider::{runpod_v2, RunpodApi};
    let mut out: Vec<(String, Option<f64>)> = Vec::new();
    let mut add = |id: &str, price: Option<f64>| {
        let id = id.trim();
        if id.is_empty() {
            return;
        }
        match out.iter_mut().find(|(g, _)| g.eq_ignore_ascii_case(id)) {
            Some((_, p)) if p.is_none() => *p = price,
            Some(_) => {}
            None => out.push((id.to_string(), price)),
        }
    };
    if let (Some(key), Ok(RunpodApi::V2)) = (cfg.get("RUNPOD_API_KEY").filter(|k| !k.trim().is_empty()), RunpodApi::from_config(cfg)) {
        if let Ok(Ok(pods)) = arena_core::ratelimit::bounded(PRICE_TIMEOUT, runpod_v2::fetch_pod_gpu_types(key)).await {
            pods.iter().for_each(|(id, price)| add(id, *price));
        }
    }
    for (id, _) in arena_core::gpu::UNLISTED {
        add(id, None);
    }
    for id in extra_gpu_types(cfg) {
        add(&id, None);
    }
    out
}

/// Config `RUNPOD_EXTRA_GPU_TYPES`: exact RunPod GPU type ids its public catalog lacks, comma
/// separated, for `--gpu` to accept and `offers runpod` to list.
pub(crate) fn extra_gpu_types(cfg: &Config) -> Vec<String> {
    cfg.get("RUNPOD_EXTRA_GPU_TYPES").map(arena_core::placement::split_list).unwrap_or_default()
}

/// Vast's catalogue: the market now ([`arena_core::provider::vast::VastProvider::market`]:
/// one search per host class — per class and card with `--gpu` — for a pod of `--gpus` GPUs
/// with the config's disk and CUDA floor), the cheapest offer per card and class, marked with
/// what `up` would rent. Without a key, or with every search failed, there's no list: an error
/// (review: codex — an empty table read as an empty market); a search that failed beside
/// others, or one that filled its limit, is a note.
async fn vast_catalog(cfg: &Config, flags: &Flags, c: &mut Catalog) -> Result<()> {
    use arena_core::provider::vast;
    if cfg.get("VAST_API_KEY").is_none_or(|k| k.trim().is_empty()) {
        anyhow::bail!("no VAST_API_KEY — Vast's market can't be searched");
    }
    let provider = vast::VastProvider::from_config(cfg)?;
    let default = vast::tier("", provider.verified_only())?;
    let gpu = flags.gpu.clone();
    let (plan, base) = plan_for(cfg, "vast", flags, gpu.as_deref(), None).await?;
    let wanted: Vec<String> = gpu.as_deref().map(|g| arena_core::placement::split_list(g).iter().map(|t| arena_core::gpu::resolve(t)).collect()).unwrap_or_default();
    let market = provider.market(&base, &wanted, PRICE_TIMEOUT).await;
    if let Some(first) = market.iter().find_map(|r| r.offers.as_ref().err()).filter(|_| market.iter().all(|r| r.offers.is_err())) {
        anyhow::bail!("couldn't search Vast's market: {first}");
    }
    for r in &market {
        let what = format!("{} hosts{}", r.class.key().to_ascii_lowercase(), r.card.as_deref().map(|c| format!(" for {c}")).unwrap_or_default());
        match &r.offers {
            Err(e) => c.notes.push(format!("vast: searching {what} failed ({e})")),
            Ok(_) if r.full => c.notes.push(format!("vast: only the cheapest offers on {what} were read — --gpu names a card to search it alone")),
            Ok(_) => {}
        }
    }
    let wanted = (!wanted.is_empty()).then(|| wanted.join(","));
    c.rows = offers::vast_rows(&vast::market_rows(&market), base.gpu_count, default, Some(&plan), &row_filter(cfg, flags, wanted.as_deref()));
    c.notes.push(format!(
        "# = what `up` would rent · each class's cheapest offer now (each create searches again) · up rents {} \
         unless --cloud datacenter|verified|unverified says",
        default.hosts()
    ));
    Ok(())
}

/// RunPod's full catalog with live community/secure prices and stock when there's a key — REST
/// `GET /v2/catalog/gpus` for both tiers on `v2` (no GraphQL there, see `runpod_v2`'s inventory),
/// GraphQL on `RUNPOD_API=v1` — else, or when that fails, the local presets; with the warning
/// to print when it failed. v1's rows are flagged against its create enum, which can be
/// narrower than the catalog (`gpus` hides what `--gpu` would get a 400 for, `offers runpod`
/// marks it); v2's create schema has no enum for `gpu.id` (its OpenAPI document, 2026-10-29),
/// so every row counts as creatable — and so may a type the catalog lacks, which only a create
/// can tell.
async fn runpod_catalog(cfg: &Config) -> Result<(RunpodCatalog, Option<String>)> {
    use arena_core::gpu;
    use arena_core::provider::{runpod, RunpodApi};
    let Some(key) = cfg.get("RUNPOD_API_KEY").filter(|s| !s.is_empty()) else {
        return Ok((RunpodCatalog::presets(offers::NO_KEY), None));
    };
    let api = RunpodApi::from_config(cfg)?;
    // v2: both tiers (finding #31: STOCK was always the community figure, so a secure-only
    // card read "None"). Rows and prices from COMMUNITY's answer (every tier's lists every
    // GPU); SECURE's adds its stock.
    let mut secure = None;
    let fetched = match api {
        RunpodApi::V1 => runpod::fetch_gpu_types(key).await.map_err(|e| e.to_string()),
        RunpodApi::V2 => {
            secure = v2_catalog(cfg, key, "SECURE").await.ok();
            v2_catalog(cfg, key, "COMMUNITY").await.map_err(|e| format!("v2 catalog (COMMUNITY): {e}"))
        }
    };
    let types = match fetched {
        Ok(types) if !types.is_empty() => types,
        Ok(_) => return Ok((RunpodCatalog::presets(offers::NOT_READ), None)),
        Err(e) => {
            let warning = format!("(couldn't fetch the live GPU list: {e} — showing local presets)\n");
            return Ok((RunpodCatalog::presets(offers::NOT_READ), Some(warning)));
        }
    };
    let creatable = match api {
        RunpodApi::V1 => runpod::fetch_creatable_gpu_ids(key).await.ok().filter(|v| !v.is_empty()),
        RunpodApi::V2 => Some(types.iter().map(|t| t.id.clone()).collect()),
    };
    let mut rows = gpu::rows_from_live(&types, creatable.as_deref());
    if let Some(secure) = &secure {
        gpu::add_secure_stock(&mut rows, secure);
    }
    let stock = match (api, secure.is_some()) {
        (RunpodApi::V2, true) => Stock::BothTiers,
        (RunpodApi::V2, false) => Stock::CommunityOnly,
        (RunpodApi::V1, _) => Stock::OneGpu,
    };
    Ok((RunpodCatalog { rows, live: true, creatable_known: creatable.is_some(), stock, hint: "" }, None))
}

/// `arena gpus`, kept for old habits and scripts, exactly as it was: RunPod's catalog as it
/// always printed it — with another `--provider`, the presets (RunPod's prices) — and `--json`
/// the same array of rows ([`arena_core::gpu::GpuRow`]). The pointer to `offers runpod` goes
/// to stderr, keeping stdout as it was.
pub(crate) async fn handle_gpus(cfg: &Config, provider: &str, json: bool) -> Result<()> {
    let (mut cat, warning) = if provider == "runpod" { runpod_catalog(cfg).await? } else { (RunpodCatalog::presets(offers::GPUS_HINT), None) };
    cat.hint = offers::GPUS_HINT;
    if let Some(w) = warning {
        eprintln!("{w}");
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&cat.rows)?);
        return Ok(());
    }
    print!("{}", offers::render_runpod_catalog(&cat));
    eprintln!("(`arena gpus` is RunPod's GPU list; `arena offers runpod` shows each tier, `arena offers all` every provider.)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena_core::placement::Order;

    fn cfg() -> Config {
        Config::parse("MACHINE_NAME_PREFIX=arena8\nGPU_TYPE=NVIDIA RTX A4000\nCLOUD_TYPE=COMMUNITY\n")
    }

    #[test]
    fn a_flag_misapplies_where_create_would_refuse_it_for_not_applying() {
        let gpu = Flags { gpu: Some("A4000".into()), ..Default::default() };
        let count = Flags { gpus: Some(2), ..Default::default() };
        let server = Flags { server_type: Some("cx33".into()), ..Default::default() };
        let cases = [
            ("hetzner", &gpu, true),
            ("hetzner", &count, true),
            ("hetzner", &server, false),
            ("runpod", &server, true),
            ("vast", &server, true),
            ("shadeform", &gpu, false),
            ("runpod", &Flags::default(), false),
        ];
        for (provider, flags, want) in cases {
            assert_eq!(misapplied(provider, flags), want, "{provider} {flags:?}");
        }
    }

    /// No key anywhere (unit tests never reach the network): RunPod alone, priced from the
    /// presets. A provider named explicitly is planned even without its key (unpriced, saying
    /// why); a flag that doesn't apply fails there as `create` fails, and a provider arena
    /// doesn't know is refused.
    #[tokio::test]
    async fn offers_plan_without_a_fleet_or_a_key() {
        let flags = Flags { gpu: Some("A4000,3090".into()), cloud: Some("community,secure".into()), max_price: Some(1.0), ..Default::default() };
        handle_offers(&cfg(), Some("vast"), None, flags.clone(), true, false).await.unwrap();
        handle_offers(&cfg(), None, None, Flags { gpus: Some(2), ..flags.clone() }, false, true).await.unwrap();
        let e = handle_offers(&cfg(), Some("hetzner"), None, flags.clone(), false, false).await.unwrap_err().to_string();
        assert!(e.starts_with("--gpu doesn't apply to hetzner"), "{e}");
        let e = handle_offers(&cfg(), Some("lambda"), None, Flags::default(), false, false).await.unwrap_err().to_string();
        assert_eq!(e, "unknown provider `lambda` (known: runpod, vast, hetzner, shadeform)");
        // Hetzner without a key: the type create would ask for, size and price unknown.
        let hz = provider_offers(&cfg(), "hetzner", &Flags { order: Order::Cheapest, ..Default::default() }, Some("cx33".into())).await.unwrap();
        assert_eq!((hz.options.len(), hz.server_type.as_ref().map(|t| t.name.as_str())), (1, Some("cx33")));
        assert_eq!(hz.notes.last().map(String::as_str), Some("no HETZNER_API_KEY — cx33's size and price are unknown"));
        // `offers runpod`/`all` and `gpus` without a key: the presets; a provider whose list
        // needs a key it hasn't fails, named or not (after the rest, not before).
        handle_offers(&cfg(), None, Some("runpod"), Flags::default(), true, false).await.unwrap();
        handle_offers(&cfg(), None, Some("all"), Flags { gpu: Some("A4000".into()), ..Default::default() }, false, false).await.unwrap();
        let e = handle_offers(&cfg(), None, Some("vast"), Flags::default(), false, false).await.unwrap_err().to_string();
        assert_eq!(e, "no VAST_API_KEY — Vast's market can't be searched");
        let e = handle_offers(&cfg(), None, Some("hetzner"), Flags::default(), true, false).await.unwrap_err().to_string();
        assert_eq!(e, "no HETZNER_API_KEY — Hetzner's server types can't be read");
        let e = handle_offers(&cfg(), None, Some("hetzner"), Flags { gpu: Some("A4000".into()), ..Default::default() }, true, false).await.unwrap_err().to_string();
        assert!(e.starts_with("--gpu doesn't apply to hetzner"), "{e}");
        handle_offers(&cfg(), None, Some("shadeform"), Flags::default(), false, false).await.unwrap();
        let (cat, warning) = runpod_catalog(&cfg()).await.unwrap();
        assert!(!cat.live && warning.is_none() && cat.rows.len() == arena_core::gpu::PRESETS.len());
        handle_gpus(&cfg(), "runpod", true).await.unwrap();
        handle_gpus(&cfg(), "vast", false).await.unwrap();
        // `--server-type` where no provider in scope takes it: nothing to show is an error.
        let e = handle_offers(&cfg(), None, None, Flags { server_type: Some("cx33".into()), ..Default::default() }, false, false).await.unwrap_err();
        assert_eq!(e.to_string(), "no provider to show: the flags apply to none of runpod (see `skipped:`)");
        // RunPod's config refusing its catalog fails `all` — after the rest is shown.
        let bad = Config::parse("MACHINE_NAME_PREFIX=arena8\nRUNPOD_API_KEY=k\nRUNPOD_API=v9\n");
        let e = handle_offers(&bad, None, Some("all"), Flags::default(), false, false).await.unwrap_err();
        assert_eq!(e.to_string(), "couldn't list runpod (see `failed:`)");
    }

    /// A view names its provider, or `all`; `--provider` must agree with it (it's the default
    /// view's way to name one); `all` leaves Shadeform out as the default view does, saying so.
    #[test]
    fn a_view_and_provider_agree() {
        let keys = |k: &str| ["RUNPOD_API_KEY", "HETZNER_API_KEY", "SHADEFORM_API_KEY"].contains(&k);
        let (scope, left) = view_scope("all", None, keys).unwrap();
        assert_eq!((scope, left.iter().map(|s| s.provider.as_str()).collect::<Vec<_>>()), (vec!["runpod", "hetzner"], vec!["shadeform"]));
        assert_eq!(view_scope("vast", None, keys).unwrap().0, ["vast"], "named: listed without a key, which then says why");
        assert_eq!(view_scope("shadeform", Some("shadeform"), keys).unwrap().0, ["shadeform"]);
        assert_eq!(view_scope("vast", Some("runpod"), keys).unwrap_err().to_string(), "`offers vast` and --provider runpod: name one provider");
        assert!(view_scope("all", Some("runpod"), keys).unwrap_err().to_string().starts_with("`offers all` lists every provider"));
    }

    /// A plan as a fake planner returns it (create's planner over no prices).
    fn planned(provider: &str) -> ProviderOffers {
        let base = PodSpec::from_config(&cfg());
        let req = arena_core::placement::Request::from_flags(None, None, &base, None, Order::Cheapest).unwrap();
        ProviderOffers::new(arena_core::placement::plan_options(&req, provider, &arena_core::placement::PriceBook::unpriced()), &[], None)
    }

    /// Over every provider (no `--provider`): a flag that doesn't apply to one skips it before
    /// it's planned, a planning failure is listed and the others still show; the server type
    /// `--server-type` names reaches Hetzner's planner. Named, either fails as `create` would.
    #[tokio::test]
    async fn collect_skips_what_doesnt_apply_and_lists_what_failed() {
        let scope = ["runpod", "vast", "hetzner"];
        let asked = std::sync::Mutex::new(Vec::new());
        let fake = |p: &'static str, server_type: Option<String>| {
            asked.lock().unwrap().push((p, server_type));
            async move {
                match p {
                    "vast" => Err(anyhow::anyhow!("--gpu: `3070x` isn't a GPU")),
                    _ => Ok(planned(p)),
                }
            }
        };
        let gpu = Flags { gpu: Some("A4000".into()), ..Default::default() };
        let out = collect(&scope, None, &gpu, fake).await.unwrap();
        assert_eq!(out.providers.iter().map(|p| p.provider.as_str()).collect::<Vec<_>>(), ["runpod"]);
        assert_eq!((out.failed[0].provider.as_str(), out.failed[0].why.as_str()), ("vast", "--gpu: `3070x` isn't a GPU"));
        assert_eq!(out.skipped.len(), 1);
        assert!(out.skipped[0].provider == "hetzner" && out.skipped[0].why.starts_with("--gpu doesn't apply to hetzner"), "{:?}", out.skipped);
        assert_eq!(std::mem::take(&mut *asked.lock().unwrap()), [("runpod", None), ("vast", None)], "hetzner never planned");

        let server = Flags { server_type: Some("CX33".into()), ..Default::default() };
        let out = collect(&scope, None, &server, fake).await.unwrap();
        assert_eq!(out.skipped.iter().map(|s| s.provider.as_str()).collect::<Vec<_>>(), ["runpod", "vast"]);
        assert_eq!(std::mem::take(&mut *asked.lock().unwrap()), [("hetzner", Some("cx33".to_string()))], "as create sends it");

        let e = collect(&["vast"], Some("vast"), &Flags::default(), fake).await.unwrap_err().to_string();
        assert_eq!(e, "--gpu: `3070x` isn't a GPU");
        let e = collect(&["hetzner"], Some("hetzner"), &gpu, fake).await.unwrap_err().to_string();
        assert!(e.starts_with("--gpu doesn't apply to hetzner"), "{e}");
        // A malformed --server-type isn't "doesn't apply": it fails even unnamed.
        let bad = Flags { server_type: Some("cx 23".into()), ..Default::default() };
        assert!(collect(&["hetzner"], None, &bad, fake).await.is_err());
    }
}
