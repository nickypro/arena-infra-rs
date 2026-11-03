//! Table tests for the AI keys' facts and verdicts (`aikeys`), and how a key FAIL sits beside
//! the machine's own verdict.

use super::*;
use crate::health::{
    failure_kind_of, parse_deep, render_health_table, render_key_report, render_summary, FailureKind, HealthPolicy, PodHealth,
};

const GOOD: &str = "sk-or-v1-0123456789abcdef-good";
const OLD: &str = "sk-or-v1-0123456789abcdef-old";
const HF_GOOD: &str = "hf_0123456789abcdefGOOD";
const WB_GOOD: &str = "0123456789abcdef0123456789abcdef01234567";

fn facts(lines: &str) -> DeepFacts {
    parse_deep(&format!("deep_check=2\n{lines}deep_check_end=1\n"))
}

fn pod(name: &str) -> Pod {
    Pod { id: format!("id-{name}"), name: name.into(), provider: "runpod".into(), ..Default::default() }
}

/// devtest's book: apple's OpenRouter key and the fleet's HF token in it, bob a staff box.
fn book() -> KeyBook {
    let cfg = Config::parse("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=( apple bloom @bob )\n");
    KeyBook::new(&cfg, &[("devtest-apple".into(), GOOD.into())], Some(HF_GOOD), &[])
}

/// `openrouter.*` lines for `secret`, answered `http` with the rest of `extra`.
fn or_lines(secret: &str, http: u16, extra: &str) -> String {
    format!("openrouter.key=set\nopenrouter.fingerprint={}\nopenrouter.curl_exit=0\nopenrouter.http={http}\n{extra}", fingerprint(secret))
}

fn hf_lines(secret: &str, http: u16) -> String {
    format!("hf.key=set\nhf.fingerprint={}\nhf.curl_exit=0\nhf.http={http}\n", fingerprint(secret))
}

fn ai(f: &DeepFacts, name: &str, book: &KeyBook) -> Check {
    key_checks(f, &pod(name), book).remove(0)
}

fn hf(f: &DeepFacts, name: &str, book: &KeyBook) -> Check {
    key_checks(f, &pod(name), book).remove(1)
}

fn wb(f: &DeepFacts, name: &str, book: &KeyBook) -> Check {
    key_checks(f, &pod(name), book).remove(2)
}

/// `wandb.*` lines for `secret`, answered `http` naming a user or not (`viewer`).
fn wb_lines(secret: &str, http: u16, viewer: &str) -> String {
    format!("wandb.key=set\nwandb.fingerprint={}\nwandb.curl_exit=0\nwandb.http={http}\nwandb.viewer={viewer}\n", fingerprint(secret))
}

#[test]
fn the_script_facts_parse_and_unknowns_stay_unknown() {
    let f = facts(&format!(
        "{}openrouter.limit=1\nopenrouter.limit_remaining=0.42\nopenrouter.limit_reset=daily\nopenrouter.usage=0.58\n\
         openrouter.disabled=false\nopenrouter.label=sk-or-v1-012...ood\nhf.key=missing\n",
        or_lines(GOOD, 200, "")
    ));
    let k = f.openrouter.as_ref().unwrap();
    assert_eq!(
        (k.set, k.fingerprint.as_deref(), k.http, k.limit, k.limit_remaining, k.limit_reset.as_deref(), k.usage, k.disabled),
        (Some(true), Some(fingerprint(GOOD).as_str()), Some(200), Some(1.0), Some(0.42), Some("daily"), Some(0.58), Some(false))
    );
    assert_eq!(f.hf.as_ref().map(|h| h.set), Some(Some(false)));
    // A field this parser doesn't know (here OpenRouter's label — the script never prints it) is
    // kept aside, not taken for one it knows.
    assert!(f.other.contains_key("openrouter.label"));
    // `limit=none`: no cap; `unknown`s are unknown; a fingerprint that isn't 12 hex isn't one.
    let f = facts("openrouter.key=set\nopenrouter.fingerprint=nothex\nopenrouter.http=unknown\nopenrouter.limit=none\nopenrouter.limit_remaining=none\nopenrouter.limit_reset=none\n");
    let k = f.openrouter.unwrap();
    assert_eq!((k.fingerprint, k.http, k.limit, k.unlimited, k.limit_remaining, k.limit_reset), (None, None, None, true, None, None));
    // An older script says nothing: no facts at all.
    assert_eq!((facts("load1=0.1\n").openrouter, facts("").hf), (None, None));
}

#[test]
fn the_fingerprint_is_sha256s_first_12_hex() {
    // `printf %s abc | sha256sum` = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
    assert_eq!(fingerprint("abc"), "ba7816bf8f01");
}

/// Every OpenRouter verdict: (what, the facts, machine, status, detail).
#[test]
fn ai_key_verdicts() {
    use Status::*;
    let b = book();
    let money = "openrouter.limit=1\nopenrouter.limit_remaining=0.42\nopenrouter.usage=0.58\n";
    let cases: Vec<(&str, String, &str, Status, &str)> = vec![
        ("ok, money left", or_lines(GOOD, 200, money), "devtest-apple", Pass, "ok, $0.42 of $1.00 left"),
        ("ok, no cap", or_lines(GOOD, 200, "openrouter.limit=none\nopenrouter.usage=0.12\n"), "devtest-apple", Pass, "ok, no cap, $0.12 spent"),
        (
            "ok, daily cap",
            or_lines(GOOD, 200, "openrouter.limit=0.05\nopenrouter.limit_remaining=0.05\nopenrouter.limit_reset=daily\n"),
            "devtest-apple",
            Pass,
            "ok, $0.050 of $0.050 left (resets daily)",
        ),
        (
            "a working key the CSV has no row for",
            or_lines(OLD, 200, money),
            "devtest-bloom",
            Pass,
            "ok, $0.42 of $1.00 left (no keys-CSV row to compare)",
        ),
        (
            "rejected, no listing to say why: paused and deleted both get 401",
            or_lines(GOOD, 401, "openrouter.error=User not found.\n"),
            "devtest-apple",
            Fail,
            "rejected (HTTP 401): paused? `arena keys resume apple`, else `arena keys rotate apple --copy`",
        ),
        (
            "rejected, no keys-CSV row: deleted",
            or_lines(OLD, 401, ""),
            "devtest-bloom",
            Fail,
            "rejected (HTTP 401), and no keys-CSV row for it — `arena keys gen bloom --copy`",
        ),
        (
            "rejected, an old key: the CSV's is the fix",
            or_lines(OLD, 401, ""),
            "devtest-apple",
            Fail,
            "rejected (HTTP 401): an old key, not the keys CSV's — `arena pods copy-keys apple`",
        ),
        (
            "paused, as a 403 says it",
            or_lines(GOOD, 403, "openrouter.error=Key is disabled\n"),
            "devtest-apple",
            Fail,
            "paused on OpenRouter (HTTP 403) — `arena keys resume apple`",
        ),
        (
            "paused, as a 200 says it",
            or_lines(GOOD, 200, "openrouter.disabled=true\nopenrouter.limit=1\nopenrouter.limit_remaining=1\n"),
            "devtest-apple",
            Fail,
            "paused on OpenRouter — `arena keys resume apple`",
        ),
        (
            "out of money",
            or_lines(GOOD, 200, "openrouter.limit=0.05\nopenrouter.limit_remaining=0\n"),
            "devtest-apple",
            Fail,
            "out of money ($0.00 of $0.050 left) — `arena keys limit apple +5`",
        ),
        (
            "out of money, an old key",
            or_lines(OLD, 200, "openrouter.limit=0.05\nopenrouter.limit_remaining=0\n"),
            "devtest-apple",
            Fail,
            "out of money: an old key, not the keys CSV's — `arena pods copy-keys apple`",
        ),
        (
            "overdrawn counts as out",
            or_lines(GOOD, 200, "openrouter.limit=5\nopenrouter.limit_remaining=-0.01\n"),
            "devtest-apple",
            Fail,
            "out of money ($-0.01 of $5.00 left) — `arena keys limit apple +5`",
        ),
        (
            "little left: under a tenth of the cap",
            or_lines(GOOD, 200, "openrouter.limit=5\nopenrouter.limit_remaining=0.40\n"),
            "devtest-apple",
            Warn,
            "only $0.40 of $5.00 left (<10% of its cap) — `arena keys limit apple +5`",
        ),
        ("a big cap warns only under $1", or_lines(GOOD, 200, "openrouter.limit=100\nopenrouter.limit_remaining=2\n"), "devtest-apple", Pass, "ok, $2.00 of $100.00 left"),
        (
            "an old key that works: stale",
            or_lines(OLD, 200, money),
            "devtest-apple",
            Warn,
            "works, but an old key, not the keys CSV's (rotated since?) — `arena pods copy-keys apple`",
        ),
        (
            "no fingerprint from the machine: whose key it is can't be said",
            "openrouter.key=set\nopenrouter.curl_exit=0\nopenrouter.http=200\nopenrouter.limit=1\nopenrouter.limit_remaining=1\n".into(),
            "devtest-apple",
            Warn,
            "works, not compared (no sha256sum there) — `arena pods copy-keys apple` to be sure",
        ),
        ("missing, the CSV has one", "openrouter.key=missing\n".into(), "devtest-apple", Fail, "missing, though the keys CSV has one for it — `arena pods copy-keys apple`"),
        (
            "missing, no row, no owner",
            "openrouter.key=missing\n".into(),
            "devtest-bloom",
            Warn,
            "missing, nor in the keys CSV — `arena keys gen bloom --copy` if it should have one",
        ),
        (
            "couldn't reach OpenRouter",
            "openrouter.key=set\nopenrouter.curl_exit=28\nopenrouter.http=000\nopenrouter.error=curl: (28) Connection timed out after 5001 milliseconds\n".into(),
            "devtest-apple",
            Warn,
            "not checked (unreachable: curl: (28) Connection timed out…) — `arena pods test --keys apple`",
        ),
        (
            "a 200 whose body was cut off says nothing of the money",
            or_lines(GOOD, 200, "").replace("curl_exit=0", "curl_exit=18") + "openrouter.error=curl: (18) transfer closed with 120 bytes remaining\n",
            "devtest-apple",
            Warn,
            "not checked (unreachable: curl: (18) transfer closed with…) — `arena pods test --keys apple`",
        ),
        ("a 5xx", or_lines(GOOD, 502, ""), "devtest-apple", Warn, "not checked (HTTP 502 from OpenRouter) — `arena pods test --keys apple`"),
        (
            "no curl",
            format!("openrouter.key=set\nopenrouter.fingerprint={}\nopenrouter.curl_exit=missing\n", fingerprint(GOOD)),
            "devtest-apple",
            Warn,
            "not checked (no curl on the machine) — `arena pods test --keys apple`",
        ),
        (
            "a staff box: a note, never a verdict",
            or_lines(OLD, 401, ""),
            "bob",
            Skip,
            "staff box: its key is the organisers' choice (one here, rejected (HTTP 401))",
        ),
        ("an older script", String::new(), "devtest-apple", Skip, "not reported (a check from before key checks, or cut off)"),
    ];
    for (what, lines, name, status, detail) in cases {
        let c = ai(&facts(&lines), name, &b);
        assert_eq!((c.name.as_str(), c.status, c.detail.as_str()), (AI_KEY, status, detail), "{what}");
    }
}

/// Every verdict's command fits the table's NOTES cell (clipped at 110 characters, `ai_key: `
/// included), for a long machine name too — a clipped command is no command. (A note after it,
/// as a not-checked old key's, may be cut.)
#[test]
fn every_verdict_fits_the_notes_cell() {
    let mut b = book();
    let long = "devtest-lightning";
    b.csv.insert(long.into(), fingerprint(GOOD));
    b.owners.insert(format!("runpod:id-{long}"), "someone-with-a-long-name".into());
    let or = [
        "openrouter.key=missing\n".to_string(),
        or_lines(GOOD, 401, ""),
        or_lines(OLD, 401, ""),
        or_lines(GOOD, 403, "openrouter.error=Key is disabled\n"),
        or_lines(GOOD, 200, "openrouter.disabled=true\n"),
        or_lines(GOOD, 200, "openrouter.limit=0.05\nopenrouter.limit_remaining=0\nopenrouter.limit_reset=monthly\n"),
        or_lines(OLD, 200, "openrouter.limit=0.05\nopenrouter.limit_remaining=0\n"),
        or_lines(GOOD, 200, "openrouter.limit=5\nopenrouter.limit_remaining=0.40\nopenrouter.limit_reset=weekly\n"),
        or_lines(OLD, 200, "openrouter.limit=5\nopenrouter.limit_remaining=0.40\n"),
        or_lines(OLD, 200, ""),
        "openrouter.key=set\nopenrouter.curl_exit=0\nopenrouter.http=200\n".into(),
        "openrouter.key=set\nopenrouter.curl_exit=28\nopenrouter.http=000\nopenrouter.error=curl: (28) Connection timed out after 5001 milliseconds\n".into(),
        or_lines(OLD, 502, ""),
    ];
    let hf = [
        "hf.key=missing\n".to_string(),
        hf_lines(HF_GOOD, 401),
        hf_lines("hf_old", 401),
        hf_lines("hf_old", 200),
        hf_lines(HF_GOOD, 429),
        "hf.key=set\nhf.curl_exit=6\nhf.http=000\nhf.error=curl: (6) Could not resolve host: huggingface.co\n".into(),
    ];
    let mut listed = b.clone();
    for state in [Listed::Paused, Listed::Gone, Listed::Live] {
        listed.listed = Some([(long.to_string(), state)].into_iter().collect());
        let c = ai(&facts(&or[1]), long, &listed);
        assert!(command_fits(&format!("{}: {}", c.name, c.detail)), "{state:?}: {}", c.detail);
    }
    let mut own = b.clone();
    own.hf_csv.insert(long.into(), fingerprint("hf_own"));
    for lines in or.iter().chain(hf.iter()) {
        for book in [&b, &own] {
            for c in key_checks(&facts(lines), &pod(long), book) {
                let cell = format!("{}: {}", c.name, c.detail);
                assert!(command_fits(&cell), "{} chars: {cell}", cell.chars().count());
            }
        }
    }
}

/// Whether `cell` keeps everything up to its last command (its closing backtick) within the
/// NOTES clip (109 characters and `…`).
fn command_fits(cell: &str) -> bool {
    let upto = cell.rfind('`').map_or(cell.chars().count(), |i| cell[..=i].chars().count());
    upto <= 109
}

/// OpenRouter's listing tells a paused key from a deleted one (from the machine both are a 401
/// `User not found.`), for the keys CSV's key on the machine; an old key's fix stays the CSV's.
#[test]
fn the_listing_names_a_rejected_keys_fix() {
    let mut b = book();
    let rejected = facts(&or_lines(GOOD, 401, "openrouter.error=User not found.\n"));
    for (state, detail) in [
        (Listed::Paused, "paused on OpenRouter (HTTP 401) — `arena keys resume apple`"),
        (Listed::Gone, "rejected by OpenRouter (HTTP 401): deleted — `arena keys rotate apple --copy`"),
        // Listed enabled, yet rejected: a resume that hasn't reached it yet (live: about a minute).
        (Listed::Live, "rejected (HTTP 401), listed enabled: just resumed? Wait a minute — `arena pods test --keys apple`"),
    ] {
        b.listed = Some([("devtest-apple".to_string(), state)].into_iter().collect());
        let c = ai(&rejected, "devtest-apple", &b);
        assert_eq!((c.status, c.detail.as_str()), (Status::Fail, detail), "{state:?}");
    }
    // An old key on the machine: whatever the listing says of the CSV's, copying that is the fix.
    b.listed = Some([("devtest-apple".to_string(), Listed::Paused)].into_iter().collect());
    let c = ai(&facts(&or_lines(OLD, 401, "")), "devtest-apple", &b);
    assert_eq!(c.detail, "rejected (HTTP 401): an old key, not the keys CSV's — `arena pods copy-keys apple`");
}

/// A keys CSV that's there but can't be read (here a directory where the file should be — an
/// error whoever reads it, root included): keys are expected, never "none configured"; and a
/// script that said it couldn't run (`error=`) is a FAIL, never two quiet Skips.
#[test]
fn an_unreadable_csv_or_a_script_error_is_never_a_quiet_pass() {
    let dir = std::env::temp_dir().join(format!("arena-keybook-unreadable-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("keys/openrouter_api_keys.csv")).unwrap();
    std::fs::create_dir_all(dir.join("keys/huggingface_api_keys.csv")).unwrap();
    std::fs::write(dir.join("config.env"), "MACHINE_NAME_PREFIX=devtest\n").unwrap();
    let b = KeyBook::load(&Config::load(&dir.join("config.env")).unwrap(), None);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(b.openrouter && b.any(), "{b:?}");
    assert!(b.unreadable.as_deref().is_some_and(|e| e.starts_with("openrouter_api_keys.csv: ")), "{b:?}");
    assert!(b.hf_unreadable.as_deref().is_some_and(|e| e.starts_with("huggingface_api_keys.csv: ")), "{b:?}");
    let c = ai(&facts(&or_lines(GOOD, 200, "")), "devtest-apple", &b);
    assert_eq!(c.status, Status::Warn, "{c:?}");
    assert!(c.detail.starts_with("ok, but not compared: openrouter_api_keys.csv: "), "{}", c.detail);
    // Missing or rejected: still a FAIL, but no "it has no row" advice (`keys gen` would mint a
    // second key while the unreadable file may hold its newest): make the file readable first.
    let c = ai(&facts(&or_lines(GOOD, 401, "")), "devtest-apple", &b);
    assert_eq!(c.status, Status::Fail);
    assert!(c.detail == "rejected (HTTP 401); the keys CSV is unreadable — fix it, then `arena pods test --keys apple`", "{}", c.detail);
    assert!(command_fits(&format!("ai_key: {}", c.detail).replace("apple", "lightning")), "{}", c.detail);
    let c = ai(&facts("openrouter.key=missing\n"), "devtest-apple", &b);
    assert!(c.status == Status::Fail && c.detail.starts_with("missing; the keys CSV is unreadable"), "{c:?}");
    assert!(c.detail.ends_with("— fix it, then `arena pods copy-keys apple`"), "{}", c.detail);
    assert!(command_fits(&format!("ai_key: {}", c.detail).replace("apple", "lightning")), "{}", c.detail);
    // HF: a rejected token still FAILs; a working one isn't compared; a missing one can't be judged.
    assert_eq!(hf(&facts(&hf_lines(HF_GOOD, 401)), "devtest-apple", &b).status, Status::Fail);
    let c = hf(&facts(&hf_lines(HF_GOOD, 200)), "devtest-apple", &b);
    assert!(c.status == Status::Warn && c.detail.starts_with("works, not compared: huggingface_api_keys.csv: "), "{c:?}");
    assert_eq!(hf(&facts("hf.key=missing\n"), "devtest-apple", &b).status, Status::Warn);
    let policy = HealthPolicy { keys: Some(book()), ..HealthPolicy::default() };
    let h = PodHealth::keys_checked(&pod("devtest-apple"), facts("error=mktemp failed (is /tmp full or read-only?)\n"), &policy);
    assert_eq!(h.status, Status::Fail);
    assert_eq!(
        h.checks.iter().map(|c| (c.name.as_str(), c.detail.as_str())).collect::<Vec<_>>(),
        [("script", "mktemp failed (is /tmp full or read-only?)")]
    );
}

/// Owners and fleets: a machine whose owner should have a key FAILs without one; a fleet with
/// no keys configured gets a note whatever it finds.
#[test]
fn who_should_have_a_key() {
    let mut b = book();
    b.owners.insert("runpod:id-devtest-bloom".into(), "alice".into());
    let missing = facts("openrouter.key=missing\n");
    let c = ai(&missing, "devtest-bloom", &b);
    assert_eq!((c.status, c.detail.as_str()), (Status::Fail, "missing, and its owner (alice) should have one — `arena keys gen bloom --copy`"));
    // No keys anywhere for this fleet: a short note, never a verdict.
    let none = KeyBook::new(&Config::parse("MACHINE_NAME_PREFIX=devtest\n"), &[], None, &[]);
    assert!(!none.any());
    let owned = KeyBook { owners: b.owners.clone(), ..none.clone() };
    assert!(owned.any(), "an owner who should have a key is something to check");
    for (lines, seen) in [("openrouter.key=missing\n".to_string(), "none here"), (or_lines(GOOD, 200, ""), "one here, accepted")] {
        let c = ai(&facts(&lines), "devtest-apple", &none);
        assert_eq!((c.status, c.detail), (Status::Skip, format!("no OpenRouter keys configured for this fleet ({seen})")));
    }
    // …unless a provisioning key is set: then keys are this fleet's to have.
    let prov = KeyBook::new(&Config::parse("MACHINE_NAME_PREFIX=devtest\nOPENROUTER_PROVISIONING_KEY=x\n"), &[], None, &[]);
    assert!(prov.any());
    assert_eq!(ai(&missing, "devtest-apple", &prov).status, Status::Warn);
}

#[test]
fn hf_token_verdicts() {
    use Status::*;
    let b = book();
    let cases: Vec<(&str, String, &str, Status, &str)> = vec![
        ("accepted", hf_lines(HF_GOOD, 200), "devtest-apple", Pass, "accepted by Hugging Face"),
        (
            "rejected",
            hf_lines(HF_GOOD, 401),
            "devtest-apple",
            Fail,
            "rejected (HTTP 401) — a new HF_TOKEN in the config, then `arena pods copy-keys`",
        ),
        (
            "an old token, rejected",
            hf_lines("hf_old", 401),
            "devtest-apple",
            Fail,
            "rejected (HTTP 401): not the config's HF_TOKEN — `arena pods copy-keys apple`",
        ),
        ("an old token that works", hf_lines("hf_old", 200), "devtest-apple", Warn, "works, but not the config's HF_TOKEN — `arena pods copy-keys apple`"),
        ("missing", "hf.key=missing\n".into(), "devtest-apple", Fail, "missing — `arena pods copy-keys apple`"),
        (
            "rate-limited",
            hf_lines(HF_GOOD, 429),
            "devtest-apple",
            Warn,
            "not checked (Hugging Face is rate-limiting: HTTP 429) — `arena pods test --keys apple`",
        ),
        ("a staff box", "hf.key=missing\n".into(), "bob", Skip, "staff box: its token is the organisers' choice (none here)"),
    ];
    for (what, lines, name, status, detail) in cases {
        let c = hf(&facts(&lines), name, &b);
        assert_eq!((c.name.as_str(), c.status, c.detail.as_str()), (HF_TOKEN_CHECK, status, detail), "{what}");
    }
    // No HF_TOKEN configured: a note. A machine's own row in huggingface_api_keys.csv wins.
    let cfg = Config::parse("MACHINE_NAME_PREFIX=devtest\n");
    let none = KeyBook::new(&cfg, &[], None, &[]);
    assert_eq!(hf(&facts("hf.key=missing\n"), "devtest-apple", &none).status, Status::Skip);
    let own = KeyBook::new(&cfg, &[], Some(HF_GOOD), &[("devtest-apple".into(), "hf_own".into())]);
    assert_eq!(hf(&facts(&hf_lines("hf_own", 200)), "devtest-apple", &own).status, Status::Pass);
    let c = hf(&facts(&hf_lines(HF_GOOD, 200)), "devtest-apple", &own);
    assert_eq!(
        (c.status, c.detail.as_str()),
        (Status::Warn, "works, but not its huggingface_api_keys.csv row's — `arena pods copy-keys apple`")
    );
    // Its own row's token rejected: the fix is that row (a new HF_TOKEN would change nothing for
    // it — the row is copied last), never the fleet's working token.
    let c = hf(&facts(&hf_lines("hf_own", 401)), "devtest-apple", &own);
    assert_eq!(c.detail, "rejected (HTTP 401) — renew its huggingface_api_keys.csv row, then `arena pods copy-keys apple`");
    // The HF cell: `stale` only for a token shown to be another; one not compared is `?`.
    let policy = HealthPolicy { keys: Some(b.clone()), ..HealthPolicy::default() };
    let cell = |lines: &str| hf_token_cell(&PodHealth::keys_checked(&pod("devtest-apple"), facts(lines), &policy));
    assert_eq!(cell(&hf_lines("hf_old", 200)).as_deref(), Some("stale"));
    assert_eq!(cell("hf.key=set\nhf.curl_exit=0\nhf.http=200\n").as_deref(), Some("?"));
}

/// The W&B key follows the HF token's rules: the config's `WANDB_API_KEY` (or the machine's own
/// row in `wandb_api_keys.csv`), fingerprints only — and W&B answers a key it doesn't know with
/// 200 and no user, which is a rejection.
#[test]
fn wandb_verdicts() {
    use Status::*;
    let cfg = Config::parse(&format!("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=( apple bloom @bob )\nWANDB_API_KEY={WB_GOOD}\n"));
    let b = KeyBook::new(&cfg, &[], None, &[]);
    let cases: Vec<(&str, String, &str, Status, &str)> = vec![
        ("accepted", wb_lines(WB_GOOD, 200, "yes"), "devtest-apple", Pass, "accepted by W&B"),
        (
            "rejected: no user for it",
            wb_lines(WB_GOOD, 200, "no"),
            "devtest-apple",
            Fail,
            "rejected (W&B names no user for it) — a new WANDB_API_KEY in the config, then `arena pods copy-keys`",
        ),
        (
            "an old key, rejected",
            wb_lines("old-wandb-key", 200, "no"),
            "devtest-apple",
            Fail,
            "rejected (W&B names no user for it): not the config's WANDB_API_KEY — `arena pods copy-keys apple`",
        ),
        ("an old key that works", wb_lines("old-wandb-key", 200, "yes"), "devtest-apple", Warn, "works, but not the config's WANDB_API_KEY — `arena pods copy-keys apple`"),
        ("missing", "wandb.key=missing\n".into(), "devtest-apple", Fail, "missing — `arena pods copy-keys apple`"),
        // A GraphQL error (HTTP 200 too), an empty answer: no user, so nothing said of the key.
        (
            "200 with no user",
            wb_lines(WB_GOOD, 200, "unknown") + "wandb.error=permission denied\n",
            "devtest-apple",
            Warn,
            "not checked (no user in W&B's answer: permission denied) — `arena pods test --keys apple`",
        ),
        (
            "200 from an older script, no viewer said",
            format!("wandb.key=set\nwandb.fingerprint={}\nwandb.curl_exit=0\nwandb.http=200\n", fingerprint(WB_GOOD)),
            "devtest-apple",
            Warn,
            "not checked (no user in W&B's answer: ?) — `arena pods test --keys apple`",
        ),
        (
            "unreachable",
            "wandb.key=set\nwandb.curl_exit=6\nwandb.http=000\nwandb.error=curl: (6) Could not resolve host: api.wandb.ai\n".into(),
            "devtest-apple",
            Warn,
            "not checked (unreachable: curl: (6) Could not resolve hos…) — `arena pods test --keys apple`",
        ),
        ("a staff box", "wandb.key=missing\n".into(), "bob", Skip, "staff box: its token is the organisers' choice (none here)"),
    ];
    for (what, lines, name, status, detail) in cases {
        let c = wb(&facts(&lines), name, &b);
        assert_eq!((c.name.as_str(), c.status, c.detail.as_str()), (WANDB_CHECK, status, detail), "{what}");
    }
    // No W&B key configured: a note whatever the machine has, never a verdict.
    let none = KeyBook::new(&Config::parse("MACHINE_NAME_PREFIX=devtest\n"), &[], None, &[]);
    let c = wb(&facts(&wb_lines(WB_GOOD, 200, "yes")), "devtest-apple", &none);
    assert_eq!((c.status, c.detail.as_str()), (Skip, "no WANDB_API_KEY configured for this fleet (one here, accepted)"));
    assert!(!none.any());
    // A machine's own row wins.
    let own = KeyBook::new(&cfg, &[], None, &[]).with_wandb_rows(&[("devtest-apple".into(), "own-wandb-key".into())]);
    assert_eq!(wb(&facts(&wb_lines("own-wandb-key", 200, "yes")), "devtest-apple", &own).status, Pass);
    assert_eq!(
        wb(&facts(&wb_lines(WB_GOOD, 200, "yes")), "devtest-apple", &own).detail,
        "works, but not its wandb_api_keys.csv row's — `arena pods copy-keys apple`"
    );
    // The W&B column shows once a machine's key was judged.
    let policy = HealthPolicy { keys: Some(b), ..HealthPolicy::default() };
    let h = PodHealth::keys_checked(&pod("devtest-apple"), facts(&wb_lines(WB_GOOD, 200, "no")), &policy);
    assert_eq!(wandb_cell(&h).as_deref(), Some("rejected"));
    assert!(render_key_report(&[h], false).starts_with("NAME           RESULT  AI KEY  HF  W&B       NOTES\n"));
    assert_eq!(failure_kind_of(WANDB_CHECK, None), FailureKind::Key);
}

/// The fleet's own files: the keys CSVs beside the config, its HF_TOKEN, the spend ledger's
/// owners — fingerprints only.
#[test]
fn the_book_loads_the_fleets_keys_and_owners() {
    let dir = std::env::temp_dir().join(format!("arena-keybook-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("keys")).unwrap();
    std::fs::write(dir.join("config.env"), format!("MACHINE_NAME_PREFIX=devtest\nHF_TOKEN={HF_GOOD}\n")).unwrap();
    std::fs::write(dir.join("keys/openrouter_api_keys.csv"), format!("host,key\ndevtest-apple,{GOOD}\n")).unwrap();
    std::fs::write(dir.join("keys/huggingface_api_keys.csv"), "devtest-bloom,hf_own\n").unwrap();
    let ledger = dir.join("spend.json");
    let mut state = crate::spend::SpendState::default();
    let pod = Pod { id: "p1".into(), name: "devtest-apple".into(), provider: "runpod".into(), status: "RUNNING".into(), ..Default::default() };
    let alice = crate::spend::OwnerRule::Given("alice".into());
    state = crate::spend::apply_event(state, &crate::spend::Event::Created { pod: &pod, owner: &alice }, 100);
    std::fs::write(&ledger, crate::spend::to_json(&state)).unwrap();
    let cfg = Config::load(&dir.join("config.env")).unwrap();
    let b = KeyBook::load(&cfg, Some(&ledger));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(b.csv.get("devtest-apple"), Some(&fingerprint(GOOD)));
    assert_eq!((b.openrouter, b.hf.clone()), (true, Some(fingerprint(HF_GOOD))));
    assert_eq!(b.hf_csv.get("devtest-bloom"), Some(&fingerprint("hf_own")));
    assert_eq!(b.owners.get("runpod:p1").map(String::as_str), Some("alice"));
    // No secret in it.
    assert!(!format!("{b:?}").contains(GOOD) && !format!("{b:?}").contains(HF_GOOD));
}

/// A key FAIL is the key's: `pods test --deep` still fails on it, but it is never the machine's
/// — not a host failure, not a software one; the machine's own verdict ignores it.
#[test]
fn a_dead_key_fails_the_check_but_not_the_machine() {
    let policy = HealthPolicy { keys: Some(book()), ..HealthPolicy::default() };
    // A CPU VM (no GPU checks to fixture) that passes everything but its key.
    let vm = Pod { provider: "hetzner".into(), ..pod("devtest-apple") };
    let f = facts(&format!(
        "load1=0.1\ncpus=2\npython=/x/python\npy_exit=0\npy_done=1\ntorch=ok\ntorch_version=2.9\ntorch_cuda=none\n\
         disk.root_avail_kb=104857600\nnet.curl_exit=0\nnet.http=206\nnet.bytes=33554432\nnet.secs=0.5\n\
         vscode.server=none\nvscode.servers=0\nvscode.python=unset\n{}{}",
        or_lines(GOOD, 401, ""),
        hf_lines(HF_GOOD, 200)
    ));
    let h = PodHealth::checked(&vm, f, &policy);
    assert_eq!(h.status, Status::Fail, "{:#?}", h.checks);
    assert_eq!(h.machine_status(), Status::Pass);
    assert_eq!(h.key_status(), Some(Status::Fail));
    assert_eq!(h.failure_kind(), Some(FailureKind::Key));
    assert_eq!((failure_kind_of(AI_KEY, None), failure_kind_of(HF_TOKEN_CHECK, None)), (FailureKind::Key, FailureKind::Key));
    assert!(h.notes().starts_with("ai_key: rejected (HTTP 401): paused? `arena keys resume apple`"), "{}", h.notes());
    // Never a bad host: no machine-level evidence, and a check that ran clears an old record.
    assert!(h.checks.iter().all(|c| !crate::health::condemns_machine(c, h.facts.as_ref().unwrap())));
    let h = PodHealth { host: Some("hetzner machine 1".into()), ..h };
    assert_eq!(crate::snapshot::clears_bad_host(&h), Some("hetzner machine 1"));
    // The summary says it's the key, not the host.
    let summary = render_summary(std::slice::from_ref(&h));
    assert_eq!(summary[0], "0 pass, 0 warn, 1 fail");
    assert!(
        summary[1].starts_with("AI key failure, not the machine: devtest-apple — NOTES (or -v) says what to run"),
        "{summary:?}"
    );
    // The policy without a book (up's check before its keys stage): no key lines at all.
    let f = facts(&or_lines(GOOD, 401, ""));
    let plain = PodHealth::checked(&vm, f, &HealthPolicy::default());
    assert!(plain.checks.iter().all(|c| !is_key_check(&c.name)), "{:#?}", plain.checks);
}

/// `up --check` folds a later keys-only run into the machine's check.
#[test]
fn a_keys_run_merges_into_the_machines_check() {
    let policy = HealthPolicy { keys: Some(book()), ..HealthPolicy::default() };
    let apple = pod("devtest-apple");
    let mut machine = PodHealth::checked(&apple, facts("load1=0.1\n"), &HealthPolicy::default());
    let before = machine.status;
    let keys = PodHealth::keys_checked(&apple, facts(&(or_lines(GOOD, 401, "") + &hf_lines(HF_GOOD, 200))), &policy);
    assert_eq!(keys.status, Status::Fail);
    machine.merge_keys(&keys);
    assert_eq!(machine.status, Status::Fail);
    assert_eq!(machine.machine_status(), before);
    assert_eq!(machine.checks.iter().filter(|c| is_key_check(&c.name)).count(), 3);
    assert_eq!(machine.facts.as_ref().unwrap().openrouter.as_ref().unwrap().http, Some(401));
    // Merged again (a second run): replaced, not doubled.
    let fixed = PodHealth::keys_checked(&apple, facts(&(or_lines(GOOD, 200, "") + &hf_lines(HF_GOOD, 200))), &policy);
    machine.merge_keys(&fixed);
    assert_eq!(machine.checks.iter().filter(|c| is_key_check(&c.name)).count(), 3);
    assert_eq!(machine.key_status(), Some(Status::Pass));
    // A keys run whose script never started: one `script` FAIL, nothing else.
    let none = PodHealth::keys_checked(&apple, parse_deep(""), &policy);
    assert_eq!(none.checks.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["script"]);
}

/// The table: an AI KEY column only when some pod's key was judged, with the money left or what
/// went wrong; `--keys`' own table.
#[test]
fn the_reports_show_the_key() {
    let policy = HealthPolicy { keys: Some(book()), ..HealthPolicy::default() };
    let money = "openrouter.limit=1\nopenrouter.limit_remaining=0.42\n";
    let ok = PodHealth::keys_checked(&pod("devtest-apple"), facts(&(or_lines(GOOD, 200, money) + &hf_lines(HF_GOOD, 200))), &policy);
    let dead = PodHealth::keys_checked(&pod("devtest-bloom"), facts(&(or_lines(OLD, 401, "") + "hf.key=missing\n")), &policy);
    assert_eq!(ai_key_cell(&ok).as_deref(), Some("$0.42 left"));
    assert_eq!(ai_key_cell(&dead).as_deref(), Some("rejected"));
    assert_eq!((hf_token_cell(&ok).as_deref(), hf_token_cell(&dead).as_deref()), (Some("ok"), Some("missing")));
    assert_eq!(
        render_key_report(&[ok.clone(), dead.clone()], false),
        "\
NAME           RESULT  AI KEY      HF       NOTES
devtest-apple  pass    $0.42 left  ok
devtest-bloom  fail    rejected    missing  ai_key: rejected (HTTP 401), and no keys-CSV row for it — `arena keys gen bloom --copy`; hf_token: missing — …

1 pass, 0 warn, 1 fail
AI key failure, not the machine: devtest-bloom — NOTES (or -v) says what to run; a machine is never replaced for its key.
"
    );
    let table = render_health_table(std::slice::from_ref(&ok));
    assert!(table.starts_with("NAME           RESULT  GPUS  DRIVER  CUDA  NET  AI KEY      NOTES\n"), "{table}");
    // Nothing judged (a fleet without keys): the table it always had.
    let unjudged = PodHealth::keys_checked(&pod("devtest-apple"), facts("openrouter.key=missing\n"), &HealthPolicy::default());
    assert_eq!(ai_key_cell(&unjudged), None);
    assert!(render_health_table(&[unjudged]).starts_with("NAME           RESULT  GPUS  DRIVER  CUDA  NET  NOTES\n"));
}

// ---- the script itself, against a local stub server (real curl; no network) ----

#[cfg(target_os = "linux")]
fn have(tool: &str) -> bool {
    std::process::Command::new("sh").args(["-c", &format!("command -v {tool}")]).output().is_ok_and(|o| o.status.success())
}

/// A stand-in for OpenRouter's `GET /api/v1/key` and Hugging Face's `whoami-v2` on 127.0.0.1:
/// answers by the Bearer token it's sent (`answers`: token → status and body; any other gets
/// OpenRouter's 401), and keeps each request's head — so a test sees what curl actually sent.
#[cfg(target_os = "linux")]
struct StubServer {
    url: String,
    requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

#[cfg(target_os = "linux")]
fn stub_server(answers: Vec<(&'static str, u16, String)>) -> StubServer {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = requests.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            let mut head = Vec::new();
            let mut buf = [0u8; 1024];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 16384 {
                match conn.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => head.extend_from_slice(&buf[..n]),
                }
            }
            let head = String::from_utf8_lossy(&head).to_string();
            let token = head
                .lines()
                .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("authorization")).map(|(_, v)| v.trim().to_string()))
                .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("Basic ")).map(str::to_string))
                .unwrap_or_default();
            seen.lock().unwrap().push(head);
            let (status, body) = answers
                .iter()
                .find(|(t, _, _)| *t == token)
                .map(|(_, s, b)| (*s, b.clone()))
                .unwrap_or((401, r#"{"error":{"message":"No auth credentials found","code":401}}"#.into()));
            let _ = write!(
                conn,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    StubServer { url, requests }
}

/// The OpenRouter answer for a live key, its redacted label included (the script must never
/// print it): the shape `GET /api/v1/key` returned on 2026-11-02.
#[cfg(target_os = "linux")]
fn key_answer(label: &str, limit: &str, left: &str) -> String {
    format!(
        r#"{{"data":{{"label":"{label}","limit":{limit},"usage":0.0021,"is_free_tier":false,"limit_remaining":{left},"limit_reset":null,"is_provisioning_key":false,"rate_limit":{{"requests":-1,"interval":"10s"}}}}}}"#
    )
}

/// Run the delivered keys-only command (`key_check_inner_command`) with `env` set (the keys
/// unset unless given), its calls pointed at `server`, with `path_first` first on PATH.
#[cfg(target_os = "linux")]
fn run_keys(server: &StubServer, env: &[(&str, &str)], path_first: Option<&std::path::Path>) -> (String, String) {
    let home = std::env::temp_dir().join(format!("arena-keys-home-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let path = match path_first {
        Some(p) => format!("{}:{}", p.display(), std::env::var("PATH").unwrap_or_default()),
        None => std::env::var("PATH").unwrap_or_default(),
    };
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c")
        .arg(crate::health::key_check_inner_command())
        .env("PATH", path)
        .env("HOME", &home)
        .env("no_proxy", "*")
        .env("NO_PROXY", "*")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("HF_TOKEN")
        .env_remove("WANDB_API_KEY")
        .env("ARENA_TEST_OPENROUTER_URL", format!("{}/api/v1/key", server.url))
        .env("ARENA_TEST_HF_URL", format!("{}/api/whoami-v2", server.url))
        .env("ARENA_TEST_WANDB_URL", format!("{}/graphql", server.url));
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    (String::from_utf8_lossy(&out.stdout).into(), String::from_utf8_lossy(&out.stderr).into())
}

/// End to end: the keys-only run asks the stub with each key as its Bearer token, reports what
/// came back and the keys' fingerprints — and no secret (nor OpenRouter's label of it) in
/// anything it prints. Then judged: a live key with money left passes, a dead one FAILs.
#[cfg(target_os = "linux")]
#[test]
fn the_script_asks_each_service_and_never_prints_a_secret() {
    if !(have("bash") && have("curl") && have("sha256sum") && have("base64") && have("timeout")) {
        eprintln!("bash/curl/sha256sum/base64/timeout missing — skipping");
        return;
    }
    let label = "sk-or-v1-012...ood";
    let server = stub_server(vec![
        (GOOD, 200, key_answer(label, "0.05", "0.0479")),
        (OLD, 401, r#"{"error":{"message":"User not found.","code":401}}"#.into()),
        (HF_GOOD, 200, r#"{"type":"user","name":"arena-bot","auth":{"accessToken":{"role":"read"}}}"#.into()),
    ]);
    let policy = HealthPolicy { keys: Some(book()), ..HealthPolicy::default() };
    let apple = pod("devtest-apple");

    // A live key and token.
    let (stdout, stderr) = run_keys(&server, &[("OPENROUTER_API_KEY", GOOD), ("HF_TOKEN", HF_GOOD)], None);
    for secret in [GOOD, HF_GOOD, label, "012...ood"] {
        assert!(!stdout.contains(secret) && !stderr.contains(secret), "{secret} printed:\n{stdout}\n{stderr}");
    }
    for line in stdout.lines() {
        assert!(line.contains('='), "stray stdout line {line:?}");
    }
    let f = parse_deep(&stdout);
    assert!(f.started && f.complete, "{stdout}\n{stderr}");
    let k = f.openrouter.clone().unwrap();
    assert_eq!(
        (k.set, k.fingerprint.as_deref(), k.http, k.limit, k.limit_remaining, k.limit_reset.as_deref(), k.usage),
        (Some(true), Some(fingerprint(GOOD).as_str()), Some(200), Some(0.05), Some(0.0479), None, Some(0.0021)),
        "{stdout}"
    );
    assert_eq!(f.hf.as_ref().map(|h| (h.set, h.http)), Some((Some(true), Some(200))), "{stdout}");
    let h = PodHealth::keys_checked(&apple, f, &policy);
    assert_eq!(h.status, Status::Pass, "{:#?}", h.checks);
    assert_eq!(h.checks[0].detail, "ok, $0.048 of $0.050 left");
    // Each service got its own key as a Bearer token (from curl's stdin config, `-K -`).
    let requests = server.requests.lock().unwrap().clone();
    assert!(requests.iter().any(|r| r.starts_with("GET /api/v1/key ") && r.contains(&format!("Bearer {GOOD}"))), "{requests:#?}");
    assert!(requests.iter().any(|r| r.starts_with("GET /api/whoami-v2 ") && r.contains(&format!("Bearer {HF_GOOD}"))), "{requests:#?}");

    // A deleted key: OpenRouter's 401 and its message.
    let (stdout, stderr) = run_keys(&server, &[("OPENROUTER_API_KEY", OLD)], None);
    assert!(!stdout.contains(OLD) && !stderr.contains(OLD), "{stdout}\n{stderr}");
    let f = parse_deep(&stdout);
    let k = f.openrouter.clone().unwrap();
    assert_eq!((k.http, k.error.as_deref()), (Some(401), Some("User not found.")), "{stdout}");
    assert_eq!(f.hf.as_ref().and_then(|h| h.set), Some(false));
    let h = PodHealth::keys_checked(&apple, f, &policy);
    assert_eq!(
        h.checks.iter().map(|c| (c.name.as_str(), c.status)).collect::<Vec<_>>(),
        [(AI_KEY, Status::Fail), (HF_TOKEN_CHECK, Status::Fail), (WANDB_CHECK, Status::Skip)]
    );

    // Nothing exported: both missing, nothing asked.
    let before = server.requests.lock().unwrap().len();
    let (stdout, _) = run_keys(&server, &[], None);
    assert!(stdout.contains("\nopenrouter.key=missing\n") && stdout.contains("\nhf.key=missing\n"), "{stdout}");
    assert_eq!(server.requests.lock().unwrap().len(), before);

    // OpenRouter unreachable: curl's error, a WARN (not checked), never a FAIL.
    let dead = StubServer { url: "http://127.0.0.1:9".into(), requests: Default::default() };
    let (stdout, _) = run_keys(&dead, &[("OPENROUTER_API_KEY", GOOD)], None);
    let h = PodHealth::keys_checked(&apple, parse_deep(&stdout), &policy);
    assert_eq!(h.checks[0].status, Status::Warn, "{stdout}\n{:#?}", h.checks);
    assert!(h.checks[0].detail.starts_with("not checked (unreachable: curl: (7)"), "{}", h.checks[0].detail);
}

/// Nothing the services or curl say can carry a secret out: a 401 whose message echoes the key
/// (past where the message is clipped), OpenRouter's label of it, a bearer token and the HF
/// token comes out scrubbed — and a `~/.curlrc` asking for a trace of the request (which would
/// hold the Authorization header) is never read.
#[cfg(target_os = "linux")]
#[test]
fn no_message_or_curl_config_leaks_a_secret() {
    if !(have("bash") && have("curl") && have("sha256sum") && have("base64") && have("timeout")) {
        eprintln!("bash/curl/sha256sum/base64/timeout missing — skipping");
        return;
    }
    let pad = "x".repeat(150);
    let echoed = format!(
        r#"{{"error":{{"message":"{pad} bad key {GOOD}, label sk-or-v1-012...ood, sent Bearer {GOOD} with hf {HF_GOOD}","code":401}}}}"#
    );
    let server = stub_server(vec![(GOOD, 401, echoed.clone()), (HF_GOOD, 401, format!(r#"{{"error":"no such token {HF_GOOD}"}}"#))]);
    // A home whose ~/.curlrc would trace every request (headers included) to a file.
    let home = std::env::temp_dir().join(format!("arena-keys-curlrc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let trace = home.join("curl.trace");
    std::fs::write(home.join(".curlrc"), format!("trace-ascii = \"{}\"\n", trace.display())).unwrap();
    let (stdout, stderr) = run_keys(&server, &[("OPENROUTER_API_KEY", GOOD), ("HF_TOKEN", HF_GOOD), ("HOME", home.to_str().unwrap())], None);
    let traced = trace.exists();
    let _ = std::fs::remove_dir_all(&home);
    assert!(!traced, "~/.curlrc was read: the request was traced");
    let out = format!("{stdout}\n{stderr}");
    for leak in [&GOOD[..12], &GOOD[GOOD.len() - 8..], &HF_GOOD[..10], "012...ood", "Bearer sk"] {
        assert!(!out.contains(leak), "{leak:?} printed:\n{out}");
    }
    let f = parse_deep(&stdout);
    let k = f.openrouter.unwrap();
    assert_eq!(k.http, Some(401), "{stdout}");
    let e = k.error.unwrap();
    assert!(e.contains("bad key") && e.chars().count() <= 160, "{e}");
    assert_eq!(f.hf.unwrap().error.as_deref(), Some("no such token [secret]"), "{stdout}");
}

/// A login environment that turns bash tracing on (an exported `SHELLOPTS=xtrace`, as a
/// debugging session can leave behind) traces nothing of the script: it would print the keys.
#[cfg(target_os = "linux")]
#[test]
fn inherited_tracing_prints_no_secret() {
    if !(have("bash") && have("curl") && have("sha256sum") && have("base64") && have("timeout")) {
        eprintln!("bash/curl/sha256sum/base64/timeout missing — skipping");
        return;
    }
    let server = stub_server(vec![(GOOD, 200, key_answer("sk-or-v1-012...ood", "1", "1")), (HF_GOOD, 200, "{}".into())]);
    let (stdout, stderr) = run_keys(&server, &[("OPENROUTER_API_KEY", GOOD), ("HF_TOKEN", HF_GOOD), ("SHELLOPTS", "xtrace")], None);
    assert!(stderr.lines().any(|l| l.starts_with('+')), "tracing was on at the start (or the test proves nothing): {stderr}");
    for secret in [GOOD, HF_GOOD] {
        assert!(!stdout.contains(secret) && !stderr.contains(secret), "traced:\n{stderr}");
    }
    assert_eq!(parse_deep(&stdout).openrouter.and_then(|k| k.http), Some(200), "{stdout}");
}

/// The secret is on no command line: a `curl` first on PATH records its arguments and execs
/// the real one — they carry the URL and `-K -`, never the key (which `ps` would show).
#[cfg(target_os = "linux")]
#[test]
fn the_secret_is_on_no_command_line() {
    if !(have("bash") && have("curl") && have("sha256sum") && have("base64") && have("timeout")) {
        eprintln!("bash/curl/sha256sum/base64/timeout missing — skipping");
        return;
    }
    let real = String::from_utf8(std::process::Command::new("sh").args(["-c", "command -v curl"]).output().unwrap().stdout).unwrap();
    let dir = std::env::temp_dir().join(format!("arena-keys-argv-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("argv.log");
    crate::remote::write_test_script(
        dir.join("curl"),
        format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexec '{}' \"$@\"\n", log.display(), real.trim()),
    )
    .unwrap();
    let server = stub_server(vec![(GOOD, 200, key_answer("sk-or-v1-012...ood", "1", "1"))]);
    let (stdout, _) = run_keys(&server, &[("OPENROUTER_API_KEY", GOOD), ("HF_TOKEN", HF_GOOD)], Some(&dir));
    let argv = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(argv.lines().count(), 2, "one call each: {argv}\n{stdout}");
    assert!(argv.lines().all(|l| l.contains("-K -")), "{argv}");
    assert!(!argv.contains(GOOD) && !argv.contains(HF_GOOD), "a secret on curl's command line:\n{argv}");
    assert_eq!(parse_deep(&stdout).openrouter.and_then(|k| k.http), Some(200), "{stdout}");
}

/// The deep run carries the keys-only run's facts too (its keys section runs beside the network
/// probe), and the delivered keys command is the same script in the same login wrap.
#[test]
fn the_keys_command_is_the_deep_script_in_its_keys_mode() {
    let cmd = crate::health::key_check_command(Some("arena-env"));
    assert!(cmd.contains("conda activate arena-env"), "{cmd}");
    assert!(cmd.contains(&crate::base64::encode(crate::health::deep_check_script().as_bytes())));
    assert!(crate::health::key_check_inner_command().ends_with("arena-deep-check keys </dev/null"));
    assert!(crate::health::deep_check_inner_command().ends_with("arena-deep-check </dev/null"));
}

/// W&B's check end to end: the key goes as basic auth from curl's stdin config (never on its
/// command line, never printed), a user in the answer is accepted, `"viewer": null` rejected.
#[cfg(target_os = "linux")]
#[test]
fn the_script_asks_wandb_with_the_key_on_stdin() {
    if !(have("bash") && have("curl") && have("sha256sum") && have("base64") && have("timeout")) {
        eprintln!("bash/curl/sha256sum/base64/timeout missing — skipping");
        return;
    }
    let basic = |k: &str| crate::base64::encode(format!("api:{k}").as_bytes());
    let good: &'static str = Box::leak(basic(WB_GOOD).into_boxed_str());
    let bad: &'static str = Box::leak(basic("not-a-wandb-key-0000000000000000000000").into_boxed_str());
    let server = stub_server(vec![
        (good, 200, r#"{"data":{"viewer":{"id":"VXNlcjoxMjM0"}}}"#.into()),
        (bad, 200, r#"{"data":{"viewer":null}}"#.into()),
    ]);
    let cfg = Config::parse(&format!("MACHINE_NAME_PREFIX=devtest\nWANDB_API_KEY={WB_GOOD}\n"));
    let policy = HealthPolicy { keys: Some(KeyBook::new(&cfg, &[], None, &[])), ..HealthPolicy::default() };
    let (stdout, stderr) = run_keys(&server, &[("WANDB_API_KEY", WB_GOOD)], None);
    assert!(!stdout.contains(WB_GOOD) && !stderr.contains(WB_GOOD), "{stdout}\n{stderr}");
    let f = parse_deep(&stdout);
    assert_eq!(f.wandb.as_ref().map(|k| (k.http, k.viewer)), Some((Some(200), Some(true))), "{stdout}");
    let h = PodHealth::keys_checked(&pod("devtest-apple"), f, &policy);
    assert_eq!(h.checks.iter().find(|c| c.name == WANDB_CHECK).map(|c| c.status), Some(Status::Pass), "{:#?}", h.checks);
    let requests = server.requests.lock().unwrap().clone();
    assert!(requests.iter().any(|r| r.starts_with("POST /graphql ") && r.contains(&format!("Basic {good}"))), "{requests:#?}");
    let (stdout, _) = run_keys(&server, &[("WANDB_API_KEY", "not-a-wandb-key-0000000000000000000000")], None);
    let f = parse_deep(&stdout);
    assert_eq!(f.wandb.as_ref().and_then(|k| k.viewer), Some(false), "{stdout}");
    let h = PodHealth::keys_checked(&pod("devtest-apple"), f, &policy);
    assert_eq!(h.checks.iter().find(|c| c.name == WANDB_CHECK).map(|c| c.status), Some(Status::Fail));
}
