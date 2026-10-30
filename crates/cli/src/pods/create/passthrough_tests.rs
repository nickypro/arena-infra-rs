use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use arena_core::provider::runpod_v2::RunpodV2Provider;
use arena_core::remote::FakeRemote;
use arena_core::{Config, Pod, PodSpec, Provider, Result};
use async_trait::async_trait;
use clap::Parser;

use crate::handle_api;
use crate::cli::{ApiCmd, Cli, Cmd, PodCmd};
use crate::pods::handle_pods;
use crate::pods::placement::first_create_spec;
use super::{create_body_text, spec_with_overrides, SpecOverrides};

/// A RunPod v2 account with no pods: the create-body rules and preview are the real v2
/// backend's (pure — no request), lists are counted, creates are recorded (with the body
/// the real backend would build for them), and anything else that would change the fleet
/// panics.
struct V2Shape {
    rules: RunpodV2Provider,
    lists: AtomicUsize,
    bodies: std::sync::Mutex<Vec<serde_json::Value>>,
}

impl V2Shape {
    fn new() -> Self {
        Self { rules: RunpodV2Provider::new("k"), lists: AtomicUsize::new(0), bodies: Default::default() }
    }
}

#[async_trait]
impl Provider for V2Shape {
    fn name(&self) -> &'static str {
        "runpod"
    }
    fn describe(&self, spec: &PodSpec) -> String {
        self.rules.describe(spec)
    }
    async fn list_pods(&self) -> Result<Vec<Pod>> {
        self.lists.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    }
    async fn create_pod(&self, spec: &PodSpec) -> Result<Pod> {
        self.bodies.lock().unwrap().push(self.rules.preview_create_body(spec)?);
        Ok(Pod { id: format!("id-{}", spec.name), name: spec.name.clone(), provider: "runpod".into(), ..Default::default() })
    }
    async fn stop_pod(&self, _id: &str) -> Result<()> {
        panic!("no stop")
    }
    async fn restart_pod(&self, _id: &str) -> Result<()> {
        panic!("no restart")
    }
    async fn terminate_pod(&self, _id: &str) -> Result<()> {
        panic!("no terminate")
    }
    fn check_create_extra(&self, extra: &arena_core::apiextra::Extra) -> Result<()> {
        self.rules.check_create_extra(extra)
    }
    fn preview_create_body(&self, spec: &PodSpec) -> Result<serde_json::Value> {
        self.rules.preview_create_body(spec)
    }
}

fn cfg(extra: &str) -> Config {
    Config::parse(&format!(
        "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(alpha bravo echo)\nIMAGE=nickypro/arena-env:9.1\n\
         GPU_TYPE=\"NVIDIA RTX A4000\"\nCLOUD_TYPE=COMMUNITY\nDISK_GB=60\n{extra}"
    ))
}

fn pods_cmd(argv: &[&str]) -> PodCmd {
    match Cli::try_parse_from(["arena", "pods"].iter().chain(argv).copied()).map(|c| c.cmd) {
        Ok(Cmd::Pods(p)) => p,
        other => panic!("{argv:?}: {:?}", other.err()),
    }
}

/// A network volume (`mounts.network`) next to the pod volume (`--volume`/VOLUME_GB > 0)
/// is a body RunPod v2 answers 400 to: `create`/`up` refuse it once the spec is known —
/// the dry run too — before any create or confirm; with `--volume 0` it goes out.
#[tokio::test]
async fn a_network_volume_next_to_the_pod_volume_is_refused_before_any_create() {
    let network = r#"{"mounts":{"network":[{"volumeId":"v","path":"/data"}]}}"#;
    for argv in [
        &["create", "echo", "--skip-proxy", "--volume", "20", "--api-json", network][..],
        &["create", "echo", "--dry-run", "--volume", "20", "--api-json", network],
        &["up", "echo", "--no-setup", "--volume", "20", "--api-json", network],
        &["up", "echo", "--dry-run", "--volume", "20", "--api-json", network],
    ] {
        let fleet = V2Shape::new();
        let e = handle_pods(pods_cmd(argv), &fleet, Arc::new(FakeRemote::new()), &cfg(""), true)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("replaces the pod volume") && e.contains("pass --volume 0"), "{argv:?}: {e}");
        assert!(fleet.bodies.lock().unwrap().is_empty(), "{argv:?}: nothing created");
    }
    // From config the same (VOLUME_GB under CREATE_EXTRA_JSON).
    let fleet = V2Shape::new();
    let line = format!("VOLUME_GB=20\nCREATE_EXTRA_JSON='{network}'");
    let e = handle_pods(pods_cmd(&["create", "echo", "--skip-proxy"]), &fleet, Arc::new(FakeRemote::new()), &cfg(&line), true)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("pass --volume 0"), "{e}");
    // `--volume 0`: the network volume alone goes out.
    let fleet = V2Shape::new();
    handle_pods(
        pods_cmd(&["create", "echo", "--skip-proxy", "--volume", "0", "--api-json", network]),
        &fleet,
        Arc::new(FakeRemote::new()),
        &cfg("VOLUME_GB=20"),
        true,
    )
    .await
    .unwrap();
    let bodies = fleet.bodies.lock().unwrap();
    assert_eq!(bodies[0]["mounts"], serde_json::json!({"network": [{"volumeId": "v", "path": "/data"}]}));
}

/// Bad JSON, a non-object, or a field arena sets itself — from the flag or from config —
/// fails before the fleet is even listed (so before anything is priced or created).
#[tokio::test]
async fn api_json_is_parsed_and_checked_before_any_api_call() {
    // (command, config line, the error)
    let cases: &[(&[&str], &str, &str)] = &[
        (&["create", "echo", "--api-json", "[1]"], "", "--api-json: must be a JSON object"),
        (&["up", "echo", "--api-json", "{"], "", "--api-json: not valid JSON"),
        (&["create", "echo", "--api-json", r#"{"gpu":{"id":"NVIDIA H100"}}"#], "", "`gpu.id` is set by arena (--gpu / GPU_TYPE)"),
        (&["up", "echo", "--api-json", r#"{"ports":["8888/http"]}"#], "", "must keep \"22/tcp\""),
        (&["up", "echo", "--api-json", r#"{"locked":true}"#], "", "`pods up --lock`"),
        (&["create", "echo"], r#"CREATE_EXTRA_JSON='{"name":"x"}'"#, "`name` is set by arena (the machine name)"),
        (&["create", "echo"], "CREATE_EXTRA_JSON='not json'", "CREATE_EXTRA_JSON: not valid JSON"),
    ];
    for (argv, line, want) in cases {
        let fleet = V2Shape::new();
        let e = handle_pods(pods_cmd(argv), &fleet, Arc::new(FakeRemote::new()), &cfg(line), true)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains(want), "{argv:?} {line}: `{want}` in {e}");
        assert_eq!(fleet.lists.load(Ordering::SeqCst), 0, "{argv:?}: refused before any API call");
        assert!(fleet.bodies.lock().unwrap().is_empty(), "{argv:?}: nothing created");
    }
    // `up --lock` where the create target can't lock: refused before anything, too.
    let fleet = V2Shape::new();
    let e = handle_pods(pods_cmd(&["up", "echo", "--lock"]), &fleet, Arc::new(FakeRemote::new()), &cfg(""), true)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("--lock: not supported on runpod"), "{e}");
    assert_eq!(fleet.lists.load(Ordering::SeqCst), 0);
    // Accepted: listed, previewed, nothing created (dry run).
    let fleet = V2Shape::new();
    let ok = r#"{"dataCenterIds":["EU-RO-1"],"globalNetworking":true}"#;
    handle_pods(pods_cmd(&["up", "echo", "--dry-run", "--api-json", ok]), &fleet, Arc::new(FakeRemote::new()), &cfg(""), true)
        .await
        .unwrap();
    assert_eq!(fleet.lists.load(Ordering::SeqCst), 1);
}

/// A real (confirmed) create sends the backend's body with the extra merged — config
/// under the flag, objects key by key (our env and GPU kept) — for every name.
#[tokio::test]
async fn create_sends_the_merged_body_for_every_name() {
    let cfg = cfg(r#"CREATE_EXTRA_JSON='{"dataCenterIds":["EU-RO-1"],"env":{"FROM_CFG":"1"}}'"#);
    let fleet = V2Shape::new();
    let flag = r#"{"globalNetworking":true,"gpu":{"minRamPerGpu":32}}"#;
    handle_pods(pods_cmd(&["create", "echo", "bravo", "--skip-proxy", "--api-json", flag]), &fleet, Arc::new(FakeRemote::new()), &cfg, true)
        .await
        .unwrap();
    let bodies = fleet.bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2);
    for (body, name) in bodies.iter().zip(["devtest-echo", "devtest-bravo"]) {
        assert_eq!(body["name"], name);
        assert_eq!(body["env"]["MACHINE_NAME"], name, "ours kept");
        assert_eq!(body["env"]["FROM_CFG"], "1");
        assert_eq!(body["dataCenterIds"], serde_json::json!(["EU-RO-1"]));
        assert_eq!(body["globalNetworking"], true);
        assert_eq!(body["gpu"], serde_json::json!({"id": "NVIDIA RTX A4000", "count": 1, "minRamPerGpu": 32}));
        assert_eq!(body["cloud"], "COMMUNITY");
    }
}

/// The dry run's create request: the backend's body with the extra merged — the flag
/// over config — and every env value redacted (PUBLIC_KEY's keys, MACHINE_NAME, an extra
/// one); nothing at all without extra fields.
#[test]
fn the_dry_run_shows_the_merged_body_with_secrets_redacted() {
    let cfg = cfg(r#"CREATE_EXTRA_JSON='{"dataCenterIds":["EU-RO-1"],"env":{"FROM_CFG":"1"}}'"#);
    let flag = r#"{"dataCenterIds":["US-KS-2"],"env":{"JUPYTER_PASSWORD":"hunter2"},"gpu":{"minRamPerGpu":32}}"#;
    let extra = arena_core::apiextra::combine(cfg.get("CREATE_EXTRA_JSON"), Some(flag)).unwrap();
    let mut base = spec_with_overrides(&cfg, &SpecOverrides { api_extra: extra, ..Default::default() });
    base.env.push(("PUBLIC_KEY".into(), "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAbcdefghijklmnopqrstuvwxyz0123456789 devtest".into()));
    let spec = first_create_spec(&base, None, "devtest-echo");
    let text = create_body_text(&V2Shape::new(), &spec).unwrap();
    for want in [
        "[dry-run] create request for devtest-echo with --api-json / CREATE_EXTRA_JSON merged",
        "\"dataCenterIds\": [\n    \"US-KS-2\"\n  ]",
        "\"minRamPerGpu\": 32",
        "\"id\": \"NVIDIA RTX A4000\"",
        "\"FROM_CFG\": \"<redacted>\"",
        "\"JUPYTER_PASSWORD\": \"<redacted>\"",
        "\"MACHINE_NAME\": \"<redacted>\"",
        "\"PUBLIC_KEY\": \"<redacted>\"",
        "\"cloud\": \"COMMUNITY\"",
    ] {
        assert!(text.contains(want), "`{want}` in\n{text}");
    }
    assert!(!text.contains("hunter2") && !text.contains("AAAAC3"), "{text}");
    let plain = spec_with_overrides(&cfg, &SpecOverrides::default());
    assert_eq!(create_body_text(&V2Shape::new(), &first_create_spec(&plain, None, "devtest-echo")), None);
}

/// `api get`: the path is checked before the key is even read (so before any request),
/// an unknown provider and a missing key are refused, and the flags parse.
#[tokio::test]
async fn api_get_refuses_before_any_request() {
    let get = |provider: &str, path: &str| ApiCmd::Get { provider: provider.into(), path: path.into(), raw: false, limits: false };
    let no_key = cfg("");
    // (provider, path, the error)
    for (provider, path, want) in [
        ("runpod", "https://evil.example/x", "absolute URLs are refused"),
        ("runpod-v2", "//evil.example/pods", "absolute URLs are refused"),
        ("vast", "../users/current/", "path segments are refused"),
        ("lambda", "pods", "unknown provider `lambda`"),
        ("runpod", "pods", "RUNPOD_API_KEY isn't set"),
        ("hetzner", "servers", "HETZNER_API_KEY isn't set"),
    ] {
        let e = handle_api(get(provider, path), &no_key).await.unwrap_err().to_string();
        assert!(e.contains(want), "{provider} {path}: `{want}` in {e}");
    }
    let with_key = cfg("RUNPOD_API_KEY=rpa_TESTKEY0123456789\n");
    let e = handle_api(get("runpod", "http://api.runpod.io/v2/pods"), &with_key).await.unwrap_err().to_string();
    assert!(e.contains("absolute URLs are refused") && !e.contains("rpa_"), "{e}");
    match Cli::try_parse_from(["arena", "api", "get", "runpod-v2", "pods?limit=5", "--raw"]).unwrap().cmd {
        Cmd::Api(ApiCmd::Get { provider, path, raw, limits }) => {
            assert_eq!((provider.as_str(), path.as_str(), raw, limits), ("runpod-v2", "pods?limit=5", true, false))
        }
        _ => panic!("not api get"),
    }
    match Cli::try_parse_from(["arena", "api", "get", "runpod", "pods", "--limits"]).unwrap().cmd {
        Cmd::Api(ApiCmd::Get { raw, limits, .. }) => assert_eq!((raw, limits), (false, true)),
        _ => panic!("not api get"),
    }
    assert!(Cli::try_parse_from(["arena", "api", "get", "runpod", "pods", "--limits", "--raw"]).is_err(), "--limits prints no body");
    // GET is the only verb: there is nothing else to parse.
    assert!(Cli::try_parse_from(["arena", "api", "post", "runpod", "pods"]).is_err());
    assert!(Cli::try_parse_from(["arena", "api", "get", "runpod", "pods", "--method", "DELETE"]).is_err());
}
