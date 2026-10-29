//! `arena update`'s pieces: pure readers (versions, the release JSON, the changelog, checksums)
//! table-tested, and the network/disk steps over a loopback server and a temp dir.

use super::*;
use crate::http::test_server::{canned, client as test_client, serve};
use serde_json::json;

const T: &str = "x86_64-unknown-linux-gnu";

#[test]
fn versions_compare_part_by_part_and_tags_parse() {
    let v = |s: &str| Version::parse(s);
    assert_eq!(v("2.0.261027.1130"), Some(Version([2, 0, 261027, 1130])));
    assert_eq!(v("v2.0.261027.0905"), v("2.0.261027.905"));
    assert_eq!(v("v2.0.261027.0905").unwrap().to_string(), "2.0.261027.0905");
    assert!(v("2.0.261027.1130") < v("2.0.261027.1131"));
    assert!(v("2.0.261027.2359") < v("2.0.261028.0000"));
    assert!(v("2.0.261231.0000") < v("2.1.000101.0000"));
    for bad in ["", "2.0.261027", "2.0.261027.1130.1", "2.0.x.1130", "latest", "v"] {
        assert_eq!(v(bad), None, "{bad:?}");
    }
}

fn release_json(base: &str, names: &[&str]) -> Value {
    json!({"tag_name": "v2.0.261101.0900", "name": "arena 2.0.261101.0900",
           "assets": names.iter().map(|n| json!({"name": n, "size": 3, "browser_download_url": format!("{base}/dl/{n}")})).collect::<Vec<_>>()})
}

#[test]
fn a_release_is_its_tag_and_assets() {
    let r = parse_release(&release_json("https://x", &["arena-x", "SHA256SUMS"])).unwrap();
    assert_eq!((r.tag.as_str(), r.version), ("v2.0.261101.0900", Version([2, 0, 261101, 900])));
    assert_eq!(r.asset("SHA256SUMS").unwrap().url, "https://x/dl/SHA256SUMS");
    let e = r.asset("arena-tui-x").unwrap_err();
    assert!(e.contains("has no `arena-tui-x`") && e.contains("arena-x, SHA256SUMS"), "{e}");
    assert!(parse_release(&json!({"tag_name": "nightly", "assets": []})).unwrap_err().contains("isn't a version"));
    assert!(parse_release(&json!({"assets": []})).is_err());
    assert!(parse_release(&json!({"tag_name": "v2.0.261101.0900"})).is_err());
}

const OLD_LOG: &str = "# Changelog\n\nPreamble.\n\n## 2026-10-26 · Hetzner: nearest location first (#8)\n- creates go nearest first\n";
const NEW_LOG: &str = "# Changelog\n\nPreamble.\n\n## 2026-10-28 · arena update (#10)\n- `arena update` installs the newest release\n\n## 2026-10-27 · RunPod pods show their SSH address again (#9)\n- the dashboard and backups reach RunPod pods again\n### Action needed\n- restart long-running `arena-tui` dashboards\n### Config\n- `RUNPOD_API` is no longer needed\n- nothing else\n\n## 2026-10-26 · Hetzner: nearest location first (#8)\n- creates go nearest first\n";

#[test]
fn the_changelog_reads_as_entries_with_their_action_and_config_lines() {
    let e = entries(NEW_LOG);
    assert_eq!(e.iter().map(|e| e.heading.as_str()).collect::<Vec<_>>(), [
        "2026-10-28 · arena update (#10)",
        "2026-10-27 · RunPod pods show their SSH address again (#9)",
        "2026-10-26 · Hetzner: nearest location first (#8)",
    ]);
    assert_eq!(e[1].action, ["restart long-running `arena-tui` dashboards"]);
    assert_eq!(e[1].config, ["`RUNPOD_API` is no longer needed", "nothing else"]);
    assert!(e[0].action.is_empty() && e[0].config.is_empty());
    assert_eq!(e[0].lines, ["- `arena update` installs the newest release"], "no trailing blank line");
    assert!(entries("# Changelog\n\njust a preamble\n").is_empty());
}

#[test]
fn an_update_brings_the_entries_this_build_lacks_action_needed_first() {
    let new = new_since(OLD_LOG, NEW_LOG);
    assert_eq!(new.len(), 2);
    let said = describe_changes(&new);
    let action = said.find("Action needed:").unwrap();
    let config = said.find("Config changes:").unwrap();
    let first_entry = said.find("## 2026-10-28").unwrap();
    assert!(action < config && config < first_entry, "{said}");
    assert!(said.contains("  ⚠ restart long-running `arena-tui` dashboards"), "{said}");
    assert!(!said.contains("nearest location"), "an entry this build has isn't repeated: {said}");
    assert!(new_since(NEW_LOG, NEW_LOG).is_empty());
    assert!(describe_changes(&[]).contains("no entry this build lacks"));
}

#[test]
fn checksums_are_read_in_sha256sums_format_and_checked() {
    let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    assert_eq!(sha256_hex(b"abc"), abc);
    let sums = parse_sums(&format!("{abc}  arena-x\n{}  *arena-tui-x\nnot a line\n{} short\n", "A".repeat(64), "b".repeat(10)));
    assert_eq!(sums.len(), 2);
    assert_eq!(sums["arena-tui-x"], "a".repeat(64), "binary marker and case");
    assert!(verify(&sums, "arena-x", b"abc").is_ok());
    let e = verify(&sums, "arena-x", b"abd").unwrap_err();
    assert!(e.contains("doesn't match SHA256SUMS") && e.contains("not installed"), "{e}");
    assert!(verify(&sums, "arena-y", b"abc").unwrap_err().contains("no line for arena-y"));
}

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("arena-update-{tag}-{}-{:?}", std::process::id(), std::thread::current().id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The swap keeps the running binary as `.prev` (replacing an older `.prev`), takes the old
/// one's mode, and leaves no staged file behind; a first install is just the new file.
#[test]
fn install_keeps_the_previous_binary_and_its_mode() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir("install");
    std::fs::write(dir.join("arena"), "old").unwrap();
    std::fs::set_permissions(dir.join("arena"), std::fs::Permissions::from_mode(0o775)).unwrap();
    std::fs::write(dir.join("arena.prev"), "older").unwrap();
    let staged = stage(&dir, "arena", b"new").unwrap();
    install(&dir, "arena", &staged).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("arena")).unwrap(), "new");
    assert_eq!(std::fs::read_to_string(dir.join("arena.prev")).unwrap(), "old");
    assert_eq!(std::fs::metadata(dir.join("arena")).unwrap().permissions().mode() & 0o777, 0o775);
    let staged = stage(&dir, "arena-tui", b"tui").unwrap();
    install(&dir, "arena-tui", &staged).unwrap();
    assert_eq!(std::fs::metadata(dir.join("arena-tui")).unwrap().permissions().mode() & 0o777, 0o755);
    assert!(!dir.join("arena-tui.prev").exists());
    let left: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|d| d.file_name().to_string_lossy().into_owned()).collect();
    assert!(left.iter().all(|n| !n.contains(".new-")), "{left:?}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Dashboards on a replaced binary: `exe` reads `<path> (deleted)` — only those.
#[test]
fn processes_on_a_replaced_binary_are_found_by_their_deleted_exe() {
    let proc = temp_dir("proc");
    let exe = Path::new("/opt/arena/arena-tui");
    for (pid, link) in [("41", "/opt/arena/arena-tui (deleted)"), ("7", "/opt/arena/arena-tui (deleted)"), ("50", "/opt/arena/arena-tui"), ("60", "/usr/bin/vim (deleted)")] {
        std::fs::create_dir_all(proc.join(pid)).unwrap();
        std::os::unix::fs::symlink(link, proc.join(pid).join("exe")).unwrap();
    }
    std::fs::create_dir_all(proc.join("self")).unwrap();
    std::fs::create_dir_all(proc.join("99")).unwrap(); // no exe (a kernel thread, or not ours)
    let found: Vec<u32> = stale_processes(&proc, exe).into_iter().map(|(p, _)| p).collect();
    assert_eq!(found, [7, 41]);
    assert!(stale_processes(&proc, exe).iter().all(|(_, t)| t.is_some()));
    assert!(stale_processes(Path::new("/nonexistent-proc"), exe).is_empty());
    std::fs::remove_dir_all(&proc).unwrap();
}

/// `check` over the real request path: up to date (no changelog read), or newer with what it
/// brings; a release without a changelog still updates.
#[tokio::test]
async fn check_reads_the_latest_release_and_what_it_brings() {
    let srv = serve(vec![canned(200, "application/json", &release_json("http://unused", &["CHANGELOG.md"]).to_string())]);
    let up_to_date = check(&test_client(), &srv.base, Version([2, 0, 261101, 900]), OLD_LOG).await.unwrap();
    assert!(!up_to_date.newer);
    assert_eq!(*srv.requests.lock().unwrap(), ["GET /releases/latest HTTP/1.1"]);

    let srv = serve(vec![canned(200, "text/plain", NEW_LOG)]);
    let base = srv.base.clone();
    let api = serve(vec![canned(200, "application/json", &release_json(&base, &["CHANGELOG.md"]).to_string())]);
    let c = check(&test_client(), &api.base, Version([2, 0, 261026, 1200]), OLD_LOG).await.unwrap();
    assert!(c.newer && c.changes.contains("Action needed:") && c.changes.contains("## 2026-10-27"), "{}", c.changes);
    assert_eq!(c.changelog_sha256.as_deref(), Some(sha256_hex(NEW_LOG.as_bytes()).as_str()), "kept for the attestation's check");
    assert_eq!(*srv.requests.lock().unwrap(), ["GET /dl/CHANGELOG.md HTTP/1.1"]);

    let escaped = NEW_LOG.replace("restart long-running", "\x1b[2Jrestart long-running");
    let srv = serve(vec![canned(200, "text/plain", &escaped)]);
    let api = serve(vec![canned(200, "application/json", &release_json(&srv.base, &["CHANGELOG.md"]).to_string())]);
    let c = check(&test_client(), &api.base, Version([2, 0, 261026, 1200]), OLD_LOG).await.unwrap();
    assert!(c.changes.contains("⚠ ?[2Jrestart long-running") && !c.changes.contains('\x1b'), "{}", c.changes);

    let api = serve(vec![canned(200, "application/json", &release_json("http://unused", &[]).to_string())]);
    let c = check(&test_client(), &api.base, Version([2, 0, 261026, 1200]), OLD_LOG).await.unwrap();
    assert!(c.newer && c.changes.contains("no CHANGELOG.md"));

    let api = serve(vec![canned(403, "application/json", r#"{"message":"API rate limit exceeded"}"#)]);
    let e = check(&test_client(), &api.base, Version([2, 0, 261026, 1200]), OLD_LOG).await.unwrap_err();
    assert!(e.contains("HTTP 403") && e.contains("rate limit"), "{e}");
    let api = serve(vec![canned(404, "application/json", r#"{"message":"Not Found"}"#)]);
    let e = check(&test_client(), &api.base, Version([2, 0, 261026, 1200]), OLD_LOG).await.unwrap_err();
    assert!(e.starts_with("no release is published yet"), "{e}");
}

/// A fake `arena` for [`check_runs`]: a script that prints `arena <version> (sha)`.
fn fake_arena(version: &str) -> String {
    format!("#!/bin/sh\necho 'arena {version} (0123456789)'\n")
}

/// The new `arena` is run even while something still has it open for writing for a moment
/// (`ETXTBSY`): here a handle this test drops after 200 ms.
#[test]
fn a_new_binary_still_open_for_writing_is_run_once_it_closes() {
    let dir = temp_dir("busy");
    let staged = stage(&dir, "arena", fake_arena("2.0.261101.0900").as_bytes()).unwrap();
    let held = std::fs::OpenOptions::new().append(true).open(&staged).unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(200));
        drop(held);
    });
    assert_eq!(check_runs(&staged, Version([2, 0, 261101, 900])), Ok(()));
    release.join().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Serve SHA256SUMS + both binaries (in `fetch`'s order) with these bodies and sums.
fn release_server(tui: &str, arena: &str, sums_for: (&str, &str)) -> (crate::http::test_server::Server, Release) {
    let sums = format!("{}  arena-tui-{T}\n{}  arena-{T}\n", sha256_hex(sums_for.0.as_bytes()), sha256_hex(sums_for.1.as_bytes()));
    let srv = serve(vec![canned(200, "text/plain", &sums), canned(200, "application/octet-stream", tui), canned(200, "application/octet-stream", arena)]);
    let names = [format!("arena-tui-{T}"), format!("arena-{T}"), "SHA256SUMS".into(), "CHANGELOG.md".into()];
    let release = parse_release(&release_json(&srv.base, &names.iter().map(String::as_str).collect::<Vec<_>>())).unwrap();
    (srv, release)
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().map(|d| d.file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

/// `--skip-attestation`'s pins for these bodies: the fake binaries can't be attested, so the
/// tests of the rest of `fetch` pin their digests, as the escape hatch does.
fn pinned(bodies: &[&str]) -> Provenance<'static> {
    Provenance::Pinned(bodies.iter().map(|b| sha256_hex(b.as_bytes())).collect())
}

/// Download, check against SHA256SUMS, stage, run the new `arena --version`, then install:
/// both in place, the old ones as `.prev`.
#[tokio::test]
async fn an_update_installs_both_binaries_and_keeps_the_old_ones() {
    let dir = temp_dir("fetch-ok");
    std::fs::write(dir.join("arena"), "old arena").unwrap();
    std::fs::write(dir.join("arena-tui"), "old tui").unwrap();
    let arena = fake_arena("2.0.261101.0900");
    let (srv, release) = release_server("new tui", &arena, ("new tui", &arena));
    let staged = fetch(&test_client(), &release, T, &dir, &pinned(&["new tui", &arena])).await.unwrap();
    assert_eq!(staged.proof, None);
    assert_eq!(std::fs::read_to_string(dir.join("arena")).unwrap(), "old arena", "nothing in place before install");
    staged.install().unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("arena")).unwrap(), arena);
    assert_eq!(std::fs::read_to_string(dir.join("arena-tui")).unwrap(), "new tui");
    assert_eq!(std::fs::read_to_string(dir.join("arena.prev")).unwrap(), "old arena");
    assert_eq!(std::fs::read_to_string(dir.join("arena-tui.prev")).unwrap(), "old tui");
    assert_eq!(names_in(&dir), ["arena", "arena-tui", "arena-tui.prev", "arena.prev"]);
    assert_eq!(*srv.requests.lock().unwrap(), ["GET /dl/SHA256SUMS HTTP/1.1".to_string(), format!("GET /dl/arena-tui-{T} HTTP/1.1"), format!("GET /dl/arena-{T} HTTP/1.1")]);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Any doubt stops before the swap and leaves nothing behind: a checksum mismatch, a new
/// `arena` of another version (refused before it runs: it would leave `ran` behind), one that
/// carries the version but doesn't say it when run (another platform's, a cut-short one), no
/// binary for this platform; dropping the staged pair (a refused confirm) removes it too.
#[tokio::test]
async fn a_doubtful_download_installs_nothing() {
    let dir = temp_dir("fetch-bad");
    std::fs::write(dir.join("arena"), "old arena").unwrap();
    let arena = fake_arena("2.0.261101.0900");
    let untouched = |dir: &Path| {
        assert_eq!(names_in(dir), ["arena"]);
        assert_eq!(std::fs::read_to_string(dir.join("arena")).unwrap(), "old arena");
    };

    let (_srv, release) = release_server("tampered", &arena, ("new tui", &arena));
    let e = fetch(&test_client(), &release, T, &dir, &pinned(&["tampered", &arena])).await.unwrap_err();
    assert!(e.contains("doesn't match SHA256SUMS"), "{e}");
    untouched(&dir);

    let other = format!("#!/bin/sh\ntouch '{}'\necho 'arena 2.0.250101.0000 (0123456789)'\n", dir.join("ran").display());
    let (_srv, release) = release_server("new tui", &other, ("new tui", &other));
    let e = fetch(&test_client(), &release, T, &dir, &pinned(&["new tui", &other])).await.unwrap_err();
    assert!(e.starts_with(&format!("arena-{T}: the new arena doesn't carry the version 2.0.261101.0900")), "{e}");
    untouched(&dir);

    let says_other = "#!/bin/sh\n# 2.0.261101.0900 (0123456789)\necho 'arena 2.0.250101.0000 (0123456789)'\n";
    let (_srv, release) = release_server("new tui", says_other, ("new tui", says_other));
    let e = fetch(&test_client(), &release, T, &dir, &pinned(&["new tui", says_other])).await.unwrap_err();
    assert!(e.contains("not arena 2.0.261101.0900"), "{e}");
    untouched(&dir);

    let (_srv, release) = release_server("new tui", &arena, ("new tui", &arena));
    let e = fetch(&test_client(), &release, "riscv64gc-unknown-linux-gnu", &dir, &pinned(&["new tui", &arena])).await.unwrap_err();
    assert!(e.contains("has no `arena-tui-riscv64gc-unknown-linux-gnu`"), "{e}");
    untouched(&dir);

    let (_srv, release) = release_server("new tui", &arena, ("new tui", &arena));
    drop(fetch(&test_client(), &release, T, &dir, &pinned(&["new tui", &arena])).await.unwrap());
    untouched(&dir);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Attestations from memory, by SHA-256; any other digest has none (GitHub's empty answer).
struct Attested(HashMap<String, Value>);

#[async_trait::async_trait]
impl attest::Source for Attested {
    async fn attestations(&self, sha256: &str) -> Result<Value> {
        Ok(self.0.get(sha256).cloned().unwrap_or_else(|| json!({"attestations": []})))
    }
}

/// A download whose SHA256SUMS matches but whose provenance isn't proven is refused before
/// either binary is written, let alone run (the fake `arena` would leave `ran` behind): with
/// no attestation at all, with a genuine one (the real release's) that doesn't name these
/// bytes, and with digests pinned by `--skip-attestation` that aren't these.
#[tokio::test]
async fn an_unproven_download_is_never_written_or_run() {
    let dir = temp_dir("fetch-unproven");
    std::fs::write(dir.join("arena"), "old arena").unwrap();
    let arena = format!("#!/bin/sh\ntouch '{}'\necho 'arena 2.0.261101.0900 (0123456789)'\n", dir.join("ran").display());
    let real = serde_json::from_str::<Value>(include_str!("fixtures/attestation_v2.0.261028.1916.json")).unwrap();
    let none = Attested(HashMap::new());
    let genuine_elsewhere = Attested(HashMap::from([(sha256_hex(arena.as_bytes()), real)]));
    let cases = [
        (Provenance::Verify { source: &none, expected: attest::RELEASE }, format!("arena-{T}: GitHub has no build-provenance attestation for it")),
        (Provenance::Verify { source: &genuine_elsewhere, expected: attest::RELEASE }, format!("arena-{T} and arena-tui-{T}: no attestation proves they were built by")),
        (pinned(&["new tui", "another arena"]), format!("arena-{T}: its sha256 {} isn't one given to --skip-attestation", sha256_hex(arena.as_bytes()))),
        (pinned(&["another tui", &arena]), format!("arena-tui-{T}: its sha256 {} isn't one given", sha256_hex(b"new tui"))),
    ];
    for (provenance, said) in cases {
        let (_srv, release) = release_server("new tui", &arena, ("new tui", &arena));
        let e = fetch(&test_client(), &release, T, &dir, &provenance).await.unwrap_err();
        assert!(e.starts_with(&said) && e.ends_with("— not installed"), "{said}: {e}");
        assert_eq!(names_in(&dir), ["arena"], "nothing staged, nothing run");
    }
    // The pinned `arena` served under both names (SHA256SUMS made to match): one pin unused.
    let (_srv, release) = release_server(&arena, &arena, (&arena, &arena));
    let e = fetch(&test_client(), &release, T, &dir, &pinned(&["new tui", &arena])).await.unwrap_err();
    assert!(e.starts_with(&format!("arena-tui-{T} is the same file as arena-{T}")), "{e}");
    assert_eq!(names_in(&dir), ["arena"], "nothing staged, nothing run");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `--skip-attestation` takes exactly the two binaries' SHA-256s, in either order, any case.
#[test]
fn skip_attestation_takes_both_binaries_sha256s() {
    let (a, b) = ("a".repeat(64), "B".repeat(64));
    let v = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    assert_eq!(parse_pins(&v(&[&a, &b])), Ok(vec![a.clone(), "b".repeat(64)]));
    assert_eq!(parse_pins(&v(&[&format!(" {b} "), &a])), Ok(vec!["b".repeat(64), a.clone()]));
    for (bad, said) in [
        (v(&[&a]), "both binaries"),
        (v(&[&a, &a]), "both binaries"),
        (v(&[&a, &b, &"c".repeat(64)]), "got 3"),
        (v(&[&a, "yes"]), "not \"yes\""),
        (v(&[&a, &"g".repeat(64)]), "64 hex digits"),
        (v(&[&a, &"a".repeat(63)]), "64 hex digits"),
    ] {
        let e = parse_pins(&bad).unwrap_err();
        assert!(e.starts_with("--skip-attestation takes ") && e.contains(said), "{bad:?}: {e}");
    }
    let pins = parse_pins(&v(&[&a, &b])).unwrap();
    let (b, c) = ("b".repeat(64), "c".repeat(64));
    assert_eq!(check_pinned(&pins, &[("arena-x", &b), ("arena-tui-x", &a)]), Ok(()));
    assert!(check_pinned(&pins, &[("arena-x", &b), ("arena-tui-x", &c)]).unwrap_err().starts_with("arena-tui-x: its sha256 ccc"));
    // One binary served under both names: both match a pin, but one pin is left unused.
    let e = check_pinned(&pins, &[("arena-x", &b), ("arena-tui-x", &b)]).unwrap_err();
    assert_eq!(e, format!("arena-tui-x is the same file as arena-x (sha256 {b}): the release serves one binary as both"));
}

/// The `arena` must be the release's version before it is written or run: by the attested
/// `VERSION` when the statement names one, else by the `--version` line its bytes carry.
#[test]
fn an_arena_of_another_version_is_refused_before_it_runs() {
    let v = Version([2, 0, 261028, 1916]);
    let (name, sha) = version_subject(v);
    assert_eq!((name, sha.as_str()), ("VERSION", sha256_hex(b"2.0.261028.1916\n").as_str()));
    let named = |d: &str| proof_naming(&[("arena-x", "c"), ("VERSION", d)]);
    let real = b"...Proceed? [y/N] prompt2.0.261028.1916 (7917eb2b6c)Streamlined...";
    let older = b"...Proceed? [y/N] prompt2.0.261028.1827 (0123456789)Streamlined; see 2.0.261027.1500 (5274b7a1c2)";
    assert_eq!(check_version(Some(&named(&sha)), older, v), Ok(()), "the attested VERSION decides");
    let e = check_version(Some(&named(&sha256_hex(b"2.0.261028.1827\n"))), real, v).unwrap_err();
    assert_eq!(e, "its attestation is for another version than 2.0.261028.1916: an older build served as 2.0.261028.1916");
    let unnamed = proof_naming(&[("arena-x", "c")]);
    for proof in [Some(&unnamed), None] {
        assert_eq!(check_version(proof, real, v), Ok(()));
        let e = check_version(proof, older, v).unwrap_err();
        assert_eq!(e, "the new arena doesn't carry the version 2.0.261028.1916: another build served as 2.0.261028.1916");
    }
    assert!(check_version(None, b"2.0.261028.19160 (x", Version([2, 0, 261028, 1916])).is_err(), "the whole version, then ` (`");
}

fn proof_naming(subjects: &[(&str, &str)]) -> attest::Proof {
    attest::Proof {
        signer: attest::Signer::default(),
        logged: None,
        subjects: subjects.iter().map(|(n, d)| (n.to_string(), d.to_string())).collect(),
    }
}

/// The changelog is trusted only as far as the attestation goes: named with its digest
/// (verified), named with another (forged after the build: stop), or not named (older releases).
#[test]
fn the_changelog_is_checked_against_the_attestation_when_it_names_one() {
    let (sha, other) = ("a".repeat(64), "b".repeat(64));
    let named = proof_naming(&[("arena-x", "c"), ("CHANGELOG.md", &sha)]);
    assert_eq!(changelog_provenance(Some(&named), Some(&sha)), Ok("verified: the attestation names this CHANGELOG.md"));
    assert_eq!(changelog_provenance(Some(&named), Some(&sha.to_ascii_uppercase())), Ok("verified: the attestation names this CHANGELOG.md"));
    let e = changelog_provenance(Some(&named), Some(&other)).unwrap_err();
    assert!(e.starts_with(&format!("the release's CHANGELOG.md (sha256 {other}) isn't the one its attestation names")) && e.ends_with("not installed"), "{e}");
    let older = proof_naming(&[("arena-x", "c")]);
    assert_eq!(changelog_provenance(Some(&older), Some(&sha)), Ok("not covered by this release's attestation"));
    assert_eq!(changelog_provenance(None, Some(&sha)), Ok("not verified: --skip-attestation"));
    let e = changelog_provenance(Some(&named), None).unwrap_err();
    assert!(e.starts_with("the attestation names a CHANGELOG.md, but the release's couldn't be read") && e.ends_with("not installed"), "{e}");
    assert_eq!(changelog_provenance(Some(&older), None), Ok("no changelog to check"));
    assert_eq!(changelog_provenance(None, None), Ok("no changelog to check"));
}

#[test]
fn release_text_loses_its_control_characters() {
    assert_eq!(printable("## x\n- a\tb\n"), "## x\n- a\tb\n");
    assert_eq!(printable("\x1b]0;pwned\x07red \x1b[31mtext\r\n"), "?]0;pwned?red ?[31mtext?\n");
    assert_eq!(printable("é ⚠ ok"), "é ⚠ ok");
    assert_eq!(printable("run \u{202E}hs.lacol\u{202C} \u{200B}x\u{2066}y\u{2069}"), "run ?hs.lacol? ?x?y?", "bidi and zero-width");
}

/// A directory the update can't write is refused with what to do, before anything changes.
#[tokio::test]
async fn an_unwritable_install_dir_says_to_use_the_owner_or_sudo() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir("readonly");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::write(dir.join("probe"), "x").is_ok() {
        // Root ignores the mode: nothing to test here.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        return;
    }
    let arena = fake_arena("2.0.261101.0900");
    let (_srv, release) = release_server("new tui", &arena, ("new tui", &arena));
    let e = fetch(&test_client(), &release, T, &dir, &pinned(&["new tui", &arena])).await.unwrap_err();
    assert!(e.contains("can't write") && e.contains("sudo"), "{e}");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}
