//! `arena update`: the newest release on GitHub, what changed since this build, and a swap
//! that keeps the previous binaries as `.prev` — prod's convention, done by hand until now.
//!
//! Releases come from `.github/workflows/release.yml`, one per green master commit: tag
//! `v<version>` (the build stamp `2.0.YYMMDD.HHMM`, from the commit's time in UTC), assets
//! `arena-<target>` and `arena-tui-<target>` per platform, `CHANGELOG.md` and `SHA256SUMS`.
//!
//! What changed comes from `CHANGELOG.md`: the entries the release's copy has that this
//! build's own copy (compiled in) doesn't ([`new_since`]) — so a build reads its own history,
//! and no entry needs a version number before its release exists. Each entry's
//! `### Action needed` and `### Config` lines are said first.
//!
//! Nothing is replaced until both binaries are downloaded, match `SHA256SUMS`, are proven built
//! by this repository's release workflow from master ([`crate::attest`]; `SHA256SUMS` comes
//! from the same release, so it can't prove that), and the new `arena --version` runs here and
//! names the release ([`check_runs`]): a forged, wrong-platform or truncated download never
//! becomes the cron's `arena`, and a binary is only run once it is proven. The swap itself is a
//! rename, so the path always holds a whole binary ([`install`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use reqwest::Client;
use serde_json::Value;

use crate::attest;

/// An update step's outcome: its failure is a message for the operator, said as is.
pub type Result<T> = std::result::Result<T, String>;

/// The repository's API base: releases are read from `{REPO_API}/releases/latest`.
pub const REPO_API: &str = "https://api.github.com/repos/nickypro/arena-infra-rs";

/// The largest asset downloaded (a release binary is ~20 MB).
const MAX_ASSET_BYTES: usize = 200 << 20;

/// A build's version, `2.0.YYMMDD.HHMM` (a leading `v`, as in a tag, is allowed). Compared
/// part by part, so a later build stamp is a newer version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub [u32; 4]);

impl Version {
    /// `2.0.261027.1130` or `v2.0.261027.1130`; `None` for anything else. Pure.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        let parts: Vec<u32> = s.split('.').map(|p| p.parse().ok()).collect::<Option<_>>()?;
        Some(Self(parts.try_into().ok()?))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let [a, b, c, d] = self.0;
        write!(f, "{a}.{b}.{c:06}.{d:04}")
    }
}

/// One release asset: its file name and where to download it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
}

/// The latest release, as `GET /releases/latest` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    pub version: Version,
    pub assets: Vec<Asset>,
}

impl Release {
    /// The asset called `name`, or what to say when there's none (the names there are).
    pub fn asset(&self, name: &str) -> std::result::Result<&Asset, String> {
        self.assets.iter().find(|a| a.name == name).ok_or_else(|| {
            let names: Vec<&str> = self.assets.iter().map(|a| a.name.as_str()).collect();
            format!("release {} has no `{name}` (it has: {})", self.tag, if names.is_empty() { "nothing".into() } else { names.join(", ") })
        })
    }
}

/// A release from the API's JSON: `tag_name` (a version), and each asset's `name` and
/// `browser_download_url`. Pure.
pub fn parse_release(v: &Value) -> std::result::Result<Release, String> {
    let tag = v.get("tag_name").and_then(Value::as_str).ok_or("the release has no tag_name")?.to_string();
    let version = Version::parse(&tag).ok_or_else(|| format!("the release tag `{tag}` isn't a version (v2.0.YYMMDD.HHMM)"))?;
    let assets = v
        .get("assets")
        .and_then(Value::as_array)
        .ok_or("the release has no assets list")?
        .iter()
        .filter_map(|a| {
            Some(Asset {
                name: a.get("name")?.as_str()?.to_string(),
                url: a.get("browser_download_url")?.as_str()?.to_string(),
            })
        })
        .collect();
    Ok(Release { tag, version, assets })
}

/// The client every update request uses: GitHub refuses requests without a `User-Agent`,
/// and asset downloads redirect to its storage host (followed, up to reqwest's 10).
pub fn client() -> Client {
    Client::builder().user_agent(concat!("arena-update/", env!("CARGO_PKG_VERSION"))).build().unwrap_or_default()
}

/// `GET {api}/releases/latest` (GitHub leaves drafts and pre-releases out of it).
/// A 404 means no release is published yet (GitHub answers so before the first one).
pub async fn latest(client: &Client, api: &str) -> Result<Release> {
    let url = format!("{api}/releases/latest");
    let resp = client.get(&url).header("Accept", "application/vnd.github+json").send().await.map_err(|e| format!("GET {url}: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(format!("no release is published yet ({url} answered 404)"));
    }
    if !status.is_success() {
        return Err(format!("GET {url}: HTTP {status}: {}", text.chars().take(200).collect::<String>()));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("the latest release isn't JSON: {e}"))?;
    parse_release(&v)
}

/// One asset's bytes, at most [`MAX_ASSET_BYTES`].
pub async fn download(client: &Client, asset: &Asset) -> Result<Vec<u8>> {
    let resp = client.get(&asset.url).header("Accept", "application/octet-stream").send().await.map_err(|e| format!("download {}: {e}", asset.name))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("download {}: HTTP {status}", asset.name));
    }
    let bytes = resp.bytes().await.map_err(|e| format!("download {}: {e}", asset.name))?;
    if bytes.len() > MAX_ASSET_BYTES {
        return Err(format!("{} is {} bytes, more than an arena binary ever is", asset.name, bytes.len()));
    }
    Ok(bytes.to_vec())
}

/// One `CHANGELOG.md` entry: a `## ` heading (its identity), its lines, and the bullets of its
/// `### Action needed` and `### Config` subsections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub heading: String,
    /// Every line under the heading, subsections included, as written.
    pub lines: Vec<String>,
    pub action: Vec<String>,
    pub config: Vec<String>,
}

/// The entries of a `CHANGELOG.md`, in file order (newest first). Text before the first `## `
/// heading is the file's preamble, not an entry. Pure.
pub fn entries(md: &str) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    let mut section = "";
    for line in md.lines() {
        if let Some(h) = line.strip_prefix("## ") {
            out.push(Entry { heading: h.trim().to_string(), lines: Vec::new(), action: Vec::new(), config: Vec::new() });
            section = "";
            continue;
        }
        let Some(e) = out.last_mut() else { continue };
        if let Some(sub) = line.strip_prefix("### ") {
            section = match sub.trim().to_ascii_lowercase().as_str() {
                "action needed" => "action",
                "config" => "config",
                _ => "",
            };
        } else if let Some(bullet) = line.trim_start().strip_prefix("- ") {
            match section {
                "action" => e.action.push(bullet.trim().to_string()),
                "config" => e.config.push(bullet.trim().to_string()),
                _ => {}
            }
        }
        e.lines.push(line.to_string());
    }
    for e in &mut out {
        while e.lines.last().is_some_and(|l| l.trim().is_empty()) {
            e.lines.pop();
        }
    }
    out
}

/// The entries of `latest` that `installed` doesn't have (by heading), newest first: what an
/// update to `latest` brings. Pure.
pub fn new_since(installed: &str, latest: &str) -> Vec<Entry> {
    let have: std::collections::HashSet<String> = entries(installed).into_iter().map(|e| e.heading).collect();
    entries(latest).into_iter().filter(|e| !have.contains(&e.heading)).collect()
}

/// What `update --check` prints about `new`: the action-needed and config lines of all of
/// them first, then each entry in full. Pure.
pub fn describe_changes(new: &[Entry]) -> String {
    let mut out = String::new();
    let action: Vec<&String> = new.iter().flat_map(|e| &e.action).collect();
    let config: Vec<&String> = new.iter().flat_map(|e| &e.config).collect();
    if !action.is_empty() {
        out.push_str("Action needed:\n");
        action.iter().for_each(|l| out.push_str(&format!("  ⚠ {l}\n")));
    }
    if !config.is_empty() {
        out.push_str("Config changes:\n");
        config.iter().for_each(|l| out.push_str(&format!("  • {l}\n")));
    }
    if new.is_empty() {
        out.push_str("(the release's changelog has no entry this build lacks)\n");
    }
    for e in new {
        out.push_str(&format!("\n## {}\n", e.heading));
        for l in &e.lines {
            out.push_str(l);
            out.push('\n');
        }
    }
    out
}

/// `SHA256SUMS` (`sha256sum`'s format: `<hex>  <name>`, or `<hex> *<name>`): name → hex. Pure.
pub fn parse_sums(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|l| {
            let (hex, name) = l.trim().split_once(char::is_whitespace)?;
            let name = name.trim_start().trim_start_matches('*');
            (hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) && !name.is_empty())
                .then(|| (name.to_string(), hex.to_ascii_lowercase()))
        })
        .collect()
}

/// SHA-256 of `bytes`, lowercase hex. Pure.
pub fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// `bytes` match `name`'s line in `sums`, or why not. Pure.
pub fn verify(sums: &HashMap<String, String>, name: &str, bytes: &[u8]) -> std::result::Result<(), String> {
    let want = sums.get(name).ok_or_else(|| format!("SHA256SUMS has no line for {name}"))?;
    let got = sha256_hex(bytes);
    if &got == want {
        Ok(())
    } else {
        Err(format!("{name}: checksum {got} doesn't match SHA256SUMS ({want}) — not installed"))
    }
}

/// A downloaded binary written next to its target: `dir/.<name>.new-<pid>`, executable.
pub fn stage(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(format!(".{name}.new-{}", std::process::id()));
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

/// That the staged `arena` runs here and is `want`: its `--version` (`arena 2.0.… (sha)`)
/// names that version. A binary for another platform, or a cut-short one, fails this.
///
/// A just-written file can't be run while any process still has it open for writing, and a
/// child forked by another thread holds our descriptor until it execs (`ETXTBSY`, "Text file
/// busy" — seen when the tests ran 16 at once), so that one error is retried for up to a second.
pub fn check_runs(staged: &Path, want: Version) -> std::result::Result<(), String> {
    let mut busy = 0;
    let out = loop {
        match std::process::Command::new(staged).arg("--version").stdin(std::process::Stdio::null()).output() {
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && busy < 20 => {
                busy += 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            r => break r.map_err(|e| format!("the new arena doesn't run here: {e}"))?,
        }
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let said = text.split_whitespace().nth(1).and_then(Version::parse);
    match said {
        Some(v) if out.status.success() && v == want => Ok(()),
        _ => Err(format!("the new arena --version said {:?}, not arena {want}", text.trim())),
    }
}

/// Put the staged binary `staged` in place as `dir/name`, keeping the one there as
/// `dir/name.prev` (replacing an older `.prev`), with the old one's mode and owner. The old
/// binary is hard-linked to `.prev` first, then the new one renamed over it: the path never
/// stops holding a whole binary, so a cron tick at that moment runs one or the other.
/// Running processes keep the old one, and their `/proc/<pid>/exe` says `(deleted)` from then
/// on ([`stale_processes`]).
pub fn install(dir: &Path, name: &str, staged: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let target = dir.join(name);
    if let Ok(meta) = std::fs::metadata(&target) {
        std::fs::set_permissions(staged, meta.permissions())?;
        // Ours to keep: root updating prod's dev-owned binaries keeps them dev's. Best effort
        // — a user can't chown to someone else, and then the file is just theirs.
        let _ = std::os::unix::fs::chown(staged, Some(meta.uid()), Some(meta.gid()));
        let prev = dir.join(format!("{name}.prev"));
        match std::fs::remove_file(&prev) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        if std::fs::hard_link(&target, &prev).is_err() {
            std::fs::copy(&target, &prev)?;
        }
    }
    std::fs::rename(staged, &target)
}

/// Processes still running a replaced `exe` (e.g. `/home/dev/.cargo/bin/arena-tui`): those
/// whose `<proc>/<pid>/exe` reads `<exe> (deleted)`, with when each started (its `<proc>/<pid>`
/// directory's time). `proc` is `/proc` (a fake one in tests). Sorted by pid.
pub fn stale_processes(proc: &Path, exe: &Path) -> Vec<(u32, Option<std::time::SystemTime>)> {
    let gone = format!("{} (deleted)", exe.display());
    let mut out: Vec<(u32, Option<std::time::SystemTime>)> = std::fs::read_dir(proc)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|d| {
            let pid: u32 = d.file_name().to_str()?.parse().ok()?;
            let link = std::fs::read_link(d.path().join("exe")).ok()?;
            (link.as_os_str() == gone.as_str()).then(|| (pid, d.metadata().ok().and_then(|m| m.modified().ok())))
        })
        .collect();
    out.sort_by_key(|(pid, _)| *pid);
    out
}

/// What [`check`] found: the latest release, whether it's newer than this build, and (when it
/// is) what it brings, as [`describe_changes`] says it.
#[derive(Debug, Clone)]
pub struct Checked {
    pub release: Release,
    pub newer: bool,
    pub changes: String,
}

/// Read the latest release and, if it's newer than `installed`, what changed: its
/// `CHANGELOG.md` against `embedded` (this build's own). A release without a changelog says
/// so instead of failing — the binaries are what matter.
pub async fn check(client: &Client, api: &str, installed: Version, embedded: &str) -> Result<Checked> {
    let release = latest(client, api).await?;
    if release.version <= installed {
        return Ok(Checked { release, newer: false, changes: String::new() });
    }
    let changes = match release.asset("CHANGELOG.md") {
        Ok(a) => match download(client, a).await {
            Ok(bytes) => describe_changes(&new_since(embedded, &String::from_utf8_lossy(&bytes))),
            Err(e) => format!("(couldn't read the release's changelog: {e})\n"),
        },
        Err(_) => "(the release has no CHANGELOG.md)\n".into(),
    };
    Ok(Checked { release, newer: true, changes })
}

/// The binaries a release installs on `target`: `(name in the install dir, asset name)`.
pub fn binaries(target: &str) -> [(&'static str, String); 2] {
    [("arena-tui", format!("arena-tui-{target}")), ("arena", format!("arena-{target}"))]
}

/// Both binaries of a release, downloaded, checked and written next to their targets, not yet
/// in place. Dropped without [`Staged::install`], the staged files are removed.
#[derive(Debug)]
pub struct Staged {
    dir: PathBuf,
    /// `(name, staged path)`, `arena-tui` first so `arena` — the one cron runs — goes last.
    files: Vec<(&'static str, PathBuf)>,
    /// Each binary's proven provenance, by asset name, in the same order (none when skipped).
    pub proofs: Vec<(String, attest::Proof)>,
}

/// How [`fetch`] checks each binary's build provenance.
pub enum Provenance<'a> {
    /// Each binary must be proven built as `expected` says ([`attest::verify`]), from the
    /// attestations `source` has.
    Verify { source: &'a dyn attest::Source, expected: attest::Expected },
    /// Not checked (`update --skip-attestation`): `SHA256SUMS` and the version check only.
    Skip,
}

/// Download `release`'s binaries for `target` and its `SHA256SUMS`, check each against it and
/// its provenance ([`Provenance`]), stage them in `dir` ([`stage`]) and check the new `arena`
/// runs here ([`check_runs`]). A binary is written, let alone run, only once it is proven.
/// Nothing in `dir` changes but the staged files, and those go again on any failure.
pub async fn fetch(client: &Client, release: &Release, target: &str, dir: &Path, provenance: &Provenance<'_>) -> Result<Staged> {
    let verifier = match provenance {
        Provenance::Verify { .. } => Some(attest::verifier()?),
        Provenance::Skip => None,
    };
    let sums = parse_sums(&String::from_utf8_lossy(&download(client, release.asset("SHA256SUMS")?).await?));
    let mut staged = Staged { dir: dir.to_path_buf(), files: Vec::new(), proofs: Vec::new() };
    for (name, asset_name) in binaries(target) {
        let asset = release.asset(&asset_name)?;
        let bytes = download(client, asset).await?;
        verify(&sums, &asset_name, &bytes)?;
        if let (Provenance::Verify { source, expected }, Some(verifier)) = (provenance, &verifier) {
            let proof = attest::verify(*source, verifier, expected, &asset_name, &sha256_hex(&bytes))
                .await
                .map_err(|e| format!("{e} — not installed"))?;
            staged.proofs.push((asset_name.clone(), proof));
        }
        let path = stage(dir, name, &bytes)
            .map_err(|e| format!("can't write {} ({e}) — run the update as the binaries' owner, or with sudo", dir.display()))?;
        staged.files.push((name, path));
    }
    let arena = staged.files.iter().find(|(n, _)| *n == "arena").map(|(_, p)| p.clone());
    if let Some(p) = arena {
        check_runs(&p, release.version)?;
    }
    Ok(staged)
}

impl Staged {
    /// Put both in place ([`install`]), keeping the old ones as `.prev`.
    /// One that fails stops it there: what was put in place stays (with its `.prev`), the
    /// rest is removed.
    pub fn install(mut self) -> Result<()> {
        while !self.files.is_empty() {
            let (name, path) = self.files.remove(0);
            if let Err(e) = install(&self.dir, name, &path) {
                let _ = std::fs::remove_file(&path);
                return Err(format!("installing {name} in {}: {e}", self.dir.display()));
            }
        }
        Ok(())
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        for (_, path) in &self.files {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
#[path = "update_tests.rs"]
mod tests;
