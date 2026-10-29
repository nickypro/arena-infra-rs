//! Build provenance for `arena update`: that a release's binaries were built together by this
//! repository's release workflow from master, as their Sigstore attestation proves.
//! `SHA256SUMS` alone can't say that: it comes from the same release, so whoever could forge a
//! release could forge it too. It still catches a corrupt download, and `update` keeps checking
//! it.
//!
//! `.github/workflows/release.yml` attests the binaries it publishes
//! (actions/attest-build-provenance): one in-toto statement naming every file and its SHA-256,
//! with SLSA provenance as its predicate, in a DSSE envelope signed with a short-lived Fulcio
//! certificate issued to the workflow run, and logged in Rekor. GitHub keeps the bundle, and
//! anyone can read it without a login: `GET {REPO_API}/attestations/sha256:<hex>` ([`GitHub`]).
//!
//! [`verify`] accepts an update's binaries only when one bundle passes all of:
//! - Sigstore's own verification (the sigstore-verify crate, against the public-good trust
//!   root compiled in): the certificate chains to Fulcio and carries a CT log's signed
//!   timestamp, the envelope is signed by the certificate's key, the Rekor entry is in the log
//!   (inclusion proof, signed checkpoint, signed entry timestamp), describes this envelope and
//!   was made while the certificate was valid, and the certificate is the release workflow's
//!   ([`Expected::identity`]) from GitHub Actions' OIDC issuer;
//! - the certificate's other claims ([`Expected::check_signer`]): this repository by GitHub's
//!   id for it, a `workflow_run` run on master, on a GitHub-hosted runner;
//! - the statement ([`check_statement`]): in-toto, SLSA provenance, naming *each* binary with
//!   its digest. One statement for both binaries binds them to one run: a genuine new `arena`
//!   can't be paired with an older, also genuine, `arena-tui`.
//!
//! What the certificate can't say is which commit was built. A `workflow_run` run always runs
//! master's `release.yml`, on `refs/heads/master`, whichever CI run started it, and records
//! master's tip when it ran; it builds the commit that CI run tested. Which CI runs may start a
//! release is the workflow's own gate (a CI run of a push to master: release.yml's `branches`
//! and `if`), which only a change to master can widen. The new `arena --version` naming the
//! release checks the rest.
//!
//! The trust root is compiled in, not fetched (TUF): no second HTTP stack, nothing written
//! under root's home on prod, and it can be tested offline. A test pins its hash, so a
//! dependency bump can't change it unreviewed. The price: when Sigstore rotates a key, a build
//! older than the rotation can't verify releases signed with the new key, and refuses them;
//! `arena update --skip-attestation <sha256>,<sha256>` (digests checked by hand) is the way past
//! that, once.

use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;
use sigstore_verify::trust_root::{TrustedRoot, SIGSTORE_PRODUCTION_TRUSTED_ROOT};
use sigstore_verify::types::{Bundle, Sha256Hash, SignatureContent};
use sigstore_verify::{IdentityMatcher, SubjectAltName, VerificationPolicy, Verifier};

/// A check's outcome: its failure is a message for the operator, said as is.
pub type Result<T> = std::result::Result<T, String>;

/// GitHub Actions' OIDC issuer: the only one whose certificates name a workflow.
pub const ISSUER: &str = "https://token.actions.githubusercontent.com";

/// The in-toto statement type actions/attest-build-provenance writes.
pub const STATEMENT_TYPE: &str = "https://in-toto.io/Statement/v1";

/// SLSA provenance v1: what the statement must say about the binary (not, say, an SBOM).
pub const SLSA_PROVENANCE: &str = "https://slsa.dev/provenance/v1";

/// The DSSE payload type of an in-toto statement.
const IN_TOTO_PAYLOAD: &str = "application/vnd.in-toto+json";

/// At most this many bundles claiming the release workflow's identity are verified: a release
/// attests its binaries once (a re-run, once more). Bundles claiming another identity are set
/// aside unverified, so a flood of honest other attestations can't push the genuine one out of
/// reach. Forged ones claiming this identity (any workflow of the repository with
/// `attestations: write` could post them) still could, past 10 or past the API's first page
/// of 100 ([`GitHub`]): that refuses the update, it can't install anything.
const MAX_BUNDLES: usize = 10;

/// Who must have built a binary: a workflow file of a repository, run on a ref by an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expected {
    /// `owner/name`.
    pub repo: &'static str,
    /// GitHub's id for the repository (Fulcio's "source repository identifier"). It stays with
    /// the repository through a rename or a transfer, and a deleted repository's name re-created
    /// by someone else gets another one, so it pins *this* repository where the name can't.
    pub repo_id: &'static str,
    /// The workflow file, from the repository's root.
    pub workflow: &'static str,
    /// The ref it ran on (a `workflow_run` run is always on the default branch).
    pub git_ref: &'static str,
    /// The event that started the run. Pinned so that giving the workflow another trigger
    /// (`workflow_dispatch`, `push`) is a deliberate change here too, not a silent widening of
    /// what `update` accepts.
    pub trigger: &'static str,
}

/// This repository's release workflow, on master, started by CI's success: what `arena update`
/// installs from.
pub const RELEASE: Expected = Expected {
    repo: "nickypro/arena-infra-rs",
    repo_id: "1368824951",
    workflow: ".github/workflows/release.yml",
    git_ref: "refs/heads/master",
    trigger: "workflow_run",
};

impl Expected {
    /// `https://github.com/<repo>`.
    pub fn repo_uri(&self) -> String {
        format!("https://github.com/{}", self.repo)
    }

    /// The certificate identity (its SAN URI) of a run of the workflow on the ref:
    /// `https://github.com/<repo>/<workflow>@<ref>`.
    pub fn identity(&self) -> String {
        format!("{}/{}@{}", self.repo_uri(), self.workflow, self.git_ref)
    }

    /// That a verified certificate's claims are this workflow's, or the first that isn't.
    /// Sigstore's verification already matched the identity and issuer; they are checked again
    /// here so this one function says everything that is pinned. Not pinned: the commit (see
    /// the module doc). Pure.
    pub fn check_signer(&self, s: &Signer) -> Result<()> {
        let runner = "github-hosted";
        let checks: [(&str, &Option<String>, String); 8] = [
            ("identity", &s.identity, self.identity()),
            ("OIDC issuer", &s.issuer, ISSUER.to_string()),
            ("source repository", &s.repo_uri, self.repo_uri()),
            ("repository id", &s.repo_id, self.repo_id.to_string()),
            ("ref", &s.git_ref, self.git_ref.to_string()),
            ("workflow", &s.build_config, self.identity()),
            ("trigger", &s.trigger, self.trigger.to_string()),
            ("runner", &s.runner, runner.to_string()),
        ];
        for (what, got, want) in checks {
            if got.as_deref() != Some(want.as_str()) {
                return Err(format!("its certificate's {what} is {}, not {want}", got.as_deref().unwrap_or("missing")));
            }
        }
        Ok(())
    }
}

/// What a verified certificate says about the run that signed: the claims
/// [`Expected::check_signer`] pins, and the ones `update` only shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Signer {
    /// The SAN, when it is a URI (a workflow's is).
    pub identity: Option<String>,
    pub issuer: Option<String>,
    pub repo_uri: Option<String>,
    pub repo_id: Option<String>,
    pub git_ref: Option<String>,
    /// The workflow that ran (`<repo>/<workflow>@<ref>`); the identity, unless that was a
    /// reusable workflow it called.
    pub build_config: Option<String>,
    /// The event that started the run (`workflow_run`).
    pub trigger: Option<String>,
    /// `github-hosted` or `self-hosted`.
    pub runner: Option<String>,
    /// Shown, not pinned: the commit master was at when the run started.
    pub commit: Option<String>,
    /// Shown, not pinned: the run's URL.
    pub run: Option<String>,
}

impl Signer {
    fn from_certificate(c: &sigstore_verify::crypto::CertificateInfo) -> Self {
        let ci = &c.ci_claims;
        Self {
            identity: match &c.identity {
                Some(SubjectAltName::Uri(u)) => Some(u.clone()),
                _ => None,
            },
            issuer: c.issuer.clone(),
            repo_uri: ci.source_repository_uri.clone(),
            repo_id: ci.source_repository_identifier.clone(),
            git_ref: ci.source_repository_ref.clone(),
            build_config: ci.build_config_uri.clone(),
            trigger: ci.build_trigger.clone(),
            runner: ci.runner_environment.clone(),
            commit: ci.source_repository_digest.clone(),
            run: ci.run_invocation_uri.clone(),
        }
    }
}

/// Binaries' verified provenance: who signed the statement naming them, when, and every file
/// the statement names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proof {
    pub signer: Signer,
    /// When it was signed, RFC 3339: the earliest authenticated time Sigstore's verification
    /// used (Rekor's integrated time, or a timestamp authority's).
    pub logged: Option<String>,
    /// Each file the verified statement names, with its SHA-256 (lowercase hex): the release's
    /// other files can be checked against it too ([`Proof::digest_of`]).
    pub subjects: Vec<(String, String)>,
}

impl Proof {
    /// The SHA-256 the verified statement gives `name`, if it names it.
    pub fn digest_of(&self, name: &str) -> Option<&str> {
        self.subjects.iter().find(|(n, _)| n == name).map(|(_, d)| d.as_str())
    }
}

/// `built by <identity>, run <url>, logged <time>`: what `update` says.
impl std::fmt::Display for Proof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = &self.signer;
        write!(f, "built by {}", s.identity.as_deref().unwrap_or("?"))?;
        if let Some(run) = &s.run {
            write!(f, ", run {run}")?;
        }
        if let Some(t) = &self.logged {
            write!(f, ", logged {t}")?;
        }
        Ok(())
    }
}

/// That an in-toto statement (a DSSE payload) is SLSA provenance naming each of `files`
/// (`(name, SHA-256 in hex)`) with that digest. The signature covers the statement, so a
/// subject edited to match another file fails verification before this runs; this says what
/// the signed statement is about. Pure.
pub fn check_statement(payload: &[u8], files: &[(&str, &str)]) -> Result<()> {
    let v: Value = serde_json::from_slice(payload).map_err(|e| format!("its statement isn't JSON: {e}"))?;
    let field = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("missing");
    if field("_type") != STATEMENT_TYPE {
        return Err(format!("its statement is of type {}, not {STATEMENT_TYPE}", field("_type")));
    }
    if field("predicateType") != SLSA_PROVENANCE {
        return Err(format!("it attests {}, not SLSA provenance ({SLSA_PROVENANCE})", field("predicateType")));
    }
    let subjects = v.get("subject").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    for &(name, sha256) in files {
        let named = subjects.iter().find(|s| s.get("name").and_then(Value::as_str) == Some(name));
        let Some(subject) = named else {
            let names: Vec<&str> = subjects.iter().filter_map(|s| s.get("name").and_then(Value::as_str)).collect();
            return Err(format!("it doesn't name {name} (it names: {})", if names.is_empty() { "nothing".into() } else { names.join(", ") }));
        };
        let digest = subject.pointer("/digest/sha256").and_then(Value::as_str).unwrap_or("missing");
        if !digest.eq_ignore_ascii_case(sha256) {
            return Err(format!("it names {name} with sha256 {digest}, not this file's {sha256}"));
        }
    }
    Ok(())
}

/// The `(name, sha256)` of each subject of an in-toto statement that has both. Pure.
fn subjects(payload: &[u8]) -> Vec<(String, String)> {
    let v: Value = serde_json::from_slice(payload).unwrap_or_default();
    let list = v.get("subject").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    list.iter()
        .filter_map(|s| Some((s.get("name")?.as_str()?.to_string(), s.pointer("/digest/sha256")?.as_str()?.to_ascii_lowercase())))
        .collect()
}

/// The bundles in the attestations API's answer (`{"attestations": [{"bundle": …}, …]}`). Pure.
pub fn bundles(v: &Value) -> Vec<&Value> {
    let list = v.get("attestations").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    list.iter().filter_map(|a| a.get("bundle").filter(|b| b.is_object())).collect()
}

/// The verifier for Sigstore's public-good instance, from the trust root compiled in.
pub fn verifier() -> Result<Verifier> {
    let root = TrustedRoot::from_json(SIGSTORE_PRODUCTION_TRUSTED_ROOT).map_err(|e| format!("the Sigstore trust root compiled in doesn't load: {e}"))?;
    Verifier::new(&root).map_err(|e| format!("the Sigstore trust root compiled in doesn't load: {e}"))
}

fn parse_bundle(v: &Value) -> Result<Bundle> {
    serde_json::from_value(v.clone()).map_err(|e| format!("it isn't a Sigstore bundle: {e}"))
}

/// The SAN of a bundle's signing certificate, read without verifying anything: only to set
/// aside, unverified, bundles that don't even claim to be the release workflow's.
fn claimed_identity(bundle: &Bundle) -> Option<String> {
    let cert = bundle.signing_certificate()?;
    let info = sigstore_verify::crypto::parse_certificate_info(cert).ok()?;
    info.identity.map(|i| i.as_str().to_string())
}

/// One bundle against `files` (`(name, SHA-256 in hex)`, at least one), all three checks of the
/// module doc. Pure (no I/O: the trust root is the verifier's, the times are the bundle's).
pub fn verify_bundle(verifier: &Verifier, bundle: &Value, expected: &Expected, files: &[(&str, &str)]) -> Result<Proof> {
    verify_parsed(verifier, &parse_bundle(bundle)?, expected, files)
}

fn verify_parsed(verifier: &Verifier, bundle: &Bundle, expected: &Expected, files: &[(&str, &str)]) -> Result<Proof> {
    let &(_, first) = files.first().ok_or("no file to verify")?;
    let digest = Sha256Hash::from_hex(first).map_err(|e| format!("{first} isn't a SHA-256: {e}"))?;
    let policy = VerificationPolicy::new(IdentityMatcher::Uri(expected.identity()), ISSUER);
    let r = verifier.verify(digest, bundle, &policy).map_err(|e| format!("Sigstore verification failed: {e}"))?;
    // The policy above skips nothing; this keeps it so should a default ever change.
    if !(r.certificate_verified() && r.sct_verified() && r.tlog_verified() && r.identity_policy_checked()) {
        return Err("Sigstore verification skipped a step (certificate, SCT, log or identity)".into());
    }
    let cert = r.certificate().ok_or("Sigstore verification returned no certificate")?;
    let signer = Signer::from_certificate(cert);
    expected.check_signer(&signer)?;
    let SignatureContent::DsseEnvelope(envelope) = &bundle.content else {
        return Err("it signs a message, not an in-toto statement".into());
    };
    if envelope.payload_type != IN_TOTO_PAYLOAD {
        return Err(format!("its payload is {}, not {IN_TOTO_PAYLOAD}", envelope.payload_type));
    }
    check_statement(envelope.payload.as_bytes(), files)?;
    let signed = r.verified_timestamps().iter().min();
    Ok(Proof {
        signer,
        logged: signed.map(|t| t.to_string()),
        subjects: subjects(envelope.payload.as_bytes()),
    })
}

/// Where attestations come from: GitHub ([`GitHub`]), or canned ones in tests.
#[async_trait]
pub trait Source: Send + Sync {
    /// The attestations API's JSON for this SHA-256 (lowercase hex). `Ok` with an empty list
    /// when there are none; `Err` when it couldn't be asked.
    async fn attestations(&self, sha256: &str) -> Result<Value>;
}

/// The repository's attestations on GitHub: `GET {api}/attestations/sha256:<hex>`, anonymous
/// (public repository; one request of the 60 an hour allowed without a login).
pub struct GitHub<'a> {
    pub client: &'a Client,
    /// `https://api.github.com/repos/<owner>/<name>` (`update::REPO_API`).
    pub api: &'a str,
}

#[async_trait]
impl Source for GitHub<'_> {
    async fn attestations(&self, sha256: &str) -> Result<Value> {
        let url = format!("{}/attestations/sha256:{sha256}?predicate_type=provenance&per_page=100", self.api);
        let resp = self.client.get(&url).header("Accept", "application/vnd.github+json").send().await.map_err(|e| format!("GET {url}: {e}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        // GitHub's answer for a digest it has no attestation of.
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(serde_json::json!({ "attestations": [] }));
        }
        if !status.is_success() {
            return Err(format!("GET {url}: HTTP {status}: {}", text.chars().take(200).collect::<String>()));
        }
        serde_json::from_str(&text).map_err(|e| format!("GET {url}: not JSON: {e}"))
    }
}

/// That the binaries `files` (`(asset name, SHA-256 in hex)`, at least one) were built
/// together as `expected` says: one attestation from `source` (looked up by the first file's
/// digest) passes [`verify_bundle`] for all of them. Otherwise why not: each candidate's
/// reason; no attestation at all, or none from the workflow, is a refusal too.
pub async fn verify(source: &dyn Source, verifier: &Verifier, expected: &Expected, files: &[(&str, &str)]) -> Result<Proof> {
    let &(first, sha256) = files.first().ok_or("no file to verify")?;
    let names = files.iter().map(|f| f.0).collect::<Vec<_>>().join(" and ");
    let answer = source.attestations(sha256).await.map_err(|e| format!("{first}: couldn't read its build-provenance attestation ({e})"))?;
    let found = bundles(&answer);
    if found.is_empty() {
        let listed = answer.get("attestations").and_then(Value::as_array).map_or(0, Vec::len);
        return Err(if listed == 0 {
            format!("{first}: GitHub has no build-provenance attestation for it (sha256 {sha256})")
        } else {
            // GitHub may one day send only `bundle_url` (snappy-compressed, as `gh` reads it).
            format!("{first}: GitHub lists {listed} attestation(s) for it, none with its bundle inline (this build can't read `bundle_url`)")
        });
    }
    let identity = expected.identity();
    let mut why = Vec::new();
    let mut others = Vec::new();
    let mut candidates = Vec::new();
    for v in found {
        match parse_bundle(v) {
            Err(e) => why.push(e),
            Ok(b) => match claimed_identity(&b) {
                Some(id) if id == identity => candidates.push(b),
                id => others.push(id.unwrap_or_else(|| "no identity".into())),
            },
        }
    }
    for b in candidates.iter().take(MAX_BUNDLES) {
        match verify_parsed(verifier, b, expected, files) {
            Ok(proof) => return Ok(proof),
            Err(e) => why.push(e),
        }
    }
    if candidates.is_empty() {
        others.sort();
        others.dedup();
        why.push(format!("none is signed by it (signed by: {})", others.join(", ")));
    }
    Err(format!("{names}: no attestation proves they were built by {identity}: {}", why.join("; ")))
}

#[cfg(test)]
#[path = "attest_tests.rs"]
mod tests;
