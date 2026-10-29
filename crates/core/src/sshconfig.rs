//! Generating the participant-facing `~/.ssh/config` (legacy `ssh_config_manual.py` /
//! `ssh_config_proxy.py`).
//!
//! Participants reach pods as `ssh <prefix>-<name>`. Two ways to wire that up:
//!   * **manual**: each `Host` block points straight at the pod's *current* provider
//!     endpoint (IP + port). Simple, but the config must be re-shared whenever a pod
//!     restarts (the endpoint moves).
//!   * **proxy**: each `Host` points at a *stable* port on the proxy host
//!     (`starting_port + machine-index`), which nginx forwards to the live endpoint. The
//!     participant config never changes; only the proxy's nginx config does.
//!
//! Both renderers are pure (just string building) so they're unit-tested. A shared
//! wildcard block (`Host <prefix>-*`) carries the common options; per-machine blocks add
//! only what differs.
//!
//! A single user can have arena keep one such file current on their own machine
//! (`SSH_CONFIG_FILE`, set up by `arena ssh-config --install`): [`sync_for`] rewrites it after
//! every pod change, behind a first line ([`MANAGED_HEADER`]) that marks it as arena's — a file
//! without it is never overwritten — and `~/.ssh/config` pulls it in with one `Include` line
//! ([`include_at`], [`with_include_first`], [`without_include`]).
//!
//! Staff get a file of their own on the proxy box ([`Audience::Admin`]: `SSH_ADMIN_CONFIG_FILE`,
//! `arena ssh-config --admin --install`): every machine at its live endpoint with the admin key
//! ([`render_admin`]), which only that box holds — kept current the same way, under its own first
//! line, so neither sync ever rewrites the other's file.

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::pod::Pod;
use crate::selector::Naming;

/// The wildcard `Host <prefix>-*` header shared by every machine: the SSH user, the
/// shared identity file, and lax host-key checking (pod host keys churn as pods are
/// recreated, so pinning them just causes spurious failures for participants).
/// `host_name` is `Some(proxy_host)` for the proxy layout (all machines share it) and
/// `None` for the manual layout (each machine sets its own `HostName`).
fn wildcard_block(prefix: &str, user: &str, identity_file: &str, host_name: Option<&str>) -> String {
    let mut s = format!("Host {prefix}-*\n    User {user}\n");
    if let Some(h) = host_name {
        s.push_str(&format!("    HostName {h}\n"));
    }
    s.push_str(&format!(
        "    IdentityFile {identity_file}\n    UserKnownHostsFile=/dev/null\n    StrictHostKeyChecking=no\n"
    ));
    s
}

/// Render the **manual** ssh config: a per-pod `Host` block with the pod's current IP +
/// port, for pods that actually have an endpoint. Pods without one are skipped (they're
/// still starting). Output is sorted by name for a stable diff.
pub fn render_manual(prefix: &str, user: &str, identity_file: &str, pods: &[Pod]) -> String {
    let mut out = wildcard_block(prefix, user, identity_file, None);
    let mut rows: Vec<(&str, &str, u16)> = pods
        .iter()
        .filter_map(|p| match (p.ssh_ip.as_deref(), p.ssh_port) {
            (Some(ip), Some(port)) if !ip.trim().is_empty() => Some((p.name.as_str(), ip, port)),
            _ => None,
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(b.0));
    for (name, ip, port) in rows {
        out.push_str(&format!("\nHost {name}\n    HostName {ip}\n    Port {port}\n"));
    }
    out
}

/// Render the **proxy** ssh config: each machine in `candidates` gets a `Host` block
/// whose `Port` is `starting_port + index`, all sharing the proxy host from the wildcard
/// block. This is index-based (not endpoint-based), so it covers every machine in the
/// list whether or not it's currently running — matching the stable-port proxy scheme.
pub fn render_proxy(
    prefix: &str,
    user: &str,
    identity_file: &str,
    proxy_host: &str,
    starting_port: u16,
    candidates: &[String],
) -> String {
    let mut out = wildcard_block(prefix, user, identity_file, Some(proxy_host));
    for (idx, entry) in candidates.iter().enumerate() {
        let port = starting_port as u32 + idx as u32;
        let Ok(port) = u16::try_from(port) else { continue };
        let name = crate::naming::qualify(prefix, entry);
        if crate::naming::is_absolute(entry) {
            // A bare name doesn't match the `Host {prefix}-*` wildcard, so it must carry the
            // shared options itself (same User/HostName/IdentityFile the wildcard supplies).
            out.push_str(&format!(
                "\nHost {name}\n    User {user}\n    HostName {proxy_host}\n    \
                 IdentityFile {identity_file}\n    UserKnownHostsFile=/dev/null\n    \
                 StrictHostKeyChecking=no\n    Port {port}\n"
            ));
        } else {
            out.push_str(&format!("\nHost {name}\n    Port {port}\n"));
        }
    }
    out
}

/// Staff's ssh config on the proxy box (`ssh-config --admin`): a self-contained `Host` block for
/// every machine — this cohort's pods and the `@` staff boxes ([`Naming::is_cohort`],
/// [`Naming::is_staff`]), named as `pods list` names them — at its live endpoint: never a proxy
/// port (the file is used on the proxy box itself), so a staff box is reached the same way. Each
/// offers the admin key alone (`identity`; `IdentitiesOnly`, so no agent key or participant key
/// is tried first and spends sshd's MaxAuthTries). A machine without an endpoint yet (starting,
/// stopped) is left out, as in [`render_manual`]; another prefix's pod or a pod the provider named
/// is no machine of this fleet's. Sorted by name; a name listed twice (a pod mid-replace) gets
/// the first block. Pure.
pub fn render_admin(user: &str, identity: &str, naming: &Naming, pods: &[Pod]) -> String {
    let mut rows: Vec<(&str, &str, u16)> = pods
        .iter()
        .filter(|p| naming.is_cohort(&p.name) || naming.is_staff(&p.name))
        .filter_map(|p| match (p.ssh_ip.as_deref().map(str::trim), p.ssh_port) {
            (Some(ip), Some(port)) if !ip.is_empty() => Some((p.name.as_str(), ip, port)),
            _ => None,
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(b.0));
    rows.dedup_by(|b, a| a.0 == b.0);
    let mut out = String::from("# Every machine at its live endpoint, with the admin key (for staff on the proxy box)\n");
    for (name, ip, port) in rows {
        out.push_str(&format!(
            "\nHost {name}\n    HostName {ip}\n    Port {port}\n    User {user}\n    IdentityFile {identity}\n    \
             IdentitiesOnly yes\n    UserKnownHostsFile=/dev/null\n    StrictHostKeyChecking=no\n"
        ));
    }
    out
}

/// [`render_admin`] from config: `SSH_USER`, and the admin key as this user reaches it
/// ([`crate::ssh::admin_key`]) — the file is for use on this box. `Err` when `ADMIN_SSH_KEY_PATH`
/// isn't set (there's no key for the file to name), or when its path isn't one plain `IdentityFile`
/// argument ([`config_path_problem`]): ssh would read some other file, or fail to parse the file
/// and with it every host `~/.ssh/config` includes it for.
pub fn render_admin_for(cfg: &Config, pods: &[Pod]) -> Result<String, String> {
    let var = crate::ssh::ADMIN_KEY_VAR;
    let identity = crate::ssh::admin_key(cfg).ok_or_else(|| format!("{var} isn't set — staff's config names the admin key"))?;
    if let Some(why) = config_path_problem(&identity) {
        return Err(format!("{var}: {why} — move the key to a path without it"));
    }
    Ok(render_admin(cfg.get("SSH_USER").unwrap_or("root"), &identity, &Naming::from_config(cfg), pods))
}

/// One `Host` block for the backup archive (`arena archive`): its own alias (`archive-<prefix>`,
/// which no fleet wildcard matches), the read-only `backups` user, ONE identity
/// (`IdentitiesOnly`, so an agent's or default key is never offered), and a host-key policy:
/// laptops trust the key on first use (`accept-new`, with the fingerprint printed above the
/// block to compare), pods get the pinned key in their own `known_hosts` file (`yes`). Unlike
/// the fleet's blocks, host keys are checked: the archive is one long-lived server. With
/// `host_key_alias`, the key is looked up under that name rather than the address (`HostKeyAlias`).
pub fn render_archive_block(alias: &str, host: &str, user: &str, identity: &str, known_hosts: Option<&str>, strict: &str, host_key_alias: Option<&str>) -> String {
    let mut s = format!(
        "Host {alias}\n    HostName {host}\n    User {user}\n    IdentityFile {identity}\n    IdentitiesOnly yes\n    StrictHostKeyChecking {strict}\n"
    );
    if let Some(kh) = known_hosts {
        s.push_str(&format!("    UserKnownHostsFile {kh}\n"));
    }
    if let Some(a) = host_key_alias {
        s.push_str(&format!("    HostKeyAlias {a}\n"));
    }
    s
}

/// The first line of the file arena keeps current (`SSH_CONFIG_FILE`, `ssh-config --install`):
/// the mark that it's arena's to rewrite. A file whose first line isn't this (up to its first
/// `—`, so a later wording still counts) is someone's own, and a sync never overwrites it.
pub const MANAGED_HEADER: &str = "# managed by arena (`arena ssh-config`) — rewritten when pods change; edits here are lost";

/// [`MANAGED_HEADER`] for staff's file (`SSH_ADMIN_CONFIG_FILE`, `ssh-config --admin --install`).
/// A mark of its own: neither sync ever takes the other's file for its own.
pub const ADMIN_MANAGED_HEADER: &str =
    "# managed by arena (`arena ssh-config --admin`) — staff's: the admin key, every machine; rewritten when pods change; edits here are lost";

/// Which file arena keeps current: the participants' config (`SSH_CONFIG_FILE`), or staff's on
/// the proxy box (`SSH_ADMIN_CONFIG_FILE`: every machine with the admin key, [`render_admin`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience {
    Participants,
    Admin,
}

impl Audience {
    /// The config key naming the file.
    pub fn key(self) -> &'static str {
        match self {
            Audience::Participants => "SSH_CONFIG_FILE",
            Audience::Admin => "SSH_ADMIN_CONFIG_FILE",
        }
    }

    /// Where `--install` writes it when neither a PATH nor [`Self::key`] says.
    pub fn default_path(self) -> &'static str {
        match self {
            Audience::Participants => "~/.ssh/arena.conf",
            Audience::Admin => "~/.ssh/arena-admin.conf",
        }
    }

    /// The file's first line: [`MANAGED_HEADER`] or [`ADMIN_MANAGED_HEADER`].
    pub fn header(self) -> &'static str {
        match self {
            Audience::Participants => MANAGED_HEADER,
            Audience::Admin => ADMIN_MANAGED_HEADER,
        }
    }

    /// The command that writes it: `arena ssh-config`, `arena ssh-config --admin`.
    pub fn command(self) -> &'static str {
        match self {
            Audience::Participants => "arena ssh-config",
            Audience::Admin => "arena ssh-config --admin",
        }
    }

    /// The header up to its first `—`: what marks a file as this audience's.
    fn mark(self) -> &'static str {
        let h = self.header();
        h.split(" — ").next().unwrap_or(h)
    }

    /// The managed file's whole text for `rendered`: the header first.
    pub fn text(self, rendered: &str) -> String {
        format!("{}\n{rendered}", self.header())
    }

    /// Whether `existing` is this audience's file to rewrite: its first line is the header (any
    /// wording after its `—`), or it holds nothing but whitespace. The other audience's file is
    /// not. Pure.
    pub fn owns(self, existing: &str) -> bool {
        let mark = self.mark();
        existing.trim().is_empty() || existing.lines().next().is_some_and(|l| l.trim_end() == mark || l.starts_with(&format!("{mark} ")))
    }

    /// What a sync does with this audience's file, given what's there (`None`: no file yet). Pure.
    pub fn plan(self, existing: Option<&str>, want: &str) -> FilePlan {
        match existing {
            None => FilePlan::Write,
            Some(t) if t == want => FilePlan::Unchanged,
            Some(t) if self.owns(t) => FilePlan::Write,
            Some(_) => FilePlan::NotOurs,
        }
    }

    /// [`Self::key`]'s path, a leading `~/` expanded against `home` ([`managed_path`]).
    pub fn path(self, cfg: &Config, home: Option<&str>) -> Option<Result<PathBuf, String>> {
        let key = self.key();
        let v = cfg.get(key).map(str::trim).filter(|v| !v.is_empty())?;
        Some(expand_home(v, home).and_then(|p| {
            if p.is_absolute() {
                Ok(p)
            } else {
                Err(format!("{key} must be an absolute path or start with ~/ (got {v})"))
            }
        }))
    }
}

/// The managed file's whole text for `rendered` (what `ssh-config` prints): the header first.
pub fn managed_text(rendered: &str) -> String {
    Audience::Participants.text(rendered)
}

/// Whether `existing` is a file arena may rewrite: its first line is [`MANAGED_HEADER`] (any
/// wording after its `—`), or it holds nothing but whitespace — nobody's work is in it. Pure.
pub fn is_managed(existing: &str) -> bool {
    Audience::Participants.owns(existing)
}

/// What a sync does with the managed file, given what's there (`None`: no file yet). Pure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilePlan {
    /// Missing, or arena's with other text: (re)written.
    Write,
    /// Already exactly `want`: left alone, not even its time touched.
    Unchanged,
    /// Someone's own file at that path: never overwritten by a sync.
    NotOurs,
}

pub fn file_plan(existing: Option<&str>, want: &str) -> FilePlan {
    Audience::Participants.plan(existing, want)
}

/// The participants' config this fleet's managed file holds: the proxy's stable ports when
/// `SSH_PROXY_HOST` is set (`pods` unused: the ports are the name list's), else each pod's own
/// endpoint — exactly as `ssh-config --proxy` / `ssh-config` print them. The backup archive's
/// block stays `ssh-config --out`'s: it needs its pinned host-key file written beside it.
pub fn render_for(cfg: &Config, pods: &[Pod]) -> Result<String, String> {
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    let user = cfg.get("SSH_USER").unwrap_or("root");
    // The identity file as the participant references it: the configured value verbatim.
    let identity = cfg.get("SHARED_SSH_KEY_PATH").unwrap_or("~/.ssh/shared_infra_key");
    if uses_proxy(cfg) {
        let px = crate::proxy::ProxyConfig::from_config(cfg).map_err(|e| e.to_string())?;
        Ok(render_proxy(prefix, user, identity, &px.proxy_host, px.starting_port, &cfg.machine_names))
    } else {
        Ok(render_manual(prefix, user, identity, pods))
    }
}

/// Whether the managed file takes the proxy layout (`SSH_PROXY_HOST` set) — then it needs no
/// listing of the pods.
pub fn uses_proxy(cfg: &Config) -> bool {
    cfg.get("SSH_PROXY_HOST").is_some_and(|h| !h.trim().is_empty())
}

/// `SSH_CONFIG_FILE`, a leading `~/` expanded against `home`: `None` when it's unset or empty;
/// `Err` for a relative path (a cron job and a shell would resolve it to different files) or a
/// `~/` with no home to expand it against.
pub fn managed_path(cfg: &Config, home: Option<&str>) -> Option<Result<PathBuf, String>> {
    Audience::Participants.path(cfg, home)
}

/// `p` with a leading `~/` (or a lone `~`) taken as `home`.
pub fn expand_home(p: &str, home: Option<&str>) -> Result<PathBuf, String> {
    let rest = match p.strip_prefix("~/") {
        Some(rest) => rest,
        None if p == "~" => "",
        None => return Ok(PathBuf::from(p)),
    };
    let home = home.map(str::trim).filter(|h| h.starts_with('/')).ok_or_else(|| format!("no home to expand {p} against"))?;
    Ok(Path::new(home).join(rest))
}

/// Bring arena's file at `path` to `want` (already [`managed_text`]) — only when the text
/// differs, and never over a file that isn't arena's ([`file_plan`]), a symlink or anything but
/// a regular file. `Ok(true)` = written.
pub fn sync_file(path: &Path, want: &str) -> Result<bool, String> {
    sync_file_as(Audience::Participants, path, want)
}

/// [`sync_file`] for `audience`'s file: never over a file that isn't that audience's — the other
/// one's included.
pub fn sync_file_as(audience: Audience, path: &Path, want: &str) -> Result<bool, String> {
    let shown = path.display();
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => return Err(format!("{shown} is a symlink — left as it is")),
        Ok(m) if !m.is_file() => return Err(format!("{shown} isn't a regular file — left as it is")),
        _ => {}
    }
    let existing = match std::fs::read_to_string(path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("reading {shown}: {e}")),
    };
    match audience.plan(existing.as_deref(), want) {
        FilePlan::Unchanged => Ok(false),
        FilePlan::NotOurs => Err(format!(
            "{shown} isn't arena's (its first line isn't `{}`) — left as it is; `{} --install` replaces it after asking",
            audience.mark(),
            audience.command()
        )),
        FilePlan::Write => write_atomic(path, want, 0o600).map(|()| true),
    }
}

/// Write `text` to `path` atomically: a temp file beside it (mode `mode`, synced), renamed over
/// it — a reader (ssh) sees the old file or the new one, never half of one. The folder is made
/// (0700, as `~/.ssh` must be) when missing. The temp file is removed if anything fails.
///
/// The temp name is new for every write (pid, a counter, the time) and created exclusively
/// (`create_new`, O_EXCL — which never follows a symlink): two writes in one process never share
/// it, and nothing already at that name — a stale temp, or a link someone planted to a key — is
/// ever opened, truncated or chmod-ed.
pub fn write_atomic(path: &Path, text: &str, mode: u32) -> Result<(), String> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let shown = path.display();
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    if !dir.exists() {
        let mut b = std::fs::DirBuilder::new();
        b.recursive(true);
        #[cfg(unix)]
        b.mode(0o700);
        b.create(dir).map_err(|e| format!("making {}: {e}", dir.display()))?;
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (mut f, tmp) = {
        let mut tries = 0;
        loop {
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
            let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let tmp = dir.join(format!(".{name}.arena-tmp-{}-{seq}-{nanos}", std::process::id()));
            let mut o = std::fs::OpenOptions::new();
            o.write(true).create_new(true);
            #[cfg(unix)]
            o.mode(mode);
            match o.open(&tmp) {
                Ok(f) => break (f, tmp),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && tries < 16 => tries += 1,
                Err(e) => return Err(format!("writing {shown}: {e}")),
            }
        }
    };
    let written = (|| -> std::io::Result<()> {
        // Through the open file (fchmod), so exactly `mode`: the umask narrows it at creation.
        #[cfg(unix)]
        f.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(mode))?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    written.map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("writing {shown}: {e}")
    })
}

/// What [`sync_for`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Synced {
    pub path: PathBuf,
    /// The text changed and was written (else it was already current).
    pub written: bool,
}

/// Keep `SSH_CONFIG_FILE` current from `pods` — a COMPLETE listing (a provider that failed to
/// list would drop its pods' blocks; the caller skips then): `None` when the key is unset;
/// `Some(Err)` with why it wasn't (a bad path, a file that isn't arena's, a failed write) —
/// one warning line for the caller, never a failed command. `home`: where `~/` points.
pub fn sync_for(cfg: &Config, pods: &[Pod], home: Option<&str>) -> Option<Result<Synced, String>> {
    let path = match managed_path(cfg, home)? {
        Ok(p) => p,
        Err(e) => return Some(Err(e)),
    };
    let want = match render_for(cfg, pods) {
        Ok(r) => managed_text(&r),
        Err(e) => return Some(Err(format!("{}: {e}", path.display()))),
    };
    Some(sync_file(&path, &want).map(|written| Synced { path, written }))
}

/// Keep `SSH_ADMIN_CONFIG_FILE` current from `pods` (a COMPLETE listing, as for [`sync_for`]):
/// staff's file on the proxy box ([`render_admin`]). `None` when the key is unset — and when this
/// user can't read the admin key: the file is of use only to someone who can, so for anyone else
/// there's nothing to keep current (`config check` says the key is unreadable). `Some(Err)`: why
/// it wasn't written — a bad path, the same file as `SSH_CONFIG_FILE` (each sync would refuse the
/// other's), `ADMIN_SSH_KEY_PATH` unset, a file that isn't staff's, a failed write.
pub fn sync_admin_for(cfg: &Config, pods: &[Pod], home: Option<&str>) -> Option<Result<Synced, String>> {
    let path = match Audience::Admin.path(cfg, home)? {
        Ok(p) => p,
        Err(e) => return Some(Err(e)),
    };
    if let Some(Ok(theirs)) = managed_path(cfg, home) {
        if theirs == path {
            return Some(Err(format!(
                "{} is SSH_CONFIG_FILE's file too — give SSH_ADMIN_CONFIG_FILE a path of its own",
                path.display()
            )));
        }
    }
    match crate::ssh::admin_key(cfg) {
        Some(k) if std::fs::File::open(&k).is_err() => return None,
        _ => {}
    }
    let want = match render_admin_for(cfg, pods) {
        Ok(r) => Audience::Admin.text(&r),
        Err(e) => return Some(Err(format!("{}: {e}", path.display()))),
    };
    Some(sync_file_as(Audience::Admin, &path, &want).map(|written| Synced { path, written }))
}

/// Both files arena keeps current ([`sync_for`], [`sync_admin_for`]) from one COMPLETE listing:
/// each one that's on, in that order.
pub fn sync_all(cfg: &Config, pods: &[Pod], home: Option<&str>) -> Vec<Result<Synced, String>> {
    [sync_for(cfg, pods, home), sync_admin_for(cfg, pods, home)].into_iter().flatten().collect()
}

/// Whether a sync ([`sync_all`]) has anything to do: a key is set.
pub fn any_on(cfg: &Config, home: Option<&str>) -> bool {
    managed_path(cfg, home).is_some() || Audience::Admin.path(cfg, home).is_some()
}

/// Whether a sync needs the pods listed: the participants' file without a proxy (each pod's own
/// endpoint), or staff's (always the live endpoints). The participants' proxy layout alone needs
/// none.
pub fn needs_listing(cfg: &Config, home: Option<&str>) -> bool {
    (managed_path(cfg, home).is_some() && !uses_proxy(cfg)) || Audience::Admin.path(cfg, home).is_some()
}

/// Where a `~/.ssh/config` pulls in arena's file (`Include <path>`, any of `forms` — the `~/`
/// spelling and the absolute one). Pure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncludeAt {
    /// Before any `Host`/`Match` line: it applies to every host.
    Global,
    /// After a `Host`/`Match` line (1-based line number): it applies only inside that block.
    InBlock(usize),
    Absent,
}

/// An `Include` line for exactly one of `forms`: ssh's keyword in any case, then whitespace or
/// one `=`, then the path alone.
fn is_include_of(line: &str, forms: &[&str]) -> bool {
    let l = line.trim();
    let Some(kw) = l.get(..7).filter(|k| k.eq_ignore_ascii_case("include")) else { return false };
    let rest = &l[kw.len()..];
    let Some(first) = rest.chars().next().filter(|c| c.is_whitespace() || *c == '=') else { return false };
    let mut rest = rest[first.len_utf8()..].trim_start();
    if first != '=' {
        rest = rest.strip_prefix('=').map_or(rest, str::trim_start);
    }
    forms.contains(&rest.trim_end())
}

/// Whether `line` starts a `Host`/`Match` block (keyword in any case).
fn starts_block(line: &str) -> bool {
    let word = line.split(|c: char| c.is_whitespace() || c == '=').next().unwrap_or_default();
    word.eq_ignore_ascii_case("host") || word.eq_ignore_ascii_case("match")
}

pub fn include_at(config: &str, forms: &[&str]) -> IncludeAt {
    let mut in_block = false;
    for (i, line) in config.lines().enumerate() {
        if is_include_of(line, forms) {
            return if in_block { IncludeAt::InBlock(i + 1) } else { IncludeAt::Global };
        }
        in_block |= starts_block(line.trim_start());
    }
    IncludeAt::Absent
}

/// `config` with `Include <path>` as its very first line — where ssh applies it to every host —
/// in the file's own line endings (CRLF when it uses them). Pure.
pub fn with_include_first(config: &str, path: &str) -> String {
    let eol = if config.contains("\r\n") { "\r\n" } else { "\n" };
    format!("Include {path}{eol}{config}")
}

/// `config` without its global `Include` line(s) for `forms` — before any `Host`/`Match` block,
/// where `--install` puts it — everything else byte for byte; `None` when it has none. One inside
/// a block is the user's own choice, left as it is (as `--install` leaves it). Pure.
pub fn without_include(config: &str, forms: &[&str]) -> Option<String> {
    let (mut in_block, mut removed) = (false, false);
    let mut out = String::with_capacity(config.len());
    for line in config.split_inclusive('\n') {
        if !in_block && is_include_of(line, forms) {
            removed = true;
            continue;
        }
        in_block |= starts_block(line.trim_start());
        out.push_str(line);
    }
    removed.then_some(out)
}

/// Why `path` can't go in an `Include` line as one plain argument, if it can't: ssh splits its
/// arguments on whitespace, reads quotes and `#`, and expands globs (`*?[]`), `${…}`, `%` tokens
/// and backslash escapes in it — any of which would include some other file, or none. Pure.
pub fn include_path_problem(path: &str) -> Option<String> {
    let bad = |c: char| c.is_whitespace() || c.is_control() || "\"'\\#$%*?[]".contains(c);
    path.chars().find(|c| bad(*c)).map(|c| {
        format!("{path} has {c:?} in it, which an ssh `Include` line can't take as one plain path — choose a path without it")
    })
}

/// Why `path` can't be one plain argument in an ssh config (`IdentityFile`), if it can't:
/// whitespace splits it (a line with extra arguments fails the whole file), quotes, `#` and
/// backslashes are read, and `${…}` / `%` tokens expanded. Pure.
pub fn config_path_problem(path: &str) -> Option<String> {
    let bad = |c: char| c.is_whitespace() || c.is_control() || "\"'\\#$%".contains(c);
    path.chars().find(|c| bad(*c)).map(|c| format!("{path} has {c:?} in it, which an ssh config can't take as one plain path"))
}

/// Every file a user's `~/.ssh/config` (its text, `config`) pulls in, as ssh resolves its
/// `Include` lines: anywhere in the file (a block's too), each argument quoted or not, `~/` taken
/// as `home`, a relative path under `~/.ssh`, glob patterns (`*`, `?`, `[…]`) matched against what
/// exists, and the included files' own `Include`s followed (to ssh's depth of 16). For telling
/// whether arena's other file is in use. Reads those files, never writes. `Err(argument)`: one
/// arena can't resolve (a `${…}` or `%` token, or no home to resolve it against) — the caller
/// can't tell what it includes.
pub fn included_files(config: &str, home: Option<&str>) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    follow_includes(config, home, 0, &mut out)?;
    Ok(out)
}

fn follow_includes(config: &str, home: Option<&str>, depth: usize, out: &mut Vec<PathBuf>) -> Result<(), String> {
    if depth > 16 {
        return Ok(());
    }
    for args in config.lines().filter_map(include_args) {
        for arg in args {
            let pattern = include_target(&arg, home).ok_or_else(|| arg.clone())?;
            for file in glob_files(&pattern) {
                if out.contains(&file) {
                    continue;
                }
                out.push(file.clone());
                if let Ok(text) = std::fs::read_to_string(&file) {
                    follow_includes(&text, home, depth + 1, out)?;
                }
            }
        }
    }
    Ok(())
}

/// One `Include` line's arguments as ssh reads them — the keyword in any case, then whitespace or
/// one `=`; whitespace-separated words, a `"…"` or `'…'` stretch kept whole without its quotes, a
/// word starting with `#` ending the line. `None` for any other line. Pure.
fn include_args(line: &str) -> Option<Vec<String>> {
    let l = line.trim();
    let kw = l.get(..7).filter(|k| k.eq_ignore_ascii_case("include"))?;
    let rest = &l[kw.len()..];
    if !rest.starts_with(|c: char| c.is_whitespace() || c == '=') {
        return None;
    }
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=').unwrap_or(rest);
    let (mut args, mut word, mut quote, mut started) = (Vec::new(), String::new(), None::<char>, false);
    for c in rest.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    args.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            (None, '#') if !started => break,
            (None, c) => {
                started = true;
                word.push(c);
            }
        }
    }
    if started {
        args.push(word);
    }
    Some(args)
}

/// Where one `Include` argument points in a user config: `~/` (or `~`) against `home`, a relative
/// path under `~/.ssh`, an absolute one as it is. `None` when it holds a `${…}` or `%` token, or
/// needs a home there's none of. Pure.
fn include_target(arg: &str, home: Option<&str>) -> Option<PathBuf> {
    if arg.contains(['$', '%']) {
        return None;
    }
    let home = home.map(str::trim).filter(|h| h.starts_with('/'));
    if arg.starts_with('~') {
        return expand_home(arg, home).ok().filter(|p| p.is_absolute());
    }
    let p = Path::new(arg);
    if p.is_absolute() {
        Some(p.to_path_buf())
    } else {
        Some(Path::new(home?).join(".ssh").join(p))
    }
}

/// The files `pattern` names, as glob(3) finds them: each component with `*`, `?` or `[…]` matched
/// against its folder (a leading `.` only by a pattern that starts with one), sorted; a path with
/// none, if it exists. Only what exists.
fn glob_files(pattern: &Path) -> Vec<PathBuf> {
    let magic = |s: &str| s.contains(['*', '?', '[']);
    let mut found = vec![PathBuf::new()];
    for comp in pattern.components() {
        let part = comp.as_os_str().to_string_lossy();
        if !magic(&part) {
            found.iter_mut().for_each(|p| p.push(comp.as_os_str()));
            continue;
        }
        let mut next = Vec::new();
        for dir in &found {
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            let mut names: Vec<String> = entries.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            names.sort();
            next.extend(names.iter().filter(|n| wild_match(&part, n)).map(|n| dir.join(n)));
        }
        found = next;
    }
    found.retain(|p| p.exists());
    found
}

/// glob(7) matching of one name: `*`, `?`, `[abc]`, `[a-z]`, `[!…]`/`[^…]`; a leading `.` only
/// by a literal one. Pure.
fn wild_match(pat: &str, name: &str) -> bool {
    if name.starts_with('.') && !pat.starts_with('.') {
        return false;
    }
    fn go(p: &[char], n: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => (0..=n.len()).any(|i| go(&p[1..], &n[i..])),
            Some('?') => !n.is_empty() && go(&p[1..], &n[1..]),
            Some('[') => {
                let Some(close) = p.iter().skip(2).position(|&c| c == ']').map(|i| i + 2) else {
                    return n.first() == Some(&'[') && go(&p[1..], &n[1..]);
                };
                let Some(&c) = n.first() else { return false };
                let mut set = &p[1..close];
                let negate = matches!(set.first(), Some('!' | '^'));
                if negate {
                    set = &set[1..];
                }
                let mut hit = false;
                let mut i = 0;
                while i < set.len() {
                    if i + 2 < set.len() && set[i + 1] == '-' {
                        hit |= set[i] <= c && c <= set[i + 2];
                        i += 3;
                    } else {
                        hit |= set[i] == c;
                        i += 1;
                    }
                }
                hit != negate && go(&p[close + 1..], &n[1..])
            }
            Some(&c) => n.first() == Some(&c) && go(&p[1..], &n[1..]),
        }
    }
    let (p, n): (Vec<char>, Vec<char>) = (pat.chars().collect(), name.chars().collect());
    go(&p, &n)
}

/// Whether `meta`'s owner is this process's effective user — what `ssh-config --install` asks of
/// `~/.ssh` and every file it edits there: run as someone else (sudo keeping `HOME`), it would
/// leave them owned by that user, and the files it writes unreadable to their owner's ssh.
#[cfg(unix)]
pub fn owned_by_me(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid takes no arguments, can't fail and touches no memory of ours.
    meta.uid() == unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(name: &str, ip: Option<&str>, port: Option<u16>) -> Pod {
        Pod {
            id: "x".into(),
            name: name.into(),
            provider: "runpod".into(),
            status: "RUNNING".into(),
            gpu_type: None,
            cost_per_hr: None,
            ssh_ip: ip.map(String::from),
            ssh_port: port,
            ..Default::default()
        }
    }

    #[test]
    fn manual_uses_live_endpoints_and_skips_unready() {
        let pods = vec![
            pod("arena8-bloom", Some("2.2.2.2"), Some(22002)),
            pod("arena8-apple", Some("1.1.1.1"), Some(22000)),
            pod("arena8-cloud", None, None), // no endpoint -> skipped
        ];
        let cfg = render_manual("arena8", "root", "~/.ssh/arena8_key", &pods);
        // shared block
        assert!(cfg.contains("Host arena8-*"));
        assert!(cfg.contains("IdentityFile ~/.ssh/arena8_key"));
        assert!(cfg.contains("StrictHostKeyChecking=no"));
        // per-pod, sorted (apple before bloom)
        let a = cfg.find("Host arena8-apple").unwrap();
        let b = cfg.find("Host arena8-bloom").unwrap();
        assert!(a < b);
        assert!(cfg.contains("HostName 1.1.1.1\n    Port 22000"));
        // unready pod absent
        assert!(!cfg.contains("arena8-cloud"));
    }

    #[test]
    fn proxy_is_index_based_and_covers_all_candidates() {
        let candidates = ["apple", "autumn", "bloom"].map(String::from).to_vec();
        let cfg = render_proxy("arena8", "root", "~/.ssh/arena8_key", "cute.sus.cat", 7000, &candidates);
        // proxy host on the wildcard block; per-machine blocks carry only the port
        assert!(cfg.contains("Host arena8-*\n    User root\n    HostName cute.sus.cat"));
        assert!(cfg.contains("Host arena8-apple\n    Port 7000"));
        assert!(cfg.contains("Host arena8-autumn\n    Port 7001"));
        assert!(cfg.contains("Host arena8-bloom\n    Port 7002"));
    }

    /// The managed file's first line marks it as arena's: a sync writes a missing file or one of
    /// arena's, leaves the same text alone, and never overwrites anyone else's — an empty file
    /// holds nobody's work.
    #[test]
    fn a_managed_file_is_rewritten_only_when_arena_s_and_different() {
        let want = managed_text("Host arena-*\n    User root\n");
        assert!(want.starts_with("# managed by arena (`arena ssh-config`) — "), "{want}");
        let older_wording = "# managed by arena (`arena ssh-config`) — kept current\nHost arena-*\n";
        let cases: &[(Option<&str>, FilePlan)] = &[
            (None, FilePlan::Write),
            (Some(want.as_str()), FilePlan::Unchanged),
            (Some(older_wording), FilePlan::Write),
            (Some(""), FilePlan::Write),
            (Some("\n  \n"), FilePlan::Write),
            // Someone's own: hand-made, or what `ssh-config --out` wrote before the header.
            (Some("Host arena-*\n    User root\n"), FilePlan::NotOurs),
            (Some("# my hosts\n# managed by arena (`arena ssh-config`) — x\n"), FilePlan::NotOurs),
            (Some("# managed by arena (`arena ssh-config`)x\n"), FilePlan::NotOurs),
        ];
        for (existing, plan) in cases {
            assert_eq!(file_plan(*existing, &want), *plan, "{existing:?}");
        }
        assert!(is_managed("# managed by arena (`arena ssh-config`)\n"), "the mark alone");
    }

    /// Where `~/.ssh/config` pulls the file in: before any Host/Match block (every host), inside
    /// one (that block only), or not at all — in either spelling of the path, ssh's keyword in
    /// any case, `Include path` or `Include=path`; never a longer path or a comment.
    #[test]
    fn include_lines_are_found_added_first_and_removed_exactly() {
        let forms = ["~/.ssh/arena.conf", "/home/u/.ssh/arena.conf"];
        let cases: &[(&str, IncludeAt)] = &[
            ("", IncludeAt::Absent),
            ("Include ~/.ssh/arena.conf\nHost *\n    User me\n", IncludeAt::Global),
            ("# mine\n\ninclude    /home/u/.ssh/arena.conf\n", IncludeAt::Global),
            ("Include=~/.ssh/arena.conf\n", IncludeAt::Global),
            ("  INCLUDE = ~/.ssh/arena.conf\r\n", IncludeAt::Global),
            ("Host box\n    HostName 1.2.3.4\n    Include ~/.ssh/arena.conf\n", IncludeAt::InBlock(3)),
            ("Match host x\nInclude ~/.ssh/arena.conf\n", IncludeAt::InBlock(2)),
            ("HostName nope\nInclude ~/.ssh/arena.conf\n", IncludeAt::Global),
            ("Include ~/.ssh/arena.conf.bak\n", IncludeAt::Absent),
            ("# Include ~/.ssh/arena.conf\n", IncludeAt::Absent),
            ("Include ~/.ssh/arena.conf ~/.ssh/other\n", IncludeAt::Absent),
            ("Includes ~/.ssh/arena.conf\n", IncludeAt::Absent),
        ];
        for (config, at) in cases {
            assert_eq!(include_at(config, &forms), *at, "{config:?}");
        }
        // Added first, before every Host/Match block, in the file's own line endings.
        assert_eq!(with_include_first("", "~/.ssh/arena.conf"), "Include ~/.ssh/arena.conf\n");
        let mine = "Host box\n    HostName 1.2.3.4\n";
        let added = with_include_first(mine, "~/.ssh/arena.conf");
        assert_eq!(added, format!("Include ~/.ssh/arena.conf\n{mine}"));
        assert_eq!(include_at(&added, &forms), IncludeAt::Global);
        assert_eq!(with_include_first("Host a\r\n  User b\r\n", "~/x"), "Include ~/x\r\nHost a\r\n  User b\r\n");
        // Removed exactly: every global copy of the line, nothing else touched (CRLF kept); one
        // inside a Host/Match block is the user's, left as it is.
        assert_eq!(without_include(&added, &forms).as_deref(), Some(mine));
        let twice = "Include ~/.ssh/arena.conf\n# mine\ninclude /home/u/.ssh/arena.conf\nHost a\n";
        assert_eq!(without_include(twice, &forms).as_deref(), Some("# mine\nHost a\n"));
        let crlf = "Include ~/.ssh/arena.conf\r\nHost a\r\n  User b\r\nInclude=/home/u/.ssh/arena.conf";
        assert_eq!(without_include(crlf, &forms).as_deref(), Some("Host a\r\n  User b\r\nInclude=/home/u/.ssh/arena.conf"));
        assert_eq!(without_include("Host a\n    Include ~/.ssh/arena.conf\n", &forms), None, "only in a block: nothing");
        assert_eq!(without_include(mine, &forms), None);
        assert_eq!(without_include("Include ~/.ssh/arena.conf.bak\n", &forms), None);
    }

    /// A path goes in an `Include` line only as one plain argument: nothing ssh would split on,
    /// unquote, expand or read as a comment.
    #[test]
    fn include_paths_are_plain_or_refused() {
        for ok in ["~/.ssh/arena.conf", "/home/u/.ssh/fleet-2.conf", "~/.ssh/a_b.c@d+e,f=g"] {
            assert_eq!(include_path_problem(ok), None, "{ok}");
        }
        for bad in ["~/.ssh/my arena.conf", "~/.ssh/a\tb", "~/x\"y", "~/x'y", "~/x\\y", "~/x#y", "~/${X}", "~/%d.conf", "~/*.conf", "~/a?", "~/[a]", "~/a\nb"] {
            assert!(include_path_problem(bad).is_some_and(|e| e.contains("can't take as one plain path")), "{bad:?}");
        }
    }

    /// The layout follows the config: the proxy's stable ports when `SSH_PROXY_HOST` is set,
    /// else each pod's own endpoint — the same text `ssh-config` / `--proxy` print.
    #[test]
    fn render_for_takes_the_proxy_layout_only_with_a_proxy_host() {
        let pods = vec![pod("devtest-apple", Some("1.1.1.1"), Some(22000))];
        let direct = Config::parse("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom)\nSHARED_SSH_KEY_PATH=~/.ssh/k\n");
        assert!(!uses_proxy(&direct));
        assert_eq!(render_for(&direct, &pods).unwrap(), render_manual("devtest", "root", "~/.ssh/k", &pods));
        let proxied = Config::parse(
            "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom)\nSHARED_SSH_KEY_PATH=~/.ssh/k\nSSH_PROXY_HOST=px.example\nSSH_PROXY_STARTING_PORT=9500\n",
        );
        assert!(uses_proxy(&proxied));
        let names = ["apple".to_string(), "bloom".to_string()];
        assert_eq!(render_for(&proxied, &[]).unwrap(), render_proxy("devtest", "root", "~/.ssh/k", "px.example", 9500, &names));
        assert!(!uses_proxy(&Config::parse("SSH_PROXY_HOST=  \n")), "blank is unset");
    }

    /// `SSH_CONFIG_FILE`: `~/` is the home; an absolute path as it is; unset or blank = off; a
    /// relative path (cron and a shell would disagree) or a `~/` without a home is an error.
    #[test]
    fn managed_path_expands_the_home_and_refuses_a_relative_path() {
        let path = |v: &str, home: Option<&str>| managed_path(&Config::parse(&format!("SSH_CONFIG_FILE={v}\n")), home);
        assert_eq!(path("~/.ssh/arena.conf", Some("/home/u")), Some(Ok(PathBuf::from("/home/u/.ssh/arena.conf"))));
        assert_eq!(path("/srv/arena.conf", None), Some(Ok(PathBuf::from("/srv/arena.conf"))));
        assert_eq!(path("\"\"", Some("/home/u")), None);
        assert_eq!(managed_path(&Config::parse(""), Some("/home/u")), None);
        assert!(path("arena.conf", Some("/home/u")).unwrap().unwrap_err().contains("absolute path or start with ~/"));
        assert!(path("~/x", None).unwrap().unwrap_err().contains("no home"));
        assert!(path("~/x", Some("relative")).unwrap().unwrap_err().contains("no home"));
    }

    /// The file on disk: written 0600 when missing or different, its folder made 0700; not
    /// touched when current (same inode, same time); never over someone's own file, a symlink
    /// or a folder; no temp file left behind either way.
    #[cfg(unix)]
    #[test]
    fn sync_file_writes_only_arena_s_file_and_only_on_a_change() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let base = std::env::temp_dir().join(format!("arena-sshconfig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let path = base.join(".ssh").join("arena.conf");
        let want = managed_text("Host a\n");
        assert_eq!(sync_file(&path, &want), Ok(true));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), want);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(base.join(".ssh")).unwrap().permissions().mode() & 0o777, 0o700);
        let before = std::fs::metadata(&path).unwrap();
        assert_eq!(sync_file(&path, &want), Ok(false), "current: not written");
        let after = std::fs::metadata(&path).unwrap();
        assert_eq!((after.ino(), after.mtime_nsec()), (before.ino(), before.mtime_nsec()));
        let newer = managed_text("Host b\n");
        assert_eq!(sync_file(&path, &newer), Ok(true));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);
        // Someone's own file stays exactly as it was.
        std::fs::write(&path, "Host mine\n").unwrap();
        assert!(sync_file(&path, &want).unwrap_err().contains("isn't arena's"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "Host mine\n");
        // A symlink (even to arena's own file) and a folder are left alone.
        let real = base.join("real.conf");
        std::fs::write(&real, &want).unwrap();
        let link = base.join("link.conf");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(sync_file(&link, &newer).unwrap_err().contains("symlink"));
        assert_eq!(std::fs::read_to_string(&real).unwrap(), want);
        assert!(sync_file(&base.join(".ssh"), &newer).unwrap_err().contains("isn't a regular file"));
        let leftovers: Vec<_> = std::fs::read_dir(base.join(".ssh")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(leftovers, ["arena.conf"], "no temp file left");
        // Writes from many threads at once never share a temp file: each one succeeds, and the
        // file is always one whole text, never a mix. The mode is exact whatever the umask did.
        let texts: Vec<String> = (0..8).map(|i| managed_text(&format!("Host t{i}\n{}", "    User x\n".repeat(40 * (i + 1))))).collect();
        std::thread::scope(|s| {
            for text in &texts {
                let path = &path;
                s.spawn(move || {
                    for _ in 0..20 {
                        write_atomic(path, text, 0o666).unwrap();
                    }
                });
            }
        });
        assert!(texts.contains(&std::fs::read_to_string(&path).unwrap()), "one whole text");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o666);
        let leftovers: Vec<_> = std::fs::read_dir(base.join(".ssh")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(leftovers, ["arena.conf"], "no temp file left");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `owned_by_me`: a file this process made is its own; `/` is root's (unless this is root).
    #[cfg(unix)]
    #[test]
    fn owned_by_me_tells_this_user_s_files_from_root_s() {
        let f = std::env::temp_dir().join(format!("arena-owned-{}", std::process::id()));
        std::fs::write(&f, "").unwrap();
        assert!(owned_by_me(&std::fs::metadata(&f).unwrap()));
        let _ = std::fs::remove_file(&f);
        // SAFETY: geteuid can't fail and touches no memory of ours.
        assert_eq!(owned_by_me(&std::fs::metadata("/").unwrap()), unsafe { libc::geteuid() } == 0);
    }

    /// `sync_for`: off without the key; the path's error, or the file's, as the caller's warning.
    #[test]
    fn sync_for_is_off_without_the_key_and_reports_a_bad_path() {
        let pods = vec![pod("devtest-apple", Some("1.1.1.1"), Some(22000))];
        assert_eq!(sync_for(&Config::parse("MACHINE_NAME_PREFIX=devtest\n"), &pods, Some("/home/u")), None);
        let bad = Config::parse("MACHINE_NAME_PREFIX=devtest\nSSH_CONFIG_FILE=arena.conf\n");
        assert!(sync_for(&bad, &pods, Some("/home/u")).unwrap().unwrap_err().contains("absolute path"));
    }

    /// Staff's config (`ssh-config --admin`): every machine of this fleet — cohort pods (a parked
    /// `-old` twin too) and `@` staff boxes — at its live endpoint, each block self-contained
    /// with the admin key alone; nothing else on the account, no machine without an endpoint, a
    /// name twice once (the first listed); sorted; never a proxy port.
    #[test]
    fn the_admin_config_has_every_machine_at_its_endpoint_with_the_admin_key_alone() {
        let list = ["apple", "bloom", "cloud", "@james-gpu", "@lab"].map(String::from).to_vec();
        let naming = Naming { prefix: "arena9", list: &list };
        let pods = vec![
            pod("arena9-bloom", Some("2.2.2.2"), Some(22002)),
            pod("james-gpu", Some("5.5.5.5"), Some(22)),
            pod("arena9-apple", Some("1.1.1.1"), Some(22001)),
            pod("arena9-apple", Some("9.9.9.9"), Some(22009)), // the same name again: the first wins
            pod("arena9-cloud", None, None),                    // starting: no endpoint
            pod("lab", Some(" "), Some(22)),                    // a blank address is none
            pod("arena9-apple-old", Some("3.3.3.3"), Some(22003)),
            pod("arena8-zebra", Some("4.4.4.4"), Some(22004)), // another cohort's
            pod("registered_pink_prawn", Some("6.6.6.6"), Some(22)), // a pod the provider named
        ];
        let got = render_admin("root", "/root/.ssh/arena_admin_key", &naming, &pods);
        let block = |name: &str, ip: &str, port: u16| {
            format!(
                "\nHost {name}\n    HostName {ip}\n    Port {port}\n    User root\n    IdentityFile /root/.ssh/arena_admin_key\n    \
                 IdentitiesOnly yes\n    UserKnownHostsFile=/dev/null\n    StrictHostKeyChecking=no\n"
            )
        };
        let want = format!(
            "# Every machine at its live endpoint, with the admin key (for staff on the proxy box)\n{}{}{}{}",
            block("arena9-apple", "1.1.1.1", 22001),
            block("arena9-apple-old", "3.3.3.3", 22003),
            block("arena9-bloom", "2.2.2.2", 22002),
            block("james-gpu", "5.5.5.5", 22),
        );
        assert_eq!(got, want);
        // From config: SSH_USER, and the admin key as this user reaches it; none set = no file.
        let cfg = Config::parse("MACHINE_NAME_PREFIX=arena9\nMACHINE_NAME_LIST=(apple bloom cloud @james-gpu @lab)\nSSH_USER=ubuntu\n");
        assert!(render_admin_for(&cfg, &pods).unwrap_err().contains("ADMIN_SSH_KEY_PATH isn't set"));
        let cfg = Config::parse(
            "MACHINE_NAME_PREFIX=arena9\nMACHINE_NAME_LIST=(apple bloom cloud @james-gpu @lab)\nSSH_USER=ubuntu\n\
             ADMIN_SSH_KEY_PATH=/nonexistent-arena/arena_admin_key\n",
        );
        let text = render_admin_for(&cfg, &pods).unwrap();
        assert!(text.contains("    User ubuntu\n    IdentityFile /nonexistent-arena/arena_admin_key\n"), "{text}");
        assert_eq!(text.matches("\nHost ").count(), 4, "{text}");
    }

    /// An admin key path ssh would split or expand is refused, never written into the file: one
    /// `IdentityFile` with extra arguments fails the whole config for every host it's included for.
    #[test]
    fn an_admin_key_path_an_ssh_config_cannot_take_is_refused() {
        for (path, bad) in [
            ("/root/.ssh/arena admin key", Some(' ')),
            ("/root/.ssh/arena%u", Some('%')),
            ("/root/.ssh/a#b", Some('#')),
            ("/root/.ssh/${HOME}", Some('$')),
            ("/root/.ssh/arena_admin_key", None),
            ("/home/dev/sandbox/ssh/devtest_admin_key", None),
        ] {
            assert_eq!(config_path_problem(path).is_some(), bad.is_some(), "{path}");
            let cfg = Config::parse(&format!("MACHINE_NAME_PREFIX=devtest\nADMIN_SSH_KEY_PATH=\"{path}\"\n"));
            match (render_admin_for(&cfg, &[]), bad) {
                (Err(e), Some(c)) => assert!(e.starts_with("ADMIN_SSH_KEY_PATH: ") && e.contains(&format!("{c:?}")), "{e}"),
                (Ok(t), None) => assert!(t.starts_with("# Every machine")),
                (r, _) => panic!("{path}: {r:?}"),
            }
        }
    }

    /// The files a `~/.ssh/config` includes, as ssh resolves them: any Include line (a block's
    /// too; `=` or spaces; quoted; several per line, a `#` word ending it), `~/` and relative paths
    /// (under `~/.ssh`), globs against what exists (no dotfile for `*`), nested Includes followed
    /// (a loop once); only existing files. A `${…}`/`%` token, or a relative path with no home,
    /// can't be resolved: the argument, as an error.
    #[cfg(unix)]
    #[test]
    fn included_files_are_resolved_as_ssh_resolves_them() {
        let base = std::env::temp_dir().join(format!("arena-includes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (home, ssh) = (base.join("home"), base.join("home/.ssh"));
        std::fs::create_dir_all(ssh.join("conf.d")).unwrap();
        std::fs::create_dir_all(base.join("abs dir")).unwrap();
        let quoted = base.join("abs dir/quoted.conf");
        for (p, text) in [
            (ssh.join("arena.conf"), ""),
            (quoted.clone(), ""),
            (ssh.join("b.conf"), ""),
            (ssh.join("arena-admin.conf"), ""),
            (ssh.join("conf.d/x.conf"), "Include nested.conf\n"),
            (ssh.join("conf.d/y.conf"), "Include conf.d/y.conf\n"), // itself: followed once
            (ssh.join("conf.d/.hidden.conf"), ""),
            (ssh.join("nested.conf"), "Host n\n"),
        ] {
            std::fs::write(p, text).unwrap();
        }
        let config = format!(
            "Include arena.conf\ninclude=\"{}\"  ~/.ssh/b.conf ~/.ssh/missing.conf # ~/.ssh/c.conf\n\
             Host box\n    Include ~/.ssh/arena-admin.conf\nInclude conf.d/*.conf\nIncluded ~/.ssh/x\nIdentityFile arena.conf\n",
            quoted.display()
        );
        let h = home.to_str();
        let got = included_files(&config, h).unwrap();
        let want =
            vec![ssh.join("arena.conf"), quoted, ssh.join("b.conf"), ssh.join("arena-admin.conf"), ssh.join("conf.d/x.conf"), ssh.join("nested.conf"), ssh.join("conf.d/y.conf")];
        assert_eq!(got, want);
        assert_eq!(included_files("Include ${HOME}/.ssh/arena.conf\n", h), Err("${HOME}/.ssh/arena.conf".to_string()));
        assert_eq!(included_files("Include %d/.ssh/arena.conf\n", h), Err("%d/.ssh/arena.conf".to_string()));
        assert_eq!(included_files("Include arena.conf\n", None), Err("arena.conf".to_string()));
        assert_eq!(included_files("Include /nonexistent-arena/*.conf\nHost x\n", None), Ok(vec![]));
        for (pat, name, hit) in [
            ("*.conf", "arena.conf", true),
            ("*.conf", ".arena.conf", false),
            (".*", ".arena.conf", true),
            ("arena?.conf", "arena1.conf", true),
            ("arena[0-9].conf", "arena7.conf", true),
            ("arena[!0-9].conf", "arena7.conf", false),
            ("arena[^0-9].conf", "arenax.conf", true),
            ("a*b*c", "aXXbYYc", true),
            ("a*b*c", "aXXbYY", false),
            ("[", "[", true),
        ] {
            assert_eq!(wild_match(pat, name), hit, "{pat} {name}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Each audience's file has its own first line: a sync writes its own (or an empty) file and
    /// refuses the other's, so the two never overwrite each other.
    #[test]
    fn the_participants_and_staff_files_never_take_each_other() {
        let theirs = managed_text("Host arena9-*\n");
        let staff = Audience::Admin.text("Host arena9-apple\n");
        assert!(staff.starts_with("# managed by arena (`arena ssh-config --admin`) — "), "{staff}");
        assert_eq!(Audience::Admin.plan(Some(&theirs), &staff), FilePlan::NotOurs);
        assert_eq!(Audience::Participants.plan(Some(&staff), &theirs), FilePlan::NotOurs);
        assert_eq!(file_plan(Some(&staff), &theirs), FilePlan::NotOurs);
        assert_eq!(Audience::Admin.plan(Some(&Audience::Admin.text("Host old\n")), &staff), FilePlan::Write);
        assert_eq!(Audience::Admin.plan(Some("  \n"), &staff), FilePlan::Write);
        assert_eq!(Audience::Admin.plan(Some(&staff), &staff), FilePlan::Unchanged);
        assert_eq!(Audience::Admin.plan(None, &staff), FilePlan::Write);
        let path = |v: &str| Audience::Admin.path(&Config::parse(&format!("SSH_ADMIN_CONFIG_FILE={v}\n")), Some("/root"));
        assert_eq!(path("~/.ssh/arena-admin.conf"), Some(Ok(PathBuf::from("/root/.ssh/arena-admin.conf"))));
        assert!(path("admin.conf").unwrap().unwrap_err().starts_with("SSH_ADMIN_CONFIG_FILE must be an absolute path"));
        assert_eq!(path(""), None);
    }

    /// `SSH_ADMIN_CONFIG_FILE` kept current: off when unset, and for a user who can't read the
    /// admin key (the file would be of no use to them); a warning when the key is unset or the
    /// file is `SSH_CONFIG_FILE`'s too; else written under its own header, 0600, only on a change,
    /// never over the participants' file — and theirs never over it.
    #[cfg(unix)]
    #[test]
    fn the_staff_file_is_kept_current_only_where_the_admin_key_is_readable() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("arena-sshconfig-admin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let key = base.join("arena_admin_key");
        std::fs::write(&key, "private").unwrap();
        let file = base.join("ssh").join("arena-admin.conf");
        let cfg = |extra: &str| {
            Config::parse(&format!(
                "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple @staff)\nSSH_ADMIN_CONFIG_FILE={}\n{extra}",
                file.display()
            ))
        };
        let pods = vec![pod("devtest-apple", Some("1.1.1.1"), Some(22001)), pod("staff", Some("2.2.2.2"), Some(22))];
        let with_key = format!("ADMIN_SSH_KEY_PATH={}\n", key.display());
        assert_eq!(sync_admin_for(&Config::parse("MACHINE_NAME_PREFIX=devtest\n"), &pods, Some("/home/u")), None, "unset: off");
        assert!(sync_admin_for(&cfg(""), &pods, None).unwrap().unwrap_err().contains("ADMIN_SSH_KEY_PATH isn't set"));
        let unreadable = format!("ADMIN_SSH_KEY_PATH={}\n", base.join("nope").display());
        assert_eq!(sync_admin_for(&cfg(&unreadable), &pods, None), None, "not this user's key: nothing to keep current");
        let same = format!("{with_key}SSH_CONFIG_FILE={}\n", file.display());
        assert!(sync_admin_for(&cfg(&same), &pods, None).unwrap().unwrap_err().contains("SSH_CONFIG_FILE's file too"));
        assert!(!file.exists());
        // Written, then current.
        let c = cfg(&with_key);
        assert_eq!(sync_admin_for(&c, &pods, None), Some(Ok(Synced { path: file.clone(), written: true })));
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.starts_with(ADMIN_MANAGED_HEADER) && text.contains("Host staff\n    HostName 2.2.2.2\n"), "{text}");
        assert!(text.contains(&format!("    IdentityFile {}\n    IdentitiesOnly yes\n", key.display())), "{text}");
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(sync_admin_for(&c, &pods, None), Some(Ok(Synced { path: file.clone(), written: false })));
        assert_eq!(sync_all(&c, &pods, None), [Ok(Synced { path: file.clone(), written: false })], "only staff's is on");
        // The participants' file at that path is never overwritten by it, nor it by theirs.
        std::fs::write(&file, managed_text("Host devtest-*\n")).unwrap();
        assert!(sync_admin_for(&c, &pods, None).unwrap().unwrap_err().contains("--admin --install` replaces it"));
        std::fs::write(&file, &text).unwrap();
        let theirs = Config::parse(&format!("MACHINE_NAME_PREFIX=devtest\nSSH_CONFIG_FILE={}\n", file.display()));
        assert!(sync_for(&theirs, &pods, None).unwrap().unwrap_err().contains("isn't arena's"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
        // Which syncs need the pods listed: staff's always, the participants' proxy layout never.
        assert!(needs_listing(&c, None) && any_on(&c, None));
        let proxied = Config::parse("SSH_CONFIG_FILE=/x/arena.conf\nSSH_PROXY_HOST=px.example\n");
        assert!(!needs_listing(&proxied, None) && any_on(&proxied, None));
        assert!(!any_on(&Config::parse(""), None));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn proxy_absolute_name_gets_bare_host_with_standalone_options() {
        let candidates = ["apple".to_string(), "@james-gpu".into()].to_vec();
        let cfg = render_proxy("arena8", "root", "~/.ssh/arena8_key", "cute.sus.cat", 7000, &candidates);
        // prefixed machine relies on the wildcard: just Host + Port
        assert!(cfg.contains("Host arena8-apple\n    Port 7000"));
        // absolute machine: bare Host, carries its own options (won't match `arena8-*`),
        // still on the proxy host at its index port (7001).
        assert!(cfg.contains(
            "Host james-gpu\n    User root\n    HostName cute.sus.cat\n    \
             IdentityFile ~/.ssh/arena8_key\n    UserKnownHostsFile=/dev/null\n    \
             StrictHostKeyChecking=no\n    Port 7001"
        ));
        // it must NOT be emitted as a prefixed host
        assert!(!cfg.contains("arena8-james-gpu"));
    }
}
