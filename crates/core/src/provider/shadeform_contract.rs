//! Contract tests: the Shadeform backend's pure readers and request builders against what
//! the API **really answered** (read-only GETs recorded 2026-10-14, scrubbed — `fixtures/
//! shadeform/`), and against the docs' shapes where nothing could be recorded yet (no instance
//! existed: `docs_shape/`, to be replaced by recordings from the first live test). When
//! Shadeform changes a shape, re-record and let these tests say what broke.
//!
//! A child module of `shadeform`, so it reaches the private builders/parsers directly.

use super::*;

/// Every fixture, by its path under `fixtures/shadeform/`. `every_fixture_is_listed_and_well_formed`
/// keeps this list and the directory in step, so a new recording can't sit there untested
/// (or unscrubbed: `fixtures_stay_scrubbed` reads every one).
const FIXTURES: &[(&str, &str)] = &[
    ("account_unfunded_2026-10-14_scrubbed.json", include_str!("fixtures/shadeform/account_unfunded_2026-10-14_scrubbed.json")),
    ("billing_empty.json", include_str!("fixtures/shadeform/billing_empty.json")),
    ("docs_shape/instance_info_active.json", include_str!("fixtures/shadeform/docs_shape/instance_info_active.json")),
    ("docs_shape/instance_info_pending.json", include_str!("fixtures/shadeform/docs_shape/instance_info_pending.json")),
    ("docs_shape/instances_list.json", include_str!("fixtures/shadeform/docs_shape/instances_list.json")),
    ("docs_shape/sshkeys_with_private_key.json", include_str!("fixtures/shadeform/docs_shape/sshkeys_with_private_key.json")),
    ("errors/400_instance_id_not_uuid.txt", include_str!("fixtures/shadeform/errors/400_instance_id_not_uuid.txt")),
    ("errors/400_user_endpoint_api_key.txt", include_str!("fixtures/shadeform/errors/400_user_endpoint_api_key.txt")),
    ("errors/401_bad_key.txt", include_str!("fixtures/shadeform/errors/401_bad_key.txt")),
    ("errors/401_no_key.txt", include_str!("fixtures/shadeform/errors/401_no_key.txt")),
    ("errors/404_instance_info.txt", include_str!("fixtures/shadeform/errors/404_instance_info.txt")),
    ("errors/404_unknown_route.txt", include_str!("fixtures/shadeform/errors/404_unknown_route.txt")),
    ("instances_empty.json", include_str!("fixtures/shadeform/instances_empty.json")),
    ("instances_types_2026-10-14_subset.json", include_str!("fixtures/shadeform/instances_types_2026-10-14_subset.json")),
    (
        "instances_types_available_sorted_2026-10-14_head.json",
        include_str!("fixtures/shadeform/instances_types_available_sorted_2026-10-14_head.json"),
    ),
    ("instances_types_no_match.json", include_str!("fixtures/shadeform/instances_types_no_match.json")),
    ("openapi_2026-10-14_excerpt.yaml", include_str!("fixtures/shadeform/openapi_2026-10-14_excerpt.yaml")),
    ("os_options_2026-10-14_distinct.json", include_str!("fixtures/shadeform/os_options_2026-10-14_distinct.json")),
    ("volumes_empty.json", include_str!("fixtures/shadeform/volumes_empty.json")),
    ("volumes_types_2026-10-14.json", include_str!("fixtures/shadeform/volumes_types_2026-10-14.json")),
];

/// What the synthetic `/sshkeys` fixture holds in place of the managed key's private half — so
/// a test can prove it never reaches an error, a `Debug` or a log line.
pub(super) const PRIVATE_MARKER: &str = "PRIVATE-KEY-MATERIAL-MUST-NEVER-BE-PRINTED";

pub(super) fn raw(name: &str) -> &'static str {
    FIXTURES.iter().find(|(n, _)| *n == name).unwrap_or_else(|| panic!("no fixture `{name}`")).1
}

pub(super) fn json_of(name: &str) -> Value {
    serde_json::from_str(raw(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// The recorded catalog subset's entries.
pub(super) fn subset() -> Vec<Value> {
    instance_types(&json_of("instances_types_2026-10-14_subset.json")).unwrap().clone()
}

pub(super) fn spec(gpu: &str, count: u32) -> PodSpec {
    PodSpec {
        name: "devtest-alpha".into(),
        image: String::new(),
        gpu_type: gpu.into(),
        gpu_count: count,
        cloud_type: "COMMUNITY".into(),
        disk_gb: 100,
        volume_gb: 0,
        ports: "22/tcp".into(),
        env: vec![("PUBLIC_KEY".into(), format!("{KEY_A}\n{KEY_B}"))],
        docker_args: None,
        allowed_cuda: Vec::new(),
        min_cuda: Some("13.0".into()),
        max_price: None,
        api_extra: None,
        avoid_machines: Vec::new(),
    }
}

pub(super) const KEY_A: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFakeCohortKeyForTestsOnly devtest";
pub(super) const KEY_B: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFakeDeployKeyForTestsOnly deploy@arena";
pub(super) const NONCE: &str = "3f0c2a8e-5b1d-4c7e-9a2f-0123456789ab";

pub(super) fn opts() -> ShadeformOpts {
    ShadeformOpts { ssh_key: "devtest".into(), prefix: "devtest".into(), ..ShadeformOpts::default() }
}

fn regions(e: &[Entry]) -> Vec<String> {
    e.iter().map(|e| format!("{}/{}", e.cloud, e.region)).collect()
}

#[test]
fn every_fixture_is_listed_and_well_formed() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/provider/fixtures/shadeform");
    let mut on_disk = Vec::new();
    for sub in ["", "docs_shape", "errors"] {
        for e in std::fs::read_dir(dir.join(sub)).unwrap() {
            let p = e.unwrap().path();
            if p.is_file() {
                let name = p.file_name().unwrap().to_string_lossy().into_owned();
                on_disk.push(if sub.is_empty() { name } else { format!("{sub}/{name}") });
            }
        }
    }
    on_disk.sort();
    let listed: Vec<String> = FIXTURES.iter().map(|(n, _)| n.to_string()).collect();
    assert_eq!(on_disk, listed, "fixtures/shadeform and FIXTURES must list the same files");
    for (name, text) in FIXTURES {
        if name.ends_with(".json") {
            json_of(name);
        } else {
            assert!(!text.trim().is_empty(), "{name}");
        }
    }
}

/// The repository is public: every fixture stays free of key material, credentials and
/// personal data — so a careless re-recording fails here, not on GitHub. No PEM armour at all,
/// no `private_key` with anything but the marker, no `@` outside `example.invalid`, no IPv4
/// outside the TEST-NETs (203.0.113.0/24, 198.51.100.0/24), no key name, no real SSH key blob.
#[test]
fn fixtures_stay_scrubbed() {
    for (name, text) in FIXTURES {
        for needle in ["BEGIN ", "-----", "SHADEFORM_API_KEY", "X-API-KEY", "AAAAB3NzaC1yc2E", "AAAAC3NzaC1lZDI1NTE5"] {
            assert!(!text.contains(needle), "{name} contains `{needle}`");
        }
        // `private_key` only ever with the marker.
        for (i, _) in text.match_indices("\"private_key\"") {
            let rest = text[i + "\"private_key\"".len()..].trim_start_matches([':', ' ']);
            assert!(rest.starts_with(&format!("\"{PRIVATE_MARKER}\"")), "{name}: a private_key value that isn't the marker");
        }
        for (i, _) in text.match_indices('@') {
            assert!(text[i..].starts_with("@example.invalid"), "{name}: an `@` outside example.invalid at {i}");
        }
        for ip in ipv4s(text) {
            assert!(ip.starts_with("203.0.113.") || ip.starts_with("198.51.100."), "{name}: IPv4 {ip} outside the TEST-NETs");
        }
    }
    // The account body: its personal fields are placeholders.
    let a = json_of("account_unfunded_2026-10-14_scrubbed.json");
    for k in ["organization", "phone_number", "tax_id", "address"] {
        assert_eq!(a[k], "<redacted>", "{k}");
    }
    assert_eq!(a["email"], "user@example.invalid");
}

/// Dotted quads in `text` (`a.b.c.d`, each 0–255, not inside a longer run of digits and dots).
fn ipv4s(text: &str) -> Vec<String> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() && (i == 0 || !(b[i - 1].is_ascii_digit() || b[i - 1] == b'.')) {
            let mut j = i;
            while j < b.len() && (b[j].is_ascii_digit() || b[j] == b'.') {
                j += 1;
            }
            let run = &text[i..j];
            let parts: Vec<&str> = run.split('.').collect();
            if parts.len() == 4 && parts.iter().all(|p| !p.is_empty() && p.len() <= 3 && p.parse::<u16>().is_ok_and(|n| n <= 255)) {
                out.push(run.to_string());
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

#[test]
fn the_ip_finder_finds_dotted_quads_only() {
    assert_eq!(ipv4s("ip 203.0.113.10, v 22.04_cuda13.0, x 1.2.3.4.5, y 10.0.0.1"), ["203.0.113.10", "10.0.0.1"]);
}

/// The recorded subset: 11 entries, every one read (none lost to a shape mismatch) — and the
/// keys `fitting_entries` reads are on each.
#[test]
fn the_recorded_catalog_parses_with_no_entry_lost() {
    let types = subset();
    assert_eq!(types.len(), 11);
    for t in &types {
        for k in ["cloud", "shade_instance_type", "hourly_price", "availability", "deployment_type", "boot_time"] {
            assert!(t.get(k).is_some(), "{k} missing: {t}");
        }
        assert!(t["configuration"]["os_options"].is_array() && t["configuration"]["gpu_type"].is_string());
    }
    // The sorted, server-filtered head: cheapest first, CPU types mixed in, all available.
    let head = json_of("instances_types_available_sorted_2026-10-14_head.json");
    let prices: Vec<u64> = instance_types(&head).unwrap().iter().map(|t| t["hourly_price"].as_u64().unwrap()).collect();
    assert!(prices.windows(2).all(|w| w[0] <= w[1]), "{prices:?}");
    assert!(instance_types(&head).unwrap().iter().any(|t| t["configuration"]["gpu_type"] == "CPU"));
    // An unknown gpu_type is a 200 with an empty list — which is why names are mapped first.
    assert!(instance_types(&json_of("instances_types_no_match.json")).unwrap().is_empty());
    // Drift fails closed.
    for body in [json!({}), json!({"instance_types": null}), json!([])] {
        assert!(instance_types(&body).unwrap_err().to_string().contains("unexpected response shape"), "{body}");
    }
}

/// `fitting_entries` on the recorded subset, the spec's matrix.
#[test]
fn recorded_catalog_fitting_matrix() {
    let types = subset();
    let o = opts();
    // A6000 at the CUDA-13 floor: Massed Compute's two in-stock regions, $0.57, its CUDA 13 image.
    let a6000 = fitting_entries(&types, &spec("NVIDIA RTX A6000", 1), &o, &[]).unwrap();
    assert_eq!(regions(&a6000), ["massedcompute/kansascity-usa-1", "massedcompute/desmoines-usa-1"]);
    assert!(a6000.iter().all(|e| e.os == "ubuntu22.04_cuda13.0_shade_os" && e.price_cents == 57 && e.shade_instance_type == "A6000"));
    assert_eq!(fmt_money("$", a6000[0].price()), "$0.57", "57 is cents");
    // The 2×A6000 at $1.14 is over the default SHADEFORM_MAX_PRICE ($1.00) — and in with a higher one.
    assert!(fitting_entries(&types, &spec("NVIDIA RTX A6000", 2), &o, &[]).unwrap().is_empty());
    let rich = ShadeformOpts { max_price: 2.0, ..opts() };
    let two = fitting_entries(&types, &spec("A6000", 2), &rich, &[]).unwrap();
    assert_eq!((two.len(), two[0].shade_instance_type.as_str(), two[0].price_cents), (2, "A6000x2", 114));
    // A cap of $0.50 (under the floor price) leaves nothing.
    let capped = PodSpec { max_price: Some(0.50), ..spec("NVIDIA RTX A6000", 1) };
    assert!(fitting_entries(&types, &capped, &o, &[]).unwrap().is_empty());
    // An avoided (cloud, region) — one that just failed — is skipped.
    let avoid = [("massedcompute".to_string(), "kansascity-usa-1".to_string())];
    assert_eq!(regions(&fitting_entries(&types, &spec("NVIDIA RTX A6000", 1), &o, &avoid).unwrap()), ["massedcompute/desmoines-usa-1"]);
    // A4000: no CUDA 13 image anywhere, and hyperstack's $0.15 is out of stock — none, even on any cloud.
    let any = ShadeformOpts { clouds: Clouds::All, ..opts() };
    assert!(fitting_entries(&types, &spec("NVIDIA RTX A4000", 1), &any, &[]).unwrap().is_empty());
    // Without the floor, on any cloud: paperspace's one in-stock region, its newest CUDA image.
    let no_floor = PodSpec { min_cuda: None, ..spec("NVIDIA RTX A4000", 1) };
    let a4000 = fitting_entries(&types, &no_floor, &any, &[]).unwrap();
    assert_eq!(regions(&a4000), ["paperspace/newyork-usa-1"]);
    assert_eq!((a4000[0].os.as_str(), a4000[0].price_cents), ("ubuntu22.04_cuda12.2_shade_os", 80));
    // …but only on an allowed cloud: the default allowlist is massedcompute alone.
    assert!(fitting_entries(&types, &no_floor, &o, &[]).unwrap().is_empty());
    // A region allowlist narrows it too.
    let only_dm = ShadeformOpts { regions: vec!["desmoines-usa-1".into()], ..opts() };
    assert_eq!(regions(&fitting_entries(&types, &spec("A6000", 1), &only_dm, &[]).unwrap()), ["massedcompute/desmoines-usa-1"]);
    // The A100 80G DGX type: in stock in one region at $1.38 — over the default cap, in under a higher one.
    assert!(fitting_entries(&types, &spec("NVIDIA A100-SXM4-80GB", 1), &o, &[]).unwrap().is_empty());
    let a100 = fitting_entries(&types, &spec("NVIDIA A100-SXM4-80GB", 1), &rich, &[]).unwrap();
    assert_eq!(regions(&a100), ["massedcompute/beltsville-usa-1"]);
}

/// Bare metal only when allowed; spot never (its own price is a dollar string, never read as
/// cents); a CPU type never for a GPU spec; a boot time over the limit skipped.
#[test]
fn recorded_catalog_kinds_spot_cpu_and_boot() {
    let mut types = subset();
    let wide = ShadeformOpts { clouds: Clouds::All, max_price: 100.0, ..opts() };
    let floorless = |gpu: &str, n: u32| PodSpec { min_cuda: None, ..spec(gpu, n) };
    // latitude's H100 is bare metal (out of stock as recorded): make its region available.
    let h100 = types.iter().position(|t| t["deployment_type"] == "baremetal").unwrap();
    types[h100]["availability"][0]["available"] = json!(true);
    assert!(fitting_entries(&types, &floorless("NVIDIA H100 80GB HBM3", 1), &wide, &[]).unwrap().is_empty(), "bare metal: not unless allowed");
    let metal = ShadeformOpts { allow_baremetal: true, ..wide.clone() };
    assert_eq!(regions(&fitting_entries(&types, &floorless("NVIDIA H100 80GB HBM3", 1), &metal, &[]).unwrap()), ["latitude/dallas-usa-1"]);
    // excesssupply's 8×L40: the spot entry is available, the on-demand one isn't → none.
    assert!(fitting_entries(&types, &floorless("NVIDIA L40", 8), &wide, &[]).unwrap().is_empty(), "spot is never used");
    // With the on-demand entry available: priced from `hourly_price` (cents), not the spot "4".
    let l40 = types.iter().position(|t| t["shade_instance_type"] == "L40x8_2").unwrap();
    types[l40]["availability"][0]["available"] = json!(true);
    let got = fitting_entries(&types, &floorless("NVIDIA L40", 8), &wide, &[]).unwrap();
    assert_eq!((got.len(), got[0].price_cents, fmt_money("$", got[0].price())), (1, 1100, "$11.00".to_string()));
    // A CPU type is never a GPU entry, and "CPU" isn't a GPU name to ask for.
    assert!(shadeform_gpu_type("CPU").is_err());
    for gpu in ["NVIDIA RTX A4000", "A6000", "NVIDIA A10"] {
        assert!(fitting_entries(&types, &floorless(gpu, 0), &wide, &[]).unwrap().iter().all(|e| e.gpu_type != "CPU"), "{gpu}");
    }
    // B300×8 boots in up to 1200 s: in at the default limit, out under a lower one (and it's
    // out of stock as recorded, so mark it available first).
    let b300 = types.iter().position(|t| t["shade_instance_type"] == "B300x8").unwrap();
    types[b300]["availability"][0]["available"] = json!(true);
    assert_eq!(fitting_entries(&types, &spec("NVIDIA B300", 8), &wide, &[]).unwrap().len(), 1);
    let quick = ShadeformOpts { max_boot_secs: 600, ..wide };
    assert!(fitting_entries(&types, &spec("NVIDIA B300", 8), &quick, &[]).unwrap().is_empty());
}

/// Every OS image in the full recording (15 distinct) parses, and the choice table holds.
#[test]
fn best_os_on_every_recorded_image() {
    let all = json_of("os_options_2026-10-14_distinct.json");
    let names: Vec<String> = all["os_options"].as_object().unwrap().keys().cloned().collect();
    assert_eq!(names.len(), 15);
    for o in &names {
        assert!(parse_os(o).is_some(), "{o}");
    }
    let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let f13 = Some((13, 0));
    let cases: &[(&[&str], Option<(u32, u32)>, Option<&str>)] = &[
        (&["ubuntu22.04_cuda12.8_shade_os", "ubuntu22.04_cuda12.2_shade_os", "ubuntu22.04"], f13, None),
        (&["ubuntu22.04_cuda12.8_shade_os", "ubuntu22.04_cuda12.2_shade_os", "ubuntu22.04"], Some((12, 0)), Some("ubuntu22.04_cuda12.8_shade_os")),
        (&["ubuntu22.04_cuda12.8_shade_os", "ubuntu22.04_cuda12.2_shade_os", "ubuntu22.04"], None, Some("ubuntu22.04_cuda12.8_shade_os")),
        (&["ubuntu22.04", "ubuntu20.04"], None, Some("ubuntu22.04")),
        (&["ubuntu22.04", "ubuntu20.04"], f13, None),
        (&["ubuntu22.04_cuda13.0_shade_os", "ubuntu24.04_cuda13.0_shade_os"], f13, Some("ubuntu24.04_cuda13.0_shade_os")),
        (&["ubuntu24.04_cuda12.8_shade_os", "ubuntu22.04_cuda13.0_shade_os"], Some((12, 8)), Some("ubuntu22.04_cuda13.0_shade_os")),
        (&["ubuntu22.04_cuda12.10_shade_os", "ubuntu22.04_cuda12.9_shade_os"], None, Some("ubuntu22.04_cuda12.10_shade_os")),
        (&["windows2022", "ubuntu22.04_cuda13.0_beta"], None, None),
        (&[], None, None),
    ];
    for (opts, floor, want) in cases {
        assert_eq!(best_os(&v(opts), *floor).as_deref(), *want, "{opts:?} {floor:?}");
    }
    assert_eq!(parse_os("ubuntu22.04_cuda13.0_shade_os"), Some(((22, 4), Some((13, 0)))));
    assert_eq!(parse_os("ubuntu24.04"), Some(((24, 4), None)));
    assert_eq!(parse_os("debian12"), None);
}

/// The recorded `text/plain` error bodies, through the real request path: classified by
/// status (401 → Auth; 404/400 → Other), never a JSON-decode error.
#[tokio::test]
async fn recorded_error_bodies_are_classified_by_status() {
    use crate::error::ProviderErrorKind as K;
    use crate::http::test_server::{canned, client, serve};
    let srv = serve(vec![
        canned(401, "text/plain", raw("errors/401_no_key.txt")),
        canned(401, "text/plain", raw("errors/401_bad_key.txt")),
        canned(404, "text/plain", raw("errors/404_instance_info.txt")),
        canned(404, "text/plain", raw("errors/404_unknown_route.txt")),
    ]);
    let p = super::tests::provider(&srv.base, client());
    for want in ["Either X-Api-Key or Authorization header must be set", "Unauthorized"] {
        let e = p.list_pods().await.unwrap_err();
        assert_eq!(e.kind(), Some(K::Auth), "{e}");
        assert!(e.to_string().contains("shadeform list HTTP 401") && e.to_string().contains(want), "{e}");
        assert!(!e.to_string().contains("decoding"), "{e}");
    }
    let e = p.pod_spec("1dc915dd-8899-4b4f-862a-f63fb2b3dc3d").await.unwrap_err();
    assert_eq!(e.kind(), Some(K::Other), "{e}");
    assert!(e.to_string().contains("HTTP 404") && e.to_string().contains("Not Found"), "{e}");
    assert_eq!(p.list_pods().await.unwrap_err().kind(), Some(K::Other));
    // The other recorded 400s, through the pure reader.
    for body in [raw("errors/400_instance_id_not_uuid.txt"), raw("errors/400_user_endpoint_api_key.txt")] {
        let e = status_error(StatusCode::BAD_REQUEST, body, "shadeform info");
        assert_eq!(e.kind(), Some(K::Other), "{e}");
    }
    // A 2xx create whose body isn't JSON may still have created an instance: unsure.
    assert!(matches!(create_outcome(StatusCode::OK, "OK").unwrap(), Created::Unsure(_)));
}

/// `GET /account` (undocumented): balance and burn read; the personal fields dropped at parse —
/// the parsed value's `Debug` names none of them.
#[test]
fn recorded_account_reads_only_the_money() {
    let text = raw("account_unfunded_2026-10-14_scrubbed.json");
    assert_eq!(parse_account(text).unwrap(), (0.0, 0.0));
    let a: AccountFields = serde_json::from_str(text).unwrap();
    let dbg = format!("{a:?}").to_lowercase();
    for word in ["email", "phone", "tax", "address", "organization", "example.invalid", "redacted"] {
        assert!(!dbg.contains(word), "{word} in {dbg}");
    }
    assert_eq!((a.instance_count, a.volume_count, a.tier.as_deref()), (Some(0), Some(0), Some("free")));
    // Drift: no numbers → an error in our own words (the body is never quoted).
    let e = parse_account(r#"{"email": "someone@example.invalid", "balance": "lots"}"#).unwrap_err();
    assert!(!e.contains("someone") && e.contains("isn't shown"), "{e}");
    assert!(parse_account("<html>").is_err());
    assert_eq!(parse_account(r#"{"balance": 12.5, "hourly_burn": "0.57"}"#).unwrap(), (12.5, 0.57));
    // The other empty account recordings parse as the empty lists they are.
    assert_eq!(json_of("billing_empty.json")["entries"], json!([]));
    assert!(instances_array(&json_of("instances_empty.json")).unwrap().is_empty());
    assert_eq!(json_of("volumes_empty.json")["volumes"], json!([]));
    assert!(json_of("volumes_types_2026-10-14.json")["volume_types"].as_array().unwrap().iter().all(|v| v["price_per_gb_per_hour"].is_string()));
}

/// The SSH-key list carries the managed key's private half: arena's type keeps id, name and
/// default only, and nothing it produces — `Debug`, a lookup's error — holds the private part.
#[test]
fn sshkeys_never_hold_the_private_key() {
    let text = raw("docs_shape/sshkeys_with_private_key.json");
    assert!(text.contains(PRIVATE_MARKER), "the fixture carries the marker");
    let keys = parse_ssh_keys(text).unwrap();
    assert_eq!(keys.len(), 2);
    assert!(!format!("{keys:?}").contains(PRIVATE_MARKER));
    assert!(!format!("{keys:?}").contains("FAKEPUBLICKEY"), "no public key either");
    assert_eq!(key_id_by_name(&keys, "devtest").unwrap(), "00000000-0000-4000-8000-00000000a002");
    assert!(keys.iter().any(|k| k.is_default && k.name == "Shadeform Managed Key"));
    let e = key_id_by_name(&keys, "arena9").unwrap_err().to_string();
    assert!(e.contains("no SSH key named `arena9`") && e.contains("`devtest`") && e.contains("POST /sshkeys/add"), "{e}");
    assert!(!e.contains(PRIVATE_MARKER) && !e.contains("FAKEPUBLICKEY"), "{e}");
    let twice = [keys[1].clone(), SshKey { id: "x".into(), ..keys[1].clone() }];
    assert!(key_id_by_name(&twice, "devtest").unwrap_err().to_string().contains("2 SSH keys are named `devtest`"));
    let e = parse_ssh_keys(&format!("{{\"private_key\": \"{PRIVATE_MARKER}\"}}")).unwrap_err().to_string();
    assert!(!e.contains(PRIVATE_MARKER), "{e}");
}

/// The docs' instance shapes: status words, endpoint only once active and past the boot grace,
/// `ssh_port: null` = 22, the price from `rental_terms` (dollars) over `hourly_price` (cents), the
/// GPU named the RunPod way, and the nonce tag as the pod's identity.
#[test]
fn docs_shape_instances_parse() {
    let active = json_of("docs_shape/instance_info_active.json");
    let at = crate::reap::parse_rfc3339("2026-01-28T20:58:02Z").unwrap() as u64;
    let p = parse_instance(&active, at + BOOT_GRACE);
    assert_eq!((p.id.as_str(), p.name.as_str(), p.provider.as_str(), p.status.as_str()), ("1dc915dd-8899-4b4f-862a-f63fb2b3dc3d", "devtest-alpha", "shadeform", "RUNNING"));
    assert_eq!((p.ssh_ip.as_deref(), p.ssh_port), (Some("203.0.113.10"), Some(22)));
    assert_eq!((p.gpu_type.as_deref(), p.gpu_count, p.cost_per_hr), (Some("NVIDIA RTX A6000"), Some(1), Some(0.57)));
    assert_eq!(p.identity.as_deref(), Some("3f0c2a8e-5b1d-4c7e-9a2f-000000000001"));
    assert_eq!(p.created_at, Some(crate::reap::parse_rfc3339("2026-01-28T20:53:39Z").unwrap() as u64));
    assert_eq!((p.machine_id, p.locked, p.delete_protected, p.proxy_only), (None, None, None, false));
    // Inside the grace: no endpoint yet (the bootstrap is installing root's keys).
    assert_eq!(parse_instance(&active, at + BOOT_GRACE - 1).ssh_ip, None);
    // Pending: no endpoint, the details kept, priced from cents.
    let pending = parse_instance(&json_of("docs_shape/instance_info_pending.json"), at + 3600);
    assert_eq!((pending.status.as_str(), pending.ssh_ip.as_deref(), pending.cost_per_hr), ("PROVISIONING", None, Some(0.57)));
    assert_eq!(pending.status_detail.as_deref(), Some("Instance created, waiting on provider to spin up instance"));
    // The list: a foreign instance (no tags, port 2222), an error, a deleting (preempted) one.
    let list = json_of("docs_shape/instances_list.json");
    let list = instances_array(&list).unwrap();
    let pods: Vec<Pod> = list.iter().map(|v| parse_instance(v, at + 3600)).collect();
    let by = |n: &str| pods.iter().find(|p| p.name == n).unwrap().clone();
    let james = by("james-gpu");
    assert_eq!((james.ssh_ip.as_deref(), james.ssh_port, james.identity), (Some("198.51.100.7"), Some(2222), None));
    assert!(!is_ours(list.iter().find(|v| v["name"] == "james-gpu").unwrap(), "devtest"), "foreign");
    assert!(list.iter().filter(|v| v["name"] != "james-gpu").all(|v| is_ours(v, "devtest")));
    let charlie = by("devtest-charlie");
    assert_eq!((charlie.status.as_str(), charlie.ssh_ip, charlie.status_detail.as_deref()), ("ERROR", None, Some("provider failed to launch")));
    let delta = by("devtest-delta");
    assert_eq!((delta.status.as_str(), delta.ssh_ip, delta.cost_per_hr), ("TERMINATED", None, Some(0.30)));
    assert!(!crate::status::is_billing(&delta.status) && crate::status::is_billing(&charlie.status));
}

/// The create body for a fixed spec is exactly the spec's (§10.2) — every field the recorded
/// OpenAPI's `CreateRequest.required` names among them — and the bootstrap it carries decodes
/// to the golden script.
#[test]
fn create_payload_is_the_golden_body() {
    let types = subset();
    let entry = fitting_entries(&types, &spec("NVIDIA RTX A6000", 1), &opts(), &[]).unwrap().remove(0);
    let body = create_payload(&opts(), &spec("NVIDIA RTX A6000", 1), &entry, NONCE, Some("00000000-0000-4000-8000-00000000a002")).unwrap();
    let script = render_bootstrap(&[KEY_A.into(), KEY_B.into()], NONCE, "devtest-alpha").unwrap();
    let golden = json!({
        "cloud": "massedcompute", "region": "kansascity-usa-1", "shade_instance_type": "A6000",
        "shade_cloud": true, "rental_type": "on_demand",
        "name": "devtest-alpha",
        "os": "ubuntu22.04_cuda13.0_shade_os",
        "ssh_key_id": "00000000-0000-4000-8000-00000000a002",
        "tags": ["arena", "arena-devtest", format!("arena-nonce-{NONCE}")],
        "launch_configuration": {"type": "script", "script_configuration": {"base64_script": crate::base64::encode(script.as_bytes())}},
    });
    assert_eq!(body, golden);
    let decoded = String::from_utf8(
        crate::base64::decode(body["launch_configuration"]["script_configuration"]["base64_script"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(decoded, GOLDEN_SCRIPT);
    for field in required_create_fields() {
        assert!(body.get(&field).is_some(), "CreateRequest.required `{field}` missing");
    }
    // The recorded status enum is exactly what map_status maps (nothing falls through).
    let statuses = yaml_list("Status", "enum");
    assert_eq!(statuses, ["creating", "pending_provider", "pending", "active", "error", "deleting", "deleted"]);
    for s in &statuses {
        assert!(["CREATING", "PROVISIONING", "PENDING", "RUNNING", "ERROR", "TERMINATED"].contains(&map_status(s).as_str()), "{s}");
    }
    // Two units in one API: `hourly_price` cents (an integer), `rental_terms.hourly_price` dollars (a string).
    let excerpt = raw("openapi_2026-10-14_excerpt.yaml");
    assert!(excerpt.contains("    HourlyPrice:\n      type: integer"), "cents");
    assert!(excerpt.contains("        hourly_price:\n          type: string"), "dollars");
}

/// The golden bootstrap for `KEY_A`, `KEY_B`, [`NONCE`], `devtest-alpha`.
const GOLDEN_SCRIPT: &str = r#"#!/bin/bash
# arena bootstrap for a Shadeform VM: authorize the cohort keys for root (and the shadeform user),
# record arena's identity nonce. Rendered by arena; public data only.
set -u
S=""; [ "$(id -u)" = 0 ] || S="sudo -n"
KEYS='ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFakeCohortKeyForTestsOnly devtest
ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFakeDeployKeyForTestsOnly deploy@arena'
# Each cohort key ends up as one plain (keys-only, no options) line, after whatever else is
# there: any line already holding one of these keys is dropped first — cloud-init's
# `disable_root` puts the instance's key in root's file behind `command="echo 'Please login as
# …';exit 142"`, and sshd uses the first line that matches. Rerunning gives the same file.
PAT=()
while read -r _ b _; do [ -n "${b:-}" ] && PAT+=(-e " $b"); done <<< "$KEYS"
for u in root shadeform; do
  h=$(getent passwd "$u" | cut -d: -f6) || continue
  [ -n "$h" ] || continue
  f="$h/.ssh/authorized_keys"
  $S install -d -m 700 -o "$u" -g "$u" "$h/.ssh"
  $S touch "$f"
  if [ -n "$KEYS" ]; then
    { $S cat "$f" | grep -vF "${PAT[@]}"; printf '%s\n' "$KEYS"; } | awk 'NF && !seen[$0]++' | $S tee "$f.arena-new" >/dev/null
    # Replaced only by a file that holds the keys: a failed rewrite never empties it.
    if $S grep -qF -- "${PAT[1]}" "$f.arena-new"; then $S mv "$f.arena-new" "$f"; else $S rm -f "$f.arena-new"; fi
  fi
  $S chown "$u:$u" "$f"; $S chmod 600 "$f"
done
$S install -d -m 755 /var/lib/arena
printf '%s\n' '3f0c2a8e-5b1d-4c7e-9a2f-0123456789ab' | $S tee /var/lib/arena/shadeform-nonce >/dev/null
printf '%s\n' 'devtest-alpha'  | $S tee /var/lib/arena/name >/dev/null
# Keys-only root login, in case the image forbids root.
case "$($S sshd -T 2>/dev/null | awk '/^permitrootlogin/{print $2}')" in
  yes|prohibit-password|without-password) ;;
  *) $S install -d -m 755 /etc/ssh/sshd_config.d
     printf 'PermitRootLogin prohibit-password\n' | $S tee /etc/ssh/sshd_config.d/10-arena-root.conf >/dev/null
     $S systemctl reload ssh 2>/dev/null || $S systemctl reload sshd 2>/dev/null ;;
esac
"#;

/// The recorded `CreateRequest.required` list.
fn required_create_fields() -> Vec<String> {
    let fields = yaml_list("CreateRequest", "required");
    assert_eq!(fields, ["cloud", "region", "shade_instance_type", "shade_cloud", "name"], "the recorded required set");
    fields
}

/// The `- item` lines under `<schema>:` → `<key>:` in the OpenAPI excerpt (a tiny reader for
/// its fixed layout — no YAML dependency for two lists).
fn yaml_list(schema: &str, key: &str) -> Vec<String> {
    let text = raw("openapi_2026-10-14_excerpt.yaml");
    let start = text.find(&format!("\n    {schema}:\n")).unwrap_or_else(|| panic!("no schema {schema}"));
    let body = &text[start + 1..];
    let k = body.find(&format!("\n      {key}:\n")).unwrap_or_else(|| panic!("no {key} in {schema}"));
    body[k..]
        .lines()
        .skip(2)
        .map_while(|l| l.strip_prefix("        - "))
        .map(|s| s.trim().to_string())
        .collect()
}
