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
    assert_eq!(*srv.requests.lock().unwrap(), ["GET /dl/CHANGELOG.md HTTP/1.1"]);

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

/// Download, check against SHA256SUMS, stage, run the new `arena --version`, then install:
/// both in place, the old ones as `.prev`.
#[tokio::test]
async fn an_update_installs_both_binaries_and_keeps_the_old_ones() {
    let dir = temp_dir("fetch-ok");
    std::fs::write(dir.join("arena"), "old arena").unwrap();
    std::fs::write(dir.join("arena-tui"), "old tui").unwrap();
    let arena = fake_arena("2.0.261101.0900");
    let (srv, release) = release_server("new tui", &arena, ("new tui", &arena));
    let staged = fetch(&test_client(), &release, T, &dir).await.unwrap();
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
/// `arena` that doesn't say the release's version (another platform's, a cut-short one), no
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
    let e = fetch(&test_client(), &release, T, &dir).await.unwrap_err();
    assert!(e.contains("doesn't match SHA256SUMS"), "{e}");
    untouched(&dir);

    let other = fake_arena("2.0.250101.0000");
    let (_srv, release) = release_server("new tui", &other, ("new tui", &other));
    let e = fetch(&test_client(), &release, T, &dir).await.unwrap_err();
    assert!(e.contains("not arena 2.0.261101.0900"), "{e}");
    untouched(&dir);

    let (_srv, release) = release_server("new tui", &arena, ("new tui", &arena));
    let e = fetch(&test_client(), &release, "riscv64gc-unknown-linux-gnu", &dir).await.unwrap_err();
    assert!(e.contains("has no `arena-tui-riscv64gc-unknown-linux-gnu`"), "{e}");
    untouched(&dir);

    let (_srv, release) = release_server("new tui", &arena, ("new tui", &arena));
    drop(fetch(&test_client(), &release, T, &dir).await.unwrap());
    untouched(&dir);
    std::fs::remove_dir_all(&dir).unwrap();
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
    let e = fetch(&test_client(), &release, T, &dir).await.unwrap_err();
    assert!(e.contains("can't write") && e.contains("sudo"), "{e}");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}
