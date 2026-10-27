//! Contract tests: the v2 backend's pure readers and request builders against responses
//! **recorded from the live API** (2026-10-08, sandbox pod `mqseb0i5acrssr`), not against
//! shapes we wrote ourselves. Each `fixtures/runpod_v2/*.json` is one exchange — the request
//! as sent (`method`, `path`, `body`), the HTTP `status` and the response `body` — scrubbed
//! (env values and SSH commands `<redacted>`) and kept byte-for-byte as recorded: when
//! RunPod changes a shape, re-record and let these tests say what broke.
//!
//! A child module of `runpod_v2`, so it reaches the private builders/parsers directly.

use super::*;
use crate::placement::{PriceBook, PriceSource};

/// Every recorded exchange, by file stem. `every_fixture_is_listed_and_well_formed` keeps
/// this list and the directory in step, so a new recording can't sit there untested.
const FIXTURES: &[(&str, &str)] = &[
    ("catalog_community_cuda13", include_str!("fixtures/runpod_v2/catalog_community_cuda13.json")),
    ("create_bad_gpu_error", include_str!("fixtures/runpod_v2/create_bad_gpu_error.json")),
    ("create_empty_body_error", include_str!("fixtures/runpod_v2/create_empty_body_error.json")),
    ("get_running", include_str!("fixtures/runpod_v2/get_running.json")),
    ("list_one_running", include_str!("fixtures/runpod_v2/list_one_running.json")),
    ("locked_delete", include_str!("fixtures/runpod_v2/locked_delete.json")),
    ("locked_restart_refused", include_str!("fixtures/runpod_v2/locked_restart_refused.json")),
    ("locked_stop_refused", include_str!("fixtures/runpod_v2/locked_stop_refused.json")),
    ("network_volumes_empty", include_str!("fixtures/runpod_v2/network_volumes_empty.json")),
    ("patch_lock", include_str!("fixtures/runpod_v2/patch_lock.json")),
    ("patch_lock2", include_str!("fixtures/runpod_v2/patch_lock2.json")),
    ("patch_name", include_str!("fixtures/runpod_v2/patch_name.json")),
    ("patch_unlock", include_str!("fixtures/runpod_v2/patch_unlock.json")),
    ("patch_unlock2", include_str!("fixtures/runpod_v2/patch_unlock2.json")),
];

/// The sandbox pod every recording is about.
const POD_ID: &str = "mqseb0i5acrssr";

/// One recorded exchange.
struct Recorded {
    method: Method,
    /// Path and query, as the recording writes them (`/v2/pods/{id}`).
    path: String,
    /// The request body (`null` for none).
    request: Value,
    status: StatusCode,
    body: Value,
}

impl Recorded {
    /// The response body as text, for the readers that take what the wire gave them.
    fn text(&self) -> String {
        self.body.to_string()
    }
}

fn recorded(name: &str) -> Recorded {
    let raw = FIXTURES.iter().find(|(n, _)| *n == name).unwrap_or_else(|| panic!("no fixture `{name}`")).1;
    let v: Value = serde_json::from_str(raw).unwrap_or_else(|e| panic!("{name}: {e}"));
    Recorded {
        method: Method::from_bytes(v["request"]["method"].as_str().expect("method").as_bytes()).unwrap(),
        path: v["request"]["path"].as_str().expect("path").to_string(),
        request: v["request"]["body"].clone(),
        status: StatusCode::from_u16(v["status"].as_u64().expect("status") as u16).unwrap(),
        body: v["body"].clone(),
    }
}

/// A built request's method and path (with its query, if any) — what a recording holds.
fn sent(rb: RequestBuilder) -> (Method, String) {
    let req = rb.build().unwrap();
    let url = req.url();
    let path = match url.query() {
        Some(q) => format!("{}?{q}", url.path()),
        None => url.path().to_string(),
    };
    (req.method().clone(), path)
}

/// A built request's JSON body.
fn sent_body(rb: RequestBuilder) -> Value {
    let req = rb.build().unwrap();
    serde_json::from_slice(req.body().expect("a body").as_bytes().expect("in memory")).unwrap()
}

fn client() -> Client {
    Client::new()
}

/// The pod as every recording of it reads — the list's view of it, and the fields the
/// proxy, setup and `pods list` live on.
fn sandbox_pod(name: &str) -> Pod {
    Pod {
        id: POD_ID.into(),
        name: name.into(),
        provider: "runpod".into(),
        status: "RUNNING".into(),
        gpu_type: Some("NVIDIA GeForce RTX 3070".into()),
        gpu_count: Some(1),
        cost_per_hr: Some(0.13),
        // `ssh.direct` — never `ssh.proxy` (ssh.runpod.io:22), which has no scp/rsync.
        ssh_ip: Some("64.119.209.250".into()),
        ssh_port: Some(23924),
        maintenance: None,
        machine_id: None,
        // Every recording but the lock PATCHes' answers is of the pod unlocked.
        locked: Some(false),
        proxy_only: false,
        // Every recording is of a 60 GB container disk, no volume.
        disk_gb: Some(60.0),
        volume_gb: Some(0.0),
        delete_protected: None,
        created_at: Some(1_791_452_013),
        started_at: Some(1_791_452_013),
        identity: None,
        status_detail: None,
    }
}

#[test]
fn every_fixture_is_listed_and_well_formed() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/provider/fixtures/runpod_v2");
    let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
        .collect();
    on_disk.sort();
    let listed: Vec<String> = FIXTURES.iter().map(|(n, _)| n.to_string()).collect();
    assert_eq!(on_disk, listed, "fixtures/runpod_v2 and FIXTURES must list the same recordings");
    for (name, _) in FIXTURES {
        let r = recorded(name);
        assert!(r.path.starts_with("/v2/"), "{name}: {}", r.path);
        assert!(!r.body.is_null(), "{name}: a recorded body");
    }
}

/// The recordings stay scrubbed: no key material, tokens or SSH commands, and every pod's
/// env values redacted — so a careless re-recording fails here, not in a public repo.
#[test]
fn recorded_fixtures_stay_scrubbed() {
    for (name, raw) in FIXTURES {
        for needle in ["ssh-ed25519 ", "ssh-rsa ", "ecdsa-sha2", "BEGIN OPENSSH", "rpa_", "Bearer ", "hf_", "sk-or-"] {
            assert!(!raw.contains(needle), "{name} contains `{needle}`");
        }
        let body = recorded(name).body;
        let pods: Vec<&Value> = match body.get("pods").and_then(Value::as_array) {
            Some(list) => list.iter().collect(),
            None if body.get("env").is_some() => vec![&body],
            None => vec![],
        };
        for pod in pods {
            for (k, v) in pod["env"].as_object().expect("env object") {
                assert_eq!(v, "<redacted>", "{name}: env {k}");
            }
            for via in ["direct", "proxy"] {
                assert_eq!(pod["ssh"][via]["command"], "ssh <redacted>", "{name}: ssh.{via}.command");
            }
        }
    }
}

/// `GET /v2/pods`: one page (`hasNextPage: false`, `nextCursor: null`), the endpoint from
/// `ssh.direct`, and the paging walk asks exactly once.
#[tokio::test]
async fn list_reads_the_recorded_page() {
    let r = recorded("list_one_running");
    let (method, path) = sent(list_request(&client(), "k", None));
    assert_eq!((method, path.split('?').next().unwrap()), (r.method.clone(), r.path.as_str()));
    assert_eq!(r.body["pagination"], json!({"hasNextPage": false, "nextCursor": null}));

    let body = judge_v2(r.status, &r.text(), "list pods").unwrap();
    let (pods, next) = parse_page(&body).unwrap();
    assert_eq!(next, None);
    assert_eq!(pods, vec![sandbox_pod("devtest-alpha")]);

    let asked = Mutex::new(Vec::new());
    let walked = collect_pages(
        |cursor| {
            asked.lock().unwrap().push(cursor);
            let page = body.clone();
            async move { Ok(page) }
        },
        MAX_PAGES,
    )
    .await
    .unwrap();
    assert_eq!(walked, pods);
    assert_eq!(asked.into_inner().unwrap(), vec![None]);
}

/// `GET /v2/pods/{id}`: the same pod as the list, and a recreate-able spec — GPU, count and
/// tier recovered (v2 reports them; v1 didn't), the identity env dropped, no volume.
#[test]
fn get_reads_the_pod_and_recovers_its_spec() {
    let r = recorded("get_running");
    assert_eq!(sent(api_request(&client(), "k", Method::GET, pod_url(POD_ID, None).unwrap())), (r.method.clone(), r.path.clone()));
    let body = judge_v2(r.status, &r.text(), "get pod").unwrap();
    assert_eq!(parse_pod(&body), sandbox_pod("devtest-alpha"));
    let s = parse_spec(&body);
    assert_eq!(
        (s.name.as_str(), s.image.as_str(), s.gpu_type.as_str(), s.gpu_count, s.cloud_type.as_str()),
        ("", "nickypro/arena-env:9.1", "NVIDIA GeForce RTX 3070", 1, "COMMUNITY")
    );
    assert_eq!((s.disk_gb, s.volume_gb, s.ports.as_str()), (60, 0, "8888/http,22/tcp"));
    assert!(s.env.is_empty(), "MACHINE_NAME/PUBLIC_KEY are re-seeded by replace: {:?}", s.env);
    assert_eq!((s.docker_args, s.allowed_cuda.len()), (None, 0));
}

/// The rename the backend sends is the request that was recorded (and live-verified
/// restart-free): `PATCH /v2/pods/{id}` with `{"name"}` alone. Its answer — the whole pod,
/// under the new name, at the same endpoint (so the proxy forward stays valid) — is a
/// success; the same answer for a different requested name is not.
#[test]
fn rename_matches_the_recorded_name_only_patch() {
    let r = recorded("patch_name");
    let new = r.request["name"].as_str().unwrap();
    assert_eq!(new, "devtest-echo");
    assert_eq!(sent(rename_request(&client(), "k", POD_ID, new).unwrap()), (r.method.clone(), r.path.clone()));
    assert_eq!(sent_body(rename_request(&client(), "k", POD_ID, new).unwrap()), r.request);
    assert_eq!(rename_payload(new), r.request);

    judge_rename(r.status, &r.text(), new).unwrap();
    let e = judge_rename(r.status, &r.text(), "devtest-foxtrot").unwrap_err().to_string();
    assert!(e.contains("still named `devtest-echo`"), "{e}");
    let pod = parse_pod(&judge_v2(r.status, &r.text(), "rename pod").unwrap());
    assert_eq!(pod, sandbox_pod("devtest-echo"));
}

/// `PATCH {"locked": …}` answers with the pod, `locked` flipped and nothing else about it
/// changed. `actions` still offers stop/restart/terminate while locked (the API refuses
/// them anyway — see `locked_refusals_are_locked_errors`), so lock state must be read from
/// `locked`, never inferred from `actions`. The lock/unlock the backend sends is exactly
/// the recorded request, and the recorded answer is a success — for that request only.
#[test]
fn lock_patches_answer_with_the_pod() {
    for (name, locked) in [("patch_lock", true), ("patch_lock2", true), ("patch_unlock", false), ("patch_unlock2", false)] {
        let r = recorded(name);
        assert_eq!(r.request, json!({ "locked": locked }), "{name}");
        assert_eq!(lock_payload(locked), r.request, "{name}");
        assert_eq!(sent(lock_request(&client(), "k", POD_ID, locked).unwrap()), (r.method.clone(), r.path.clone()), "{name}");
        assert_eq!(sent_body(lock_request(&client(), "k", POD_ID, locked).unwrap()), r.request, "{name}");
        let body = judge_v2(r.status, &r.text(), "lock pod").unwrap();
        assert_eq!(body["locked"], json!(locked), "{name}");
        assert_eq!(body["actions"], json!(["stop", "restart", "terminate"]), "{name}");
        assert_eq!(parse_pod(&body), Pod { locked: Some(locked), ..sandbox_pod("devtest-echo") }, "{name}");
        judge_lock(r.status, &r.text(), locked).unwrap();
        // The same 200 for the opposite request: the pod didn't change — not a success.
        let e = judge_lock(r.status, &r.text(), !locked).unwrap_err().to_string();
        assert!(e.contains(&format!("still reports locked={locked}")), "{name}: {e}");
    }
    // The list and the GET say which pods are locked (both recorded unlocked).
    for name in ["list_one_running", "get_running"] {
        let r = recorded(name);
        let body = judge_v2(r.status, &r.text(), "list pods").unwrap();
        let pod = body.get("pods").and_then(|p| p.get(0)).unwrap_or(&body);
        assert_eq!(parse_pod(pod).locked, Some(false), "{name}");
    }
}

/// A lock answered with an error reads as everywhere on v2: a locked-pod refusal stays
/// `Locked`, a 401 stays `Auth` — and the context names which way it went.
#[test]
fn lock_errors_keep_their_meaning() {
    let r = recorded("locked_stop_refused");
    let e = judge_lock(r.status, &r.text(), false).unwrap_err();
    assert!(e.is_locked() && e.to_string().contains("unlock pod HTTP 400"), "{e}");
    let e = judge_lock(StatusCode::UNAUTHORIZED, "", true).unwrap_err();
    assert_eq!(e.kind(), Some(ProviderErrorKind::Auth), "{e}");
    assert!(e.to_string().contains("lock pod HTTP 401"), "{e}");
    // A 2xx with no pod in it: the status is the contract.
    judge_lock(StatusCode::NO_CONTENT, "", true).unwrap();
}

/// A locked pod's stop, restart and DELETE come back `400 {"detail": "Pod is locked"}`:
/// a [`ProviderErrorKind::Locked`] error, its message kept, never retried, never capacity.
#[tokio::test]
async fn locked_refusals_are_locked_errors() {
    let action = |a: &str| api_request(&client(), "k", Method::POST, pod_url(POD_ID, Some("action")).unwrap()).json(&action_body(a));
    let cases = [
        ("locked_stop_refused", Some(action("stop")), "stop pod"),
        ("locked_restart_refused", Some(action("restart")), "restart pod"),
        ("locked_delete", None, "terminate pod"),
    ];
    for (name, built, ctx) in cases {
        let r = recorded(name);
        match built {
            Some(rb) => {
                let body = sent_body(rb.try_clone().unwrap());
                assert_eq!(body, r.request, "{name}");
                assert_eq!(sent(rb), (r.method.clone(), r.path.clone()), "{name}");
            }
            None => {
                let rb = api_request(&client(), "k", Method::DELETE, pod_url(POD_ID, None).unwrap());
                assert_eq!(sent(rb), (r.method.clone(), r.path.clone()), "{name}");
                assert!(r.request.is_null(), "{name}");
            }
        }
        let e = judge_v2(r.status, &r.text(), ctx).unwrap_err();
        assert!(e.is_locked(), "{name}: {e:?}");
        assert_ne!(e.kind(), Some(ProviderErrorKind::Capacity), "{name}");
        let msg = e.to_string();
        assert!(msg.contains(&format!("{ctx} HTTP 400")) && msg.contains("Pod is locked"), "{name}: {msg}");
        // What other crates' fakes replay (`recorded::locked_refusal`) reads the same.
        let replayed = super::recorded::locked_refusal(ctx);
        assert!(replayed.is_locked() && replayed.to_string() == msg, "{name}: {replayed} vs {msg}");
        // Not retried: one attempt, then the error.
        let attempts = std::sync::atomic::AtomicU32::new(0);
        let policy = crate::retry::RetryPolicy { max_retries: 3, base_delay: std::time::Duration::ZERO, max_delay: std::time::Duration::ZERO };
        let r2 = crate::retry::retrying(&policy, || {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let res: Result<()> = Err(v2_error(r.status, &r.text(), ctx));
            async move { res }
        })
        .await;
        assert!(r2.unwrap_err().is_locked(), "{name}");
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1, "{name}");
    }
}

/// The recorded create that named a GPU RunPod doesn't have: `422 "Unknown GPU type: …"`.
/// A config error (the `--gpu`/`GPU_TYPE` is wrong — the class the pre-create check uses),
/// never capacity (waiting can't conjure a GPU type), never retried, message kept whole.
/// The request got past schema validation to the GPU lookup, so its field names are v2's:
/// every one of them is what our create payload sends.
#[tokio::test]
async fn unknown_gpu_create_is_a_config_error() {
    let r = recorded("create_bad_gpu_error");
    let e = judge_create(r.status, &r.text()).unwrap_err();
    assert!(matches!(e, Error::Config(_)), "{e:?}");
    assert_eq!(e.kind(), None);
    let msg = e.to_string();
    assert!(msg.contains("create pod HTTP 422") && msg.contains("Unknown GPU type: NVIDIA NOT A GPU"), "{msg}");
    assert!(!crate::retry::is_retryable(&e));

    let attempts = std::sync::atomic::AtomicU32::new(0);
    let r2 = crate::retry::retrying(&crate::retry::RetryPolicy::default(), || {
        attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let res: Result<Value> = judge_create(r.status, &r.text());
        async move { res }
    })
    .await;
    assert!(r2.is_err());
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);

    let (method, path) = sent(api_request(&client(), "k", Method::POST, base_url("pods")));
    assert_eq!((method, path), (r.method.clone(), r.path.clone()));
    let spec = PodSpec {
        name: "devtest-x".into(),
        image: "nickypro/arena-env:9.1".into(),
        gpu_type: "NVIDIA NOT A GPU".into(),
        gpu_count: 1,
        cloud_type: "COMMUNITY".into(),
        disk_gb: 60,
        volume_gb: 0,
        ports: "8888/http,22/tcp".into(),
        env: Vec::new(),
        docker_args: None,
        allowed_cuda: Vec::new(),
        min_cuda: None,
        max_price: None,
        api_extra: None,
        avoid_machines: Vec::new(),
        server_type: None,
    };
    let ours = create_payload(&spec, &[]).unwrap();
    for (k, v) in r.request.as_object().unwrap() {
        assert_eq!(ours.get(k), Some(v), "recorded field `{k}`");
    }
}

/// `POST /v2/pods {}`: `422 "Request validation failed."` with `errors` naming the missing
/// fields — a config error whose message keeps that list. Our payload always carries both.
#[test]
fn validation_errors_keep_their_field_list() {
    let r = recorded("create_empty_body_error");
    assert_eq!(r.request, json!({}));
    let e = judge_create(r.status, &r.text()).unwrap_err();
    assert!(matches!(e, Error::Config(_)) && e.kind().is_none(), "{e:?}");
    let msg = e.to_string();
    for want in ["create pod HTTP 422", "Request validation failed.", "missing property 'name'", "missing property 'image'"] {
        assert!(msg.contains(want), "`{want}` in {msg}");
    }
    assert!(!crate::retry::is_retryable(&e));
}

/// `GET /v2/catalog/gpus` (COMMUNITY; recorded with a `minCudaVersion` we don't send): every
/// GPU is listed whatever its tier; a tier's price counts only where that tier offers the
/// card (RunPod quotes placeholders — `community: false` with a $0.50 community price);
/// stock is the tier's. And the parsed catalog drives the `--gpu` check and placement.
#[test]
fn catalog_parses_prices_stock_and_placeholders() {
    let r = recorded("catalog_community_cuda13");
    let (method, ours) = sent(catalog_request(&client(), "k", "COMMUNITY", None));
    assert_eq!(method, r.method);
    assert!(r.path.starts_with(&ours), "recorded {} vs ours {ours}", r.path);
    assert_eq!(ours, "/v2/catalog/gpus?include=AVAILABILITY&product=POD&cloud=COMMUNITY");
    // With a CUDA floor the request is exactly the recorded one (finding #31).
    assert_eq!(sent(catalog_request(&client(), "k", "COMMUNITY", Some("13"))).1, r.path);

    let types = parse_catalog(&judge_v2(r.status, &r.text(), "gpu catalog").unwrap()).unwrap();
    assert_eq!(types.len(), r.body["gpus"].as_array().unwrap().len());
    assert_eq!(types.len(), 49);
    let get = |id: &str| types.iter().find(|t| t.id == id).unwrap_or_else(|| panic!("{id}")).clone();
    assert_eq!(
        get("NVIDIA RTX A4000"),
        GpuType {
            id: "NVIDIA RTX A4000".into(),
            display_name: "RTX A4000".into(),
            memory_gb: 16,
            community_price: Some(0.17),
            secure_price: Some(0.25),
            stock_status: Some("Low".into()),
        }
    );
    // Secure-only cards are in the COMMUNITY answer too (so one tier's catalog checks any
    // `--gpu`), with their placeholder community price dropped.
    let mi300 = get("AMD Instinct MI300X OAM");
    assert_eq!((mi300.community_price, mi300.secure_price, mi300.stock_status.as_deref()), (None, Some(2.39), Some("None")));
    let h200nvl = get("NVIDIA H200 NVL");
    assert_eq!((h200nvl.community_price, h200nvl.secure_price), (None, Some(3.79)));
    // Community-only: a 0 secure price, and a $0.50 placeholder beside `secure: false`.
    assert_eq!(get("NVIDIA A100-SXM4-40GB").secure_price, None);
    let maxq = get("NVIDIA RTX PRO 6000 Blackwell Max-Q Workstation Edition");
    assert_eq!((maxq.community_price, maxq.secure_price, maxq.stock_status.as_deref()), (Some(1.64), None, Some("Low")));
    assert_eq!(get("NVIDIA GeForce RTX 3070").community_price, Some(0.13));
    // RunPod's `unknown` placeholder parses; the readers below skip it.
    assert_eq!(get("unknown").memory_gb, 0);

    // The `--gpu` check resolves aliases and short names against it, and refuses typos.
    assert_eq!(crate::gpu::check_gpu_flag("3070", &types).unwrap(), "NVIDIA GeForce RTX 3070");
    assert_eq!(crate::gpu::check_gpu_flag("A4000,RTX 4090", &types).unwrap(), "NVIDIA RTX A4000,NVIDIA GeForce RTX 4090");
    let e = crate::gpu::check_gpu_flag("3070x", &types).unwrap_err().to_string();
    assert!(e.contains("did you mean") && e.contains("NVIDIA GeForce RTX 3070"), "{e}");
    assert!(crate::gpu::check_gpu_flag("unknown", &types).is_err());

    // Placement prices a COMMUNITY option from it, live, with the tier's stock.
    let book = PriceBook::runpod(vec![(Some("COMMUNITY".into()), types.clone())]);
    let q = book.quote("NVIDIA RTX A4000", Some("COMMUNITY"));
    assert_eq!((q.price_per_gpu, q.source, q.stock.as_deref()), (Some(0.17), Some(PriceSource::Live), Some("Low")));
    let q = book.quote("AMD Instinct MI300X OAM", Some("COMMUNITY"));
    assert_eq!((q.price_per_gpu, q.source), (None, Some(PriceSource::Live)), "not offered on community: no price, not a placeholder");
}

/// `GET /v2/network-volumes` on an account with none: an empty list, read as such.
#[test]
fn network_volumes_read_the_recorded_empty_list() {
    let r = recorded("network_volumes_empty");
    assert_eq!(sent(volumes_request(&client(), "k")), (r.method.clone(), r.path.clone()));
    let body = judge_v2(r.status, &r.text(), "list network volumes").unwrap();
    assert!(parse_network_volumes(&body).unwrap().is_empty());
}

/// Reproducer of the list/detail discrepancy reported 2026-10-26. This fixture is
/// derived from the scrubbed recording, with the two omitted list fields set to null.
#[tokio::test]
async fn list_without_runtime_recovers_endpoints_from_detail() {
    let v: Value = serde_json::from_str(include_str!("fixtures/runpod_v2_list_missing_runtime.json")).unwrap();
    let pods = collect_pages(|_| { let v = v.clone(); async move { Ok(v) } }, 1).await.unwrap();
    assert!(pods[0].ssh_ip.is_none());
    let pods = fill_endpoints(pods, |id| async move {
        assert_eq!(id, POD_ID);
        Ok(recorded("get_running").body)
    }, &Mutex::new(EndpointMemo::default()), 1_792_000_000).await;
    assert_eq!(pods, vec![sandbox_pod("devtest-alpha")]);
}

/// A failed or malformed lookup leaves only that pod without an endpoint, and says why; the
/// others are filled. (The proxy keeps a listed pod's forward; a backup counts a pod without
/// SSH as failed work.) Order and every pod are kept.
#[tokio::test]
async fn a_failed_lookup_leaves_only_that_pod_without_an_endpoint() {
    let running = |id: &str| Pod { id: id.into(), status: "RUNNING".into(), started_at: Some(1000), ..Pod::default() };
    let got = fill_endpoints(vec![running("bad"), running("good"), running("odd")], |id| async move {
        match id.as_str() {
            "bad" => Err(Error::provider("HTTP 429 Too Many Requests")),
            "odd" => Ok(json!({"id": "wrong", "status": "RUNNING"})),
            _ => Ok(json!({"id": id, "status": "RUNNING", "ssh": {"direct": {"host": "203.0.113.4", "port": 2222}}})),
        }
    }, &Mutex::new(EndpointMemo::default()), 5000).await;
    assert_eq!(got.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["bad", "good", "odd"]);
    assert_eq!((got[1].ssh_ip.as_deref(), got[1].ssh_port, &got[1].status_detail), (Some("203.0.113.4"), Some(2222), &None));
    for (p, why) in [(&got[0], "429"), (&got[2], "invalid detail for odd")] {
        assert_eq!((p.ssh_ip.as_deref(), p.ssh_port, p.status.as_str()), (None, None, "RUNNING"), "{p:?}");
        let said = p.status_detail.as_deref().unwrap_or_default();
        assert!(said.starts_with("SSH endpoint lookup failed: ") && said.contains(why), "{said}");
    }
}

/// The provider keeps what it looked up between listings — a dashboard is one provider for
/// its whole run — so its second listing of the same pods sends no detail GET.
#[tokio::test]
async fn a_providers_second_listing_reuses_the_endpoints() {
    use std::sync::{atomic::{AtomicUsize, Ordering}, Arc};
    let list: Value = serde_json::from_str(include_str!("fixtures/runpod_v2_list_missing_runtime.json")).unwrap();
    let details = Arc::new(AtomicUsize::new(0));
    let p = RunpodV2Provider { api_key: "rpa_TEST".into(), client: Client::new(), account_keys: Mutex::new(None), endpoints: Mutex::new(EndpointMemo::default()) };
    for _ in 0..3 {
        let details = details.clone();
        let pods = p.list_via(|_| { let v = list.clone(); async move { Ok(v) } }, move |_| {
            details.fetch_add(1, Ordering::SeqCst);
            async { Ok(recorded("get_running").body) }
        }).await.unwrap();
        assert_eq!(pods, vec![sandbox_pod("devtest-alpha")]);
    }
    assert_eq!(details.load(Ordering::SeqCst), 1);
}

/// The endpoint is looked up once per start: refreshes reuse it (no GET), a restart (a new
/// `startedAt`) or [`ENDPOINT_RECHECK_SECS`] looks it up again, and a failed recheck keeps the
/// one remembered for that start. Only a found endpoint with a known start is remembered;
/// a pod no longer listed is forgotten.
#[tokio::test]
async fn an_endpoint_is_remembered_while_the_pod_keeps_its_start() {
    use std::sync::{atomic::{AtomicU16, AtomicUsize, Ordering}, Arc};
    let gets = Arc::new(AtomicUsize::new(0));
    let port = Arc::new(AtomicU16::new(2201));
    let fails = Arc::new(AtomicUsize::new(0));
    let memo = Mutex::new(EndpointMemo::default());
    let pod = |started_at: Option<u64>| Pod { id: "p1".into(), status: "RUNNING".into(), started_at, ..Pod::default() };
    let fill = |pods: Vec<Pod>, now: u64| {
        let (gets, port, fails) = (gets.clone(), port.clone(), fails.clone());
        fill_endpoints(pods, move |id| {
            gets.fetch_add(1, Ordering::SeqCst);
            let (port, failing) = (port.load(Ordering::SeqCst), fails.load(Ordering::SeqCst) > 0);
            async move {
                if failing {
                    return Err(Error::provider("HTTP 503"));
                }
                Ok(json!({"id": id, "status": "RUNNING", "ssh": {"direct": {"host": "203.0.113.7", "port": port}}}))
            }
        }, &memo, now)
    };
    let endpoint = |p: &Pod| (p.ssh_ip.clone(), p.ssh_port);
    let up = |port: u16| (Some("203.0.113.7".to_string()), Some(port));

    assert_eq!(endpoint(&fill(vec![pod(Some(1000))], 5000).await[0]), up(2201));
    assert_eq!(gets.load(Ordering::SeqCst), 1);
    assert_eq!(endpoint(&fill(vec![pod(Some(1000))], 5000 + ENDPOINT_RECHECK_SECS - 1).await[0]), up(2201));
    assert_eq!(gets.load(Ordering::SeqCst), 1, "a refresh is no GET");

    port.store(2202, Ordering::SeqCst);
    assert_eq!(endpoint(&fill(vec![pod(Some(2000))], 5100).await[0]), up(2202), "a restart is a new lookup");
    assert_eq!(gets.load(Ordering::SeqCst), 2);
    port.store(2203, Ordering::SeqCst);
    assert_eq!(endpoint(&fill(vec![pod(Some(2000))], 5100 + ENDPOINT_RECHECK_SECS).await[0]), up(2203), "rechecked");
    assert_eq!(gets.load(Ordering::SeqCst), 3);

    fails.store(1, Ordering::SeqCst);
    let kept = fill(vec![pod(Some(2000))], 5100 + 3 * ENDPOINT_RECHECK_SECS).await;
    assert_eq!(endpoint(&kept[0]), up(2203), "a failed recheck keeps the endpoint of the same start");
    assert!(kept[0].status_detail.as_deref().is_some_and(|d| d.contains("503")), "{kept:?}");
    let restarted = fill(vec![pod(Some(3000))], 5100 + 3 * ENDPOINT_RECHECK_SECS).await;
    assert_eq!(endpoint(&restarted[0]), (None, None), "but never one from an earlier start");
    fails.store(0, Ordering::SeqCst);

    let before = gets.load(Ordering::SeqCst);
    fill(vec![pod(None)], 9000).await;
    fill(vec![pod(None)], 9000).await;
    assert_eq!(gets.load(Ordering::SeqCst), before + 2, "no start time: looked up every time");

    fill(vec![pod(Some(4000))], 9000).await;
    assert!(memo.lock().unwrap().by_id.contains_key("p1"));
    fill(vec![Pod { id: "other".into(), status: "EXITED".into(), ..Pod::default() }], 9000).await;
    assert!(memo.lock().unwrap().by_id.is_empty(), "a pod no longer listed is forgotten");
}

/// One pod's detail answers (its ports), in order.
type Answers = std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<Option<u16>>>>;

/// One pod's detail answers, in order, with the GETs counted — for the walks below.
fn scripted(ports: &[Option<u16>]) -> (Answers, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    (std::sync::Arc::new(std::sync::Mutex::new(ports.iter().copied().collect())), std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)))
}

/// The live restart (2026-10-27): the listing showed the new `startedAt` at once while the
/// detail kept the old port for ~30 s. A lookup within [`SETTLE_SECS`] of the start is looked
/// up again every [`UNSETTLED_RECHECK_SECS`], so the new port shows within one of those; once
/// a lookup comes after the settle time it's trusted for [`ENDPOINT_RECHECK_SECS`].
#[tokio::test]
async fn a_lookup_soon_after_a_start_is_checked_again_until_it_settles() {
    use std::sync::atomic::Ordering;
    let (answers, gets) = scripted(&[Some(23995), Some(23810), Some(23810), Some(23810), Some(23810), Some(23810), Some(23810), Some(23810)]);
    let memo = Mutex::new(EndpointMemo::default());
    let start = 10_000;
    let at = |now: u64| {
        let (answers, gets) = (answers.clone(), gets.clone());
        let memo = &memo;
        async move {
            let pod = Pod { id: "p1".into(), status: "RUNNING".into(), started_at: Some(start), ..Pod::default() };
            let got = fill_endpoints(vec![pod], move |id| {
                gets.fetch_add(1, Ordering::SeqCst);
                let port = answers.lock().unwrap().pop_front().flatten();
                async move { Ok(json!({"id": id, "status": "RUNNING", "ssh": {"direct": {"host": "64.119.209.250", "port": port}}})) }
            }, memo, now).await;
            got[0].ssh_port
        }
    };
    assert_eq!(at(start + 15).await, Some(23995), "the detail still had the old port");
    assert_eq!(at(start + 15 + UNSETTLED_RECHECK_SECS - 1).await, Some(23995));
    assert_eq!(gets.load(Ordering::SeqCst), 1);
    assert_eq!(at(start + 15 + UNSETTLED_RECHECK_SECS).await, Some(23810), "looked up again: the new port");
    assert_eq!(gets.load(Ordering::SeqCst), 2);
    // Rechecked every 30 s while the lookups are younger than the settle time...
    let mut now = start + 15 + UNSETTLED_RECHECK_SECS;
    while now - start < SETTLE_SECS {
        now += UNSETTLED_RECHECK_SECS;
        assert_eq!(at(now).await, Some(23810));
    }
    let settled_gets = gets.load(Ordering::SeqCst);
    // ...then the first lookup past it holds for the long recheck.
    assert_eq!(at(now + ENDPOINT_RECHECK_SECS - 1).await, Some(23810));
    assert_eq!(gets.load(Ordering::SeqCst), settled_gets, "settled: no GET");
    at(now + ENDPOINT_RECHECK_SECS).await;
    assert_eq!(gets.load(Ordering::SeqCst), settled_gets + 1);
}

/// Rechecks share [`RECHECKS_PER_MINUTE`] per process (a cohort's new pods would otherwise all
/// recheck every 30 s); a pod due beyond it keeps its endpoint until a later listing. A pod
/// with no endpoint at all is looked up regardless — and counts against the minute.
#[tokio::test]
async fn rechecks_are_capped_per_minute_but_a_pod_without_an_endpoint_never_waits() {
    use std::sync::{atomic::{AtomicUsize, Ordering}, Arc};
    let memo = Mutex::new(EndpointMemo::default());
    let now = 50_000;
    let pods: Vec<Pod> = (0..45).map(|i| Pod { id: format!("p{i}"), status: "RUNNING".into(), started_at: Some(1000), ..Pod::default() }).collect();
    {
        let mut m = memo.lock().unwrap();
        for p in &pods[..40] {
            m.by_id.insert(p.id.clone(), Remembered { started_at: 1000, ip: "203.0.113.9".into(), port: 2000, at: now - ENDPOINT_RECHECK_SECS });
        }
    }
    let gets = Arc::new(AtomicUsize::new(0));
    let fill = |now: u64| {
        let gets = gets.clone();
        fill_endpoints(pods.clone(), move |id| {
            gets.fetch_add(1, Ordering::SeqCst);
            async move { Ok(json!({"id": id, "status": "RUNNING", "ssh": {"direct": {"host": "203.0.113.9", "port": 2001}}})) }
        }, &memo, now)
    };
    let got = fill(now).await;
    assert_eq!(gets.load(Ordering::SeqCst), RECHECKS_PER_MINUTE, "5 without an endpoint + 25 rechecks");
    assert!(got.iter().all(|p| p.ssh_port.is_some()), "every pod has an endpoint");
    assert!(got[40..].iter().all(|p| p.ssh_port == Some(2001)), "the 5 without one were looked up");
    assert_eq!(got[..40].iter().filter(|p| p.ssh_port == Some(2000)).count(), 15, "15 rechecks wait, on what was remembered");
    fill(now + 59).await;
    assert_eq!(gets.load(Ordering::SeqCst), RECHECKS_PER_MINUTE, "the minute is used up");
    fill(now + 60).await;
    assert_eq!(gets.load(Ordering::SeqCst), RECHECKS_PER_MINUTE + 15, "the rest, a minute later");

    // Over the cap, a pod with no endpoint is still looked up at once.
    let mut more = pods.clone();
    more.push(Pod { id: "new".into(), status: "RUNNING".into(), started_at: Some(1000), ..Pod::default() });
    let before = gets.load(Ordering::SeqCst);
    let gets2 = gets.clone();
    let got = fill_endpoints(more, move |id| {
        gets2.fetch_add(1, Ordering::SeqCst);
        async move { Ok(json!({"id": id, "status": "RUNNING", "ssh": {"direct": {"host": "203.0.113.9", "port": 2002}}})) }
    }, &memo, now + 61).await;
    assert_eq!(gets.load(Ordering::SeqCst), before + 1);
    assert_eq!(got.last().unwrap().ssh_port, Some(2002));
}

/// A pod listed as not running, or whose detail has no endpoint, is forgotten: its next start
/// or endpoint is looked up, never the old one assumed.
#[tokio::test]
async fn a_pod_not_running_or_without_an_endpoint_is_forgotten() {
    let memo = Mutex::new(EndpointMemo::default());
    let remember = || memo.lock().unwrap().by_id.insert("p1".into(), Remembered { started_at: 1000, ip: "203.0.113.9".into(), port: 2000, at: 4000 });
    remember();
    let exited = Pod { id: "p1".into(), status: "EXITED".into(), started_at: Some(1000), ..Pod::default() };
    fill_endpoints(vec![exited], |_| async { unreachable!("an exited pod isn't looked up") }, &memo, 4001).await;
    assert!(memo.lock().unwrap().by_id.is_empty());

    remember();
    let running = Pod { id: "p1".into(), status: "RUNNING".into(), started_at: Some(1000), ..Pod::default() };
    let got = fill_endpoints(vec![running], |id| async move { Ok(json!({"id": id, "status": "RUNNING", "ssh": {"direct": null}})) }, &memo, 4000 + ENDPOINT_RECHECK_SECS).await;
    assert_eq!((got[0].ssh_ip.as_deref(), got[0].ssh_port), (None, None), "the detail's answer, not the old endpoint");
    assert!(memo.lock().unwrap().by_id.is_empty());
}

#[tokio::test]
async fn fills_only_running_missing_endpoints_and_bounds_concurrency() {
    use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
    let active = Arc::new(AtomicUsize::new(0));
    let max = Arc::new(AtomicUsize::new(0));
    let mut pods: Vec<Pod> = (0..19).map(|i| Pod { id: format!("p{i}"), status: "RUNNING".into(), ..Pod::default() }).collect();
    pods.push(Pod { id: "stopped".into(), status: "EXITED".into(), ..Pod::default() });
    let mut ready = sandbox_pod("ready"); ready.id = "ready".into(); pods.push(ready);
    let got = fill_endpoints(pods.clone(), |id| {
        let active = active.clone(); let max = max.clone();
        async move {
            assert!(id.starts_with('p'));
            let n = active.fetch_add(1, Ordering::SeqCst) + 1;
            max.fetch_max(n, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            active.fetch_sub(1, Ordering::SeqCst);
            Ok(json!({"id":id,"status":"RUNNING", "runtime":{"uptime":1200,"ports":[{"ip":"203.0.113.2","private":22,"public":1234,"type":"tcp"}]}}))
        }
    }, &Mutex::new(EndpointMemo::default()), 1_792_000_000).await;
    assert!(max.load(Ordering::SeqCst) <= 8);
    assert!(max.load(Ordering::SeqCst) > 1);
    assert_eq!(got.iter().map(|p| &p.id).collect::<Vec<_>>(), pods.iter().map(|p| &p.id).collect::<Vec<_>>());
    assert!(got[..19].iter().all(|p| p.ssh_port == Some(1234) && p.started_at.is_some()));
    assert_eq!(got[19..], pods[19..]);
}

#[test]
fn runtime_ports_are_a_direct_ssh_fallback_never_the_gateway_or_http_port() {
    let mut body = recorded("get_running").body;
    body["ssh"]["direct"] = Value::Null;
    assert_eq!(parse_pod(&body).ssh_port, Some(23924));
    body["runtime"]["ports"][0]["type"] = json!("http");
    assert!(parse_pod(&body).ssh_ip.is_none());
    body["runtime"]["ports"][0]["type"] = json!("tcp");
    body["runtime"]["ports"][0]["public"] = json!(70000);
    assert!(parse_pod(&body).ssh_ip.is_none());
}
