//! Build-provenance checks against real attestations: the ones GitHub keeps for releases
//! v2.0.261028.1916 and v2.0.261028.1827 (`fixtures/attestation_<tag>.json`, the API's answer
//! for each one's `arena-x86_64-unknown-linux-gnu`, its short-lived `bundle_url` removed). Each
//! release's one statement names its four binaries. Every refusal below is one of those
//! bundles, or what is expected of it, changed in one way. No network: the source is canned.

use super::*;
use serde_json::json;

const FIXTURE: &str = include_str!("fixtures/attestation_v2.0.261028.1916.json");
const PREVIOUS: &str = include_str!("fixtures/attestation_v2.0.261028.1827.json");

/// v2.0.261028.1916's binaries and their SHA-256s (its SHA256SUMS).
const SUBJECTS: [(&str, &str); 4] = [
    ("arena-aarch64-unknown-linux-gnu", "21730a75a1e3aa6a581524a0b617c7b553cc241b9d3146129e54a70e1ea073bc"),
    ("arena-tui-aarch64-unknown-linux-gnu", "2975bf93e57dd2c91cfc1b41eb21d2d30646faa9b0a0fa5a072bb2ba172ec21e"),
    ("arena-tui-x86_64-unknown-linux-gnu", "e39126f6b558c25094af61dbb7e5012db80e5bc38c717205c63b1802f764c87e"),
    ("arena-x86_64-unknown-linux-gnu", "351ad4235e7163a53effb9acf1cd16628bb7c26e438f6f2c0e7b089d0cf0196e"),
];
const NAME: &str = SUBJECTS[3].0;
const SHA: &str = SUBJECTS[3].1;
const TUI: &str = SUBJECTS[2].0;
const TUI_SHA: &str = SUBJECTS[2].1;
/// v2.0.261028.1827's x86_64 binaries (its SHA256SUMS): genuine, attested, and older.
const PREV_SHA: &str = "fdcfeaa9f1a8371ca2a5ca8ecbead2309de68596a8131392244c253c79e29722";
const PREV_TUI_SHA: &str = "272d3c8849b2e3e85313e18a97de1aa3365b2e2ff9a40dea5aeba1d9a1e63a44";
/// Some other file's digest.
const OTHER: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

fn answer() -> Value {
    serde_json::from_str(FIXTURE).unwrap()
}

fn bundle() -> Value {
    answer()["attestations"][0]["bundle"].clone()
}

fn previous_bundle() -> Value {
    serde_json::from_str::<Value>(PREVIOUS).unwrap()["attestations"][0]["bundle"].clone()
}

/// The bundle's in-toto statement, decoded.
fn statement(b: &Value) -> Value {
    let payload = crate::base64::decode(b["dsseEnvelope"]["payload"].as_str().unwrap()).unwrap();
    serde_json::from_slice(&payload).unwrap()
}

fn with_statement(mut b: Value, s: &Value) -> Value {
    b["dsseEnvelope"]["payload"] = Value::String(crate::base64::encode(&serde_json::to_vec(s).unwrap()));
    b
}

/// The bundle with its certificate's bytes `from` replaced by `to` (same length, so it still
/// parses; its signature no longer holds).
fn with_cert_bytes(mut b: Value, from: &[u8], to: &[u8]) -> Value {
    let raw = &mut b["verificationMaterial"]["certificate"]["rawBytes"];
    let mut der = crate::base64::decode(raw.as_str().unwrap()).unwrap();
    let at = der.windows(from.len()).position(|w| w == from).unwrap();
    der[at..at + from.len()].copy_from_slice(to);
    *raw = Value::String(crate::base64::encode(&der));
    b
}

fn check(b: &Value, expected: &Expected, files: &[(&str, &str)]) -> Result<Proof> {
    verify_bundle(&verifier().unwrap(), b, expected, files)
}

/// Attestations from memory, the same answer for any digest: `None` is an API that can't be
/// asked.
struct Canned(Option<Value>);

#[async_trait]
impl Source for Canned {
    async fn attestations(&self, _sha256: &str) -> Result<Value> {
        self.0.clone().ok_or_else(|| "HTTP 403 Forbidden: API rate limit exceeded".to_string())
    }
}

#[test]
fn the_recorded_release_attestation_proves_its_binaries_together_and_each_alone() {
    let all: Vec<(&str, &str)> = SUBJECTS.to_vec();
    let proofs = std::iter::once(check(&bundle(), &RELEASE, &all)).chain(SUBJECTS.iter().map(|f| check(&bundle(), &RELEASE, &[*f])));
    for proof in proofs {
        let proof = proof.unwrap();
        let s = &proof.signer;
        assert_eq!(s.identity.as_deref(), Some("https://github.com/nickypro/arena-infra-rs/.github/workflows/release.yml@refs/heads/master"));
        assert_eq!((s.trigger.as_deref(), s.commit.as_deref()), (Some("workflow_run"), Some("7917eb2b6cc1652ee05355569e101826649b1359")));
        assert_eq!(
            proof.to_string(),
            "built by https://github.com/nickypro/arena-infra-rs/.github/workflows/release.yml@refs/heads/master, \
             run https://github.com/nickypro/arena-infra-rs/actions/runs/40753802298/attempts/1, logged 2026-10-28T19:25:45Z"
        );
    }
    let previous = check(&previous_bundle(), &RELEASE, &[(NAME, PREV_SHA), (TUI, PREV_TUI_SHA)]).unwrap();
    assert_eq!(previous.logged.as_deref(), Some("2026-10-28T18:38:34Z"));
}

/// The statement edited to name another file's digest: its signature no longer matches. And
/// the real statement doesn't vouch for a file it doesn't name.
#[test]
fn a_tampered_subject_digest_is_refused() {
    let mut s = statement(&bundle());
    let subject = s["subject"].as_array_mut().unwrap().iter_mut().find(|x| x["name"] == NAME).unwrap();
    subject["digest"]["sha256"] = json!(OTHER);
    let e = check(&with_statement(bundle(), &s), &RELEASE, &[(NAME, OTHER)]).unwrap_err();
    assert!(e.starts_with("Sigstore verification failed: invalid signature: DSSE signature verification failed"), "{e}");

    let e = check(&bundle(), &RELEASE, &[(NAME, OTHER)]).unwrap_err();
    assert!(e.contains("does not match any subject"), "{e}");
}

/// A genuine new `arena` with a genuine older `arena-tui` (or the other way round): each is
/// attested, but no one statement names both, so they weren't built together.
#[tokio::test]
async fn binaries_from_different_releases_are_refused() {
    let v = verifier().unwrap();
    let e = verify(&Canned(Some(answer())), &v, &RELEASE, &[(NAME, SHA), (TUI, PREV_TUI_SHA)]).await.unwrap_err();
    assert!(e.ends_with(&format!("it names {TUI} with sha256 {TUI_SHA}, not this file's {PREV_TUI_SHA}")), "{e}");
    assert!(e.starts_with(&format!("{NAME} and {TUI}: no attestation proves they were built by ")), "{e}");

    let previous = serde_json::from_str::<Value>(PREVIOUS).unwrap();
    let e = verify(&Canned(Some(previous.clone())), &v, &RELEASE, &[(NAME, PREV_SHA), (TUI, TUI_SHA)]).await.unwrap_err();
    assert!(e.ends_with(&format!("not this file's {TUI_SHA}")), "{e}");
    assert!(verify(&Canned(Some(previous)), &v, &RELEASE, &[(NAME, PREV_SHA), (TUI, PREV_TUI_SHA)]).await.is_ok());
}

/// Expecting another workflow of the repository, or the release workflow on another ref
/// (release.yml's own pull-request runs are on `refs/pull/N/merge`): the certificate's
/// identity isn't that.
#[test]
fn a_wrong_workflow_or_ref_is_refused() {
    for expected in [
        Expected { workflow: ".github/workflows/ci.yml", ..RELEASE },
        Expected { git_ref: "refs/heads/main", ..RELEASE },
        Expected { git_ref: "refs/pull/18/merge", ..RELEASE },
    ] {
        let e = check(&bundle(), &expected, &[(NAME, SHA)]).unwrap_err();
        assert!(e.contains("identity mismatch") && e.contains(&expected.identity()), "{e}");
    }
    let dispatched = Expected { trigger: "workflow_dispatch", ..RELEASE };
    let e = check(&bundle(), &dispatched, &[(NAME, SHA)]).unwrap_err();
    assert_eq!(e, "its certificate's trigger is workflow_run, not workflow_dispatch");
}

/// Another repository's release workflow (a fork's, say) isn't this one's; nor is a
/// repository of the same name with another id (deleted and re-created by someone else).
#[test]
fn a_wrong_repo_is_refused() {
    let fork = Expected { repo: "someone/arena-infra-rs", ..RELEASE };
    let e = check(&bundle(), &fork, &[(NAME, SHA)]).unwrap_err();
    assert!(e.contains("identity mismatch") && e.contains("someone/arena-infra-rs"), "{e}");

    let recreated = Expected { repo_id: "999999999", ..RELEASE };
    let e = check(&bundle(), &recreated, &[(NAME, SHA)]).unwrap_err();
    assert_eq!(e, "its certificate's repository id is 1368824951, not 999999999");
}

/// One bit of the envelope's signature flipped.
#[test]
fn a_tampered_signature_is_refused() {
    let mut b = bundle();
    let mut sig = crate::base64::decode(b["dsseEnvelope"]["signatures"][0]["sig"].as_str().unwrap()).unwrap();
    let mid = sig.len() / 2;
    sig[mid] ^= 1;
    b["dsseEnvelope"]["signatures"][0]["sig"] = Value::String(crate::base64::encode(&sig));
    let e = check(&b, &RELEASE, &[(NAME, SHA)]).unwrap_err();
    assert!(e.starts_with("Sigstore verification failed: invalid signature: DSSE signature verification failed"), "{e}");
}

/// The certificate edited to name another workflow: its chain no longer verifies.
#[test]
fn a_tampered_certificate_is_refused() {
    let b = with_cert_bytes(bundle(), b"release.yml", b"releasx.yml");
    let e = check(&b, &RELEASE, &[(NAME, SHA)]).unwrap_err();
    assert!(e.starts_with("Sigstore verification failed: "), "{e}");
}

/// The Rekor entry is verified, not just read: a hash of its inclusion proof changed, its
/// integrated time moved (the signed entry timestamp covers it), the proof dropped, or another
/// release's genuine entry (with its own valid proof) put in its place — refused because its
/// time is before this certificate was issued. (Its body, describing another envelope, would
/// fail sigstore-verify's consistency check next; recorded data can't reach that check alone:
/// no second entry logged under the same certificate exists.)
#[test]
fn a_tampered_log_entry_is_refused() {
    let flip = |b64: &str| {
        let mut h = crate::base64::decode(b64).unwrap();
        h[0] ^= 1;
        Value::String(crate::base64::encode(&h))
    };
    let mut proof_hash = bundle();
    let h = &mut proof_hash["verificationMaterial"]["tlogEntries"][0]["inclusionProof"]["hashes"][0];
    *h = flip(h.as_str().unwrap());
    let mut time = bundle();
    time["verificationMaterial"]["tlogEntries"][0]["integratedTime"] = json!("1793215546");
    let mut no_proof = bundle();
    no_proof["verificationMaterial"]["tlogEntries"][0].as_object_mut().unwrap().remove("inclusionProof");
    let mut swapped = bundle();
    swapped["verificationMaterial"]["tlogEntries"] = previous_bundle()["verificationMaterial"]["tlogEntries"].clone();
    for (b, said) in [
        (proof_hash, "invalid inclusion proof: inclusion proof verification failed: root hash mismatch"),
        (time, "invalid inclusion promise: SET verification failed"),
        (no_proof, "bundle must have an inclusion proof"),
        (swapped, "certificate chain is invalid: certificate chain validation failed: CertNotValidYet"),
    ] {
        let e = check(&b, &RELEASE, &[(NAME, SHA)]).unwrap_err();
        assert!(e.starts_with("Sigstore verification failed: ") && e.contains(said), "{said}: {e}");
    }
}

/// No attestation (GitHub's 404, an empty list), entries without a bundle inline, or an API
/// that can't be asked: refused, and the message says which.
#[tokio::test]
async fn a_missing_attestation_is_refused() {
    let v = verifier().unwrap();
    for none in [json!({"attestations": []}), json!({})] {
        let e = verify(&Canned(Some(none)), &v, &RELEASE, &[(NAME, SHA), (TUI, TUI_SHA)]).await.unwrap_err();
        assert_eq!(e, format!("{NAME}: GitHub has no build-provenance attestation for it (sha256 {SHA})"));
    }
    let only_url = json!({"attestations": [{"bundle_url": "https://x"}, {"bundle": null}]});
    let e = verify(&Canned(Some(only_url)), &v, &RELEASE, &[(NAME, SHA)]).await.unwrap_err();
    assert!(e.ends_with("lists 2 attestation(s) for it, none with its bundle inline (this build can't read `bundle_url`)"), "{e}");
    let e = verify(&Canned(None), &v, &RELEASE, &[(NAME, SHA)]).await.unwrap_err();
    assert!(e.contains("couldn't read its build-provenance attestation") && e.contains("rate limit"), "{e}");
}

/// Bundles that aren't a bundle, or that claim another signer, are set aside (said, not
/// verified): the release workflow's among them still proves the binaries; without it, each
/// reason is said, and who the others claim to be.
#[tokio::test]
async fn bundles_of_other_signers_are_set_aside_and_the_workflows_tried() {
    let v = verifier().unwrap();
    let other = with_cert_bytes(bundle(), b"release.yml", b"releasx.yml");
    let mixed = json!({"attestations": [{"bundle": {"mediaType": "nonsense"}}, {"bundle": other}, {"bundle": bundle()}]});
    let proof = verify(&Canned(Some(mixed)), &v, &RELEASE, &[(NAME, SHA), (TUI, TUI_SHA)]).await.unwrap();
    assert_eq!(proof.signer.repo_id.as_deref(), Some("1368824951"));

    let without = json!({"attestations": [{"bundle": {"mediaType": "nonsense"}}, {"bundle": other}]});
    let e = verify(&Canned(Some(without)), &v, &RELEASE, &[(NAME, SHA)]).await.unwrap_err();
    assert!(e.starts_with(&format!("{NAME}: no attestation proves they were built by {}: it isn't a Sigstore bundle", RELEASE.identity())), "{e}");
    assert!(e.ends_with("; none is signed by it (signed by: https://github.com/nickypro/arena-infra-rs/.github/workflows/releasx.yml@refs/heads/master)"), "{e}");

    let many = json!({"attestations": (0..15).map(|_| json!({"bundle": bundle()})).collect::<Vec<_>>()});
    let e = verify(&Canned(Some(many)), &v, &RELEASE, &[(NAME, OTHER)]).await.unwrap_err();
    assert_eq!(e.matches("does not match any subject").count(), MAX_BUNDLES, "at most MAX_BUNDLES are verified: {e}");
}

fn release_signer() -> Signer {
    let wf = "https://github.com/nickypro/arena-infra-rs/.github/workflows/release.yml@refs/heads/master";
    Signer {
        identity: Some(wf.into()),
        issuer: Some(ISSUER.into()),
        repo_uri: Some("https://github.com/nickypro/arena-infra-rs".into()),
        repo_id: Some("1368824951".into()),
        git_ref: Some("refs/heads/master".into()),
        build_config: Some(wf.into()),
        trigger: Some("workflow_run".into()),
        runner: Some("github-hosted".into()),
        ..Signer::default()
    }
}

/// Every pinned claim, one wrong (or missing) at a time; the claims only shown don't matter.
#[test]
fn each_pinned_claim_must_be_the_release_workflows() {
    assert_eq!(RELEASE.check_signer(&release_signer()), Ok(()));
    let shown = Signer { commit: Some("x".into()), run: None, ..release_signer() };
    assert_eq!(RELEASE.check_signer(&shown), Ok(()));
    type Change = fn(&mut Signer);
    let cases: [(Change, &str); 9] = [
        (|s| s.identity = Some("https://github.com/nickypro/arena-infra-rs/.github/workflows/ci.yml@refs/heads/master".into()), "identity is https://github.com/nickypro/arena-infra-rs/.github/workflows/ci.yml@refs/heads/master"),
        (|s| s.issuer = Some("https://accounts.google.com".into()), "OIDC issuer is https://accounts.google.com"),
        (|s| s.repo_uri = Some("https://github.com/someone/arena-infra-rs".into()), "source repository is https://github.com/someone/arena-infra-rs"),
        (|s| s.repo_id = None, "repository id is missing"),
        (|s| s.git_ref = Some("refs/pull/18/merge".into()), "ref is refs/pull/18/merge"),
        (|s| s.build_config = Some("https://github.com/someone/x/.github/workflows/y.yml@refs/heads/main".into()), "workflow is https://github.com/someone/x/"),
        (|s| s.trigger = Some("push".into()), "trigger is push, not workflow_run"),
        (|s| s.runner = Some("self-hosted".into()), "runner is self-hosted, not github-hosted"),
        (|s| s.runner = None, "runner is missing"),
    ];
    for (change, said) in cases {
        let mut s = release_signer();
        change(&mut s);
        let e = RELEASE.check_signer(&s).unwrap_err();
        assert!(e.starts_with("its certificate's ") && e.contains(said), "{said}: {e}");
    }
}

#[test]
fn the_statement_must_be_slsa_provenance_naming_each_file_and_digest() {
    let good = statement(&bundle());
    let text = |v: &Value| serde_json::to_vec(v).unwrap();
    assert_eq!(check_statement(&text(&good), &[(NAME, SHA), (TUI, TUI_SHA)]), Ok(()));
    assert_eq!(check_statement(&text(&good), &[(NAME, &SHA.to_ascii_uppercase())]), Ok(()), "hex case");

    let mut sbom = good.clone();
    sbom["predicateType"] = json!("https://spdx.dev/Document/v2.3");
    let mut old = good.clone();
    old["_type"] = json!("https://in-toto.io/Statement/v0.1");
    let mut empty = good.clone();
    empty["subject"] = json!([]);
    let riscv = "arena-riscv64gc-unknown-linux-gnu";
    type Files<'a> = Vec<(&'a str, &'a str)>;
    let cases: [(Vec<u8>, Files, &str); 7] = [
        (text(&sbom), vec![(NAME, SHA)], "it attests https://spdx.dev/Document/v2.3, not SLSA provenance"),
        (text(&old), vec![(NAME, SHA)], "of type https://in-toto.io/Statement/v0.1"),
        (text(&good), vec![(riscv, SHA)], "doesn't name arena-riscv64gc-unknown-linux-gnu (it names: arena-aarch64-unknown-linux-gnu, "),
        (text(&good), vec![(NAME, OTHER)], "it names arena-x86_64-unknown-linux-gnu with sha256 351ad42"),
        (text(&good), vec![(NAME, SHA), (TUI, OTHER)], "it names arena-tui-x86_64-unknown-linux-gnu with sha256 e39126f"),
        (text(&empty), vec![(NAME, SHA)], "(it names: nothing)"),
        (b"not json".to_vec(), vec![(NAME, SHA)], "isn't JSON"),
    ];
    for (payload, files, said) in cases {
        let e = check_statement(&payload, &files).unwrap_err();
        assert!(e.contains(said), "{said}: {e}");
    }
}

#[test]
fn the_api_answer_reads_as_its_bundles() {
    assert_eq!(bundles(&answer()).len(), 1);
    assert!(bundles(&json!({"attestations": [{"bundle": null}, {"bundle_url": "u"}]})).is_empty());
    assert!(bundles(&json!({"message": "Not Found"})).is_empty());
}

#[test]
fn the_release_workflow_is_this_repositorys() {
    assert_eq!(RELEASE.identity(), "https://github.com/nickypro/arena-infra-rs/.github/workflows/release.yml@refs/heads/master");
    assert!(crate::update::REPO_API.ends_with(&format!("/repos/{}", RELEASE.repo)), "update reads releases from the repository it verifies against");
}

/// The Sigstore trust root compiled in (sigstore-trust-root's copy, which this check trusts
/// for every Fulcio CA, CT log and Rekor key) is the one reviewed: a dependency bump that
/// changes it fails here. Before updating the hash, check the new file is Sigstore's own, byte
/// for byte: `targets/trusted_root.json` in github.com/sigstore/root-signing (the copy its TUF
/// repository serves), not something a hijacked crate release slipped in. Last checked
/// 2026-10-28: identical.
#[test]
fn the_compiled_in_trust_root_is_the_reviewed_one() {
    assert_eq!(crate::update::sha256_hex(SIGSTORE_PRODUCTION_TRUSTED_ROOT.as_bytes()), "6494e21ea73fa7ee769f85f57d5a3e6a08725eae1e38c755fc3517c9e6bc0b66");
}

/// Opt-in, live: the latest release's attestation, read from GitHub (anonymous, 2 requests),
/// proves its x86_64 binaries were built together by the release workflow, and doesn't prove
/// they were built by another workflow or repository. What would catch a Sigstore or GitHub
/// change that the fixtures can't (a rotated key, a new bundle format). No account or key.
/// `ARENA_LIVE_ATTEST=1 cargo test -p arena-core --lib attest -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "network: GitHub's API (set ARENA_LIVE_ATTEST=1)"]
async fn live_the_latest_release_is_proven_and_nothing_else_is() {
    if std::env::var("ARENA_LIVE_ATTEST").as_deref() != Ok("1") {
        eprintln!("skipped: set ARENA_LIVE_ATTEST=1");
        return;
    }
    let client = crate::update::client();
    let api = crate::update::REPO_API;
    let release = crate::update::latest(&client, api).await.unwrap();
    let sums = crate::update::download(&client, release.asset("SHA256SUMS").unwrap()).await.unwrap();
    let sums = crate::update::parse_sums(&String::from_utf8_lossy(&sums));
    let files = [(NAME, sums[NAME].as_str()), (TUI, sums[TUI].as_str())];
    let answer = GitHub { client: &client, api }.attestations(files[0].1).await.unwrap();
    let github = Canned(Some(answer));
    let v = verifier().unwrap();
    let proof = verify(&github, &v, &RELEASE, &files).await.unwrap();
    eprintln!("{} {files:?}: {proof}", release.tag);
    for wrong in [Expected { workflow: ".github/workflows/ci.yml", ..RELEASE }, Expected { repo_id: "1", ..RELEASE }] {
        let e = verify(&github, &v, &wrong, &files).await.unwrap_err();
        eprintln!("refused, as expected: {e}");
    }
}
