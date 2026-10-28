//! `arena offers` and its old name `arena gpus`: the I/O around [`arena_core::offers`].
//!
//! Each provider's rows come from the very functions `create`/`up` run — the `--gpu` check
//! ([`checked_gpu`]), the request from the flags and [`plan_placement`] — so the table is what
//! they would try there, in their order: when placement changes how it orders or prices
//! options, `offers` shows it without a change here. Read-only throughout: catalog and price
//! reads, and on Hetzner one `GET /server_types` plus the location timing a create does.

use anyhow::Result;
use arena_core::offers::{self, Catalogs, Offers, ProviderOffers, RunpodCatalog, Skipped, Stock};
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

/// `arena offers`: what `create`/`up` would try on each provider ([`offers::providers`]: the
/// one `--provider` names, else every configured one), or with `all` every type each sells.
/// `named`: `--provider` when given on the command line (its default can't tell). `--json`
/// prints [`Offers`] and nothing else on stdout. A provider that failed ([`collect`]) exits
/// non-zero, after the rest is shown.
pub(crate) async fn handle_offers(cfg: &Config, named: Option<&str>, flags: Flags, all: bool, json: bool) -> Result<()> {
    let has_key = |k: &str| cfg.get(k).is_some_and(|v| !v.trim().is_empty());
    let scope = offers::providers(named, has_key).map_err(|e| anyhow::anyhow!(e))?;
    let left_out = offers::opt_in_left_out(named, has_key);
    if all {
        return handle_catalogs(cfg, &scope, &left_out, json).await;
    }
    let mut out = collect(&scope, named, &flags, |p, server_type| provider_offers(cfg, p, &flags, server_type)).await?;
    out.skipped.extend(left_out);
    if json {
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        print!("{}", offers::render(&out));
    }
    if !out.failed.is_empty() {
        let names: Vec<&str> = out.failed.iter().map(|f| f.provider.as_str()).collect();
        anyhow::bail!("couldn't plan {} as create would (see `failed:`)", names.join(", "));
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

/// One provider's plan, made as `create` makes it, with what the table adds: VRAM, and on
/// Hetzner the server type a create would ask for.
async fn provider_offers(cfg: &Config, provider: &str, flags: &Flags, server_type: Option<String>) -> Result<ProviderOffers> {
    let gpu = checked_gpu(cfg, provider, flags.gpu.clone()).await?;
    let base = spec_with_overrides(cfg, &SpecOverrides { gpus: flags.gpus, server_type, ..Default::default() });
    let req = arena_core::placement::Request::from_flags(gpu.as_deref(), flags.cloud.as_deref(), &base, flags.max_price, flags.order)?;
    let plan = plan_placement(cfg, provider, &req, &base).await?;
    // Live catalog/offer prices are remembered for the balance's known-GPU stand-ins.
    money::BalanceCtx::from_config(cfg).remember_gpu_prices(&plan);
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
    match tokio::time::timeout(PRICE_TIMEOUT, HetznerProvider::new(key, opts).offer(base)).await {
        Ok(Ok(t)) => Ok((t, None)),
        Ok(Err(arena_core::Error::Config(why))) => anyhow::bail!("create would refuse {name}: {why}"),
        Ok(Err(e)) => Ok((bare, Some(format!("couldn't read {name} from Hetzner's server types ({e}) — size and price unknown")))),
        Err(_) => Ok((bare, Some(format!("Hetzner didn't answer within {}s — {name}'s size and price are unknown", PRICE_TIMEOUT.as_secs())))),
    }
}

/// `offers --all`: each provider's whole list — RunPod's GPU catalog (what `gpus` showed),
/// Hetzner's server types — and for a marketplace, how to price a GPU on it.
async fn handle_catalogs(cfg: &Config, scope: &[&str], left_out: &[Skipped], json: bool) -> Result<()> {
    let mut cats = Catalogs::default();
    let mut text: Vec<String> = Vec::new();
    for &provider in scope {
        match provider {
            "runpod" => {
                let (cat, warning) = runpod_catalog(cfg).await?;
                if let Some(w) = warning {
                    eprintln!("{w}");
                }
                text.push(format!("runpod — GPU types (--gpu):\n{}", offers::render_runpod_catalog(&cat)));
                cats.runpod = Some(cat.rows);
            }
            "hetzner" => {
                let opts = HetznerOpts::from_config(cfg);
                let configured = opts.server_type.clone();
                let read = match cfg.get("HETZNER_API_KEY").filter(|k| !k.trim().is_empty()) {
                    None => Err("no HETZNER_API_KEY".to_string()),
                    Some(key) => match tokio::time::timeout(PRICE_TIMEOUT, HetznerProvider::new(key, opts).catalog()).await {
                        Ok(read) => read.map_err(|e| e.to_string()),
                        Err(_) => Err(format!("no answer within {}s", PRICE_TIMEOUT.as_secs())),
                    },
                };
                match read {
                    Ok(types) => {
                        text.push(format!("hetzner — server types (--server-type):\n{}", offers::render_server_types(&types, &configured)));
                        cats.hetzner = Some(types);
                    }
                    Err(e) => {
                        let note = format!("hetzner: couldn't read the server types ({e})");
                        text.push(format!("{note}\n"));
                        cats.notes.push(note);
                    }
                }
            }
            other => {
                let note = offers::no_catalog(other);
                text.push(format!("{note}\n"));
                cats.notes.push(note);
            }
        }
    }
    for s in left_out {
        let note = format!("{}: {}", s.provider, s.why);
        text.push(format!("{note}\n"));
        cats.notes.push(note);
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&cats)?);
    } else {
        print!("{}", text.join("\n"));
    }
    Ok(())
}

/// RunPod's full catalog with live community/secure prices and stock when there's a key — REST
/// `GET /v2/catalog/gpus` for both tiers on `v2` (no GraphQL there, see `runpod_v2`'s inventory),
/// GraphQL on `RUNPOD_API=v1` — else, or when that fails, the local presets; with the warning
/// to print when it failed. v1's rows are flagged against its create enum, which can be
/// narrower than the catalog (the table hides what `--gpu` would get a 400 for; the JSON keeps
/// it); v2's create validates against this very catalog, so every row is creatable.
async fn runpod_catalog(cfg: &Config) -> Result<(RunpodCatalog, Option<String>)> {
    use arena_core::gpu;
    use arena_core::provider::{runpod, RunpodApi};
    let presets = RunpodCatalog { rows: gpu::rows_from_presets(), live: false, creatable_known: false, stock: Stock::OneGpu };
    let Some(key) = cfg.get("RUNPOD_API_KEY").filter(|s| !s.is_empty()) else {
        return Ok((presets, None));
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
        Ok(_) => return Ok((presets, None)),
        Err(e) => return Ok((presets, Some(format!("(couldn't fetch the live GPU list: {e} — showing local presets)\n")))),
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
    Ok((RunpodCatalog { rows, live: true, creatable_known: creatable.is_some(), stock }, None))
}

/// `arena gpus`, kept for old habits and scripts, exactly as it was: RunPod's section of `offers
/// --all` — with another `--provider`, the presets (RunPod's prices) as it always showed them —
/// and `--json` the same array of rows ([`arena_core::gpu::GpuRow`]). The pointer to `offers
/// --all` goes to stderr, keeping stdout as it was.
pub(crate) async fn handle_gpus(cfg: &Config, provider: &str, json: bool) -> Result<()> {
    let (cat, warning) = if provider == "runpod" {
        runpod_catalog(cfg).await?
    } else {
        (RunpodCatalog { rows: arena_core::gpu::rows_from_presets(), live: false, creatable_known: false, stock: Stock::OneGpu }, None)
    };
    if let Some(w) = warning {
        eprintln!("{w}");
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&cat.rows)?);
        return Ok(());
    }
    print!("{}", offers::render_runpod_catalog(&cat));
    eprintln!("(`arena gpus` is RunPod's part of `arena offers --all`, which lists Hetzner's server types too.)");
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
        handle_offers(&cfg(), Some("vast"), flags.clone(), false, true).await.unwrap();
        handle_offers(&cfg(), None, Flags { gpus: Some(2), ..flags.clone() }, false, false).await.unwrap();
        let e = handle_offers(&cfg(), Some("hetzner"), flags.clone(), false, false).await.unwrap_err().to_string();
        assert!(e.starts_with("--gpu doesn't apply to hetzner"), "{e}");
        let e = handle_offers(&cfg(), Some("lambda"), Flags::default(), false, false).await.unwrap_err().to_string();
        assert_eq!(e, "unknown provider `lambda` (known: runpod, vast, hetzner, shadeform)");
        // Hetzner without a key: the type create would ask for, size and price unknown.
        let hz = provider_offers(&cfg(), "hetzner", &Flags { order: Order::Cheapest, ..Default::default() }, Some("cx33".into())).await.unwrap();
        assert_eq!((hz.options.len(), hz.server_type.as_ref().map(|t| t.name.as_str())), (1, Some("cx33")));
        assert_eq!(hz.notes.last().map(String::as_str), Some("no HETZNER_API_KEY — cx33's size and price are unknown"));
        // `--all` and `gpus` without a key: the presets.
        handle_offers(&cfg(), None, Flags::default(), true, true).await.unwrap();
        let (cat, warning) = runpod_catalog(&cfg()).await.unwrap();
        assert!(!cat.live && warning.is_none() && cat.rows.len() == arena_core::gpu::PRESETS.len());
        handle_gpus(&cfg(), "runpod", true).await.unwrap();
        handle_gpus(&cfg(), "vast", false).await.unwrap();
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
