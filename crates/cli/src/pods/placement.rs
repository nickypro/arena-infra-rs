//! Where `create`/`up` put pods: the `--gpu` check, catalogs and prices, the placement plan and
//! its preview.

use std::time::Duration;

use anyhow::Result;
use arena_core::provider::Provider;
use arena_core::{Config, PodSpec};

use crate::pods::create::{spec_with_overrides, CreateFailed, Made, OnMade, SpecOverrides};
use crate::{money, offers, Interrupt};

/// `--max-price`: a positive, finite $/h (a leading `$` is fine).
pub(crate) fn parse_max_price(s: &str) -> std::result::Result<f64, String> {
    let v: f64 = s.trim().trim_start_matches('$').parse().map_err(|_| format!("`{s}` is not a $/h price (e.g. 0.50)"))?;
    if v.is_finite() && v > 0.0 {
        Ok(v)
    } else {
        Err(format!("must be a positive $/h (got {s})"))
    }
}

/// Decide how `create`/`up` place pods, before anything is listed or created. Returns the
/// overrides the single-spec path creates from, and: `None` = one GPU, one cloud and no
/// price cap: today's single-spec path, unchanged (same output, same `--keep-trying`).
/// `Some` = ordered placement over several options, with the spec base (config + every
/// other override) the options are layered on.
///
/// A list that collapses to one option (`--gpu A4000,a4000`, `--cloud community,`) is
/// "single" because it names one option — so the single path must create *that* option:
/// `--gpu`/`--cloud` come back as the parsed values, never the raw list string, which
/// would otherwise reach the provider as a GPU id `A4000,a4000` / tier `COMMUNITY,`.
///
/// `--keep-trying` waits on *one* pool; with several options the retry window
/// (`--retry-mins`) is the waiting mechanism — each round re-tries every option — so the
/// combination is refused rather than given a third meaning.
pub(crate) fn placement_request(
    cfg: &Config,
    provider_name: &str,
    ov: SpecOverrides,
    max_price: Option<f64>,
    order: arena_core::placement::Order,
    keep_trying: bool,
) -> Result<(SpecOverrides, Option<(arena_core::placement::Request, PodSpec)>)> {
    let base = spec_with_overrides(cfg, &SpecOverrides { gpu: None, cloud: None, ..ov.clone() });
    let req = arena_core::placement::Request::from_flags(ov.gpu.as_deref(), ov.cloud.as_deref(), &base, max_price, order)?
        .with_near(arena_core::near::Near::for_provider(cfg, provider_name)?, provider_name, &base);
    // Near the proxy turns one pool into two (there, then anywhere); `--keep-trying` waits on
    // one, so it keeps today's single path — anywhere — and says so.
    let single = arena_core::placement::Request { near: None, ..req.clone() };
    if keep_trying && req.near.is_some() && single.is_single() {
        let near = req.near.map(|n| n.region.label()).unwrap_or_default();
        eprintln!("note: --keep-trying waits on one pool, so this run doesn't try {near} first (NEAR) — --retry-mins N keeps that");
    }
    let req = if keep_trying && single.is_single() { single } else { req };
    if req.is_single() {
        // Only a flag that was given is replaced (an absent one keeps "what config says",
        // exactly as today); the parsed GPU is already resolved, and resolving is idempotent.
        let single = SpecOverrides {
            gpu: ov.gpu.as_ref().map(|_| req.gpus[0].clone()),
            cloud: ov.cloud.as_ref().map(|_| req.clouds[0].clone()),
            ..ov
        };
        return Ok((single, None));
    }
    req.validate_for(provider_name)?;
    if keep_trying {
        anyhow::bail!(
            "--keep-trying waits on one GPU/cloud; with several options or --max-price use \
             --retry-mins N instead (each round re-tries every option)"
        );
    }
    Ok((ov, Some((req, base))))
}

/// Upper bound on each read-only catalog lookup before placement. The HTTP client has no
/// timeout of its own; a stalled price API must not hang a create that doesn't need it
/// (it falls back to the preset estimates instead).
pub(crate) const PRICE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

async fn bounded_catalog<F>(fetch: F) -> arena_core::Result<Vec<arena_core::provider::runpod::GpuType>>
where
    F: std::future::Future<Output = arena_core::Result<Vec<arena_core::provider::runpod::GpuType>>>,
{
    match arena_core::ratelimit::bounded(PRICE_TIMEOUT, fetch).await {
        Ok(r) => r,
        Err(_) => Err(arena_core::Error::provider(format!("timed out after {}s", PRICE_TIMEOUT.as_secs()))),
    }
}

/// RunPod's REST v2 GPU catalog for one tier (read-only, bounded by [`PRICE_TIMEOUT`]),
/// fetched at most once per tier per run: the `--gpu` check and the placement plan's price
/// book share the answer (or the failure — see [`arena_core::provider::runpod_v2::CatalogCache`]).
///
/// Stock is counted over the hosts a create would accept: the config's CUDA floor goes along
/// as `minCudaVersion` (one floor per run, so the cache key needn't carry it).
pub(crate) async fn v2_catalog(cfg: &Config, key: &str, cloud: &str) -> std::result::Result<Vec<arena_core::provider::runpod::GpuType>, String> {
    use arena_core::provider::runpod_v2;
    static CACHE: std::sync::OnceLock<runpod_v2::CatalogCache> = std::sync::OnceLock::new();
    // A floor that isn't a version isn't sent to the read-only catalog (the create refuses it
    // with a clear error; a junk query parameter would only fail the `--gpu` check obscurely).
    let floor = PodSpec::from_config(cfg).cuda_floor().filter(|f| arena_core::pod::cuda_key(f).is_some());
    CACHE
        .get_or_init(runpod_v2::CatalogCache::default)
        .get_or_fetch(key, cloud, |tier| bounded_catalog(runpod_v2::fetch_gpu_types(key, tier, floor.as_deref())))
        .await
}

/// The tier whose v2 catalog answers questions that aren't about a tier (the `--gpu`
/// check, `arena offers runpod`): the configured `CLOUD_TYPE`, or COMMUNITY. Every tier's catalog
/// lists every GPU, so this only picks which tier's stock a reader sees — and with the
/// configured tier, a plain `create` reuses the check's fetch for its prices.
pub(crate) fn v2_default_cloud(cfg: &Config) -> &'static str {
    arena_core::provider::runpod_v2::normalize_cloud(&PodSpec::from_config(cfg).cloud_type).unwrap_or("COMMUNITY")
}

/// RunPod's live GPU catalog, for checking `--gpu` (read-only; each fetch bounded by
/// [`PRICE_TIMEOUT`]): on `RUNPOD_API=v2` the REST v2 catalog — the one v2's create
/// validates `gpu.id` against — and nothing else (no GraphQL, see `runpod_v2`'s GraphQL
/// inventory); otherwise the GraphQL `gpuTypes`, the full catalog whatever the tier (a
/// superset of what create accepts, so checking against it never refuses a creatable id).
/// `Err` says why there's no catalog (no key, unreachable, empty).
pub(crate) async fn live_gpu_catalog(cfg: &Config) -> std::result::Result<Vec<arena_core::provider::runpod::GpuType>, String> {
    use arena_core::provider::{runpod, RunpodApi};
    let Some(key) = cfg.get("RUNPOD_API_KEY").filter(|s| !s.is_empty()) else {
        return Err("no RUNPOD_API_KEY".into());
    };
    if matches!(RunpodApi::from_config(cfg), Ok(RunpodApi::V2)) {
        let cloud = v2_default_cloud(cfg);
        return v2_catalog(cfg, key, cloud).await.map_err(|e| format!("v2 catalog ({cloud}): {e}"));
    }
    match bounded_catalog(runpod::fetch_gpu_types(key)).await {
        Ok(types) if !types.is_empty() => Ok(types),
        Ok(_) => Err("GraphQL catalog: empty".to_string()),
        Err(e) => Err(format!("GraphQL catalog: {e}")),
    }
}

/// What to do with a `--gpu` value given the catalog fetch's outcome. Pure (the policy is
/// table-tested): with a catalog, every token must be a RunPod GPU — the value comes back
/// as exact ids, or the error names the bad tokens with "did you mean"
/// ([`arena_core::gpu::check_gpu_flag_with`]) — or a type the catalog lacks that arena has
/// seen (`seen`: the account's pods, `RUNPOD_EXTRA_GPU_TYPES`; and arena's own list), with a
/// warning each; without a catalog, the value passes through unchanged with a warning —
/// being unable to *check* must not block a create.
pub(crate) fn gpu_flag_or_warning(
    raw: String,
    catalog: std::result::Result<Vec<arena_core::provider::runpod::GpuType>, String>,
    seen: &[String],
) -> Result<(String, Vec<String>)> {
    match catalog {
        Ok(types) => {
            let checked = arena_core::gpu::check_gpu_flag_with(&raw, &types, seen)?;
            let warnings = checked
                .unlisted
                .iter()
                .map(|id| format!("warning: --gpu `{id}` isn't in RunPod's public catalog — trying it; RunPod's create decides"))
                .collect();
            Ok((checked.ids, warnings))
        }
        Err(why) => {
            let warning =
                format!("warning: couldn't check --gpu `{raw}` against RunPod's live GPU catalog ({why}) — using it unchecked");
            Ok((raw, vec![warning]))
        }
    }
}

/// `--gpu` as `create`/`up`/`offers`/`replace`/`migrate` should use it: on RunPod, checked
/// against the live catalog first — a typo (or a card without an alias) used to go to the
/// provider as an invalid id and fail late ("no known price", a 400 at create). Other
/// providers name GPUs their own way (Vast matches loosely; Hetzner has none): passed
/// through. Read-only, before anything is listed or created.
pub(crate) async fn checked_gpu(cfg: &Config, provider_name: &str, gpu: Option<String>) -> Result<Option<String>> {
    use arena_core::provider::{runpod, RunpodApi};
    let Some(raw) = gpu else { return Ok(None) };
    if provider_name == "shadeform" {
        // Shadeform answers an unknown GPU name with an empty catalog (read as "no stock"
        // forever): every token must map to one of its types, checked here before anything.
        for token in arena_core::placement::split_list(&raw) {
            arena_core::provider::shadeform::shadeform_gpu_type(&arena_core::gpu::resolve(&token))?;
        }
        return Ok(Some(raw));
    }
    if provider_name != "runpod" {
        return Ok(Some(raw));
    }
    let catalog = live_gpu_catalog(cfg).await;
    let types = catalog.as_ref().ok().cloned();
    // A type the catalog lacks may be one arena has seen: the config's list first, then — only
    // when that isn't enough, so a plain `--gpu A4000` costs no extra request — the account's pods.
    let extra = offers::extra_gpu_types(cfg);
    let (gpu, warnings) = match gpu_flag_or_warning(raw.clone(), catalog.clone(), &extra) {
        Ok(ok) => ok,
        Err(e) => {
            let pods = pod_gpu_types(cfg).await;
            if pods.is_empty() {
                return Err(e);
            }
            gpu_flag_or_warning(raw, catalog, &[extra, pods].concat())?
        }
    };
    for w in warnings {
        eprintln!("{w}");
    }
    // REST v1 only: its create validates against an OpenAPI enum that lags the catalog.
    let v1 = matches!(RunpodApi::from_config(cfg), Ok(RunpodApi::V1));
    if let (true, Some(types), Some(key)) = (v1, types, cfg.get("RUNPOD_API_KEY").filter(|k| !k.is_empty())) {
        if let Ok(Ok(creatable)) = arena_core::ratelimit::bounded(PRICE_TIMEOUT, runpod::fetch_creatable_gpu_ids(key)).await {
            v1_creatable_check(&gpu, &types, &creatable)?;
        }
    }
    Ok(Some(gpu))
}

/// The GPU types on the account's RunPod pods (v2 with a key; read-only, bounded by
/// [`PRICE_TIMEOUT`]); none when they can't be read.
async fn pod_gpu_types(cfg: &Config) -> Vec<String> {
    use arena_core::provider::{runpod_v2, RunpodApi};
    let Some(key) = cfg.get("RUNPOD_API_KEY").filter(|k| !k.trim().is_empty()) else { return Vec::new() };
    if !matches!(RunpodApi::from_config(cfg), Ok(RunpodApi::V2)) {
        return Vec::new();
    }
    match arena_core::ratelimit::bounded(PRICE_TIMEOUT, runpod_v2::fetch_pod_gpu_types(key)).await {
        Ok(Ok(types)) => types.into_iter().map(|(id, _)| id).collect(),
        _ => Vec::new(),
    }
}

/// RunPod REST v1 validates a create's `gpuTypeIds` against its OpenAPI enum, which lags the
/// live catalog: live (finding #22), `PRO 6000 MIG 24GB` passed the catalog check, ranked
/// first in `offers`, and its create was a `400 … schema requirements`. So on v1, with the
/// enum at hand, a `--gpu` id outside it is refused before anything is listed or created —
/// saying how to get back to v2 (the default), whose create takes it. An empty enum
/// (couldn't be read) refuses nothing: placement then skips an option the API rejects. Pure.
pub(crate) fn v1_creatable_check(ids: &str, catalog: &[arena_core::provider::runpod::GpuType], creatable: &[String]) -> Result<()> {
    if creatable.is_empty() {
        return Ok(());
    }
    let bad: Vec<String> = arena_core::placement::split_list(ids)
        .into_iter()
        .filter(|id| !creatable.iter().any(|c| c == id))
        .map(|id| match catalog.iter().find(|g| g.id == id && !g.display_name.is_empty()) {
            Some(g) => format!("`{}` ({id})", g.display_name),
            None => format!("`{id}`"),
        })
        .collect();
    if bad.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "--gpu {}: in RunPod's catalog, but REST v1's create API rejects it (not in its gpuTypeIds list) — \
         v2's create takes it: {}, or pick another --gpu (`arena offers runpod` marks what v1 can't create)",
        bad.join(", "),
        arena_core::provider::RUNPOD_V1_REMEDY
    )
}

/// The prices placement can see (read-only), plus notes on where they came from. RunPod:
/// the live catalog — on `RUNPOD_API=v2` one `/v2/catalog/gpus` per requested tier (its stock
/// is per tier; each tier fetched once per run, see [`v2_catalog`]) and no GraphQL; on v1 the
/// GraphQL `gpuTypes` (Phase 0.D) — backed by the preset estimates. Vast: the cheapest offer
/// fitting `base` (disk, CUDA floor) per GPU option, one marketplace search per card, each
/// bounded by [`PRICE_TIMEOUT`]. Shadeform: the cheapest in-stock catalog entry per option
/// (allowed clouds, CUDA floor, `SHADEFORM_MAX_PRICE`), from one catalog read. Hetzner quotes
/// nothing.
async fn fetch_price_book(
    cfg: &Config,
    provider_name: &str,
    req: &arena_core::placement::Request,
    base: &PodSpec,
) -> Result<(arena_core::placement::PriceBook, Vec<String>)> {
    use arena_core::placement::PriceBook;
    use arena_core::provider::{runpod, vast, RunpodApi};
    let clouds = &req.clouds;
    if provider_name == "vast" {
        if cfg.get("VAST_API_KEY").is_none_or(str::is_empty) {
            let note = "no VAST_API_KEY — vast options are unpriced (--max-price can't check them)";
            return Ok((PriceBook::unpriced(), vec![note.into()]));
        }
        // Built as create builds it (`provider::build`), so the plan quotes only hosts create
        // would rent: per tier (`--cloud datacenter,verified`, already Vast's tiers here —
        // `plan_placement`), the default verified-only unless VAST_ALLOW_UNVERIFIED.
        let vast = vast::VastProvider::from_config(cfg)?;
        let mut book = PriceBook::unpriced();
        let mut notes = Vec::new();
        for cloud in clouds {
            let spec = PodSpec { gpu_count: req.gpu_count, cloud_type: cloud.clone(), ..base.clone() };
            let tier = vast.tier_for(&spec)?;
            // Near the proxy: the same search in the region's countries too, at the same time,
            // for the near options' prices.
            let near_spec = req.near.map(|n| PodSpec { region: Some(n.region), ..spec.clone() });
            let near_quotes = async {
                match &near_spec {
                    Some(s) => Some(vast.quote(&req.gpus, s, PRICE_TIMEOUT).await),
                    None => None,
                }
            };
            let (quotes, near_quotes) = tokio::join!(vast.quote(&req.gpus, &spec, PRICE_TIMEOUT), near_quotes);
            let (mut tiered, more) = vast::price_book(&quotes, req.gpu_count, tier);
            notes.extend(more);
            if let (Some(near), Some(near_quotes)) = (req.near, near_quotes) {
                let (with_near, more) = vast::with_near_prices(tiered, &near_quotes, req.gpu_count, near.region, tier);
                tiered = with_near;
                notes.extend(more);
            }
            book = book.merge(tiered);
        }
        return Ok((book, notes));
    }
    if provider_name == "shadeform" {
        if cfg.get("SHADEFORM_API_KEY").is_none_or(str::is_empty) {
            let note = "no SHADEFORM_API_KEY — shadeform options are unpriced (--max-price can't check them)";
            return Ok((PriceBook::unpriced(), vec![note.into()]));
        }
        // One catalog read for every option; built as create builds it, so the plan only
        // quotes clouds, images and prices create would take.
        let spec = PodSpec { gpu_count: req.gpu_count, ..base.clone() };
        let sf = arena_core::provider::shadeform::ShadeformProvider::from_config(cfg)?;
        let quotes = sf.quote(&req.gpus, &spec, PRICE_TIMEOUT).await;
        return Ok(arena_core::provider::shadeform::price_book(&quotes, req.gpu_count, sf.opts()));
    }
    if provider_name != "runpod" {
        let note = format!(
            "{provider_name} quotes no price before create — options are unpriced, tried as listed \
             (--max-price can't check them)"
        );
        return Ok((PriceBook::unpriced(), vec![note]));
    }
    let Some(key) = cfg.get("RUNPOD_API_KEY").filter(|s| !s.is_empty()) else {
        return Ok((PriceBook::runpod(Vec::new()), vec!["no RUNPOD_API_KEY — prices are preset estimates (~)".into()]));
    };
    let mut catalogs = Vec::new();
    let mut errs = Vec::new();
    if RunpodApi::from_config(cfg)? == RunpodApi::V2 {
        for cloud in clouds {
            match v2_catalog(cfg, key, cloud).await {
                Ok(types) => catalogs.push((Some(cloud.clone()), types)),
                Err(e) => errs.push(format!("v2 catalog ({cloud}): {e}")),
            }
        }
    } else {
        match bounded_catalog(runpod::fetch_gpu_types(key)).await {
            Ok(types) if !types.is_empty() => catalogs.push((None, types)),
            Ok(_) => errs.push("GraphQL catalog: empty".into()),
            Err(e) => errs.push(format!("GraphQL catalog: {e}")),
        }
    }
    let mut notes = Vec::new();
    if catalogs.is_empty() {
        notes.push(format!("live prices unavailable ({}) — using preset estimates (~)", errs.join("; ")));
    } else if !errs.is_empty() {
        notes.push(format!("partly live ({})", errs.join("; ")));
    }
    Ok((PriceBook::runpod(catalogs), notes))
}

/// Price and order the options for `req` on `provider_name` — read-only. The one planner
/// behind `arena offers`, the create/up dry-runs and their confirm prompt. `base` is the spec
/// the options are layered on (config + overrides): a marketplace's price depends on more
/// than the GPU — Vast prices the disk asked for and only offers hosts above the CUDA floor.
pub(crate) async fn plan_placement(
    cfg: &Config,
    provider_name: &str,
    req: &arena_core::placement::Request,
    base: &PodSpec,
) -> Result<arena_core::placement::OptionPlan> {
    req.validate_for(provider_name)?;
    // Vast: `--cloud` (or config's CLOUD_TYPE, a RunPod tier) as Vast's own tiers, so each
    // option names the hosts its create rents on.
    let mut tier_note = None;
    let vast_req;
    let req = if provider_name == "vast" {
        let (clouds, note) = arena_core::provider::vast::plan_tiers(&req.clouds, !arena_core::provider::vast::unverified_allowed(cfg)?)?;
        tier_note = note;
        vast_req = arena_core::placement::Request { clouds, ..req.clone() };
        &vast_req
    } else {
        req
    };
    // Near the proxy (`NEAR`) here too, so every caller of the shared planner — `offers`
    // included — shows the order create/up would run. No caller plans a request that left
    // NEAR out on purpose: `--keep-trying` without a list takes the single path, which never
    // comes here (nor does `single_request`'s quote).
    let configured = arena_core::near::Near::for_provider(cfg, provider_name)?;
    let req = &req.clone().with_near(configured, provider_name, base);
    let (book, mut notes) = fetch_price_book(cfg, provider_name, req, base).await?;
    if let (Some(n), None, "runpod") = (configured, req.near, provider_name) {
        notes.push(if base.api_extra.as_ref().is_some_and(arena_core::near::runpod_pins_location) {
            format!("near ({}): off — CREATE_EXTRA_JSON / --api-json already says where (data centres, a network volume or countries)", n.region.label())
        } else {
            format!(
                "near ({}): RunPod's COMMUNITY cloud can't be placed by data centre (its hosts have none) — near \
                 applies to SECURE options (--cloud community,secure; NEAR=any turns this note off)",
                n.region.label()
            )
        });
    }
    let mut plan = arena_core::placement::plan_options(req, provider_name, &book);
    plan.notes.splice(0..0, notes.into_iter().chain(tier_note));
    Ok(plan)
}

/// The single-spec path's one option, as a request: the spec's GPU, count and tier.
fn single_request(spec: &PodSpec) -> arena_core::placement::Request {
    use arena_core::placement::{Order, Request};
    Request {
        gpus: vec![spec.gpu_type.clone()],
        clouds: vec![spec.cloud_type.trim().to_uppercase()],
        gpu_count: spec.gpu_count,
        max_price: None,
        order: Order::Listed,
        near: None,
        server_types: Vec::new(),
    }
}

/// The single-spec path's one option, as a plan — what an `up --check` replacement creates
/// from when no `--gpu`/`--cloud` list or `--max-price` was given: the same GPU, count and
/// tier the first create used. Unpriced (that path never quoted one).
pub(crate) fn single_option_plan(provider_name: &str, spec: &PodSpec) -> arena_core::placement::OptionPlan {
    arena_core::placement::plan_options(&single_request(spec), provider_name, &arena_core::placement::PriceBook::unpriced())
}

/// Print the `create`/`up` balance preflight ([`money::balance_preflight`]) — on the
/// single-spec path with the spec's live price read alongside ([`single_spec_quote`]).
pub(crate) async fn print_balance_preflight(
    provider: &dyn Provider,
    cfg: &Config,
    bal: &money::BalanceCtx,
    new_pods: usize,
    options: Option<&arena_core::placement::OptionPlan>,
    spec: &PodSpec,
) {
    let quote = single_spec_quote(cfg, provider.name(), spec);
    money::balance_preflight(provider, cfg, bal, new_pods, options, spec, Some(quote)).await.iter().for_each(|l| println!("{l}"));
}

/// The single spec's live price for the `create`/`up` preflight: its one option priced the
/// way placement prices it ([`fetch_price_book`] — RunPod's catalog, v2's per tier (cached for
/// the run) or v1's GraphQL; Vast's cheapest matching offer), within the preflight's bound
/// ([`arena_core::balance::INLINE_TIMEOUT`]); `None` when nothing answered in time. Live, the
/// preflight otherwise stood the A4000 preset ($0.32) in for a $0.17 community card and
/// warned. In this crate's unit tests nothing is fetched: a stray key in a test config must
/// never reach the network (tests hand the preflight a quote of their own).
fn single_spec_quote<'a>(cfg: &'a Config, provider_name: &'a str, spec: &'a PodSpec) -> money::SpecQuote<'a> {
    Box::pin(async move {
        if cfg!(test) {
            return None;
        }
        let req = single_request(spec);
        let fetch = fetch_price_book(cfg, provider_name, &req, spec);
        let (book, _) = arena_core::ratelimit::bounded(arena_core::balance::INLINE_TIMEOUT, fetch).await.ok()?.ok()?;
        Some(arena_core::placement::plan_options(&req, provider_name, &book))
    })
}

/// `provider.describe(spec)` — what the one-GPU path's dry run and confirm say a create is —
/// with a MIG slice named as `pods list`, READY and the summary name it (`1×B300 MIG 34GB`,
/// the option label) instead of by its catalog id (`NVIDIA B300 SXM6 AC MIG 1g.34gb x1`, live
/// G #17). Anything else is the provider's own description, unchanged.
pub(crate) fn describe_create(provider: &dyn Provider, spec: &PodSpec) -> String {
    let desc = provider.describe(spec);
    let mig = spec.gpu_type.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| w.eq_ignore_ascii_case("mig"));
    let id = format!("{} x{}", spec.gpu_type, spec.gpu_count);
    match single_option_plan(provider.name(), spec).options.first() {
        Some(o) if mig && desc.contains(&id) => desc.replacen(&id, &o.label, 1),
        _ => desc,
    }
}

/// The single-spec path's `[created]` line: the pod, and — as the placement path's says it —
/// what it was created on (`1×RTX 3070 COMMUNITY`, the option's label and tier) and its price
/// when the provider reported one. Pure.
pub(crate) fn created_line(pod: &arena_core::Pod, on: Option<&str>) -> String {
    let mut line = format!("[created] {} id={}", pod.name, pod.id);
    if let Some(on) = on {
        line.push_str(&format!(" on {on}"));
    }
    if let Some(c) = pod.cost_per_hr.filter(|c| c.is_finite() && *c > 0.0) {
        line.push_str(&format!(" ({}/h)", arena_core::fleet::fmt_money(arena_core::fleet::currency_symbol(&pod.provider), c)));
    }
    line
}

/// The spec the first create would use: the first name, on placement's first option (or the
/// single spec) — what [`print_create_body`] shows. As `create_pods`/`spec_for` build it.
pub(crate) fn first_create_spec(base: &PodSpec, options: Option<&arena_core::placement::OptionPlan>, name: &str) -> PodSpec {
    match options.and_then(|plan| plan.options.first()) {
        Some(first) => arena_core::placement::spec_for(base, name, first),
        None => {
            let mut spec = base.clone();
            spec.name = name.to_string();
            spec.env.push(("MACHINE_NAME".into(), name.to_string()));
            spec
        }
    }
}

/// The line above an option table: provider, GPUs per pod, order, cap.
pub(crate) fn plan_header(plan: &arena_core::placement::OptionPlan) -> String {
    use arena_core::placement::Order;
    let order = match plan.order {
        Order::Cheapest => "cheapest first",
        Order::Listed => "as listed",
    };
    let cap = plan
        .max_price
        .map(|c| format!(", max {}/h per pod", arena_core::fleet::fmt_money("$", c)))
        .unwrap_or_default();
    format!("Placement options on {} ({} GPU(s) per pod, {order}{cap}):", plan.provider, plan.gpu_count)
}

/// The create/up dry-run for several options: the option table and which names would be
/// attempted, in what way.
pub(crate) fn print_placement_preview(
    provider: &dyn Provider,
    plan: &arena_core::placement::OptionPlan,
    base: &PodSpec,
    names: &[String],
) {
    println!("{}", plan_header(plan));
    print!("{}", arena_core::placement::render_plan(plan));
    let Some(first) = plan.options.first() else {
        println!("[dry-run] nothing would be attempted for {} — no option left to try", names.join(", "));
        return;
    };
    println!(
        "[dry-run] would place {} pod(s) on {}: each name tries options 1→{} in order, one create \
         at a time, keeping the first that has capacity:\n  {}",
        names.len(),
        provider.name(),
        plan.options.len(),
        names.join(", ")
    );
    println!("[dry-run] option 1 is: {}", provider.describe(&arena_core::placement::spec_for(base, "", first)));
}

/// The `[y/N]` text for a multi-option create (the option table is printed just above it).
/// Refuses when the cap left nothing to try.
pub(crate) fn placement_prompt(
    provider: &dyn Provider,
    plan: &arena_core::placement::OptionPlan,
    base: &PodSpec,
    names: &[String],
    then: &str,
) -> Result<String> {
    println!("{}", plan_header(plan));
    print!("{}", arena_core::placement::render_plan(plan));
    let Some(first) = plan.options.first() else {
        anyhow::bail!("nothing to create: no option left to try (see the table above, or `arena offers`)");
    };
    Ok(format!(
        "Will create up to {} pod(s) on {}, each name trying the {} option(s) above in order, one \
         create at a time (option 1: {}){then}:\n  {}",
        names.len(),
        provider.name(),
        plan.options.len(),
        provider.describe(&arena_core::placement::spec_for(base, "", first)),
        names.join(", ")
    ))
}

/// Ordered placement for `names` (see `arena_core::placement::place`): progress lines as it
/// goes, then the per-name summary. Same contract as [`create_with_retry`] (Ctrl+C between
/// rounds via the command's `interrupt`, left pressed for what runs next): every pod made,
/// or — on an auth/other failure — those pods inside [`CreateFailed`], so the caller still
/// syncs the proxy for them. Each [`Made`] carries the option it landed on.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn place_names(
    provider: &dyn Provider,
    cfg: &Config,
    names: Vec<String>,
    topup_target: usize,
    base: &PodSpec,
    plan: &arena_core::placement::OptionPlan,
    retry_mins: u64,
    retry_secs: u64,
    interrupt: &Interrupt,
    on_made: OnMade<'_>,
) -> std::result::Result<Vec<Made>, CreateFailed> {
    use arena_core::placement::{self, End, Outcome, Progress, Rounds, TopUp};
    let rounds = Rounds {
        window: std::time::Duration::from_secs(retry_mins * 60),
        every: std::time::Duration::from_secs(retry_secs.max(1)),
    };
    // -n/-a: retry rounds never top up past the provider-scoped total that was confirmed.
    let topup = (topup_target > 0).then(|| TopUp {
        target: topup_target,
        provider: provider.name().to_string(),
        prefix: cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena").to_string(),
    });
    let mut show = |p: &Progress| {
        if p.is_created() {
            println!("{}", p.line());
        } else {
            eprintln!("{}", p.line());
        }
        // Hand the pod on the moment it exists (`up` starts its pipeline now).
        if let Progress::Attempt { pod: Some(pod), index, .. } = p {
            on_made(&Made { pod: (*pod).clone(), at: tokio::time::Instant::now(), option: Some(*index) });
        }
    };
    let start = tokio::time::Instant::now();
    let run =
        placement::place(provider, base, &names, plan, rounds, topup.as_ref(), || interrupt.wait(), &mut show).await;
    print!("\n{}", placement::render_summary(plan, &run));
    let (placed, total) = (run.created.len(), names.len());
    // Each pod with its option and create time, from the attempt that made it.
    let made: Vec<Made> = run
        .created
        .iter()
        .map(|pod| {
            let created = run.log.iter().flat_map(|l| &l.attempts).find_map(|a| match &a.outcome {
                Outcome::Created { pod_id } if *pod_id == pod.id => Some((a.option, a.at)),
                _ => None,
            });
            Made {
                pod: pod.clone(),
                at: start + created.map_or(Duration::ZERO, |(_, at)| at),
                option: created.map(|(k, _)| k),
            }
        })
        .collect();
    match run.end {
        End::Filled => println!("Placed {placed} of {total}."),
        End::Exhausted => eprintln!(
            "[stop] no capacity on any option right now (placed {placed}/{total}). \
             Re-run with --retry-mins N to wait for it to free up."
        ),
        End::WindowElapsed => eprintln!("retry window ({retry_mins}m) elapsed — placed {placed} of {total}"),
        End::Interrupted => eprintln!("interrupted — stopping retries with {placed} of {total}"),
        End::Aborted { name, error } => {
            // Every option refused (RunPod v2 403s) is about the key's access, not a bad key.
            let what = if error.kind() == Some(arena_core::ProviderErrorKind::Denied) {
                "access refused"
            } else {
                "authentication failed"
            };
            let error = anyhow::anyhow!("{what} creating {name}: {error}");
            return Err(CreateFailed::new(made, error));
        }
        End::Failed { name, error } => {
            eprintln!("placed {placed}/{total} before failure");
            let error = match name {
                Some(name) => anyhow::anyhow!("creating {name}: {error}"),
                None => anyhow::Error::new(error),
            };
            return Err(CreateFailed::new(made, error));
        }
    }
    Ok(made)
}

#[cfg(test)]
mod placement_cli_tests;
