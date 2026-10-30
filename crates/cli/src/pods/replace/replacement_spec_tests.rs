use super::{build_replacement_spec, gpu_id_from_listing, plan_replacement};
use crate::cli::PlaceFlags;
use crate::pods::create::SpecOverrides;
use crate::pods::placement::gpu_flag_or_warning;
use arena_core::provider::runpod::GpuType;
use arena_core::{Config, Pod, PodSpec, Provider, Result};
use async_trait::async_trait;

/// A provider whose `pod_spec` returns a fixed snapshot (as RunPod's would: no CUDA
/// constraint, GPU/cloud blank on v1).
struct Snapshot {
    name: &'static str,
    spec: PodSpec,
}

#[async_trait]
impl Provider for Snapshot {
    fn name(&self) -> &'static str {
        self.name
    }
    fn describe(&self, _spec: &PodSpec) -> String {
        String::new()
    }
    async fn list_pods(&self) -> Result<Vec<Pod>> {
        Ok(Vec::new())
    }
    async fn create_pod(&self, _spec: &PodSpec) -> Result<Pod> {
        unimplemented!("not exercised")
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
    async fn pod_spec(&self, _id: &str) -> Result<PodSpec> {
        Ok(self.spec.clone())
    }
}

fn snapshot(allowed_cuda: &[&str]) -> PodSpec {
    PodSpec {
        name: String::new(),
        image: "img:cu130".into(),
        gpu_type: String::new(), // v1 can't report it: config's GPU_TYPE fills in
        gpu_count: 1,
        cloud_type: String::new(),
        disk_gb: 40,
        volume_gb: 0,
        ports: "22/tcp".into(),
        env: Vec::new(),
        docker_args: None,
        allowed_cuda: allowed_cuda.iter().map(|s| s.to_string()).collect(),
        min_cuda: None,
        max_price: None,
        api_extra: None,
        avoid_machines: Vec::new(),
        server_type: None,
        vm_image: None,
        region: None,
    }
}

fn cfg(extra: &str) -> Config {
    Config::parse(&format!(
        "MACHINE_NAME_PREFIX=devtest\nGPU_TYPE=\"NVIDIA RTX A4000\"\nSHARED_SSH_KEY_PATH=/nonexistent/k\n{extra}"
    ))
}

/// The source pod as the owner lists it.
fn src() -> Pod {
    Pod { id: "id-1".into(), name: "devtest-apple".into(), provider: "runpod".into(), status: "RUNNING".into(), ..Default::default() }
}

/// Review finding: the replacement never got `ALLOWED_CUDA_VERSIONS` back (the API
/// doesn't report it), so a cu130 image could land on a CUDA 12 host.
#[tokio::test]
async fn replacement_reapplies_the_configured_cuda_constraint() {
    let owner = Snapshot { name: "runpod", spec: snapshot(&[]) };
    let ov = SpecOverrides::default();
    // (config, what the replacement is constrained to)
    // (config, exact set, floor) — ALLOWED_CUDA_VERSIONS is a floor now (finding #10).
    let none: Vec<&str> = vec![];
    for (extra, exact, floor) in [
        ("ALLOWED_CUDA_VERSIONS=\"13.0\"\n", none.clone(), Some("13.0")),
        ("ALLOWED_CUDA_VERSIONS=\"13.0, 12.8\"\n", none.clone(), Some("12.8")),
        ("RUNPOD_ALLOWED_CUDA_VERSIONS=13.0\n", none.clone(), Some("13.0")), // the legacy key, as create reads it
        ("MIN_CUDA_VERSION=13.0\n", none.clone(), Some("13.0")),
        ("EXACT_CUDA_VERSIONS=13.0,13.1\n", vec!["13.0", "13.1"], None),
        ("", none.clone(), None), // unconfigured: any host, as before
    ] {
        let (spec, notes) = build_replacement_spec(&cfg(extra), &owner, "id-1", "devtest-apple", None, &ov).await.unwrap();
        assert_eq!((spec.allowed_cuda.clone(), spec.min_cuda.as_deref()), (exact.iter().map(|s| s.to_string()).collect(), floor), "{extra:?}");
        assert!(notes.iter().any(|n| n.starts_with("GPU NVIDIA RTX A4000: config GPU_TYPE")), "{notes:?}");
        assert_eq!((spec.gpu_type.as_str(), spec.disk_gb), ("NVIDIA RTX A4000", 40), "snapshot + config fallback");
    }
    // A snapshot that does carry a constraint keeps it (and gets no floor beside it).
    let pinned = Snapshot { name: "runpod", spec: snapshot(&["12.8"]) };
    let (spec, _) = build_replacement_spec(&cfg("ALLOWED_CUDA_VERSIONS=13.0\n"), &pinned, "id-1", "x", None, &ov).await.unwrap();
    assert_eq!((spec.allowed_cuda.clone(), spec.min_cuda), (vec!["12.8".to_string()], None));
    // --gpu still applies on top (unchecked here: no key → no catalog) — through the plan.
    let ov = SpecOverrides { gpu: Some("3070".into()), ..Default::default() };
    let rep = plan_replacement(&cfg("ALLOWED_CUDA_VERSIONS=13.0\n"), &owner, &src(), &ov, &PlaceFlags::default()).await.unwrap();
    let spec = rep.first_spec("devtest-apple-new");
    assert_eq!((spec.gpu_type.as_str(), spec.min_cuda.as_deref()), ("NVIDIA GeForce RTX 3070", Some("13.0")));
    assert!(rep.notes.iter().all(|n| !n.starts_with("GPU ")), "--gpu given: no note on where the GPU came from: {:?}", rep.notes);
}

/// Config CREATE_EXTRA_JSON reaches a replacement's (and `migrate copy`'s) create like
/// any create's, checked against the owner's own fields — refused before anything is
/// created; a backend that can't merge extras refuses it outright.
#[tokio::test]
async fn replacement_carries_config_create_extra_json_checked_against_the_owner() {
    /// The snapshot backend with RunPod v2's create rules.
    struct V2Owner(Snapshot);
    #[async_trait]
    impl Provider for V2Owner {
        fn name(&self) -> &'static str {
            "runpod"
        }
        fn describe(&self, _spec: &PodSpec) -> String {
            String::new()
        }
        async fn list_pods(&self) -> Result<Vec<Pod>> {
            Ok(Vec::new())
        }
        async fn create_pod(&self, _spec: &PodSpec) -> Result<Pod> {
            unimplemented!("not exercised")
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
        async fn pod_spec(&self, id: &str) -> Result<PodSpec> {
            self.0.pod_spec(id).await
        }
        fn check_create_extra(&self, extra: &arena_core::apiextra::Extra) -> Result<()> {
            arena_core::provider::runpod_v2::RunpodV2Provider::new("k").check_create_extra(extra)
        }
    }
    let owner = V2Owner(Snapshot { name: "runpod", spec: snapshot(&[]) });
    let ov = SpecOverrides::default();
    let (spec, _) = build_replacement_spec(&cfg(""), &owner, "id-1", "x", None, &ov).await.unwrap();
    assert_eq!((spec.api_extra.as_ref(), super::extra_keys_line(&spec)), (None, None));
    let (spec, _) = build_replacement_spec(&cfg(r#"CREATE_EXTRA_JSON='{"dataCenterIds":["EU-RO-1"],"env":{"A":"hf_x"}}'"#), &owner, "id-1", "x", None, &ov)
        .await
        .unwrap();
    assert_eq!(spec.api_extra.as_ref().unwrap()["dataCenterIds"], serde_json::json!(["EU-RO-1"]));
    assert_eq!(
        super::extra_keys_line(&spec).unwrap(),
        "+ --api-json / CREATE_EXTRA_JSON [dataCenterIds, env] merged into the create request"
    );
    for (line, want) in [
        (r#"CREATE_EXTRA_JSON='{"gpu":{"id":"x"}}'"#, "`gpu.id` is set by arena"),
        ("CREATE_EXTRA_JSON='nope'", "CREATE_EXTRA_JSON: not valid JSON"),
    ] {
        let e = build_replacement_spec(&cfg(line), &owner, "id-1", "x", None, &ov).await.unwrap_err().to_string();
        assert!(e.contains(want), "{line}: {e}");
    }
    // The plain snapshot backend merges no extras: refused, not silently dropped.
    let plain = Snapshot { name: "vast", spec: snapshot(&[]) };
    let e = build_replacement_spec(&cfg(r#"CREATE_EXTRA_JSON='{"price":0.1}'"#), &plain, "id-1", "x", None, &ov).await.unwrap_err();
    assert!(e.to_string().contains("isn't supported on provider `vast`"), "{e}");
}

/// Live finding #17: on RunPod v1 the source's spec has no GPU type, so a replacement of a
/// 1×RTX 3070 pod was created on config's A4000. The source's own GPU comes from its
/// (enriched) listing first — mapped to a create id through the catalog, or through the
/// known aliases without one — and config only when that can't be read (said in a note).
#[tokio::test]
async fn a_replacement_keeps_the_sources_gpu_from_its_listing() {
    let catalog = || {
        Ok(vec![
            GpuType { id: "NVIDIA GeForce RTX 3070".into(), display_name: "RTX 3070".into(), memory_gb: 8, ..Default::default() },
            GpuType { id: "NVIDIA B300 MIG 34GB".into(), display_name: "B300 MIG 34GB".into(), memory_gb: 34, ..Default::default() },
        ])
    };
    // (as the listing shows it, the catalog?, the create id)
    let cases: [(&str, bool, Option<&str>); 7] = [
        ("RTX 3070", true, Some("NVIDIA GeForce RTX 3070")),
        ("B300 MIG 34GB", true, Some("NVIDIA B300 MIG 34GB")),
        ("NVIDIA GeForce RTX 3070", true, Some("NVIDIA GeForce RTX 3070")),
        ("RTX 3070", false, Some("NVIDIA GeForce RTX 3070")), // a known alias, no catalog needed
        ("B300 MIG 34GB", false, Some("NVIDIA B300 SXM6 AC MIG 1g.34gb")), // the unlisted slice's alias
        ("PRO 6000 MIG 24GB", false, None),                   // no catalog, no alias: no guess
        ("Mystery GPU", true, None),
    ];
    for (shown, with_catalog, want) in cases {
        let cat = if with_catalog { catalog() } else { Err("offline".to_string()) };
        assert_eq!(gpu_id_from_listing(shown, cat).as_deref(), want, "{shown} (catalog: {with_catalog})");
    }

    /// v1's shape: no GPU in the spec snapshot, the listing's details name it.
    struct V1Owner;
    #[async_trait]
    impl Provider for V1Owner {
        fn name(&self) -> &'static str {
            "runpod"
        }
        fn describe(&self, _spec: &PodSpec) -> String {
            String::new()
        }
        async fn list_pods(&self) -> Result<Vec<Pod>> {
            Ok(Vec::new())
        }
        async fn enrich(&self, pods: &mut [Pod]) -> Result<()> {
            for p in pods.iter_mut() {
                p.gpu_type = Some("RTX 3070".into());
            }
            Ok(())
        }
        async fn create_pod(&self, _spec: &PodSpec) -> Result<Pod> {
            unimplemented!("not exercised")
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
        async fn pod_spec(&self, _id: &str) -> Result<PodSpec> {
            Ok(snapshot(&[]))
        }
    }
    let rep = plan_replacement(&cfg(""), &V1Owner, &src(), &SpecOverrides::default(), &PlaceFlags::default()).await.unwrap();
    assert_eq!(rep.first_spec("devtest-apple-new").gpu_type, "NVIDIA GeForce RTX 3070", "not config's A4000");
    assert!(rep.notes.iter().any(|n| n.starts_with("GPU NVIDIA GeForce RTX 3070: the source's, from its listing")), "{:?}", rep.notes);
    assert!(rep.notes.iter().any(|n| n.starts_with("tier COMMUNITY: config CLOUD_TYPE")), "{:?}", rep.notes);
    // A listing that can't say: config, with a note telling the operator to choose.
    let blank = Snapshot { name: "runpod", spec: snapshot(&[]) };
    let rep = plan_replacement(&cfg(""), &blank, &src(), &SpecOverrides::default(), &PlaceFlags::default()).await.unwrap();
    assert_eq!(rep.first_spec("x").gpu_type, "NVIDIA RTX A4000");
    assert!(rep.notes.iter().any(|n| n.contains("config GPU_TYPE") && n.contains("pass --gpu")), "{:?}", rep.notes);
}

/// Live finding #19: `--gpu 3070,A4000` reached the API as ONE id
/// (`NVIDIA GeForce RTX 3070,NVIDIA RTX A4000`, a 400). A list is placement options now —
/// one create at a time, in order — each a real id; a single `--gpu` stays one option.
#[tokio::test]
async fn a_replacement_gpu_list_is_placement_options_not_one_id() {
    let owner = Snapshot { name: "runpod", spec: snapshot(&[]) };
    let ov = SpecOverrides { gpu: Some("3070,A4000".into()), ..Default::default() };
    let place = PlaceFlags { order: arena_core::placement::Order::Listed, ..Default::default() };
    let rep = plan_replacement(&cfg(""), &owner, &src(), &ov, &place).await.unwrap();
    let gpus: Vec<&str> = rep.plan.options.iter().map(|o| o.gpu.as_str()).collect();
    assert_eq!(gpus, ["NVIDIA GeForce RTX 3070", "NVIDIA RTX A4000"]);
    assert!(rep.is_placement());
    assert!(!rep.first_spec("x").gpu_type.contains(','), "never a comma-joined id");
    let one = plan_replacement(&cfg(""), &owner, &src(), &SpecOverrides { gpu: Some("3070".into()), ..Default::default() }, &PlaceFlags::default())
        .await
        .unwrap();
    assert_eq!((one.plan.options.len(), one.is_placement()), (1, false));
    // A list on the cloud too: gpu × cloud, as `up` expands it — with NEAR on (the
    // default), SECURE tried in Europe just before anywhere (as listed); off, as before.
    let ov = SpecOverrides { gpu: Some("3070".into()), cloud: Some("community,secure".into()), ..Default::default() };
    let rep = plan_replacement(&cfg(""), &owner, &src(), &ov, &place).await.unwrap();
    let tiers: Vec<(Option<&str>, bool)> = rep.plan.options.iter().map(|o| (o.cloud.as_deref(), o.near.is_some())).collect();
    assert_eq!(tiers, [(Some("COMMUNITY"), false), (Some("SECURE"), true), (Some("SECURE"), false)]);
    let rep = plan_replacement(&cfg("NEAR=any\n"), &owner, &src(), &ov, &place).await.unwrap();
    let tiers: Vec<Option<&str>> = rep.plan.options.iter().map(|o| o.cloud.as_deref()).collect();
    assert_eq!(tiers, [Some("COMMUNITY"), Some("SECURE")]);
}

/// Finding #22: on REST v1 a catalog GPU its create enum lacks (the PRO 6000 MIG slices)
/// is refused before any create, by its display name, saying how to get back to v2 (the
/// default); an unreadable enum refuses nothing.
#[test]
fn v1_refuses_a_gpu_its_create_api_rejects() {
    use crate::pods::placement::v1_creatable_check;
    let mig = "NVIDIA RTX PRO 6000 Blackwell Server Edition MIG 1g.24gb";
    let catalog = vec![
        GpuType { id: mig.into(), display_name: "PRO 6000 MIG 24GB".into(), memory_gb: 24, ..Default::default() },
        GpuType { id: "NVIDIA RTX A4000".into(), display_name: "RTX A4000".into(), memory_gb: 16, ..Default::default() },
    ];
    let creatable = vec!["NVIDIA RTX A4000".to_string(), "NVIDIA B300 SXM6 AC MIG 1g.34gb".to_string()];
    assert!(v1_creatable_check("NVIDIA RTX A4000", &catalog, &creatable).is_ok());
    let e = v1_creatable_check(&format!("NVIDIA RTX A4000,{mig}"), &catalog, &creatable).unwrap_err().to_string();
    assert!(
        e.contains("`PRO 6000 MIG 24GB`") && e.contains("REST v1's create API rejects it")
            && e.contains("v2's create takes it: remove RUNPOD_API=v1 (v2 is the default), or pick another --gpu"),
        "{e}"
    );
    assert!(!e.contains("`RTX A4000`"), "only the bad token: {e}");
    assert!(v1_creatable_check(mig, &catalog, &[]).is_ok(), "no enum: nothing refused");
}

#[test]
fn gpu_flag_policy_checks_with_a_catalog_and_warns_without_one() {
    let catalog = || {
        Ok(vec![
            GpuType { id: "NVIDIA GeForce RTX 3070".into(), display_name: "RTX 3070".into(), memory_gb: 8, ..Default::default() },
            GpuType { id: "NVIDIA RTX A4000".into(), display_name: "RTX A4000".into(), memory_gb: 16, ..Default::default() },
        ])
    };
    // Catalog reachable: canonical ids, no warning.
    let (gpu, warn) = gpu_flag_or_warning("3070,a4000".into(), catalog(), &[]).unwrap();
    assert_eq!((gpu.as_str(), warn), ("NVIDIA GeForce RTX 3070,NVIDIA RTX A4000", Vec::<String>::new()));
    // …and a typo fails loudly, with a suggestion.
    let e = gpu_flag_or_warning("3070x".into(), catalog(), &[]).unwrap_err().to_string();
    assert!(e.contains("`3070x` isn't a RunPod GPU type") && e.contains("did you mean `NVIDIA GeForce RTX 3070`"), "{e}");
    // A type the catalog lacks that arena has seen (its own list, a pod, the config): taken,
    // with a warning each.
    let (gpu, warn) = gpu_flag_or_warning("B300 MIG,NVIDIA X9".into(), catalog(), &["NVIDIA X9".into()]).unwrap();
    assert_eq!(gpu, "NVIDIA B300 SXM6 AC MIG 1g.34gb,NVIDIA X9");
    assert_eq!(warn, [
        "warning: --gpu `NVIDIA B300 SXM6 AC MIG 1g.34gb` isn't in RunPod's public catalog — trying it; RunPod's create decides",
        "warning: --gpu `NVIDIA X9` isn't in RunPod's public catalog — trying it; RunPod's create decides",
    ]);
    // Unreachable: passed through as typed, with one warning saying why.
    let (gpu, warn) = gpu_flag_or_warning("3070x".into(), Err("GraphQL catalog: timed out after 30s".into()), &[]).unwrap();
    assert_eq!(gpu, "3070x");
    assert!(warn.len() == 1 && warn[0].starts_with("warning: couldn't check --gpu `3070x`") && warn[0].contains("timed out"), "{warn:?}");
}

/// Non-RunPod providers keep pass-through (no catalog fetched at all), and so does a
/// RunPod config without a key (the warning path; no network in either case).
#[tokio::test]
async fn checked_gpu_passes_through_off_runpod_and_without_a_key() {
    let c = cfg("");
    assert_eq!(crate::pods::placement::checked_gpu(&c, "vast", Some("RTX 3070 typo".into())).await.unwrap().as_deref(), Some("RTX 3070 typo"));
    assert_eq!(crate::pods::placement::checked_gpu(&c, "hetzner", None).await.unwrap(), None);
    // Shadeform: every token must name one of its GPU types (a typo there is an empty
    // catalog, i.e. "no stock" forever) — checked offline, no key needed.
    assert_eq!(crate::pods::placement::checked_gpu(&c, "shadeform", Some("A6000,l40s".into())).await.unwrap().as_deref(), Some("A6000,l40s"));
    let e = crate::pods::placement::checked_gpu(&c, "shadeform", Some("A6000,3090".into())).await.unwrap_err().to_string();
    assert!(e.contains("no Shadeform GPU type"), "{e}");
    assert_eq!(crate::pods::placement::checked_gpu(&c, "runpod", Some("3070".into())).await.unwrap().as_deref(), Some("3070"));
    let v2 = cfg("RUNPOD_API=v2\n");
    assert_eq!(crate::pods::placement::live_gpu_catalog(&v2).await.unwrap_err(), "no RUNPOD_API_KEY");
    assert_eq!(crate::pods::placement::checked_gpu(&v2, "runpod", Some("3070".into())).await.unwrap().as_deref(), Some("3070"));
}

/// The tier whose v2 catalog serves the `--gpu` check and `arena offers runpod`: the configured
/// one (any spelling) — so a plain create's price book reuses that fetch — else COMMUNITY.
#[test]
fn v2_catalog_tier_follows_the_configured_cloud() {
    assert_eq!(crate::pods::placement::v2_default_cloud(&cfg("")), "COMMUNITY");
    assert_eq!(crate::pods::placement::v2_default_cloud(&cfg("CLOUD_TYPE=secure\n")), "SECURE");
    assert_eq!(crate::pods::placement::v2_default_cloud(&cfg("RUNPOD_CLOUD_TYPE=COMMUNITY\n")), "COMMUNITY");
    assert_eq!(crate::pods::placement::v2_default_cloud(&cfg("CLOUD_TYPE=ALL\n")), "COMMUNITY");
}

/// Review G4 (item 17): `up` with ONE GPU (config `GPU_TYPE`, or a single `--gpu` — the
/// common path, `placement_request` → `None`) labels its option through
/// `single_option_plan`, with an unpriced book: no catalog, so a MIG slice keeps its id in
/// the READY line and placement text (`READY … — 1×B300 SXM6 AC MIG 1g.34gb SECURE`, live
/// BPF S6) while `pods list` says `1×B300 MIG 34GB` — docs/REFERENCE.md claims they now match.
#[test]
fn review_g4_single_spec_up_labels_a_mig_slice_as_pods_list_does() {
    let spec = PodSpec {
        gpu_type: "NVIDIA B300 SXM6 AC MIG 1g.34gb".into(),
        gpu_count: 1,
        cloud_type: "SECURE".into(),
        ..PodSpec::from_config(&cfg(""))
    };
    let plan = crate::pods::placement::single_option_plan("runpod", &spec);
    assert_eq!(plan.options[0].label, "1×B300 MIG 34GB");
}

/// Live G #17: on the one-GPU path the dry run and the confirm describe the create with
/// `provider.describe` — the MIG slice by its catalog id. They now name it as `pods list`,
/// READY and the summary do; every other GPU's description is the provider's, unchanged.
#[test]
fn the_one_gpu_dry_run_and_confirm_name_a_mig_slice_as_pods_list_does() {
    let provider = arena_core::provider::runpod::RunpodProvider::new("k");
    let spec = |gpu: &str| PodSpec { gpu_type: gpu.into(), gpu_count: 1, cloud_type: "SECURE".into(), ..PodSpec::from_config(&cfg("")) };
    let mig = crate::pods::placement::describe_create(&provider, &spec("NVIDIA B300 SXM6 AC MIG 1g.34gb"));
    assert!(mig.starts_with("1×B300 MIG 34GB, SECURE, disk "), "{mig}");
    assert!(!mig.contains("SXM6"), "{mig}");
    let a4000 = spec("NVIDIA RTX A4000");
    assert_eq!(crate::pods::placement::describe_create(&provider, &a4000), arena_core::Provider::describe(&provider, &a4000));
}

/// Live G: the one-GPU path's `[created]` line named no GPU; it now says what the pod was
/// created on, and its price when the provider reported one — as the placement path's does.
#[test]
fn the_one_gpu_created_line_names_the_gpu_and_price() {
    let pod = |cost: Option<f64>| arena_core::Pod {
        id: "qd3hgsswhhmk1b".into(),
        name: "devtest-delta".into(),
        provider: "runpod".into(),
        cost_per_hr: cost,
        ..Default::default()
    };
    let spec = PodSpec { gpu_type: "NVIDIA GeForce RTX 3070".into(), gpu_count: 1, cloud_type: "COMMUNITY".into(), ..PodSpec::from_config(&cfg("")) };
    let on = crate::pods::placement::single_option_plan("runpod", &spec).options[0].describe();
    assert_eq!(
        crate::pods::placement::created_line(&pod(Some(0.13)), Some(&on)),
        "[created] devtest-delta id=qd3hgsswhhmk1b on 1×RTX 3070 COMMUNITY ($0.13/h)"
    );
    assert_eq!(crate::pods::placement::created_line(&pod(None), Some(&on)), "[created] devtest-delta id=qd3hgsswhhmk1b on 1×RTX 3070 COMMUNITY");
    assert_eq!(crate::pods::placement::created_line(&pod(None), None), "[created] devtest-delta id=qd3hgsswhhmk1b");
}
