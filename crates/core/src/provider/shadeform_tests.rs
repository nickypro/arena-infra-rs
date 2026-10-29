//! Unit and loopback tests for the Shadeform backend: the pure tables, then the real request
//! paths against `http::test_server` (nothing leaves the machine). Fixtures and helpers come
//! from the contract module.

use super::contract::{json_of, opts, raw, spec, KEY_A, NONCE, PRIVATE_MARKER};
use super::*;
use crate::error::ProviderErrorKind as K;
use crate::http::test_server::{canned, client, hang_up, serve, Canned};

const KEY: &str = "SFKEY_TEST_0123456789";
const ID: &str = "1dc915dd-8899-4b4f-862a-f63fb2b3dc3d";

fn fixed_nonce() -> String {
    NONCE.to_string()
}

/// A provider on the loopback server: the contract's options, a fixed nonce, a quick relist.
pub(super) fn provider(base: &str, client: Client) -> ShadeformProvider {
    ShadeformProvider {
        api_key: KEY.into(),
        client,
        base: base.to_string(),
        opts: opts(),
        relist: Relist { tries: 2, every: Duration::ZERO },
        make_nonce: fixed_nonce,
        price_reads: vec![Duration::ZERO; 5],
    }
}

fn json_reply(body: &str) -> Canned {
    canned(200, "application/json", body)
}

/// An instance as `info`/the list would show it: ours (named and tagged for `devtest`) unless
/// `tags` says otherwise.
fn instance(id: &str, name: &str, status: &str, cents: u64, tags: Value) -> Value {
    json!({
        "id": id, "cloud": "massedcompute", "region": "kansascity-usa-1", "shade_instance_type": "A6000",
        "name": name, "status": status, "status_details": "", "ip": null, "ssh_port": null,
        "configuration": {"gpu_type": "A6000", "num_gpus": 1, "storage_in_gb": 256, "interconnect": "pcie"},
        "hourly_price": cents, "created_at": "2026-10-14T13:00:00.123456Z", "tags": tags,
    })
}

fn our_tags() -> Value {
    json!(["arena", "arena-devtest", format!("arena-nonce-{NONCE}")])
}

/// The GETs every create makes before its POST: the key list, the name check, the catalog.
fn create_prelude() -> Vec<Canned> {
    vec![
        json_reply(raw("docs_shape/sshkeys_with_private_key.json")),
        json_reply(raw("instances_empty.json")),
        json_reply(raw("instances_types_2026-10-14_subset.json")),
    ]
}

fn requests(srv: &crate::http::test_server::Server) -> Vec<String> {
    srv.requests.lock().unwrap().clone()
}

fn posts(srv: &crate::http::test_server::Server) -> usize {
    requests(srv).iter().filter(|r| r.starts_with("POST ")).count()
}

// ---------------------------------------------------------------------------------------
// Pure tables
// ---------------------------------------------------------------------------------------

/// RunPod-style ids (what `--gpu`/`GPU_TYPE` resolve to) → Shadeform's names, and back.
#[test]
fn gpu_names_map_both_ways() {
    for (id, sf) in [
        ("NVIDIA RTX A4000", "A4000"),
        ("NVIDIA RTX A5000", "A5000"),
        ("NVIDIA RTX A6000", "A6000"),
        ("NVIDIA RTX 4000 Ada Generation", "RTX4000Ada"),
        ("NVIDIA RTX 6000 Ada Generation", "RTX6000Ada"),
        ("NVIDIA GeForce RTX 4090", "RTX4090"),
        ("NVIDIA GeForce RTX 5090", "RTX5090"),
        ("NVIDIA A10", "A10"),
        ("NVIDIA A30", "A30"),
        ("NVIDIA A40", "A40"),
        ("NVIDIA L4", "L4"),
        ("NVIDIA L40", "L40"),
        ("NVIDIA L40S", "L40S"),
        ("NVIDIA A100 80GB PCIe", "A100_80G"),
        ("NVIDIA A100-SXM4-80GB", "A100_80G"),
        ("NVIDIA A100-SXM4-40GB", "A100"),
        ("NVIDIA A100 PCIe", "A100"),
        ("NVIDIA H100 80GB HBM3", "H100"),
        ("NVIDIA H100 PCIe", "H100"),
        ("NVIDIA H100 NVL", "H100_nvl"),
        ("NVIDIA H200", "H200"),
        ("NVIDIA B200", "B200"),
        ("NVIDIA B300", "B300"),
        ("NVIDIA RTX PRO 6000 Blackwell Server Edition", "RTXPro6000"),
        ("NVIDIA RTX PRO 6000 Blackwell Workstation Edition", "RTXPro6000"),
        // Already Shadeform's spelling (any case), and the aliases `gpu::resolve` passes through.
        ("A6000", "A6000"),
        ("a100_80g", "A100_80G"),
        ("h100_NVL", "H100_nvl"),
    ] {
        assert_eq!(shadeform_gpu_type(id).unwrap(), sf, "{id}");
    }
    assert_eq!(shadeform_gpu_type(&crate::gpu::resolve("a6000")).unwrap(), "A6000");
    assert_eq!(shadeform_gpu_type(&crate::gpu::resolve("l40s")).unwrap(), "L40S");
    // Unmapped: an error naming the recorded types — never sent (the API answers a typo with
    // an empty catalog).
    for bad in ["NVIDIA GeForce RTX 3090", "A6OOO", "CPU", ""] {
        let e = shadeform_gpu_type(bad).unwrap_err();
        assert!(matches!(e, Error::Config(_)) && e.to_string().contains("A6000, B200"), "{bad}: {e}");
    }
    // Back: the RunPod id where there's one, the interconnect deciding the A100/H100 form.
    assert_eq!(runpod_gpu_id("A6000", None), Some("NVIDIA RTX A6000"));
    assert_eq!(runpod_gpu_id("A100_80G", Some("sxm4")), Some("NVIDIA A100-SXM4-80GB"));
    assert_eq!(runpod_gpu_id("A100_80G", Some("pcie")), Some("NVIDIA A100 80GB PCIe"));
    assert_eq!(runpod_gpu_id("H100", Some("SXM5")), Some("NVIDIA H100 80GB HBM3"));
    assert_eq!(runpod_gpu_id("GAUDI2", None), None);
    // Every mapped id round-trips to a Shadeform type that maps back to the same type.
    for (id, sf) in GPU_TABLE {
        let back = runpod_gpu_id(sf, Some(if id.contains("SXM") || id.contains("HBM") { "sxm" } else { "pcie" })).unwrap();
        assert_eq!(shadeform_gpu_type(back).unwrap(), *sf, "{id}");
    }
}

#[test]
fn status_words_map_onto_arenas() {
    for (sf, arena) in [
        ("creating", "CREATING"),
        ("pending_provider", "PROVISIONING"),
        ("pending", "PENDING"),
        ("active", "RUNNING"),
        ("error", "ERROR"),
        ("deleting", "TERMINATED"),
        ("deleted", "TERMINATED"),
        ("ACTIVE", "RUNNING"),
        ("hibernating", "HIBERNATING"),
    ] {
        assert_eq!(map_status(sf), arena, "{sf}");
    }
}

/// The endpoint: only `active` and past the grace; a null port is 22; a hostname `ip` is kept;
/// `rental_terms` (dollars) wins over `hourly_price` (cents); an odd `active_at` doesn't
/// withhold the endpoint forever.
#[test]
fn parse_instance_endpoint_and_price_rules() {
    let now = 2_000_000_000;
    let mut v = instance(ID, "devtest-alpha", "active", 57, our_tags());
    v["ip"] = json!("vm-12.example.invalid");
    v["active_at"] = json!(crate::snapshot::rfc3339(now - 61));
    let p = parse_instance(&v, now);
    assert_eq!((p.ssh_ip.as_deref(), p.ssh_port, p.cost_per_hr), (Some("vm-12.example.invalid"), Some(22), Some(0.57)));
    v["active_at"] = json!(crate::snapshot::rfc3339(now - 30));
    assert_eq!(parse_instance(&v, now).ssh_ip, None, "inside the boot grace");
    v["active_at"] = json!("not a time");
    assert!(parse_instance(&v, now).ssh_ip.is_some(), "an unreadable active_at doesn't hold it back forever");
    v["ssh_port"] = json!(2222);
    v["rental_terms"] = json!({"rental_type": "on_demand", "hourly_price": "0.55"});
    let p = parse_instance(&v, now);
    assert_eq!((p.ssh_port, p.cost_per_hr), (Some(2222), Some(0.55)));
    for s in ["pending", "creating", "error"] {
        v["status"] = json!(s);
        assert_eq!(parse_instance(&v, now).ssh_ip, None, "{s}");
    }
    // A CPU type has no GPU label; an unmapped GPU is said as Shadeform's.
    v["configuration"] = json!({"gpu_type": "CPU", "num_gpus": 0});
    assert_eq!((parse_instance(&v, now).gpu_type, parse_instance(&v, now).gpu_count), (None, None));
    v["configuration"] = json!({"gpu_type": "GAUDI2", "num_gpus": 8});
    assert_eq!(parse_instance(&v, now).gpu_type.as_deref(), Some("GAUDI2 (shadeform)"));
}

/// Ownership: name AND tag; never one without the other.
#[test]
fn ownership_needs_the_prefix_name_and_the_tag() {
    let ours = instance(ID, "devtest-alpha", "active", 57, our_tags());
    assert!(is_ours(&ours, "devtest"));
    assert!(!is_ours(&instance(ID, "devtest-alpha", "active", 57, json!([])), "devtest"), "no tag");
    assert!(!is_ours(&instance(ID, "james-gpu", "active", 57, our_tags()), "devtest"), "not our name");
    assert!(!is_ours(&instance(ID, "devtest-alpha", "active", 57, json!(["arena", "arena-arena9"])), "devtest"), "another cohort");
    assert!(!is_ours(&ours, "arena9"), "another prefix");
    assert_eq!(ownership_tags("Dev_Test", NONCE), ["arena", "arena-dev-test", &format!("arena-nonce-{NONCE}")]);
    let n = new_nonce();
    assert!(valid_id(&n) && n.as_bytes()[14] == b'4', "a UUID v4: {n}");
    assert_ne!(new_nonce(), n);
    for (id, ok) in [(ID, true), ("1DC915DD-8899-4B4F-862A-F63FB2B3DC3D", true), ("12345", false), ("", false), ("1dc915dd-8899-4b4f-862a-f63fb2b3dc3", false), ("1dc915dd/8899-4b4f-862a-f63fb2b3dc3d", false)] {
        assert_eq!(valid_id(id), ok, "{id}");
    }
}

/// Every row of the create-answer table.
#[test]
fn create_outcome_table() {
    let s = |n: u16| StatusCode::from_u16(n).unwrap();
    assert_eq!(create_outcome(s(200), &format!(r#"{{"id": "{ID}"}}"#)).unwrap(), Created::Id(ID.into()));
    for (status, body) in [(200, r#"{"ok": true}"#), (200, "OK"), (201, r#"{"id": "not-a-uuid"}"#), (500, "boom"), (502, "Bad Gateway"), (503, "")] {
        assert!(matches!(create_outcome(s(status), body).unwrap(), Created::Unsure(_)), "{status} {body}");
    }
    for (status, kind) in [(401, K::Auth), (403, K::Auth), (429, K::RateLimited)] {
        assert_eq!(create_outcome(s(status), "out of capacity, insufficient balance").unwrap_err().kind(), Some(kind), "{status}");
    }
    let wallet = create_outcome(s(400), "Insufficient balance in wallet").unwrap_err();
    assert_eq!(wallet.kind(), Some(K::Denied));
    assert!(wallet.to_string().contains("fund the Shadeform wallet"), "{wallet}");
    for body in ["Region kansascity-usa-1 is out of capacity", "no capacity for A6000", "instance type is currently unavailable in this region"] {
        assert_eq!(create_outcome(s(400), body).unwrap_err().kind(), Some(K::Capacity), "{body}");
    }
    let other = create_outcome(s(400), "Invalid os").unwrap_err();
    assert_eq!(other.kind(), Some(K::Other));
    assert!(!crate::retry::is_retryable(&other));
}

/// The bootstrap: keys checked one by one (no quote, newline or control character can get into
/// the script), the nonce a UUID, the name tame, the size capped — and the rendered keys reach
/// the script's `KEYS` exactly as given.
#[test]
fn render_bootstrap_checks_what_it_writes() {
    let ok = render_bootstrap(&[KEY_A.into()], NONCE, "devtest-alpha").unwrap();
    assert!(ok.contains(&format!("KEYS='{KEY_A}'")) && ok.contains(NONCE) && ok.contains("'devtest-alpha'"));
    for bad in [
        "ssh-ed25519 AAAA'; rm -rf / #",
        "ssh-ed25519 AAAA ok\nssh-ed25519 BBBB second",
        "ssh-dss AAAAB3NzaC1kc3M old",
        "ssh-ed25519",
        "ssh-ed25519 AAAA$(id) x",
        "-----BEGIN OPENSSH PRIVATE KEY-----",
        "ssh-ed25519 AAAA tab\there",
    ] {
        let e = render_bootstrap(&[KEY_A.into(), bad.into()], NONCE, "devtest-alpha").unwrap_err().to_string();
        assert!(e.contains("SSH key #2") && !e.contains("rm -rf"), "{bad:?}: {e}");
    }
    assert!(render_bootstrap(&[KEY_A.into()], "not-a-nonce", "devtest-alpha").is_err());
    assert!(render_bootstrap(&[KEY_A.into()], NONCE, "dev'test").is_err());
    assert!(render_bootstrap(&[KEY_A.into()], NONCE, "a b").is_err());
    let many: Vec<String> = (0..400).map(|i| format!("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI{i:08} key{i}")).collect();
    assert!(render_bootstrap(&many, NONCE, "devtest-alpha").unwrap_err().to_string().contains("cap 16384"));
    // A comment with shell syntax in it stays a literal: evaluate only the script's head (up to
    // the loop) and print what $KEYS holds.
    let odd = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOdd $(touch /nonexistent/x) `id` \\ \"q\" end";
    let script = render_bootstrap(&[KEY_A.into(), odd.into()], NONCE, "devtest-alpha").unwrap();
    let head = &script[..script.find("for u in").unwrap()];
    let out = std::process::Command::new("bash").arg("-c").arg(format!("{head}\nprintf '%s' \"$KEYS\"")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), format!("{KEY_A}\n{odd}"));
    let parse = std::process::Command::new("bash").args(["-n", "-c", &script]).output().unwrap();
    assert!(parse.status.success(), "{}", String::from_utf8_lossy(&parse.stderr));
}

/// The price guard: above the cap or 10 % over the quote → delete; at the edge, kept.
#[test]
fn price_guard_table() {
    assert_eq!(price_guard(0.57, 0.57, 1.0), None);
    assert_eq!(price_guard(0.62, 0.57, 1.0), None, "within 10 % of the quote");
    assert_eq!(price_guard(1.0, 1.0, 1.0), None, "at the cap");
    let why = price_guard(1.20, 0.57, 1.0).unwrap();
    assert!(why.contains("$1.20") && why.contains("$0.57"), "{why}");
    assert!(price_guard(0.64, 0.57, 1.0).is_some(), "over the quote by more than 10 %");
    assert!(price_guard(0.90, 0.90, 0.80).is_some(), "over the cap");
}

/// Every option is validated, and a bad one is a config error naming the key — a pasted
/// secret is never echoed back.
#[test]
fn options_parse_and_refuse_bad_values() {
    let o = |text: &str| ShadeformOpts::from_config(&Config::parse(text));
    let d = o("MACHINE_NAME_PREFIX=devtest").unwrap();
    assert_eq!(d.clouds, Clouds::Only(vec!["massedcompute".into()]));
    assert_eq!((d.max_price, d.max_boot_secs, d.ready_mins, d.allow_baremetal), (1.0, 1200, 15, false));
    assert_eq!((d.ssh_key.as_str(), d.prefix.as_str()), ("devtest", "devtest"), "the key named after the prefix");
    let set = o("SHADEFORM_CLOUDS=MassedCompute, hyperstack\nSHADEFORM_REGIONS=kansascity-usa-1\nSHADEFORM_MAX_PRICE=0.70\n\
                 SHADEFORM_SSH_KEY=devtest\nSHADEFORM_MAX_BOOT_SECS=600\nSHADEFORM_READY_MINS=20\nSHADEFORM_ALLOW_BAREMETAL=yes")
        .unwrap();
    assert_eq!(set.clouds, Clouds::Only(vec!["massedcompute".into(), "hyperstack".into()]));
    assert_eq!((set.regions.clone(), set.max_price, set.max_boot_secs, set.ready_mins, set.allow_baremetal), (vec!["kansascity-usa-1".to_string()], 0.70, 600, 20, true));
    assert_eq!(o("SHADEFORM_CLOUDS=*").unwrap().clouds, Clouds::All);
    for (text, key) in [
        ("SHADEFORM_MAX_PRICE=cheap", "SHADEFORM_MAX_PRICE"),
        ("SHADEFORM_MAX_PRICE=0", "SHADEFORM_MAX_PRICE"),
        ("SHADEFORM_MAX_PRICE=-1", "SHADEFORM_MAX_PRICE"),
        ("SHADEFORM_CLOUDS=mass compute", "SHADEFORM_CLOUDS"),
        ("SHADEFORM_CLOUDS=,", "SHADEFORM_CLOUDS"),
        ("SHADEFORM_REGIONS=us/east", "SHADEFORM_REGIONS"),
        ("SHADEFORM_READY_MINS=0", "SHADEFORM_READY_MINS"),
        ("SHADEFORM_READY_MINS=soon", "SHADEFORM_READY_MINS"),
        ("SHADEFORM_MAX_BOOT_SECS=99999999", "SHADEFORM_MAX_BOOT_SECS"),
        ("SHADEFORM_ALLOW_BAREMETAL=maybe", "SHADEFORM_ALLOW_BAREMETAL"),
    ] {
        let e = o(text).unwrap_err();
        assert!(matches!(e, Error::Config(_)) && e.to_string().contains(key), "{text}: {e}");
    }
    let e = o("SHADEFORM_MAX_PRICE=sf_live_SECRETVALUE1234567").unwrap_err().to_string();
    assert!(!e.contains("SECRETVALUE") && e.contains("chars, not shown"), "{e}");
    assert_eq!(d.cap(None), 1.0);
    assert_eq!(d.cap(Some(0.7)), 0.7);
    assert_eq!(d.cap(Some(5.0)), 1.0, "never above SHADEFORM_MAX_PRICE");
}

/// Building it: the key is required; the fleet fails on a bad option even when Shadeform
/// isn't the primary (gap 10) — instead of quietly dropping every Shadeform pod from `pods list`.
#[test]
fn build_and_build_fleet_validate_shadeform() {
    let p = crate::provider::build("shadeform", &Config::parse("SHADEFORM_API_KEY=k\nMACHINE_NAME_PREFIX=devtest")).unwrap();
    assert_eq!(p.name(), "shadeform");
    assert!(crate::provider::build("shadeform", &Config::parse("")).err().unwrap().to_string().contains("SHADEFORM_API_KEY"));
    let bad = Config::parse("RUNPOD_API_KEY=r\nSHADEFORM_API_KEY=k\nSHADEFORM_MAX_PRICE=cheap");
    let e = crate::provider::build_fleet("runpod", &bad, false).err().unwrap();
    assert!(e.to_string().contains("SHADEFORM_MAX_PRICE"), "{e}");
    let ok = Config::parse("RUNPOD_API_KEY=r\nSHADEFORM_API_KEY=k");
    assert!(crate::provider::build_fleet("runpod", &ok, false).is_ok());
    assert!(crate::provider::build_fleet("shadeform", &ok, false).is_ok());
    // Without its key Shadeform isn't in the fleet, and can't be the primary.
    assert!(crate::provider::build_fleet("shadeform", &Config::parse("RUNPOD_API_KEY=r"), false).is_err());
}

/// The trait answers: no stop or start (said up front), a ready deadline, restart gated as a
/// wipe (unverified), no lock, no key repair, the describe line.
#[tokio::test]
async fn trait_answers() {
    let p = provider("http://127.0.0.1:9", client());
    let pod = Pod { provider: "shadeform".into(), ..Pod::default() };
    for e in [p.stop_support(&pod).unwrap_err(), p.stop_pod(ID).await.unwrap_err(), p.start_pod(ID).await.unwrap_err()] {
        assert!(matches!(e, Error::NotImplemented(_)), "{e}");
        assert!(e.to_string().contains("bills until it's deleted") && e.to_string().contains("arena pods terminate"), "{e}");
    }
    assert_eq!(p.ready_deadline(&pod), Some(Duration::from_secs(15 * 60)));
    assert!(p.restart_wipes_container_disk(&pod));
    assert!(p.lock_support(&pod).is_err());
    assert!(p.authorize_ssh_keys(&pod, &[]).await.is_err());
    let d = p.describe(&spec("NVIDIA RTX A6000", 1));
    assert!(
        d.starts_with("1×A6000 VM via Shadeform (clouds: massedcompute; on-demand; CUDA ≥ 13.0 image; disk fixed by type; ≤ $1.00/h)"),
        "{d}"
    );
    assert!(d.contains("key `devtest`") && d.contains("no stop"), "{d}");
    let mut vol = spec("A6000", 1);
    vol.volume_gb = 20;
    assert!(p.describe(&vol).contains("VOLUME_GB > 0 is refused"));
}

/// `--api-json`: the passthrough fields merge, every managed one is refused by name; the
/// preview shows the bootstrap decoded beside the body.
#[test]
fn api_extra_and_preview() {
    let p = provider("http://127.0.0.1:9", client());
    let mut s = spec("NVIDIA RTX A6000", 1);
    s.api_extra = Some(serde_json::from_str(r#"{"auto_delete": {"date_threshold": "2026-10-14T15:00:00Z", "spend_threshold": "1.50"}, "alert": {"x": 1}}"#).unwrap());
    let body = p.preview_create_body(&s).unwrap();
    assert_eq!(body["auto_delete"]["spend_threshold"], "1.50");
    assert_eq!((body["name"].as_str(), body["shade_cloud"].as_bool(), body["rental_type"].as_str()), (Some("devtest-alpha"), Some(true), Some("on_demand")));
    assert_eq!(body["ssh_key_id"], "<id of `devtest`>");
    let script = body["(bootstrap script, decoded — not a field)"].as_str().unwrap();
    assert!(script.contains(KEY_A) && script.contains("/var/lib/arena/shadeform-nonce"), "{script}");
    for field in ["name", "cloud", "region", "shade_instance_type", "shade_cloud", "rental_type", "os", "ssh_key_id", "tags", "launch_configuration", "template_id", "volume_ids"] {
        let extra: Extra = serde_json::from_str(&format!(r#"{{"{field}": "x"}}"#)).unwrap();
        let e = p.check_create_extra(&extra).unwrap_err();
        assert!(matches!(e, Error::Config(_)) && e.to_string().contains(&format!("`{field}` is set by arena")), "{field}: {e}");
    }
    for ok in [r#"{"envs": [{"name": "A", "value": "1"}]}"#, r#"{"volume_mount": {"auto": true}}"#, r#"{"auto_delete": {"spend_threshold": "1"}}"#] {
        p.check_create_extra(&serde_json::from_str(ok).unwrap()).unwrap();
    }
}

// ---------------------------------------------------------------------------------------
// Loopback: the real request paths
// ---------------------------------------------------------------------------------------

/// List: 200 → pods (foreign ones too); a 200 without the array fails closed.
#[tokio::test]
async fn list_reads_every_instance_and_fails_closed_on_drift() {
    let srv = serve(vec![json_reply(raw("docs_shape/instances_list.json")), json_reply("{}")]);
    let p = provider(&srv.base, client());
    let names: Vec<String> = p.list_pods().await.unwrap().into_iter().map(|p| p.name).collect();
    assert_eq!(names, ["devtest-alpha", "devtest-bravo", "james-gpu", "devtest-charlie", "devtest-delta"]);
    let e = p.list_pods().await.unwrap_err().to_string();
    assert!(e.contains("unexpected response shape"), "{e}");
    assert_eq!(requests(&srv), ["GET /instances HTTP/1.1", "GET /instances HTTP/1.1"]);
}

/// The happy path, request by request: key list, name check, catalog (the mapped GPU, in stock,
/// cheapest first), one create on the cheapest entry, its `info`. The key travels in
/// `X-API-KEY` only — never `Authorization`, never the URL.
#[tokio::test]
async fn create_happy_path() {
    let new_id = "aaaaaaaa-0000-4000-8000-000000000001";
    let mut replies = create_prelude();
    replies.push(json_reply(&format!(r#"{{"id": "{new_id}"}}"#)));
    replies.push(json_reply(&instance(new_id, "devtest-alpha", "creating", 57, our_tags()).to_string()));
    let srv = serve(replies);
    let p = provider(&srv.base, client());
    let pod = p.create_pod(&spec("NVIDIA RTX A6000", 1)).await.unwrap();
    assert_eq!((pod.id.as_str(), pod.status.as_str(), pod.cost_per_hr, pod.identity.as_deref()), (new_id, "CREATING", Some(0.57), Some(NONCE)));
    assert_eq!(
        requests(&srv),
        [
            "GET /sshkeys HTTP/1.1".to_string(),
            "GET /instances HTTP/1.1".into(),
            "GET /instances/types?gpu_type=A6000&num_gpus=1&available=true&sort=price HTTP/1.1".into(),
            "POST /instances/create HTTP/1.1".into(),
            format!("GET /instances/{new_id}/info HTTP/1.1"),
        ]
    );
    for (i, head) in srv.headers.lock().unwrap().iter().enumerate() {
        assert!(head.iter().any(|h| h == &format!("x-api-key: {KEY}")), "request {i}: {head:?}");
        assert!(!head.iter().any(|h| h.to_ascii_lowercase().starts_with("authorization")), "request {i}: {head:?}");
    }
    assert!(requests(&srv).iter().all(|r| !r.contains(KEY)), "the key never goes in a URL");
    let body: Value = serde_json::from_str(&srv.bodies.lock().unwrap()[3]).unwrap();
    assert_eq!((body["region"].as_str(), body["ssh_key_id"].as_str()), (Some("kansascity-usa-1"), Some("00000000-0000-4000-8000-00000000a002")));
    assert_eq!(body["tags"], our_tags());
}

/// A capacity refusal moves to the next entry, the failed (cloud, region) avoided; when every
/// entry refuses, it's capacity (the next option) — two POSTs, never more than the entries.
#[tokio::test]
async fn create_on_capacity_tries_the_next_entry() {
    let new_id = "aaaaaaaa-0000-4000-8000-000000000002";
    let mut replies = create_prelude();
    replies.push(canned(400, "text/plain", "Region kansascity-usa-1 is out of capacity"));
    replies.push(json_reply(&format!(r#"{{"id": "{new_id}"}}"#)));
    replies.push(json_reply(&instance(new_id, "devtest-alpha", "creating", 57, our_tags()).to_string()));
    let srv = serve(replies);
    let pod = provider(&srv.base, client()).create_pod(&spec("NVIDIA RTX A6000", 1)).await.unwrap();
    assert_eq!(pod.id, new_id);
    let bodies = srv.bodies.lock().unwrap().clone();
    let region = |i: usize| serde_json::from_str::<Value>(&bodies[i]).unwrap()["region"].as_str().unwrap().to_string();
    assert_eq!((region(3), region(4)), ("kansascity-usa-1".into(), "desmoines-usa-1".into()));

    let mut replies = create_prelude();
    replies.push(canned(400, "text/plain", "no capacity"));
    replies.push(canned(400, "text/plain", "no capacity"));
    let srv = serve(replies);
    let e = provider(&srv.base, client()).create_pod(&spec("NVIDIA RTX A6000", 1)).await.unwrap_err();
    assert_eq!(e.kind(), Some(K::Capacity), "{e}");
    assert_eq!(posts(&srv), 2);
    // Nothing in stock that fits: capacity, no POST at all.
    let mut replies = create_prelude();
    replies[2] = json_reply(raw("instances_types_no_match.json"));
    let srv = serve(replies);
    let e = provider(&srv.base, client()).create_pod(&spec("NVIDIA RTX A6000", 1)).await.unwrap_err();
    assert!(e.kind() == Some(K::Capacity) && e.to_string().contains("no in-stock 1×A6000 VM on massedcompute"), "{e}");
    assert_eq!(posts(&srv), 0);
}

/// Ambiguous create (a 502, a hang-up after the request went out): the listing is read for the
/// nonce — found: adopted, exactly one POST; never found: the run stops with "may have been
/// created", still one POST, and the error is not retryable.
#[tokio::test]
async fn ambiguous_create_is_settled_by_the_nonce() {
    let adopted = "aaaaaaaa-0000-4000-8000-000000000003";
    for unsure in [canned(502, "text/html", "<html>Bad Gateway</html>"), hang_up()] {
        let mut replies = create_prelude();
        replies.push(unsure.clone());
        let listing = json!({"instances": [instance(adopted, "devtest-alpha", "creating", 57, our_tags())]});
        replies.push(json_reply(&listing.to_string()));
        let srv = serve(replies);
        let pod = provider(&srv.base, client()).create_pod(&spec("NVIDIA RTX A6000", 1)).await.unwrap();
        assert_eq!(pod.id, adopted);
        assert_eq!(posts(&srv), 1);

        let mut replies = create_prelude();
        replies.push(unsure);
        replies.push(json_reply(raw("instances_empty.json")));
        let other = json!({"instances": [instance(adopted, "devtest-alpha", "creating", 57, json!(["arena", "arena-devtest", "arena-nonce-someone-else"]))]});
        replies.push(json_reply(&other.to_string()));
        let srv = serve(replies);
        let e = provider(&srv.base, client()).create_pod(&spec("NVIDIA RTX A6000", 1)).await.unwrap_err();
        assert!(e.to_string().contains("may have been created anyway") && e.to_string().contains("devtest-alpha"), "{e}");
        assert_eq!(e.kind(), Some(K::Other));
        assert!(!crate::retry::is_retryable(&e));
        assert_eq!((posts(&srv), requests(&srv).len()), (1, 6), "two relists, one create");
    }
}

/// Through placement: an unsure create ends the run (never the next option, never a retry).
#[tokio::test]
async fn placement_stops_on_an_unsure_create() {
    use crate::placement::{place, plan_options, End, Order, PriceBook, Request, Rounds};
    let mut replies = create_prelude();
    replies.push(canned(503, "text/plain", "upstream"));
    replies.push(json_reply(raw("instances_empty.json")));
    replies.push(json_reply(raw("instances_empty.json")));
    let srv = serve(replies);
    let p = provider(&srv.base, client());
    let base = spec("NVIDIA RTX A6000", 1);
    let req = Request { gpus: vec!["NVIDIA RTX A6000".into(), "NVIDIA L40S".into()], clouds: vec!["COMMUNITY".into()], gpu_count: 1, max_price: None, order: Order::Listed, near: None };
    let plan = plan_options(&req, "shadeform", &PriceBook::unpriced());
    let rounds = Rounds { window: Duration::ZERO, every: Duration::from_secs(1) };
    let placed = place(&p, &base, &["devtest-alpha".to_string()], &plan, rounds, None, std::future::pending::<()>, &mut |_| {}).await;
    assert!(matches!(placed.end, End::Failed { .. }), "{:?}", placed.end);
    assert!(placed.created.is_empty());
    assert_eq!(posts(&srv), 1, "one create, no second option");
}

/// A transport failure: one that never connected is the plain (retryable) error; a timeout
/// after sending is unsure.
#[tokio::test]
async fn create_send_errors() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = closed.local_addr().unwrap();
    drop(closed);
    let e = client().post(format!("http://{addr}/instances/create")).send().await.unwrap_err();
    let plain = create_send_error(e).unwrap();
    assert!(matches!(plain, Error::Http(_)) && crate::retry::is_retryable(&plain), "{plain}");
    let srv = serve(vec![crate::http::test_server::stall()]);
    let e = client().post(format!("{}/instances/create", srv.base)).timeout(Duration::from_millis(100)).send().await.unwrap_err();
    assert!(create_send_error(e).unwrap_err().contains("no answer within"));
}

/// The price guard after a create: priced above the cap → deleted at once (`POST …/delete`),
/// the error says both prices.
#[tokio::test]
async fn price_guard_deletes_an_overpriced_instance() {
    let new_id = "aaaaaaaa-0000-4000-8000-000000000004";
    let mut replies = create_prelude();
    replies.push(json_reply(&format!(r#"{{"id": "{new_id}"}}"#)));
    replies.push(json_reply(&instance(new_id, "devtest-alpha", "creating", 120, our_tags()).to_string()));
    replies.push(json_reply(""));
    let srv = serve(replies);
    let e = provider(&srv.base, client()).create_pod(&spec("NVIDIA RTX A6000", 1)).await.unwrap_err().to_string();
    assert!(e.contains("Shadeform charged $1.20/h, quoted $0.57/h") && e.contains("deleted devtest-alpha"), "{e}");
    assert_eq!(requests(&srv).last().unwrap(), &format!("POST /instances/{new_id}/delete HTTP/1.1"));
}

/// Up-front refusals: no request at all.
#[tokio::test]
async fn create_refusals_before_any_request() {
    let srv = serve(vec![]);
    let p = provider(&srv.base, client());
    let mut vol = spec("NVIDIA RTX A6000", 1);
    vol.volume_gb = 10;
    let mut prefix = spec("NVIDIA RTX A6000", 1);
    prefix.name = "arena9-alpha".into();
    let mut nokeys = spec("NVIDIA RTX A6000", 1);
    nokeys.env.clear();
    let mut cuda = spec("NVIDIA RTX A6000", 1);
    cuda.min_cuda = Some("13.x".into());
    let mut managed = spec("NVIDIA RTX A6000", 1);
    managed.api_extra = Some(serde_json::from_str(r#"{"rental_type": "spot"}"#).unwrap());
    for (s, why) in [
        (vol, "VOLUME_GB=10"),
        (prefix, "doesn't start with `devtest-`"),
        (nokeys, "no SSH public key"),
        (cuda, "isn't a version"),
        (spec("NVIDIA GeForce RTX 3090", 1), "no Shadeform GPU type"),
        (managed, "`rental_type` is set by arena"),
    ] {
        let e = p.create_pod(&s).await.unwrap_err();
        assert!(matches!(e, Error::Config(_)) && e.to_string().contains(why), "{why}: {e}");
    }
    assert!(requests(&srv).is_empty());
}

/// The cohort key must be on the account; the managed key isn't silently used; the name must be
/// free on the account (ours or not) — each refused before any POST, and the `/sshkeys` body (it
/// holds a private key) is never quoted.
#[tokio::test]
async fn create_needs_the_key_and_a_free_name() {
    let srv = serve(vec![json_reply(raw("docs_shape/sshkeys_with_private_key.json"))]);
    let mut p = provider(&srv.base, client());
    p.opts.ssh_key = "arena9".into();
    let e = p.create_pod(&spec("NVIDIA RTX A6000", 1)).await.unwrap_err().to_string();
    assert!(e.contains("no SSH key named `arena9`") && !e.contains(PRIVATE_MARKER), "{e}");
    assert_eq!(posts(&srv), 0);
    let srv = serve(vec![canned(500, "text/plain", PRIVATE_MARKER)]);
    let e = provider(&srv.base, client()).create_pod(&spec("NVIDIA RTX A6000", 1)).await.unwrap_err().to_string();
    assert!(e.contains("shadeform sshkeys HTTP 500") && !e.contains(PRIVATE_MARKER), "{e}");

    let taken = json!({"instances": [instance(ID, "devtest-alpha", "active", 57, json!([]))]});
    let srv = serve(vec![json_reply(raw("docs_shape/sshkeys_with_private_key.json")), json_reply(&taken.to_string())]);
    let e = provider(&srv.base, client()).create_pod(&spec("NVIDIA RTX A6000", 1)).await.unwrap_err().to_string();
    assert!(e.contains("already on the Shadeform account"), "{e}");
    assert_eq!(posts(&srv), 0);
    // A deleting one doesn't hold the name.
    let gone = json!({"instances": [instance(ID, "devtest-alpha", "deleting", 57, our_tags())]});
    let mut replies = vec![json_reply(raw("docs_shape/sshkeys_with_private_key.json")), json_reply(&gone.to_string())];
    replies.push(json_reply(raw("instances_types_no_match.json")));
    let srv = serve(replies);
    assert_eq!(provider(&srv.base, client()).create_pod(&spec("NVIDIA RTX A6000", 1)).await.unwrap_err().kind(), Some(K::Capacity));
}

/// Terminate / rename / restart: a fresh `info` first; an instance arena didn't create (no
/// tags, another name) is refused with **no POST**; ours gets exactly the call — delete with no
/// body, rename with `{"name"}` only.
#[tokio::test]
async fn mutations_are_guarded_and_shaped() {
    let foreign = instance(ID, "james-gpu", "active", 57, json!([]));
    let srv = serve(vec![json_reply(&foreign.to_string()), json_reply(&foreign.to_string()), json_reply(&foreign.to_string())]);
    let p = provider(&srv.base, client());
    for e in [p.terminate_pod(ID).await.unwrap_err(), p.rename_pod(ID, "devtest-x").await.unwrap_err(), p.restart_pod(ID).await.unwrap_err()] {
        assert!(e.to_string().contains("`james-gpu` on Shadeform wasn't created by arena for prefix `devtest`"), "{e}");
    }
    assert_eq!(posts(&srv), 0);
    assert_eq!(requests(&srv).len(), 3);

    let ours = instance(ID, "devtest-alpha", "active", 57, our_tags()).to_string();
    let srv = serve(vec![json_reply(&ours), json_reply(""), json_reply(&ours), json_reply(""), json_reply(&ours), json_reply("")]);
    let p = provider(&srv.base, client());
    p.terminate_pod(ID).await.unwrap();
    p.rename_pod(ID, "devtest-bravo").await.unwrap();
    p.restart_pod(ID).await.unwrap();
    let r = requests(&srv);
    assert_eq!(
        [&r[1], &r[3], &r[5]],
        [&format!("POST /instances/{ID}/delete HTTP/1.1"), &format!("POST /instances/{ID}/update HTTP/1.1"), &format!("POST /instances/{ID}/restart HTTP/1.1")]
    );
    let bodies = srv.bodies.lock().unwrap().clone();
    assert_eq!(bodies[1], "", "delete sends no body");
    assert_eq!(serde_json::from_str::<Value>(&bodies[3]).unwrap(), json!({"name": "devtest-bravo"}));
    // A non-UUID id (another provider's) is never sent.
    for e in [p.terminate_pod("12345").await.unwrap_err(), p.rename_pod("", "x").await.unwrap_err(), p.pod_spec("12345").await.unwrap_err()] {
        assert!(matches!(e, Error::Config(_)) && e.to_string().contains("nothing sent"), "{e}");
    }
    assert_eq!(requests(&srv).len(), 6);
}

/// Plan-time prices: one catalog read for every option; an option with nothing in stock is said
/// and unpriced, an unmapped GPU too — and `plan_options` then orders and caps them as on Vast.
#[tokio::test]
async fn quotes_price_the_options_from_one_catalog_read() {
    use crate::placement::{plan_options, Order, Request};
    let srv = serve(vec![json_reply(raw("instances_types_2026-10-14_subset.json"))]);
    let p = provider(&srv.base, client());
    let gpus = vec!["NVIDIA RTX A6000".to_string(), "NVIDIA L40S".to_string(), "NVIDIA GeForce RTX 3090".to_string()];
    let quotes = p.quote(&gpus, &spec("A6000", 1), Duration::from_secs(5)).await;
    assert_eq!(requests(&srv), [format!("GET {AVAILABLE_PATH} HTTP/1.1")]);
    assert_eq!(quotes[0].1.as_ref().unwrap().as_ref().unwrap().price_cents, 57);
    assert_eq!(quotes[1].1, Ok(None));
    assert!(quotes[2].1.as_ref().unwrap_err().contains("no Shadeform GPU type"));
    let (book, notes) = price_book(&quotes, 1, p.opts());
    assert!(notes[0].contains("cheapest in-stock VM right now on massedcompute") && notes[0].contains("bills until terminated"), "{notes:?}");
    assert!(notes.iter().any(|n| n.contains("no in-stock 1×L40S on massedcompute right now — unpriced (no stock)")), "{notes:?}");
    assert!(notes.iter().any(|n| n.contains("massedcompute/kansascity-usa-1/A6000 at $0.57")), "{notes:?}");
    let req = Request { gpus: gpus[..2].to_vec(), clouds: vec!["COMMUNITY".into()], gpu_count: 1, max_price: Some(0.9), order: Order::Cheapest, near: None };
    let plan = plan_options(&req, "shadeform", &book);
    assert_eq!(plan.options.len(), 1);
    assert_eq!((plan.options[0].label.as_str(), plan.options[0].price_per_pod, plan.options[0].cloud.as_deref()), ("1×RTX A6000", Some(0.57), None));
    assert_eq!(plan.dropped.len(), 1, "the unpriced L40S can't be checked against --max-price");
    // A failed catalog read: every option says why.
    let srv = serve(vec![canned(503, "text/plain", "busy")]);
    let quotes = provider(&srv.base, client()).quote(&gpus[..1], &spec("A6000", 1), Duration::from_secs(5)).await;
    assert!(quotes[0].1.as_ref().unwrap_err().contains("HTTP 503"));
}

/// `pod_spec`: the GPU (the RunPod way), count, disk and current price, from `info`.
#[tokio::test]
async fn pod_spec_recovers_the_gpu_and_price() {
    let srv = serve(vec![json_reply(raw("docs_shape/instance_info_active.json"))]);
    let s = provider(&srv.base, client()).pod_spec(ID).await.unwrap();
    assert_eq!((s.gpu_type.as_str(), s.gpu_count, s.disk_gb, s.max_price, s.volume_gb), ("NVIDIA RTX A6000", 1, 256, Some(0.57), 0));
    assert_eq!(json_of("docs_shape/instance_info_active.json")["id"], ID);
}

/// The fleet routes a Shadeform pod's calls to Shadeform by its provider tag; a UUID never
/// reaches another backend, and `terminate_pod_on("shadeform", …)` only Shadeform.
#[tokio::test]
async fn the_fleet_routes_shadeform_pods_to_shadeform() {
    let ours = instance(ID, "devtest-alpha", "active", 57, our_tags()).to_string();
    let srv = serve(vec![json_reply(&format!(r#"{{"instances": [{ours}]}}"#)), json_reply(&ours), json_reply("")]);
    let fleet = crate::provider::multi::tests_support::fleet_of(vec![Box::new(provider(&srv.base, client()))]);
    let listed = fleet.list_by_provider().await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].0, "shadeform");
    let pod = listed[0].1.as_ref().unwrap()[0].clone();
    assert!(fleet.stop_support(&pod).is_err());
    assert_eq!(fleet.ready_deadline(&pod), Some(Duration::from_secs(900)));
    fleet.terminate_pod_on("shadeform", ID).await.unwrap();
    assert!(fleet.terminate_pod_on("vast", ID).await.is_err());
    assert_eq!(requests(&srv).last().unwrap(), &format!("POST /instances/{ID}/delete HTTP/1.1"));
}

// ---------------------------------------------------------------------------------------
// Adversarial review (branch `review`): each test fails on `shadeform` for the reason its
// doc comment gives.
// ---------------------------------------------------------------------------------------

/// REVIEW sf SF4: the post-create price guard fails open. `settle` reads `info` once; when that
/// one read fails (here a 503 right after the create answered with an id), the pod is built
/// from the catalog entry — priced at the *quote* — and nothing checks the real price again:
/// an instance Shadeform bills at $1.20/h, over the $1.00 cap (and 2× the $0.57 quote), stays
/// up, shown in `pods list`/the footer at $0.57. (Likewise an `info` whose price can't be read
/// — no `rental_terms`, a non-integer `hourly_price` — skips the guard.) A failed read must
/// not end the guard: re-read the instance (`info` again, or the listing) before accepting it.
/// The canned re-read below answers both shapes (an instance carrying an `instances` array).
#[tokio::test]
async fn review_sf_sf4_price_guard_survives_a_failed_info_read() {
    let new_id = "aaaaaaaa-0000-4000-8000-000000000005";
    let over = instance(new_id, "devtest-alpha", "creating", 120, our_tags());
    let mut both = over.clone();
    both["instances"] = json!([over]);
    let mut replies = create_prelude();
    replies.push(json_reply(&format!(r#"{{"id": "{new_id}"}}"#)));
    replies.push(canned(503, "text/plain", "upstream connect error"));
    replies.push(json_reply(&both.to_string()));
    replies.push(json_reply(""));
    let srv = serve(replies);
    let r = provider(&srv.base, client()).create_pod(&spec("NVIDIA RTX A6000", 1)).await;
    let reqs = requests(&srv);
    assert!(
        r.is_err() && reqs.iter().any(|l| l == &format!("POST /instances/{new_id}/delete HTTP/1.1")),
        "an instance billed at $1.20/h (cap $1.00) was kept after one failed `info` read — the price guard never ran: \
         {:?}; requests {reqs:?}",
        r.map(|p| p.cost_per_hr)
    );
}

/// REVIEW sf SF5: `rename_pod` checks that the instance is ours, not that the NEW name keeps it
/// ours. `pods rename` accepts any MACHINE_NAME_LIST entry, absolute ones (`@staff-box` →
/// `staff-box`) included; renamed so, the instance no longer starts with `<prefix>-`, so
/// [`is_ours`] fails from then on — every later terminate / restart / rename by arena (`up`'s
/// replace, teardown's printed fix, the rename back) is refused while it keeps billing, and
/// teardown files it under "not this cohort's". Create refuses such a name up front; rename
/// must too — before any POST.
#[tokio::test]
async fn review_sf_sf5_rename_refuses_a_name_that_would_disown_the_instance() {
    let ours = instance(ID, "devtest-alpha", "active", 57, our_tags()).to_string();
    let srv = serve(vec![json_reply(&ours), json_reply("")]);
    let p = provider(&srv.base, client());
    let r = p.rename_pod(ID, "staff-box").await;
    assert!(
        r.is_err() && posts(&srv) == 0,
        "renamed to `staff-box` (no `devtest-` prefix): arena can't terminate it afterwards — {r:?}, {} POST(s)",
        posts(&srv)
    );
    // After such a rename, arena's own terminate is refused (what the operator then meets).
    let disowned = instance(ID, "staff-box", "active", 57, our_tags()).to_string();
    let srv = serve(vec![json_reply(&disowned)]);
    assert!(provider(&srv.base, client()).terminate_pod(ID).await.is_err());
}

/// REVIEW sf SF6: the bootstrap only *appends* the cohort keys to root's `authorized_keys` (then
/// `sort -u`s it). An Ubuntu cloud image's cloud-init (`disable_root: true`, its default) has
/// already put the instance's key — `ssh_key_id`, which is the cohort key arena uploads as
/// `devtest` — into root's file behind `no-port-forwarding,…,command="echo 'Please login as the
/// user \"shadeform\" rather than the user \"root\".';…;exit 142"`. sshd uses the FIRST line
/// whose key matches, and `sort -u` keeps that restricted line first (`no-…` < `ssh-…`; a plain
/// append would leave it first too): root login with the cohort key runs the echo and exits
/// 142 — arena (which connects as root) is locked out, setup fails, the VM bills. The script
/// must leave each cohort key's first match in root's file without options (drop a restricted
/// line for a key it authorizes, or put its own lines first and not re-sort them below).
/// Run for real under bash with `id`/`getent`/`sudo` stubbed onto a temp root.
#[cfg(target_os = "linux")]
#[test]
fn review_sf_sf6_bootstrap_root_login_survives_cloud_inits_disable_root_line() {
    let base = std::env::temp_dir().join(format!("arena-sf-review-boot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (stubs, fake) = (base.join("stubs"), base.join("fake"));
    for d in [stubs.clone(), fake.join("root/.ssh"), fake.join("home/shadeform/.ssh")] {
        std::fs::create_dir_all(d).unwrap();
    }
    let stub = |name: &str, body: &str| {
        let p = stubs.join(name);
        crate::remote::write_test_script(&p, format!("#!/bin/bash\n{body}\n")).unwrap();
    };
    // Not root: the script goes through `sudo -n`, which the stub runs as-is on the fake root
    // (paths outside the homes mapped under it; ownership changes dropped).
    stub("id", "echo 1000");
    stub(
        "getent",
        r#"case "$2" in root) echo "root:x:0:0:root:$FAKE/root:/bin/bash";; shadeform) echo "shadeform:x:1000:1000::$FAKE/home/shadeform:/bin/bash";; *) exit 2;; esac"#,
    );
    stub(
        "sudo",
        r#"[ "$1" = -n ] && shift
cmd=$1; shift; args=()
for a in "$@"; do a=${a//\/var\/lib\/arena/$FAKE/var/lib/arena}; a=${a//\/etc\/ssh/$FAKE/etc/ssh}; args+=("$a"); done
case "$cmd" in
  chown) exit 0;;
  sshd) echo "permitrootlogin prohibit-password"; exit 0;;
  systemctl) exit 0;;
  install) out=(); skip=0; for a in "${args[@]}"; do if [ $skip = 1 ]; then skip=0; continue; fi; case "$a" in -o|-g) skip=1;; *) out+=("$a");; esac; done; exec install "${out[@]}";;
  *) exec "$cmd" "${args[@]}";;
esac"#,
    );
    let blob = KEY_A.split(' ').nth(1).unwrap();
    let restricted = format!(
        "no-port-forwarding,no-agent-forwarding,no-X11-forwarding,command=\"echo 'Please login as the user \\\"shadeform\\\" \
         rather than the user \\\"root\\\".';echo;sleep 10;exit 142\" {KEY_A}\n"
    );
    std::fs::write(fake.join("root/.ssh/authorized_keys"), &restricted).unwrap();
    let script = render_bootstrap(&[KEY_A.into()], NONCE, "devtest-alpha").unwrap();
    let path = format!("{}:/usr/bin:/bin", stubs.display());
    let out = std::process::Command::new("bash")
        .arg("-c")
        .arg(&script)
        .env_clear()
        .env("PATH", path)
        .env("FAKE", &fake)
        .output()
        .unwrap();
    let keys = std::fs::read_to_string(fake.join("root/.ssh/authorized_keys")).unwrap();
    let nonce = std::fs::read_to_string(fake.join("var/lib/arena/shadeform-nonce")).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&base);
    assert!(out.status.success() && nonce.trim() == NONCE, "the harness ran the script: {}", String::from_utf8_lossy(&out.stderr));
    // sshd: the first line whose key matches is the one used, options and all.
    let first = keys.lines().find(|l| l.contains(blob)).unwrap();
    assert!(
        first.starts_with("ssh-ed25519 "),
        "root's first entry for the cohort key is cloud-init's lock-out line — root login runs its `command=` and exits \
         142:\n{keys}"
    );
}

// ---------------------------------------------------------------------------------------
// Review fixes (SF4–SF7): the fixes' own regression tests, beside the review's.
// ---------------------------------------------------------------------------------------

/// SF4: a price that can't be read after the create — `info` failing every time and the listing
/// too — is no price to accept: the instance is deleted and the next entry tried (here it
/// succeeds), the first one's region avoided. If that delete fails, the run stops saying it
/// may still be running — never a silent keep.
#[tokio::test]
async fn sf4_an_unreadable_price_deletes_the_instance_and_tries_the_next_entry() {
    let (first, second) = ("aaaaaaaa-0000-4000-8000-000000000006", "aaaaaaaa-0000-4000-8000-000000000007");
    let mut replies = create_prelude();
    replies.push(json_reply(&format!(r#"{{"id": "{first}"}}"#)));
    replies.push(canned(503, "text/plain", "busy"));
    replies.push(canned(503, "text/plain", "busy"));
    replies.push(canned(503, "text/plain", "busy"));
    replies.push(json_reply(""));
    replies.push(json_reply(&format!(r#"{{"id": "{second}"}}"#)));
    replies.push(json_reply(&instance(second, "devtest-alpha", "creating", 57, our_tags()).to_string()));
    let srv = serve(replies);
    let mut p = provider(&srv.base, client());
    p.price_reads = vec![Duration::ZERO];
    let pod = p.create_pod(&spec("NVIDIA RTX A6000", 1)).await.unwrap();
    assert_eq!(pod.id, second);
    let r = requests(&srv);
    assert_eq!(r[3..8], [
        "POST /instances/create HTTP/1.1".to_string(),
        format!("GET /instances/{first}/info HTTP/1.1"),
        format!("GET /instances/{first}/info HTTP/1.1"),
        "GET /instances HTTP/1.1".into(),
        format!("POST /instances/{first}/delete HTTP/1.1"),
    ]);
    let bodies = srv.bodies.lock().unwrap().clone();
    let region = |i: usize| serde_json::from_str::<Value>(&bodies[i]).unwrap()["region"].as_str().unwrap().to_string();
    assert_eq!((region(3), region(8)), ("kansascity-usa-1".into(), "desmoines-usa-1".into()), "the failed region is avoided");

    // The listing has it but without a price (no rental_terms, no integer hourly_price): the same.
    let mut unpriced = instance(first, "devtest-alpha", "creating", 0, our_tags());
    unpriced["hourly_price"] = json!("n/a");
    let mut replies = create_prelude();
    replies.push(json_reply(&format!(r#"{{"id": "{first}"}}"#)));
    replies.push(json_reply(&unpriced.to_string()));
    replies.push(json_reply(&unpriced.to_string()));
    replies.push(json_reply(&json!({"instances": [unpriced]}).to_string()));
    replies.push(canned(500, "text/plain", "boom"));
    let srv = serve(replies);
    let mut p = provider(&srv.base, client());
    p.price_reads = vec![Duration::ZERO];
    let e = p.create_pod(&spec("NVIDIA RTX A6000", 1)).await.unwrap_err();
    let msg = e.to_string();
    assert!(msg.contains("couldn't be read") && msg.contains("deleting it FAILED") && msg.contains("may still be running"), "{msg}");
    assert_eq!(e.kind(), Some(K::Other), "the run stops: nothing more is created");
    assert_eq!(posts(&srv), 2, "one create, one delete attempt");
    // The real backoff: ~30 s in all.
    assert_eq!(PRICE_READS.iter().sum::<Duration>(), Duration::from_secs(30));
}

/// SF5: a rename that keeps the prefix still works; one that drops it (or isn't a name) is
/// refused before any request.
#[tokio::test]
async fn sf5_rename_keeps_the_prefix_or_is_refused_unsent() {
    let srv = serve(vec![]);
    let p = provider(&srv.base, client());
    for bad in ["staff-box", "devtest", "devtest-", "devtestx-alpha", "devtest-al pha", "devtest-a'b"] {
        let e = p.rename_pod(ID, bad).await.unwrap_err();
        assert!(matches!(e, Error::Config(_)) && e.to_string().contains("only manages `devtest-…` names"), "{bad}: {e}");
    }
    assert!(requests(&srv).is_empty());
}

/// SF6: run the rendered bootstrap for real (bash, `sudo`/`id`/`getent` stubbed onto a temp
/// root). A cohort key cloud-init left behind a `command=` lock-out in root's file ends up as one
/// plain line; unrelated lines stay; a rerun gives the same file; the `shadeform` user gets the
/// plain lines too.
#[cfg(target_os = "linux")]
#[test]
fn sf6_bootstrap_rewrites_restricted_cohort_lines_and_is_idempotent() {
    let base = std::env::temp_dir().join(format!("arena-sf6-boot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (stubs, fake) = (base.join("stubs"), base.join("fake"));
    for d in [stubs.clone(), fake.join("root/.ssh"), fake.join("home/shadeform/.ssh")] {
        std::fs::create_dir_all(d).unwrap();
    }
    let stub = |name: &str, body: &str| {
        let p = stubs.join(name);
        crate::remote::write_test_script(&p, format!("#!/bin/bash\n{body}\n")).unwrap();
    };
    stub("id", "echo 1000");
    stub(
        "getent",
        r#"case "$2" in root) echo "root:x:0:0:root:$FAKE/root:/bin/bash";; shadeform) echo "shadeform:x:1000:1000::$FAKE/home/shadeform:/bin/bash";; *) exit 2;; esac"#,
    );
    stub(
        "sudo",
        r#"[ "$1" = -n ] && shift
cmd=$1; shift; args=()
for a in "$@"; do a=${a//\/var\/lib\/arena/$FAKE/var/lib/arena}; a=${a//\/etc\/ssh/$FAKE/etc/ssh}; args+=("$a"); done
case "$cmd" in
  chown|systemctl) exit 0;;
  sshd) echo "permitrootlogin prohibit-password"; exit 0;;
  install) out=(); skip=0; for a in "${args[@]}"; do if [ $skip = 1 ]; then skip=0; continue; fi; case "$a" in -o|-g) skip=1;; *) out+=("$a");; esac; done; exec install "${out[@]}";;
  *) exec "$cmd" "${args[@]}";;
esac"#,
    );
    let other = "ssh-rsa AAAAB3OtherKeyNotTheCohorts other@laptop";
    let restricted = format!("no-port-forwarding,command=\"echo 'Please login as the user \\\"shadeform\\\".';exit 142\" {KEY_A}");
    std::fs::write(fake.join("root/.ssh/authorized_keys"), format!("{other}\n{restricted}\n")).unwrap();
    std::fs::write(fake.join("home/shadeform/.ssh/authorized_keys"), format!("{KEY_A}\n")).unwrap();
    let key_b = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFakeDeployKeyForTestsOnly deploy@arena";
    let script = render_bootstrap(&[KEY_A.into(), key_b.into()], NONCE, "devtest-alpha").unwrap();
    let run = || {
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(&script)
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", stubs.display()))
            .env("FAKE", &fake)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        std::fs::read_to_string(fake.join("root/.ssh/authorized_keys")).unwrap()
    };
    let once = run();
    let twice = run();
    let shadeform = std::fs::read_to_string(fake.join("home/shadeform/.ssh/authorized_keys")).unwrap();
    let leftovers = std::fs::read_dir(fake.join("root/.ssh")).unwrap().count();
    let _ = std::fs::remove_dir_all(&base);
    assert_eq!(once, format!("{other}\n{KEY_A}\n{key_b}\n"), "the lock-out line is gone, the other key kept, ours plain");
    assert_eq!(twice, once, "a rerun gives the same file");
    assert_eq!(shadeform, format!("{KEY_A}\n{key_b}\n"));
    assert_eq!(leftovers, 1, "no temp file left behind");
}

/// SF7: the account's volumes, read over the loopback (`GET /volumes`, then `/volumes/types` for
/// the price): sized, priced per (cloud, region) at the recorded rate; a body without the array
/// fails closed; a failed price read only leaves them unpriced.
#[tokio::test]
async fn sf7_volumes_are_listed_and_priced() {
    let vols = json!({"volumes": [
        {"id": "v-1", "cloud": "crusoe", "region": "culpeper-usa-1", "name": "data", "size_in_gb": 100, "cost_estimate": "0.5"},
        {"id": "v-2", "cloud": "massedcompute", "region": "nowhere-1", "name": "odd", "size_in_gb": 10},
    ]});
    let srv = serve(vec![json_reply(&vols.to_string()), json_reply(raw("volumes_types_2026-10-14.json"))]);
    let got = provider(&srv.base, client()).list_volumes().await.unwrap();
    assert_eq!(requests(&srv), ["GET /volumes HTTP/1.1", "GET /volumes/types HTTP/1.1"]);
    assert_eq!((got[0].id.as_str(), got[0].size_gb, got[0].usd_per_gb_hour), ("v-1", Some(100), Some(0.00012)));
    assert!((got[0].usd_per_month().unwrap() - 100.0 * 0.00012 * 730.0).abs() < 1e-9);
    assert_eq!((got[1].usd_per_gb_hour, got[1].usd_per_month()), (None, None), "no listed rate there");
    let srv = serve(vec![json_reply(&vols.to_string()), canned(503, "text/plain", "busy")]);
    assert!(provider(&srv.base, client()).list_volumes().await.unwrap().iter().all(|v| v.usd_per_gb_hour.is_none()));
    let srv = serve(vec![json_reply("{}")]);
    assert!(provider(&srv.base, client()).list_volumes().await.unwrap_err().to_string().contains("no volumes array"));
    assert!(parse_volumes(&json_of("volumes_empty.json"), None).unwrap().is_empty());
}
