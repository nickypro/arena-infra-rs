//! Build-provenance checks against a real attestation: the one GitHub keeps for release
//! v2.0.261028.1916's binaries (`fixtures/attestation_v2.0.261028.1916.json`, the API's answer
//! for `arena-x86_64-unknown-linux-gnu`, its short-lived `bundle_url` removed). The statement
//! names all four binaries, so it proves each of them. Every refusal below is that bundle, or
//! what is expected of it, changed in one way. No network: the source is canned.

use super::*;
use serde_json::json;

const FIXTURE: &str = include_str!("fixtures/attestation_v2.0.261028.1916.json");

/// The release's binaries and their SHA-256s (its SHA256SUMS).
const SUBJECTS: [(&str, &str); 4] = [
    ("arena-aarch64-unknown-linux-gnu", "21730a75a1e3aa6a581524a0b617c7b553cc241b9d3146129e54a70e1ea073bc"),
    ("arena-tui-aarch64-unknown-linux-gnu", "2975bf93e57dd2c91cfc1b41eb21d2d30646faa9b0a0fa5a072bb2ba172ec21e"),
    ("arena-tui-x86_64-unknown-linux-gnu", "e39126f6b558c25094af61dbb7e5012db80e5bc38c717205c63b1802f764c87e"),
    ("arena-x86_64-unknown-linux-gnu", "351ad4235e7163a53effb9acf1cd16628bb7c26e438f6f2c0e7b089d0cf0196e"),
];
const NAME: &str = SUBJECTS[3].0;
const SHA: &str = SUBJECTS[3].1;
/// Some other file's digest.
const OTHER: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

fn answer() -> Value {
    serde_json::from_str(FIXTURE).unwrap()
}

fn bundle() -> Value {
    answer()["attestations"][0]["bundle"].clone()
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

fn check(b: &Value, expected: &Expected, name: &str, sha: &str) -> Result<Proof> {
    verify_bundle(&verifier().unwrap(), b, expected, name, sha)
}

/// Attestations from memory: `None` is an API that can't be asked.
struct Canned(Option<Value>);

#[async_trait]
impl Source for Canned {
    async fn attestations(&self, _sha256: &str) -> Result<Value> {
        self.0.clone().ok_or_else(|| "HTTP 403 Forbidden: API rate limit exceeded".to_string())
    }
}

#[test]
fn the_recorded_release_attestation_proves_each_of_its_binaries() {
    for (name, sha) in SUBJECTS {
        let proof = check(&bundle(), &RELEASE, name, sha).unwrap_or_else(|e| panic!("{name}: {e}"));
        let s = &proof.signer;
        assert_eq!(s.identity.as_deref(), Some("https://github.com/nickypro/arena-infra-rs/.github/workflows/release.yml@refs/heads/master"));
        assert_eq!((s.trigger.as_deref(), s.commit.as_deref()), (Some("workflow_run"), Some("7917eb2b6cc1652ee05355569e101826649b1359")));
        assert_eq!(s.run.as_deref(), Some("https://github.com/nickypro/arena-infra-rs/actions/runs/40753802298/attempts/1"));
        assert_eq!(proof.logged.as_deref(), Some("2026-10-28T19:25:45Z"));
        assert_eq!(
            proof.to_string(),
            "built by https://github.com/nickypro/arena-infra-rs/.github/workflows/release.yml@refs/heads/master, \
             run https://github.com/nickypro/arena-infra-rs/actions/runs/40753802298/attempts/1, logged 2026-10-28T19:25:45Z"
        );
    }
}

/// The statement edited to name another file's digest: its signature no longer matches. And
/// the real statement doesn't vouch for a file it doesn't name.
#[test]
fn a_tampered_subject_digest_is_refused() {
    let mut s = statement(&bundle());
    let subject = s["subject"].as_array_mut().unwrap().iter_mut().find(|x| x["name"] == NAME).unwrap();
    subject["digest"]["sha256"] = json!(OTHER);
    let e = check(&with_statement(bundle(), &s), &RELEASE, NAME, OTHER).unwrap_err();
    assert!(e.starts_with("Sigstore verification failed: invalid signature: DSSE signature verification failed"), "{e}");

    let e = check(&bundle(), &RELEASE, NAME, OTHER).unwrap_err();
    assert!(e.contains("does not match any subject"), "{e}");
}

/// Expecting another workflow of the repository, or the release workflow on another ref
/// (a pull request's run, say): the certificate's identity isn't that.
#[test]
fn a_wrong_workflow_or_ref_is_refused() {
    for expected in [
        Expected { workflow: ".github/workflows/ci.yml", ..RELEASE },
        Expected { git_ref: "refs/heads/main", ..RELEASE },
        Expected { git_ref: "refs/pull/18/merge", ..RELEASE },
    ] {
        let e = check(&bundle(), &expected, NAME, SHA).unwrap_err();
        assert!(e.contains("identity mismatch") && e.contains(&expected.identity()), "{e}");
    }
}

/// Another repository's release workflow (a fork's, say) isn't this one's; nor is a
/// repository of the same name with another id (deleted and re-created by someone else).
#[test]
fn a_wrong_repo_is_refused() {
    let fork = Expected { repo: "someone/arena-infra-rs", ..RELEASE };
    let e = check(&bundle(), &fork, NAME, SHA).unwrap_err();
    assert!(e.contains("identity mismatch") && e.contains("someone/arena-infra-rs"), "{e}");

    let recreated = Expected { repo_id: "999999999", ..RELEASE };
    let e = check(&bundle(), &recreated, NAME, SHA).unwrap_err();
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
    let e = check(&b, &RELEASE, NAME, SHA).unwrap_err();
    assert!(e.starts_with("Sigstore verification failed: invalid signature: DSSE signature verification failed"), "{e}");
}

/// The Rekor entry is verified, not just read: a hash of its inclusion proof changed, its
/// integrated time moved (the signed entry timestamp covers it), or the proof dropped.
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
    for (b, said) in [
        (proof_hash, "invalid inclusion proof: inclusion proof verification failed: root hash mismatch"),
        (time, "invalid inclusion promise: SET verification failed"),
        (no_proof, "bundle must have an inclusion proof"),
    ] {
        let e = check(&b, &RELEASE, NAME, SHA).unwrap_err();
        assert!(e.starts_with("Sigstore verification failed: ") && e.contains(said), "{said}: {e}");
    }
}

/// No attestation (GitHub's 404, an empty list, or entries without a bundle), or none that
/// could be read: refused, and the message says which.
#[tokio::test]
async fn a_missing_attestation_is_refused() {
    let v = verifier().unwrap();
    for none in [json!({"attestations": []}), json!({})] {
        let e = verify(&Canned(Some(none)), &v, &RELEASE, NAME, SHA).await.unwrap_err();
        assert_eq!(e, format!("{NAME}: GitHub has no build-provenance attestation for it (sha256 {SHA})"));
    }
    let only_url = json!({"attestations": [{"bundle_url": "https://x"}, {"bundle": null}]});
    let e = verify(&Canned(Some(only_url)), &v, &RELEASE, NAME, SHA).await.unwrap_err();
    assert!(e.ends_with("lists 2 attestation(s) for it, none with its bundle inline (this build can't read `bundle_url`)"), "{e}");
    let e = verify(&Canned(None), &v, &RELEASE, NAME, SHA).await.unwrap_err();
    assert!(e.contains("couldn't read its build-provenance attestation") && e.contains("rate limit"), "{e}");
}

/// Through [`verify`]: the real answer proves the binary; with a bad bundle before it, the good
/// one still does; with only bad ones, each one's reason is said.
#[tokio::test]
async fn verify_takes_the_first_bundle_that_proves_the_binary() {
    let v = verifier().unwrap();
    let proof = verify(&Canned(Some(answer())), &v, &RELEASE, NAME, SHA).await.unwrap();
    assert_eq!(proof.signer.repo_id.as_deref(), Some("1368824951"));

    let bad = json!({"mediaType": "nonsense"});
    let both = json!({"attestations": [{"bundle": bad}, {"bundle": bundle()}]});
    assert!(verify(&Canned(Some(both)), &v, &RELEASE, NAME, SHA).await.is_ok());

    let only_bad = json!({"attestations": [{"bundle": bad}, {"bundle": bundle()}]});
    let e = verify(&Canned(Some(only_bad)), &v, &RELEASE, NAME, OTHER).await.unwrap_err();
    assert!(e.starts_with(&format!("{NAME}: its attestation doesn't prove it was built by {}: it isn't a Sigstore bundle", RELEASE.identity())), "{e}");
    assert!(e.contains("; Sigstore verification failed"), "{e}");
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
        runner: Some("github-hosted".into()),
        ..Signer::default()
    }
}

/// Every pinned claim, one wrong (or missing) at a time; the claims only shown don't matter.
#[test]
fn each_pinned_claim_must_be_the_release_workflows() {
    assert_eq!(RELEASE.check_signer(&release_signer()), Ok(()));
    let shown = Signer { commit: Some("x".into()), trigger: Some("push".into()), run: None, ..release_signer() };
    assert_eq!(RELEASE.check_signer(&shown), Ok(()));
    type Change = fn(&mut Signer);
    let cases: [(Change, &str); 8] = [
        (|s| s.identity = Some("https://github.com/nickypro/arena-infra-rs/.github/workflows/ci.yml@refs/heads/master".into()), "identity is https://github.com/nickypro/arena-infra-rs/.github/workflows/ci.yml@refs/heads/master"),
        (|s| s.issuer = Some("https://accounts.google.com".into()), "OIDC issuer is https://accounts.google.com"),
        (|s| s.repo_uri = Some("https://github.com/someone/arena-infra-rs".into()), "source repository is https://github.com/someone/arena-infra-rs"),
        (|s| s.repo_id = None, "repository id is missing"),
        (|s| s.git_ref = Some("refs/pull/18/merge".into()), "ref is refs/pull/18/merge"),
        (|s| s.build_config = Some("https://github.com/someone/x/.github/workflows/y.yml@refs/heads/main".into()), "workflow is https://github.com/someone/x/"),
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
fn the_statement_must_be_slsa_provenance_naming_this_file_and_digest() {
    let good = statement(&bundle());
    let text = |v: &Value| serde_json::to_vec(v).unwrap();
    assert_eq!(check_statement(&text(&good), NAME, SHA), Ok(()));
    assert_eq!(check_statement(&text(&good), NAME, &SHA.to_ascii_uppercase()), Ok(()), "hex case");

    let mut sbom = good.clone();
    sbom["predicateType"] = json!("https://spdx.dev/Document/v2.3");
    let mut old = good.clone();
    old["_type"] = json!("https://in-toto.io/Statement/v0.1");
    let mut empty = good.clone();
    empty["subject"] = json!([]);
    let cases: [(Vec<u8>, &str, &str, &str); 6] = [
        (text(&sbom), NAME, SHA, "it attests https://spdx.dev/Document/v2.3, not SLSA provenance"),
        (text(&old), NAME, SHA, "of type https://in-toto.io/Statement/v0.1"),
        (text(&good), "arena-riscv64gc-unknown-linux-gnu", SHA, "doesn't name arena-riscv64gc-unknown-linux-gnu (it names: arena-aarch64-unknown-linux-gnu, "),
        (text(&good), NAME, OTHER, "it names arena-x86_64-unknown-linux-gnu with sha256 351ad42"),
        (text(&empty), NAME, SHA, "(it names: nothing)"),
        (b"not json".to_vec(), NAME, SHA, "isn't JSON"),
    ];
    for (payload, name, sha, said) in cases {
        let e = check_statement(&payload, name, sha).unwrap_err();
        assert!(e.contains(said), "{said}: {e}");
    }
}

#[test]
fn the_api_answer_reads_as_its_bundles() {
    assert_eq!(bundles(&answer()).len(), 1);
    let many = json!({"attestations": (0..15).map(|_| json!({"bundle": {}})).collect::<Vec<_>>()});
    assert_eq!(bundles(&many).len(), MAX_BUNDLES);
    assert!(bundles(&json!({"attestations": [{"bundle": null}, {"bundle_url": "u"}]})).is_empty());
    assert!(bundles(&json!({"message": "Not Found"})).is_empty());
}

/// Opt-in, live: the latest release's `arena-x86_64` attestation, read from GitHub (anonymous,
/// 2 requests), proves it was built by the release workflow, and doesn't prove it was built by
/// another workflow or repository. What would catch a Sigstore or GitHub change that the
/// fixture can't (a rotated key, a new bundle format). No account or key: just network.
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
    let sha = &sums[NAME];
    let answer = GitHub { client: &client, api }.attestations(sha).await.unwrap();
    let github = Canned(Some(answer));
    let v = verifier().unwrap();
    let proof = verify(&github, &v, &RELEASE, NAME, sha).await.unwrap();
    eprintln!("{} {NAME} ({sha}): {proof}", release.tag);
    for wrong in [Expected { workflow: ".github/workflows/ci.yml", ..RELEASE }, Expected { repo_id: "1", ..RELEASE }] {
        let e = verify(&github, &v, &wrong, NAME, sha).await.unwrap_err();
        eprintln!("refused, as expected: {e}");
    }
}

#[test]
fn the_release_workflow_is_this_repositorys() {
    assert_eq!(RELEASE.identity(), "https://github.com/nickypro/arena-infra-rs/.github/workflows/release.yml@refs/heads/master");
    assert!(crate::update::REPO_API.ends_with(&format!("/repos/{}", RELEASE.repo)), "update reads releases from the repository it verifies against");
}
