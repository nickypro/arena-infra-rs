//! `pods create`/`up` sizing on Hetzner (item 4 of `specs/backup-secrets-autofix.md`, from an
//! evals node, 2026-10-15): the server type could only be set in the config file — `--gpu` was
//! silently ignored — so `--server-type` now reaches the create, `--gpu`/`--gpus` are refused on
//! Hetzner pointing at it, and `--server-type` is refused anywhere else. Fakes only: nothing is
//! created, listed or priced for real.

use super::*;
use arena_core::{Pod, Result as CoreResult};
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// A provider named `name` that records each create's spec, lists what it made (counting the
/// lists), and hands every pod an endpoint at once.
struct Sizer {
    name: &'static str,
    specs: Mutex<Vec<PodSpec>>,
    made: Mutex<Vec<Pod>>,
    lists: AtomicUsize,
}

impl Sizer {
    fn new(name: &'static str) -> Self {
        Sizer { name, specs: Mutex::default(), made: Mutex::default(), lists: AtomicUsize::new(0) }
    }
}

#[async_trait]
impl Provider for Sizer {
    fn name(&self) -> &'static str {
        self.name
    }
    fn describe(&self, spec: &PodSpec) -> String {
        format!("{} CPU VM", spec.server_type.as_deref().unwrap_or("configured"))
    }
    async fn list_pods(&self) -> CoreResult<Vec<Pod>> {
        self.lists.fetch_add(1, Ordering::SeqCst);
        Ok(self.made.lock().unwrap().clone())
    }
    async fn create_pod(&self, spec: &PodSpec) -> CoreResult<Pod> {
        self.specs.lock().unwrap().push(spec.clone());
        let n = self.made.lock().unwrap().len() as u16;
        let pod = Pod {
            id: format!("{}", 100 + n),
            name: spec.name.clone(),
            provider: self.name.into(),
            status: "RUNNING".into(),
            ssh_ip: Some("203.0.113.7".into()),
            ssh_port: Some(22),
            ..Default::default()
        };
        self.made.lock().unwrap().push(pod.clone());
        Ok(pod)
    }
    async fn stop_pod(&self, _id: &str) -> CoreResult<()> {
        Ok(())
    }
    async fn restart_pod(&self, _id: &str) -> CoreResult<()> {
        Ok(())
    }
    async fn terminate_pod(&self, _id: &str) -> CoreResult<()> {
        Ok(())
    }
}

/// No proxy (the post-create sync is a skip note), no provider keys (no live price or catalog).
fn cfg() -> Config {
    Config::parse("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom)\nGPU_TYPE=\"NVIDIA RTX A4000\"\n")
}

fn create(gpu: Option<&str>, gpus: Option<u32>, server_type: Option<&str>) -> PodCmd {
    PodCmd::Create {
        names: vec!["apple".into()],
        count: None,
        add: None,
        gpu: gpu.map(String::from),
        gpus,
        server_type: server_type.map(String::from),
        cloud: None,
        max_price: None,
        order: Default::default(),
        disk: None,
        volume: None,
        image: None,
        bootstrap: false,
        api_json: None,
        reap: crate::ReapAtCreate::default(),
        owner: crate::spend::OwnerArgs::default(),
        skip_proxy: true,
        dry_run: false,
        keep_trying: false,
        retry_mins: 0,
        retry_secs: 0,
    }
}

/// `up --no-wait --no-setup`: the create and nothing after it.
fn up(gpu: Option<&str>, server_type: Option<&str>) -> PodCmd {
    PodCmd::Up {
        names: vec!["bloom".into()],
        count: None,
        add: None,
        gpu: gpu.map(String::from),
        gpus: None,
        server_type: server_type.map(String::from),
        cloud: None,
        max_price: None,
        order: Default::default(),
        disk: None,
        volume: None,
        image: None,
        bootstrap: false,
        api_json: None,
        reap: crate::ReapAtCreate::default(),
        owner: crate::spend::OwnerArgs::default(),
        dry_run: false,
        no_wait: true,
        keep_trying: false,
        retry_mins: 0,
        retry_secs: 0,
        no_setup: true,
        check: false,
        check_attempts: 2,
        retry_bad_hosts: true,
        lock: false,
        timeout: 5,
        interval: 1,
    }
}

fn remote() -> Arc<dyn Remote> {
    Arc::new(arena_core::remote::FakeRemote::new())
}

/// One [`sizing_flags`] case: the create target, `--gpu`, `--gpus`, `--server-type`, then what
/// is expected (`W`: the server type to create with, or a piece of the refusal).
type SizingCase<'a, W> = (&'a str, Option<&'a str>, Option<u32>, Option<&'a str>, W);

/// Which sizing flags each create target takes. Pure.
#[test]
fn sizing_flags_fit_the_create_target() {
    let ok: &[SizingCase<Option<&str>>] = &[
        ("hetzner", None, None, Some("cpx42"), Some("cpx42")),
        ("hetzner", None, None, Some(" CPX42 "), Some("cpx42")),
        ("hetzner", None, None, Some("104"), Some("104")),
        ("hetzner", None, None, None, None),
        ("runpod", Some("A4000"), Some(2), None, None),
        ("vast", None, Some(1), None, None),
        ("shadeform", Some("A6000"), None, None, None),
    ];
    for (provider, gpu, gpus, ty, want) in ok {
        let got = sizing_flags(provider, *gpu, *gpus, ty.map(String::from)).unwrap();
        assert_eq!(got.as_deref(), *want, "{provider} {gpu:?} {gpus:?} {ty:?}");
    }
    let long = "x".repeat(33);
    let refused: &[SizingCase<&str>] = &[
        ("hetzner", Some("A4000"), None, None, "--gpu doesn't apply to hetzner, which creates CPU VMs sized by server type — pass --server-type"),
        ("hetzner", None, Some(2), Some("cx23"), "--gpus doesn't apply to hetzner"),
        ("hetzner", Some("A4000"), Some(1), None, "--gpu / --gpus doesn't apply to hetzner"),
        ("runpod", None, None, Some("cx23"), "--server-type is for Hetzner VMs only; these pods would be created on runpod — pass --provider hetzner"),
        ("vast", Some("3090"), None, Some("cx23"), "--server-type is for Hetzner VMs only; these pods would be created on vast"),
        ("shadeform", None, None, Some("cx23"), "created on shadeform"),
        ("hetzner", None, None, Some("cx23; reboot"), "--server-type takes a Hetzner server type name"),
        ("hetzner", None, None, Some(" "), "--server-type takes a Hetzner server type name"),
        ("hetzner", None, None, Some(long.as_str()), "--server-type takes a Hetzner server type name"),
    ];
    for (provider, gpu, gpus, ty, want) in refused {
        let e = sizing_flags(provider, *gpu, *gpus, ty.map(String::from)).unwrap_err().to_string();
        assert!(e.contains(want), "{provider} {gpu:?} {gpus:?} {ty:?}: {e}");
    }
}

/// `--server-type` parses on `create` and `up`, and goes into the create's spec — and from
/// there into the body the real Hetzner backend would send, over its HETZNER_SERVER_TYPE.
#[tokio::test]
async fn the_server_type_flag_reaches_the_hetzner_create_body() {
    use clap::Parser;
    for argv in [
        &["arena", "--provider", "hetzner", "pods", "create", "apple", "--server-type", "cpx42"][..],
        &["arena", "--provider", "hetzner", "pods", "up", "apple", "--server-type", "cpx42"],
    ] {
        match Cli::try_parse_from(argv).unwrap().cmd {
            Cmd::Pods(PodCmd::Create { server_type, .. } | PodCmd::Up { server_type, .. }) => {
                assert_eq!(server_type.as_deref(), Some("cpx42"), "{argv:?}")
            }
            _ => panic!("{argv:?}"),
        }
    }
    let real = arena_core::provider::build("hetzner", &Config::parse("HETZNER_API_KEY=x\nHETZNER_SERVER_TYPE=cx23\n")).unwrap();
    for (cmd, name) in [(create(None, None, Some("CPX42")), "devtest-apple"), (up(None, Some("cpx42")), "devtest-bloom")] {
        let p = Sizer::new("hetzner");
        handle_pods(cmd, &p, remote(), &cfg(), true).await.unwrap();
        let specs = p.specs.lock().unwrap().clone();
        assert_eq!(specs.iter().map(|s| (s.name.as_str(), s.server_type.as_deref())).collect::<Vec<_>>(), [(name, Some("cpx42"))]);
        assert_eq!(real.preview_create_body(&specs[0]).unwrap()["server_type"], "cpx42");
        assert_eq!(real.describe(&specs[0]), "cpx42 CPU VM, image ubuntu-24.04, location any (nearest first, the next when one has no room)");
    }
    // Without the flag: the configured type, as before.
    let p = Sizer::new("hetzner");
    handle_pods(create(None, None, None), &p, remote(), &cfg(), true).await.unwrap();
    let spec = p.specs.lock().unwrap()[0].clone();
    assert_eq!((spec.server_type.as_deref(), real.preview_create_body(&spec).unwrap()["server_type"].as_str()), (None, Some("cx23")));
}

/// Item 4: `--gpu` on Hetzner was ignored — the VM came up as the configured type. Now `create`
/// and `up` refuse it, naming `--server-type`, before anything is listed or created; and
/// `--server-type` where the pods would land on a GPU provider is refused the same way.
#[tokio::test]
async fn a_flag_the_target_cant_honour_is_refused_before_anything_happens() {
    let cases = [
        ("hetzner", create(Some("A4000"), None, None), "--gpu doesn't apply to hetzner"),
        ("hetzner", create(None, Some(2), None), "--gpus doesn't apply to hetzner"),
        ("hetzner", up(Some("RTX 4090"), Some("cx23")), "--gpu doesn't apply to hetzner"),
        ("runpod", create(None, None, Some("cx23")), "--server-type is for Hetzner VMs only; these pods would be created on runpod"),
        ("vast", up(None, Some("cpx42")), "--server-type is for Hetzner VMs only; these pods would be created on vast"),
    ];
    for (name, cmd, want) in cases {
        let p = Sizer::new(name);
        let e = handle_pods(cmd, &p, remote(), &cfg(), true).await.unwrap_err().to_string();
        assert!(e.contains(want) && (name != "hetzner" || e.contains("--server-type (e.g. cx23, cpx42")), "{name}: {e}");
        assert!(p.specs.lock().unwrap().is_empty(), "{name}: nothing created");
        assert_eq!(p.lists.load(Ordering::SeqCst), 0, "{name}: nothing even listed");
    }
}

/// `config check` shows Hetzner's create options as the provider reads them — defaults
/// marked — whenever Hetzner is in play, and the SSH key every server gets, which must be in
/// the project under that name (the evals node's `arena9`, with only `arena8` there). No ✗:
/// nothing here can be judged without a request.
#[test]
fn config_check_shows_the_hetzner_key_name_up_front() {
    let rows = hetzner_rows(&Config::parse("MACHINE_NAME_PREFIX=arena9\nHETZNER_API_KEY=x\n")).join("\n");
    for want in [
        "  · HETZNER_SERVER_TYPE          cx33 (default) — `pods create/up --server-type` overrides it",
        "  · HETZNER_LOCATION             any (nearest first, the next when one has no room) (default)",
        "  · HETZNER_IMAGE                ubuntu-24.04 (default)",
        "  · HETZNER_SSH_KEY              arena9 (default: MACHINE_NAME_PREFIX)",
        "every new server gets SSH key `arena9`: it must be in the Hetzner project under that name — checked at create \
         (`arena api get hetzner ssh_keys` lists the project's keys)",
    ] {
        assert!(rows.contains(want), "{want}\n{rows}");
    }
    let rows = hetzner_rows(&Config::parse("MACHINE_NAME_PREFIX=arena9\nHETZNER_SSH_KEY=arena8\nHETZNER_SERVER_TYPE=cpx42\n")).join("\n");
    assert!(rows.contains("  ✓ HETZNER_SSH_KEY              arena8\n") && rows.contains("  ✓ HETZNER_SERVER_TYPE          cpx42 — "), "{rows}");
    assert!(!rows.contains('✗'), "{rows}");
    // In the report: with Hetzner selected, or a Hetzner key/option set; not otherwise.
    for (text, provider, shown) in [
        ("MACHINE_NAME_PREFIX=arena9\n", "hetzner", true),
        ("MACHINE_NAME_PREFIX=arena9\nHETZNER_API_KEY=x\n", "runpod", true),
        ("MACHINE_NAME_PREFIX=arena9\nHETZNER_LOCATION=fsn1\n", "runpod", true),
        ("MACHINE_NAME_PREFIX=arena9\nRUNPOD_API_KEY=r\n", "runpod", false),
    ] {
        let (report, missing) = config_report(&Config::parse(text), provider);
        assert_eq!(report.contains("HETZNER_SSH_KEY              arena9"), shown, "{provider} {text:?}\n{report}");
        assert!(!missing.iter().any(|m| m.starts_with("HETZNER_SSH_KEY") || m.starts_with("HETZNER_SERVER_TYPE")), "{missing:?}");
    }
}
