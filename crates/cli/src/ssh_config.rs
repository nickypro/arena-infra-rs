//! `arena ssh-config`: the ~/.ssh/config for the pods (participants' or `--admin`), printed or
//! kept current in a file (`--install`/`--uninstall`), and its `config check` row.

use std::path::PathBuf;

use anyhow::{Context, Result};
use arena_core::provider::Provider;
use arena_core::sshconfig::Audience;
use arena_core::Config;

use crate::proxy::fleet_listing;
use crate::{archive, confirm};

/// `arena ssh-config`: print (and optionally write) the participant-facing
/// `~/.ssh/config` — direct pod endpoints, or stable proxy ports with `--proxy`.
pub(crate) async fn handle_ssh_config(
    provider: &dyn Provider,
    cfg: &Config,
    proxy: bool,
    out: Option<&std::path::Path>,
) -> Result<()> {
    use arena_core::sshconfig;

    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    let user = cfg.get("SSH_USER").unwrap_or("root");
    // The identity file as the *participant* will reference it — keep the configured
    // value verbatim (do not resolve to this operator's local copy).
    let identity = cfg.get("SHARED_SSH_KEY_PATH").unwrap_or("~/.ssh/shared_infra_key");

    let rendered = if proxy {
        let px = arena_core::proxy::ProxyConfig::from_config(cfg)?;
        sshconfig::render_proxy(prefix, user, identity, &px.proxy_host, px.starting_port, &cfg.machine_names)
    } else {
        let pods = provider.list_pods().await.context("listing pods for ssh-config")?;
        sshconfig::render_manual(prefix, user, identity, &pods)
    };
    // The backup archive's block, when this user has one (`arena archive up`): its own alias,
    // which no block above matches.
    let rendered = match archive::laptop_block_for(cfg) {
        Ok(Some(block)) => format!("{rendered}\n{block}"),
        Ok(None) => rendered,
        Err(why) => {
            eprintln!("warning: the backup archive's state isn't readable here ({why}) — its block is left out");
            rendered
        }
    };

    print!("{rendered}");
    if let Some(path) = out {
        std::fs::write(path, &rendered).with_context(|| format!("writing {}", path.display()))?;
        eprintln!("\n(wrote {})", path.display());
        // The archive block reads its host key from a file of its own (W7 A9): written beside
        // the config, so handing out both again is the whole re-pin after a rebuild.
        if let Ok(Some(kh)) = archive::laptop_known_hosts_for(cfg) {
            let name = arena_core::archive::render::LAPTOP_KNOWN_HOSTS.trim_start_matches("~/.ssh/");
            let file = path.parent().unwrap_or(std::path::Path::new(".")).join(name);
            std::fs::write(&file, kh).with_context(|| format!("writing {}", file.display()))?;
            eprintln!("(wrote {} — the archive's pinned host key; it belongs in ~/.ssh/ beside the config)", file.display());
        }
    }
    Ok(())
}

/// `arena ssh-config --admin`: print (and with `--out`, write) staff's config for the proxy box —
/// every machine at its live endpoint with the admin key alone
/// ([`arena_core::sshconfig::render_admin`]). Refused without `ADMIN_SSH_KEY_PATH`; a key this
/// user can't read is said (the file would be of no use here), not refused: it may be printed
/// for the box that holds it. A provider that failed to list is said too — its machines are
/// missing from what's printed.
pub(crate) async fn handle_ssh_config_admin(provider: &dyn Provider, cfg: &Config, out: Option<&std::path::Path>) -> Result<()> {
    let key = arena_core::ssh::admin_key(cfg).ok_or_else(|| {
        anyhow::anyhow!(
            "{} isn't set — `--admin` is staff's config with the admin key, which only the proxy box holds \
             (`arena config set {0} /root/.ssh/arena_admin_key`)",
            arena_core::ssh::ADMIN_KEY_VAR
        )
    })?;
    if arena_core::ssh::readable_admin_key(cfg).is_none() {
        eprintln!("warning: the admin key {key} isn't readable by this user — these entries work only where it is (the proxy box)");
    }
    let listing = fleet_listing(provider).await;
    for (p, e) in listing.errors() {
        eprintln!("warning: {p} failed to list ({e}) — its machines are missing below");
    }
    let rendered = arena_core::sshconfig::render_admin_for(cfg, &listing.pods()).map_err(anyhow::Error::msg)?;
    print!("{rendered}");
    if let Some(path) = out {
        arena_core::sshconfig::write_atomic(path, &rendered, 0o600).map_err(anyhow::Error::msg)?;
        eprintln!("\n(wrote {}, owner-only)", path.display());
    }
    Ok(())
}

/// The home `ssh-config --install`/`--uninstall` work in: `$HOME`, set and absolute — passed in,
/// so a test never touches the real `~/.ssh`.
fn ssh_home(home: Option<&str>) -> Result<PathBuf> {
    match home.map(str::trim).filter(|h| h.starts_with('/')) {
        Some(h) => Ok(PathBuf::from(h)),
        None => anyhow::bail!("HOME isn't set to an absolute path — run this as the user whose ~/.ssh it is"),
    }
}

/// The file `--install`/`--uninstall` mean: the PATH given, else the audience's key
/// (`SSH_CONFIG_FILE`, `--admin`: `SSH_ADMIN_CONFIG_FILE`), else its default
/// (`~/.ssh/arena.conf`, `~/.ssh/arena-admin.conf`) — a leading `~/` taken as `home`; absolute,
/// or refused.
fn install_target(cfg: &Config, audience: Audience, path: Option<&str>, home: &std::path::Path) -> Result<PathBuf> {
    let h = home.to_str();
    let p = match path.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => arena_core::sshconfig::expand_home(p, h).map_err(anyhow::Error::msg)?,
        None => match audience.path(cfg, h) {
            Some(p) => p.map_err(anyhow::Error::msg)?,
            None => arena_core::sshconfig::expand_home(audience.default_path(), h).map_err(anyhow::Error::msg)?,
        },
    };
    anyhow::ensure!(p.is_absolute(), "{} isn't an absolute path (or one starting with ~/)", p.display());
    Ok(p)
}

/// How `path` is spelled in the `Include` line — `~/…` under `home` (ssh expands it), else as
/// it is — and its absolute spelling: either one already there counts.
fn include_forms(path: &std::path::Path, home: &std::path::Path) -> (String, String) {
    let abs = path.display().to_string();
    let shown = path.strip_prefix(home).map_or_else(|_| abs.clone(), |rest| format!("~/{}", rest.display()));
    (shown, abs)
}

/// A file's text, `None` when there is none.
fn read_opt(path: &std::path::Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::anyhow!("reading {}: {e}", path.display())),
    }
}

/// A copy of `path` beside it as `<path>.arena-bak-<stamp>` (`-2`, `-3`, … when that name is
/// taken: an `--uninstall` in the same second as the `--install` must never overwrite the copy
/// of the config from before both), owner-only, never over an existing file; its name, shown.
fn keep_copy(path: &std::path::Path, stamp: &str, shown: &str) -> Result<String> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    for n in 1..100 {
        let suffix = if n == 1 { format!(".arena-bak-{stamp}") } else { format!(".arena-bak-{stamp}-{n}") };
        let copy = std::path::PathBuf::from(format!("{}{suffix}", path.display()));
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        o.mode(0o600);
        match o.open(&copy) {
            Ok(mut f) => {
                f.write_all(&bytes).and_then(|()| f.sync_all()).with_context(|| format!("writing {}", copy.display()))?;
                return Ok(format!("{shown}{suffix}"));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(anyhow::anyhow!("copying {} to {}: {e}", path.display(), copy.display())),
        }
    }
    anyhow::bail!("no free name for a copy of {} beside it", path.display())
}

/// Whether this process's effective user owns `meta`'s file ([`arena_core::sshconfig::owned_by_me`]).
pub(crate) fn mine(meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        arena_core::sshconfig::owned_by_me(meta)
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        true
    }
}

/// Whether `a` and `b` name the same file: the same inode when both exist, else the same name
/// in the same real folder (a folder reached through a link counts as itself).
pub(crate) fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    #[cfg(unix)]
    if let (Ok(x), Ok(y)) = (std::fs::metadata(a), std::fs::metadata(b)) {
        use std::os::unix::fs::MetadataExt;
        return (x.dev(), x.ino()) == (y.dev(), y.ino());
    }
    let real = |p: &std::path::Path| p.parent().and_then(|d| std::fs::canonicalize(d).ok()).zip(p.file_name()).map(|(d, n)| d.join(n));
    a == b || real(a).is_some_and(|x| Some(x) == real(b))
}

/// Why `--install` (`target`: PATH) or `--uninstall` (`None`) won't edit anything, checked before
/// the first write and again after the prompt. `~/.ssh/config` or PATH a symlink or not a regular
/// file: the rename that replaces it would cut a dotfiles link without a word. Either one, or
/// `~/.ssh` (the home when there's none yet), owned by another user: run as someone else (sudo
/// keeping `HOME`) it would leave files there that their owner's ssh can't read. PATH being
/// `~/.ssh/config` itself: it would Include itself, and every ssh would fail.
fn ssh_setup_refusal(home: &std::path::Path, config_path: &std::path::Path, target: Option<(&std::path::Path, &str)>) -> Option<String> {
    let ssh = home.join(".ssh");
    let (folder, folder_shown) = if std::fs::symlink_metadata(&ssh).is_ok() { (ssh, "~/.ssh") } else { (home.to_path_buf(), "your home") };
    match std::fs::metadata(&folder) {
        Ok(m) if !m.is_dir() => return Some(format!("{folder_shown} isn't a folder")),
        Ok(m) if !mine(&m) => {
            return Some(format!("{folder_shown} ({}) isn't owned by this user — run it as the user whose ~/.ssh it is", folder.display()))
        }
        Ok(_) => {}
        Err(e) => return Some(format!("{folder_shown} ({}): {e}", folder.display())),
    }
    if let Some((path, shown)) = target {
        if same_file(path, config_path) {
            return Some(format!("{shown} is ~/.ssh/config itself, which would then Include itself — choose another PATH (default ~/.ssh/arena.conf)"));
        }
    }
    let files = std::iter::once((config_path, "~/.ssh/config")).chain(target);
    for (path, shown) in files {
        match std::fs::symlink_metadata(path) {
            Ok(m) if m.file_type().is_symlink() => {
                let to = std::fs::read_link(path).map_or_else(|_| "?".to_string(), |t| t.display().to_string());
                return Some(format!("{shown} is a symlink (to {to}) — arena never replaces a link; edit the file it points to yourself"));
            }
            Ok(m) if !m.is_file() => return Some(format!("{shown} isn't a regular file")),
            Ok(m) if !mine(&m) => return Some(format!("{shown} isn't owned by this user — run it as the user whose ~/.ssh it is")),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Some(format!("{shown}: {e}")),
        }
    }
    None
}

/// A file's permission bits, `default` when it doesn't exist.
fn mode_or(path: &std::path::Path, default: u32) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).map_or(default, |m| m.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        default
    }
}

/// `arena ssh-config --install [PATH]`: keep one copy of the participants' config current on
/// this machine. PATH (`SSH_CONFIG_FILE`, else `~/.ssh/arena.conf`) is written as arena's file
/// ([`arena_core::sshconfig::MANAGED_HEADER`]) and `Include PATH` made the first line of
/// `~/.ssh/config` — ssh applies an `Include` only where it stands, so it must come before any
/// `Host`/`Match` block to count for every host. Says what changes and asks first; keeps a copy
/// of `~/.ssh/config` (and of a PATH that wasn't arena's) beside it before writing. Idempotent:
/// a second run changes nothing. The pods' endpoints come from a complete listing only — a
/// provider that failed to list would drop its pods' blocks. `home`: `$HOME`.
///
/// `--admin` ([`Audience::Admin`]): staff's file on the proxy box instead — every machine at its
/// live endpoint with the admin key ([`arena_core::sshconfig::render_admin`]), PATH
/// `SSH_ADMIN_CONFIG_FILE` else `~/.ssh/arena-admin.conf`, under a first line of its own. Refused
/// unless this user can read the admin key: the file names it, and would get no one in elsewhere.
/// Neither install takes the other's file: PATH being the other key's, or a file that starts with
/// the other's first line, is refused.
pub(crate) async fn handle_ssh_config_install(
    provider: &dyn Provider,
    cfg: &Config,
    audience: Audience,
    path: Option<&str>,
    yes: bool,
    dry_run: bool,
    home: Option<&str>,
) -> Result<()> {
    use arena_core::sshconfig::{self, FilePlan, IncludeAt};
    let home = ssh_home(home)?;
    let target = install_target(cfg, audience, path, &home)?;
    let (shown, abs) = include_forms(&target, &home);
    if let Some(why) = sshconfig::include_path_problem(&shown) {
        anyhow::bail!("not installed: {why}");
    }
    let config_path = home.join(".ssh").join("config");
    if let Some(why) = ssh_setup_refusal(&home, &config_path, Some((&target, &shown))) {
        anyhow::bail!("not installed: {why}");
    }
    let other = match audience {
        Audience::Participants => Audience::Admin,
        Audience::Admin => Audience::Participants,
    };
    if other.path(cfg, home.to_str()).and_then(|p| p.ok()).is_some_and(|p| same_file(&p, &target)) {
        anyhow::bail!("not installed: {shown} is {}'s file (`{} --install`) — choose another PATH", other.key(), other.command());
    }
    if audience == Audience::Admin {
        let var = arena_core::ssh::ADMIN_KEY_VAR;
        match (arena_core::ssh::admin_key(cfg), arena_core::ssh::readable_admin_key(cfg)) {
            (None, _) => anyhow::bail!(
                "not installed: {var} isn't set — staff's config names the admin key, which only this (the proxy) box holds \
                 (`arena config set {var} /root/.ssh/arena_admin_key`)"
            ),
            (Some(k), None) => anyhow::bail!(
                "not installed: the admin key {k} isn't readable by this user — install it as the user who holds it (on the proxy box)"
            ),
            _ => {}
        }
    }
    let pods = if audience == Audience::Participants && sshconfig::uses_proxy(cfg) {
        Vec::new()
    } else {
        let listing = fleet_listing(provider).await;
        let failed: Vec<String> = listing.errors().iter().map(|(p, e)| format!("{p}: {e}")).collect();
        anyhow::ensure!(failed.is_empty(), "not installed: a provider failed to list ({}) — try again", failed.join("; "));
        listing.pods()
    };
    let rendered = match audience {
        Audience::Participants => sshconfig::render_for(cfg, &pods),
        Audience::Admin => sshconfig::render_admin_for(cfg, &pods),
    };
    let want = audience.text(&rendered.map_err(anyhow::Error::msg)?);
    let existing = read_opt(&target)?;
    if existing.as_deref().is_some_and(|t| !t.trim().is_empty() && other.owns(t)) {
        anyhow::bail!("not installed: {shown} is arena's other file (`{} --install`) — choose another PATH", other.command());
    }
    let plan = audience.plan(existing.as_deref(), &want);
    let config = read_opt(&config_path)?;
    // Both files name the same hosts: ssh takes each one's address, port and user from whichever
    // Include comes first, and offers both files' keys — the participants' proxy ports would shadow
    // staff's live endpoints, or the other way round. One of the two per user
    // ([`sshconfig::one_per_user`]); here, an Include arena can't follow refuses rather than guesses.
    if let Some(why) = sshconfig::one_per_user(audience, cfg, home.to_str()) {
        anyhow::bail!("not installed: {why} — or use this one without an Include: `{} --out FILE`, then `ssh -F FILE <name>`", audience.command());
    }
    if let Some(arg) = sshconfig::included_files(config.as_deref().unwrap_or(""), home.to_str()).unresolved.first() {
        anyhow::bail!(
            "not installed: ~/.ssh/config has an Include arena can't follow ({arg}), so it can't tell whether arena's other file \
             (`{} --install`) is in use — with the same Host names, ssh would take each host's address from whichever comes first. \
             Use this one without an Include: `{} --out FILE`, then `ssh -F FILE <name>`",
            other.command(),
            audience.command()
        );
    }
    let at = sshconfig::include_at(config.as_deref().unwrap_or(""), &[&shown, &abs]);
    let stamp = arena_core::restore::stamp(arena_core::snapshot::unix_now());
    let hosts = want.lines().filter(|l| l.starts_with("Host ") && !l.ends_with('*')).count();
    let mut todo = Vec::new();
    match plan {
        FilePlan::Write if existing.is_none() => todo.push(format!("write {shown} ({hosts} host(s), owner-only)")),
        FilePlan::Write => todo.push(format!("rewrite {shown} ({hosts} host(s)): arena's, and its text changed")),
        FilePlan::Unchanged => {}
        FilePlan::NotOurs => todo.push(format!(
            "replace {shown}: it isn't arena's (no `{}` first line) — the old one is kept as {shown}.arena-bak-{stamp}",
            audience.header().split(" — ").next().unwrap_or_default()
        )),
    }
    match at {
        IncludeAt::Absent if config.is_none() => todo.push(format!("create ~/.ssh/config (owner-only) holding `Include {shown}`")),
        IncludeAt::Absent => todo.push(format!(
            "add `Include {shown}` as the first line of ~/.ssh/config — the old one is kept as ~/.ssh/config.arena-bak-{stamp}"
        )),
        IncludeAt::Global => {}
        IncludeAt::InBlock(n) => println!(
            "note: ~/.ssh/config includes {shown} on line {n}, inside a Host/Match block, where it applies to that block only — \
             left as it is: move it above the first Host/Match line yourself"
        ),
    }
    let hint = match audience.path(cfg, home.to_str()) {
        Some(Ok(p)) if p == target => None,
        _ => Some(format!("to keep it current after every pod change: arena config set {} '{shown}'", audience.key())),
    };
    if todo.is_empty() {
        println!("{shown} is current and ~/.ssh/config includes it — nothing to change.");
        if let Some(h) = &hint {
            println!("{h}");
        }
        return Ok(());
    }
    for t in &todo {
        println!("  · {t}");
    }
    if dry_run {
        println!("\nPreview only — run without --dry-run to make these changes.");
        return Ok(());
    }
    if !confirm(yes, "\nMake these changes?")? {
        println!("aborted.");
        return Ok(());
    }
    // What was shown is what gets written: checked again after the prompt — a file edited, or
    // swapped for a link, meanwhile stops it before anything is written.
    if let Some(why) = ssh_setup_refusal(&home, &config_path, Some((&target, &shown))) {
        anyhow::bail!("not installed: {why}");
    }
    anyhow::ensure!(
        read_opt(&target)? == existing && read_opt(&config_path)? == config,
        "not installed: {shown} or ~/.ssh/config changed while asking — nothing written; run it again"
    );
    if plan != FilePlan::Unchanged {
        if plan == FilePlan::NotOurs {
            println!("kept {}", keep_copy(&target, &stamp, &shown)?);
        }
        sshconfig::write_atomic(&target, &want, 0o600).map_err(anyhow::Error::msg)?;
        println!("wrote {shown}");
    }
    if at == IncludeAt::Absent {
        if config.is_some() {
            println!("kept {}", keep_copy(&config_path, &stamp, "~/.ssh/config")?);
        }
        let text = sshconfig::with_include_first(config.as_deref().unwrap_or(""), &shown);
        sshconfig::write_atomic(&config_path, &text, mode_or(&config_path, 0o600)).map_err(anyhow::Error::msg)?;
        println!("~/.ssh/config now starts with `Include {shown}`");
    }
    if let Some(h) = hint {
        println!("{h}");
    }
    Ok(())
}

/// `arena ssh-config --uninstall [PATH]`: take the `Include PATH` line(s) out of
/// `~/.ssh/config` — exactly those (either spelling), everything else byte for byte, a copy
/// of the old file kept beside it. PATH itself and earlier copies stay. Says what changes and
/// asks first. `home`: `$HOME`.
pub(crate) fn handle_ssh_config_uninstall(cfg: &Config, audience: Audience, path: Option<&str>, yes: bool, dry_run: bool, home: Option<&str>) -> Result<()> {
    use arena_core::sshconfig::{include_at, without_include, IncludeAt};
    let home = ssh_home(home)?;
    let target = install_target(cfg, audience, path, &home)?;
    let (shown, abs) = include_forms(&target, &home);
    let forms = [shown.as_str(), abs.as_str()];
    let config_path = home.join(".ssh").join("config");
    if let Some(why) = ssh_setup_refusal(&home, &config_path, None) {
        anyhow::bail!("not changed: {why}");
    }
    let config = read_opt(&config_path)?;
    let in_block = |text: &str| match include_at(text, &forms) {
        IncludeAt::InBlock(n) => Some(n),
        _ => None,
    };
    let Some(new) = config.as_deref().and_then(|c| without_include(c, &forms)) else {
        match config.as_deref().and_then(in_block) {
            Some(n) => println!(
                "~/.ssh/config includes {shown} only inside a Host/Match block (line {n}) — left as it is: --install never puts \
                 it there, so it's yours"
            ),
            None => println!("~/.ssh/config doesn't include {shown} — nothing to take out."),
        }
        return Ok(());
    };
    let stamp = arena_core::restore::stamp(arena_core::snapshot::unix_now());
    println!("  · take `Include {shown}` out of ~/.ssh/config — the old one is kept as ~/.ssh/config.arena-bak-{stamp}; {shown} stays");
    if dry_run {
        println!("\nPreview only — run without --dry-run to make this change.");
        return Ok(());
    }
    if let Some(n) = in_block(&new) {
        println!("    (the one inside a Host/Match block, line {n} after this, is left as it is)");
    }
    if !confirm(yes, "\nMake this change?")? {
        println!("aborted.");
        return Ok(());
    }
    if let Some(why) = ssh_setup_refusal(&home, &config_path, None) {
        anyhow::bail!("not changed: {why}");
    }
    anyhow::ensure!(read_opt(&config_path)? == config, "not changed: ~/.ssh/config changed while asking — nothing written; run it again");
    println!("kept {}", keep_copy(&config_path, &stamp, "~/.ssh/config")?);
    arena_core::sshconfig::write_atomic(&config_path, &new, mode_or(&config_path, 0o600)).map_err(anyhow::Error::msg)?;
    println!("took `Include {shown}` out of ~/.ssh/config");
    if audience.path(cfg, home.to_str()).is_some() {
        println!("note: {} is still set, so pod changes keep rewriting it — take that line out of the config to stop", audience.key());
    }
    Ok(())
}

/// After a pod change (every [`sync_proxy`]): keep `SSH_CONFIG_FILE` and staff's
/// `SSH_ADMIN_CONFIG_FILE` current ([`arena_core::sshconfig::sync_all`]). The participants' proxy
/// layout alone needs no listing; otherwise the fleet is listed and the files written from a
/// complete listing only — one that failed somewhere would drop that provider's pods. One line
/// per file it rewrote or couldn't; nothing when both keys are unset or the files are current.
pub(crate) async fn ssh_config_after(cfg: &Config, fleet: &dyn Provider, why: &str) -> Vec<String> {
    use arena_core::sshconfig;
    let home = if cfg!(test) { None } else { std::env::var("HOME").ok() };
    // Off (both unset): no listing, nothing said. A bad value is the sync's warning below.
    if !sshconfig::any_on(cfg, home.as_deref()) {
        return Vec::new();
    }
    let pods = if !sshconfig::needs_listing(cfg, home.as_deref()) {
        Vec::new()
    } else {
        let listing = fleet_listing(fleet).await;
        let failed: Vec<String> = listing.errors().iter().map(|(p, _)| p.to_string()).collect();
        if !failed.is_empty() {
            return vec![format!("[ssh-config] after {why}: not rewritten — {} failed to list (the next change rewrites it)", failed.join(", "))];
        }
        listing.pods()
    };
    ssh_config_lines(why, sshconfig::sync_all(cfg, &pods, home.as_deref()))
}

/// [`ssh_config_line`] for each file a sync handled ([`arena_core::sshconfig::sync_all`]).
pub(crate) fn ssh_config_lines(why: &str, synced: Vec<std::result::Result<arena_core::sshconfig::Synced, String>>) -> Vec<String> {
    synced.into_iter().filter_map(|r| ssh_config_line(why, Some(r))).collect()
}

/// The one line a sync of `SSH_CONFIG_FILE` prints (`why`: what it followed): a rewrite, or why
/// it couldn't (a warning — never a failed command); nothing when it was current or is off. Pure.
fn ssh_config_line(why: &str, synced: Option<std::result::Result<arena_core::sshconfig::Synced, String>>) -> Option<String> {
    match synced? {
        Ok(s) if !s.written => None,
        Ok(s) => Some(format!("[ssh-config] after {why}: rewrote {}", s.path.display())),
        Err(e) => Some(format!("warning: [ssh-config] after {why}: not rewritten — {e}")),
    }
}

/// `config check`'s `SSH_CONFIG_FILE` row, and the error to count when it's unusable. Reads the
/// file and `~/.ssh/config` (never writes): arena's or not, and whether ssh is pointed at it.
pub(crate) fn ssh_config_check_row(cfg: &Config, home: Option<&str>) -> (String, Option<String>) {
    ssh_config_check_row_as(Audience::Participants, cfg, home)
}

/// [`ssh_config_check_row`] for `audience`'s file: staff's `SSH_ADMIN_CONFIG_FILE` too, which is
/// unusable (✗) when it's `SSH_CONFIG_FILE`'s file as well.
pub(crate) fn ssh_config_check_row_as(audience: Audience, cfg: &Config, home: Option<&str>) -> (String, Option<String>) {
    use arena_core::sshconfig::{self, IncludeAt};
    let key = audience.key();
    let path = match audience.path(cfg, home) {
        None if audience == Audience::Admin => {
            return (
                format!(
                    "  · {key:<28} (unset) — on the proxy box, `arena ssh-config --admin --install` keeps ~/.ssh/arena-admin.conf \
                     current: every machine, with the admin key"
                ),
                None,
            )
        }
        None => {
            return (
                format!("  · {key:<28} (unset) — `arena ssh-config --install` keeps ~/.ssh/arena.conf current after every pod change"),
                None,
            )
        }
        Some(Err(e)) => return (format!("  ✗ {key:<28} {e}"), Some(e)),
        Some(Ok(p)) => p,
    };
    if audience == Audience::Admin {
        let var = arena_core::ssh::ADMIN_KEY_VAR;
        match (arena_core::ssh::admin_key(cfg), arena_core::ssh::readable_admin_key(cfg)) {
            (None, _) => {
                let e = format!("{key}: {var} isn't set — staff's file names the admin key");
                return (format!("  ✗ {key:<28} {} — {var} isn't set, nothing to write", path.display()), Some(e));
            }
            (Some(_), None) => {
                return (format!("  · {key:<28} {} — not kept by this user: the admin key isn't readable here", path.display()), None)
            }
            _ => {}
        }
    }
    if let Some(why) = sshconfig::one_per_user(audience, cfg, home) {
        return (format!("  ✗ {key:<28} {} — {why}", path.display()), Some(format!("{key}: {why}")));
    }
    let home_path = home.map(std::path::Path::new);
    let (shown, abs) = home_path.map_or_else(|| (path.display().to_string(), path.display().to_string()), |h| include_forms(&path, h));
    let row = match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            format!("  ⚠ {key:<28} {shown} — not written yet (the next pod change or `pods list` writes it)")
        }
        Err(e) => format!("  ⚠ {key:<28} {shown} — unreadable: {e}"),
        Ok(t) if !audience.owns(&t) => {
            format!("  ⚠ {key:<28} {shown} isn't arena's — not rewritten (`{} --install` replaces it after asking)", audience.command())
        }
        Ok(_) => {
            let config = home_path.and_then(|h| std::fs::read_to_string(h.join(".ssh").join("config")).ok()).unwrap_or_default();
            match sshconfig::include_at(&config, &[&shown, &abs]) {
                IncludeAt::Global => format!("  ✓ {key:<28} {shown} (arena's), included by ~/.ssh/config"),
                IncludeAt::InBlock(n) => {
                    format!("  ⚠ {key:<28} {shown} (arena's), but ~/.ssh/config includes it inside a Host/Match block (line {n})")
                }
                IncludeAt::Absent => {
                    format!("  ⚠ {key:<28} {shown} (arena's), not included by ~/.ssh/config (`{} --install`)", audience.command())
                }
            }
        }
    };
    (row, None)
}

#[cfg(test)]
mod tests;
