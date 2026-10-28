//! Build provenance for `arena update`: that a release binary was built by this repository's
//! release workflow from master, as its Sigstore attestation proves. `SHA256SUMS` alone can't
//! say that: it comes from the same release, so whoever could forge a release could forge it
//! too. It still catches a corrupt download, and `update` keeps checking it.
//!
//! `.github/workflows/release.yml` attests the binaries it publishes
//! (actions/attest-build-provenance): an in-toto statement naming each file and its SHA-256,
//! with SLSA provenance as its predicate, in a DSSE envelope signed with a short-lived Fulcio
//! certificate issued to the workflow run, and logged in Rekor. GitHub keeps the bundle, and
//! anyone can read it without a login: `GET {REPO_API}/attestations/sha256:<hex>` ([`GitHub`]).
//!
//! [`verify`] accepts a binary only when one of its bundles passes all of:
//! - Sigstore's own verification (the sigstore-verify crate, against the public-good trust
//!   root compiled in): the certificate chains to Fulcio and carries a CT log's signed
//!   timestamp, the envelope is signed by the certificate's key, the Rekor entry is in the log
//!   (inclusion proof, signed checkpoint, signed entry timestamp) and was made while the
//!   certificate was valid, and the certificate is the release workflow's
//!   ([`Expected::identity`]) from GitHub Actions' OIDC issuer;
//! - the certificate's other claims ([`Expected::check_signer`]): this repository by GitHub's
//!   id for it, a run on master, on a GitHub-hosted runner;
//! - the statement ([`check_statement`]): in-toto, SLSA provenance, and naming this file with
//!   this digest.
//!
//! The trust root is compiled in, not fetched (TUF): no second HTTP stack, nothing written
//! under root's home on prod, and it can be tested offline. The price: when Sigstore rotates
//! a key, a build older than the rotation can't verify releases signed with the new key, and
//! refuses them. `arena update --skip-attestation` is the way past that, once; the new build
//! has the new root (Dependabot keeps sigstore-trust-root, where it comes from, current).
//!
//! What it doesn't prove: which commit was built. A `workflow_run` release runs on master and
//! builds the commit CI passed, but its certificate records master's tip when it ran, which can
//! be a later commit. The new `arena --version` naming the release is the check for that.

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

/// At most this many of a digest's attestations are tried: a release attests a binary once,
/// a rebuild of the same commit once more.
const MAX_BUNDLES: usize = 10;

/// Who must have built a binary: a workflow file of a repository, run on a ref.
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
}

/// This repository's release workflow, on master: what `arena update` installs from.
pub const RELEASE: Expected = Expected {
    repo: "nickypro/arena-infra-rs",
    repo_id: "1368824951",
    workflow: ".github/workflows/release.yml",
    git_ref: "refs/heads/master",
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
    /// the module doc) and the trigger (the ref check already keeps pull requests out, and the
    /// workflow decides what may run it). Pure.
    pub fn check_signer(&self, s: &Signer) -> Result<()> {
        let runner = "github-hosted";
        let checks: [(&str, &Option<String>, String); 7] = [
            ("identity", &s.identity, self.identity()),
            ("OIDC issuer", &s.issuer, ISSUER.to_string()),
            ("source repository", &s.repo_uri, self.repo_uri()),
            ("repository id", &s.repo_id, self.repo_id.to_string()),
            ("ref", &s.git_ref, self.git_ref.to_string()),
            ("workflow", &s.build_config, self.identity()),
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
    /// `github-hosted` or `self-hosted`.
    pub runner: Option<String>,
    /// Shown, not pinned: the commit master was at when the run started.
    pub commit: Option<String>,
    /// Shown, not pinned: the event that started the run (`workflow_run`).
    pub trigger: Option<String>,
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
            runner: ci.runner_environment.clone(),
            commit: ci.source_repository_digest.clone(),
            trigger: ci.build_trigger.clone(),
            run: ci.run_invocation_uri.clone(),
        }
    }
}

/// A binary's verified provenance: who signed, and when Rekor logged it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proof {
    pub signer: Signer,
    /// Rekor's integrated time, RFC 3339.
    pub logged: Option<String>,
}

/// `built by <identity>, run <url>, logged <time>`: what `update` says per binary.
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

/// That an in-toto statement (a DSSE payload) is SLSA provenance naming `name` with this
/// SHA-256 (lowercase hex). The signature covers the statement, so a subject edited to match
/// another file fails verification before this runs; this says what the signed statement is
/// about. Pure.
pub fn check_statement(payload: &[u8], name: &str, sha256: &str) -> Result<()> {
    let v: Value = serde_json::from_slice(payload).map_err(|e| format!("its statement isn't JSON: {e}"))?;
    let field = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("missing");
    if field("_type") != STATEMENT_TYPE {
        return Err(format!("its statement is of type {}, not {STATEMENT_TYPE}", field("_type")));
    }
    if field("predicateType") != SLSA_PROVENANCE {
        return Err(format!("it attests {}, not SLSA provenance ({SLSA_PROVENANCE})", field("predicateType")));
    }
    let subjects = v.get("subject").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let named = subjects.iter().find(|s| s.get("name").and_then(Value::as_str) == Some(name));
    let Some(subject) = named else {
        let names: Vec<&str> = subjects.iter().filter_map(|s| s.get("name").and_then(Value::as_str)).collect();
        return Err(format!("it doesn't name {name} (it names: {})", if names.is_empty() { "nothing".into() } else { names.join(", ") }));
    };
    let digest = subject.pointer("/digest/sha256").and_then(Value::as_str).unwrap_or("missing");
    if !digest.eq_ignore_ascii_case(sha256) {
        return Err(format!("it names {name} with sha256 {digest}, not this file's {sha256}"));
    }
    Ok(())
}

/// The bundles in the attestations API's answer (`{"attestations": [{"bundle": …}, …]}`),
/// at most [`MAX_BUNDLES`]. Pure.
pub fn bundles(v: &Value) -> Vec<&Value> {
    let list = v.get("attestations").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    list.iter().filter_map(|a| a.get("bundle").filter(|b| b.is_object())).take(MAX_BUNDLES).collect()
}

/// The verifier for Sigstore's public-good instance, from the trust root compiled in.
pub fn verifier() -> Result<Verifier> {
    let root = TrustedRoot::from_json(SIGSTORE_PRODUCTION_TRUSTED_ROOT).map_err(|e| format!("the Sigstore trust root compiled in doesn't load: {e}"))?;
    Verifier::new(&root).map_err(|e| format!("the Sigstore trust root compiled in doesn't load: {e}"))
}

/// One bundle against `name` (sha256 `sha256`), all three checks of the module doc. Pure (no
/// I/O: the trust root is the verifier's, the times are the bundle's).
pub fn verify_bundle(verifier: &Verifier, bundle: &Value, expected: &Expected, name: &str, sha256: &str) -> Result<Proof> {
    let bundle: Bundle = serde_json::from_value(bundle.clone()).map_err(|e| format!("it isn't a Sigstore bundle: {e}"))?;
    let digest = Sha256Hash::from_hex(sha256).map_err(|e| format!("{sha256} isn't a SHA-256: {e}"))?;
    let policy = VerificationPolicy::new(IdentityMatcher::Uri(expected.identity()), ISSUER);
    let r = verifier.verify(digest, &bundle, &policy).map_err(|e| format!("Sigstore verification failed: {e}"))?;
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
    check_statement(envelope.payload.as_bytes(), name, sha256)?;
    Ok(Proof { signer, logged: r.integrated_time().map(|t| t.to_string()) })
}

/// Where attestations come from: GitHub ([`GitHub`]), or canned ones in tests.
#[async_trait]
pub trait Source: Send + Sync {
    /// The attestations API's JSON for this SHA-256 (lowercase hex). `Ok` with an empty list
    /// when there are none; `Err` when it couldn't be asked.
    async fn attestations(&self, sha256: &str) -> Result<Value>;
}

/// The repository's attestations on GitHub: `GET {api}/attestations/sha256:<hex>`, anonymous
/// (public repository; the 60-an-hour limit `update` already lives within).
pub struct GitHub<'a> {
    pub client: &'a Client,
    /// `https://api.github.com/repos/<owner>/<name>` (`update::REPO_API`).
    pub api: &'a str,
}

#[async_trait]
impl Source for GitHub<'_> {
    async fn attestations(&self, sha256: &str) -> Result<Value> {
        let url = format!("{}/attestations/sha256:{sha256}?predicate_type=provenance", self.api);
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

/// That the binary `name`, whose SHA-256 is `sha256`, was built as `expected` says: one of its
/// attestations from `source` passes [`verify_bundle`]. Otherwise why not, each bundle's reason
/// said; no attestation at all is a refusal too.
pub async fn verify(source: &dyn Source, verifier: &Verifier, expected: &Expected, name: &str, sha256: &str) -> Result<Proof> {
    let answer = source.attestations(sha256).await.map_err(|e| format!("{name}: couldn't read its build-provenance attestation ({e})"))?;
    let found = bundles(&answer);
    if found.is_empty() {
        let listed = answer.get("attestations").and_then(Value::as_array).map_or(0, Vec::len);
        return Err(if listed == 0 {
            format!("{name}: GitHub has no build-provenance attestation for it (sha256 {sha256})")
        } else {
            // GitHub may one day send only `bundle_url` (snappy-compressed, as `gh` reads it).
            format!("{name}: GitHub lists {listed} attestation(s) for it, none with its bundle inline (this build can't read `bundle_url`)")
        });
    }
    let mut why = Vec::new();
    for b in found {
        match verify_bundle(verifier, b, expected, name, sha256) {
            Ok(proof) => return Ok(proof),
            Err(e) => why.push(e),
        }
    }
    Err(format!("{name}: its attestation doesn't prove it was built by {}: {}", expected.identity(), why.join("; ")))
}

#[cfg(test)]
#[path = "attest_tests.rs"]
mod tests;
