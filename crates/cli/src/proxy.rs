//! `arena proxy plan|apply`: the SSH proxy's forwards — read the current file, merge, write and
//! reload (or write-only), under the proxy lock; and the sync other commands run after a change.

use std::path::PathBuf;

use anyhow::{Context, Result};
use arena_core::provider::Provider;
use arena_core::Config;
use clap::Subcommand;

use crate::{confirm, emit_proxy_plan, expand_tilde, shell_quote, ssh_config_after};

#[derive(Subcommand)]
pub(crate) enum ProxyCmd {
    /// Preview the proxy forwards and nginx config (read-only)
    ///
    /// Merges the current config with every provider's listing (`+ ~ - =` per machine) and prints
    /// the nginx `stream` config. Never connects to the proxy host.
    #[command(verbatim_doc_comment)]
    Plan {
        /// Also write the rendered nginx config to this local file (never to the proxy)
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Write the proxy config and reload nginx (confirms first)
    ///
    /// Merges the fleet's listing into the live config, shows the changes, writes it and runs
    /// SSH_PROXY_RELOAD_CMD. A forward is removed only once its pod is confirmed gone; nothing is
    /// written if no provider answered.
    #[command(verbatim_doc_comment)]
    Apply {
        /// Preview only: print what would happen, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
}

pub(crate) async fn handle_proxy(cmd: ProxyCmd, provider: &dyn Provider, cfg: &Config, yes: bool) -> Result<()> {
    match cmd {
        ProxyCmd::Plan { out } => {
            let listing = fleet_listing(provider).await;
            emit_proxy_plan(&listing, cfg, out.as_deref())?;
        }
        ProxyCmd::Apply { dry_run } => {
            let listing = fleet_listing(provider).await;
            let prepared = prepare_proxy(cfg, &listing).await?;
            if let Some(why) = &prepared.plan.abort {
                anyhow::bail!("refusing to write the proxy config: {why}");
            }
            print_proxy_review(&prepared, &listing);
            if dry_run {
                print_proxy_dry_run(cfg, &prepared);
                return Ok(());
            }
            if prepared.up_to_date() {
                println!("proxy config already up to date — nothing to write.");
                return Ok(());
            }
            if !confirm(yes, &format!("Will {}.", proxy_action(&prepared.pxcfg)))? {
                println!("aborted.");
                return Ok(());
            }
            let written = write_proxy(cfg, &prepared).await?;
            if let Some(line) = written_line(&prepared.pxcfg, prepared.plan.forwards.len(), written) {
                println!("{line}");
            }
        }
    }
    Ok(())
}

/// The whole fleet's per-provider listing for the proxy merge. Deliberately NOT
/// `list_pods`: that swallows a failing provider, which the merge would read as "every pod
/// on it is gone" and drop their forwards.
pub(crate) async fn fleet_listing(provider: &dyn Provider) -> arena_core::proxy::Listing {
    arena_core::proxy::Listing::from_results(provider.list_by_provider().await)
}

/// The merge planner with this config's prefix + machine list.
pub(crate) fn plan_proxy(
    cfg: &Config,
    pxcfg: &arena_core::proxy::ProxyConfig,
    prev: &[arena_core::proxy::Forward],
    listing: &arena_core::proxy::Listing,
) -> arena_core::proxy::ProxyPlan {
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    arena_core::proxy::plan_forwards(pxcfg, prefix, &cfg.machine_names, prev, listing)
}

/// "deploy … and reload nginx" / "write … (no reload)" — for prompts and summaries.
pub(crate) fn proxy_action(pxcfg: &arena_core::proxy::ProxyConfig) -> String {
    let dest = if pxcfg.local {
        format!("this host:{}", expand_tilde(&pxcfg.nginx_path))
    } else {
        format!("{}@{}:{}", pxcfg.proxy_user, pxcfg.proxy_host, pxcfg.nginx_path)
    };
    if pxcfg.write_only() {
        format!("write the proxy config to {dest} (write-only: SSH_PROXY_RELOAD_CMD is empty, nginx is not reloaded)")
    } else {
        format!("write the proxy config to {dest} and run `{}`", pxcfg.reload_cmd)
    }
}

/// A merge planned against the live config, ready to review and write.
#[derive(Debug)]
struct PreparedProxy {
    pxcfg: arena_core::proxy::ProxyConfig,
    /// The config as it is now; `None` = no file yet.
    current: Option<String>,
    plan: arena_core::proxy::ProxyPlan,
    rendered: String,
}

impl PreparedProxy {
    /// The rendered config is byte-identical to the live one — writing/reloading is a no-op.
    fn up_to_date(&self) -> bool {
        self.current.as_deref() == Some(self.rendered.as_str())
    }
}

/// Read the live config, parse the previous forwards from it, and plan the merge against
/// `listing`. Read-only (a remote proxy is read with `cat` over SSH).
async fn prepare_proxy(cfg: &Config, listing: &arena_core::proxy::Listing) -> Result<PreparedProxy> {
    let pxcfg = arena_core::proxy::ProxyConfig::from_config(cfg)?;
    let current = read_current_proxy(cfg, &pxcfg).await?;
    let parsed = current.as_deref().map(arena_core::proxy::parse_nginx_detailed).unwrap_or_default();
    if parsed.ignored_blocks > 0 {
        eprintln!(
            "warning: {} server block(s) in the current proxy config aren't in a format arena can read \
             back — they'll be dropped when it's rewritten",
            parsed.ignored_blocks
        );
    }
    let plan = plan_proxy(cfg, &pxcfg, &parsed.forwards, listing);
    let rendered = arena_core::proxy::render_nginx(&plan.forwards);
    Ok(PreparedProxy { pxcfg, current, plan, rendered })
}

/// The live proxy config (`None` = no file yet), which the merge starts from. A read that
/// fails is an error, never "empty": a config rebuilt from an empty `prev` because we
/// couldn't read the real one would drop every forward the merge is meant to keep.
async fn read_current_proxy(cfg: &Config, pxcfg: &arena_core::proxy::ProxyConfig) -> Result<Option<String>> {
    use arena_core::ssh::{self, SshTarget};
    if pxcfg.local {
        let path = expand_tilde(&pxcfg.nginx_path);
        return match std::fs::read_to_string(&path) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(anyhow::anyhow!(
                "reading the current proxy config {path}: {e} — refusing to write one built without it"
            )),
        };
    }
    let target = SshTarget::for_host(&pxcfg.proxy_user, &pxcfg.proxy_host, 22, cfg.get("SHARED_SSH_KEY_PATH"));
    let p = &pxcfg.nginx_path;
    let q = sh_path(p);
    // Exit 3 means "no file yet" — distinct from cat's 1 and ssh's own 255 — so a missing
    // file is an empty `prev` while any failure to read it aborts the deploy.
    let out = ssh::run(&target, &format!("test -e {q} || exit 3; cat {q}"))
        .await
        .with_context(|| format!("reading the current proxy config on {}", pxcfg.proxy_host))?;
    match out.code {
        Some(0) => Ok(Some(out.stdout)),
        Some(3) => Ok(None),
        code => anyhow::bail!(
            "couldn't read the current proxy config {p} on {} ({}): {} — refusing to write \
             one built without it",
            pxcfg.proxy_host,
            arena_core::ssh::exit_label(code),
            out.stderr.trim()
        ),
    }
}

/// Print a prepared merge for review: provider failures, one `+ ~ - =` line per change
/// (unchanged lines omitted), and the summary.
fn print_proxy_review(p: &PreparedProxy, listing: &arena_core::proxy::Listing) {
    for (prov, e) in listing.errors() {
        eprintln!("warning: {prov} listing failed: {e}");
    }
    for line in p.plan.change_lines(&p.pxcfg.proxy_host, false) {
        println!("  {line}");
    }
    let skipped = if p.plan.skipped.is_empty() {
        String::new()
    } else {
        format!("; {} pod(s) skipped (`arena proxy plan` lists why)", p.plan.skipped.len())
    };
    println!("proxy: {}{skipped}", p.plan.summary());
}

/// `proxy apply --dry-run`: exactly what a real apply would write and run.
fn print_proxy_dry_run(cfg: &Config, p: &PreparedProxy) {
    let px = &p.pxcfg;
    let dest = if px.local { "this host".into() } else { format!("{}@{}", px.proxy_user, px.proxy_host) };
    if p.up_to_date() {
        println!("[dry-run] {dest}:{} already matches — nothing would be written", px.nginx_path);
        return;
    }
    println!("[dry-run] would write {} forward(s) to {dest}:{}", p.plan.forwards.len(), px.nginx_path);
    if px.local {
        if px.write_only() {
            println!(
                "  write {} only — SSH_PROXY_RELOAD_CMD is empty, so nginx is never reloaded",
                expand_tilde(&px.nginx_path)
            );
        } else {
            println!(
                "  write {} + run `{}` locally (the previous config is put back if that fails)",
                expand_tilde(&px.nginx_path),
                px.reload_cmd
            );
        }
    } else {
        let target = arena_core::ssh::SshTarget::for_host(&px.proxy_user, &px.proxy_host, 22, cfg.get("SHARED_SSH_KEY_PATH"));
        println!("  {}", target.display_scp("<rendered nginx>", "/tmp/.arena-proxy-<unique>.conf"));
        if px.write_only() {
            println!("  then over SSH: install it as {} (no reload — SSH_PROXY_RELOAD_CMD is empty)", px.nginx_path);
        } else {
            println!(
                "  then over SSH: install it as {}, run `{}` (the previous config is put back if that fails)",
                px.nginx_path, px.reload_cmd
            );
        }
    }
    println!(
        "(preview only — run without --dry-run to {})",
        if px.write_only() { "write it" } else { "deploy and reload nginx" }
    );
}

/// Is there a proxy a write can land on? `Err` carries the one-line reason a post-lifecycle
/// sync skips. `pods up` makes the same check to decide whether to deploy as endpoints
/// appear, so the poll loop and the final sync never disagree.
pub(crate) async fn proxy_deployable(cfg: &Config) -> std::result::Result<(), String> {
    use arena_core::ssh::{self, SshTarget};

    let Ok(px) = arena_core::proxy::ProxyConfig::from_config(cfg) else {
        return Err(undeployable_reason(None, false).unwrap_or_default());
    };
    // Write-only mode never runs nginx, so it needs no probe at all.
    let nginx_found = if px.write_only() {
        true
    } else if px.local {
        std::process::Command::new("sh")
            .args(["-c", "command -v nginx >/dev/null 2>&1"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    } else {
        let target = SshTarget::for_host(&px.proxy_user, &px.proxy_host, 22, cfg.get("SHARED_SSH_KEY_PATH"));
        matches!(
            ssh::run(&target, "command -v nginx >/dev/null 2>&1 && echo yes").await,
            Ok(out) if out.success && out.stdout.contains("yes")
        )
    };
    match undeployable_reason(Some(&px), nginx_found) {
        None => Ok(()),
        Some(why) => Err(why),
    }
}

/// Why nothing can be deployed (pure, so each case is tested): no proxy configured, or
/// nginx missing where the reload would run. A remote host that doesn't answer reads the
/// same as one without nginx — either way a deploy couldn't land. Write-only never needs
/// nginx.
fn undeployable_reason(px: Option<&arena_core::proxy::ProxyConfig>, nginx_found: bool) -> Option<String> {
    match px {
        None => Some("no proxy configured (SSH_PROXY_HOST unset)".into()),
        Some(px) if px.write_only() || nginx_found => None,
        Some(px) if px.local => Some(
            "nginx not found on this host — install it, then `arena proxy apply` \
             (`arena proxy plan` prints the config)"
                .into(),
        ),
        Some(px) => Some(format!(
            "proxy host {} has no nginx or is unreachable — `arena proxy apply` once it's set up \
             (if this box IS the proxy, unset PROXY_LOCAL)",
            px.proxy_host
        )),
    }
}

/// How a merged config reached the proxy (what [`write_proxy`] did).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Written {
    /// The rendered config already matched the live one byte for byte: nothing written,
    /// nothing reloaded.
    Unchanged,
    /// Written, nginx deliberately not reloaded (`SSH_PROXY_RELOAD_CMD=""`).
    WriteOnly,
    /// Written and the reload command succeeded.
    Reloaded,
}

/// `[proxy] deployed 3 forward(s) locally and reloaded nginx` and friends, for the
/// verbose paths (`proxy apply`, the `up` poll loop); `None` when nothing was written.
fn written_line(px: &arena_core::proxy::ProxyConfig, n: usize, w: Written) -> Option<String> {
    let dest = if px.local { expand_tilde(&px.nginx_path) } else { format!("{}:{}", px.proxy_host, px.nginx_path) };
    match w {
        Written::Unchanged => None,
        Written::WriteOnly => Some(format!("[proxy] wrote {n} forward(s) to {dest} (write-only: nginx not reloaded)")),
        Written::Reloaded if px.local => Some(format!("[proxy] deployed {n} forward(s) locally and reloaded nginx")),
        Written::Reloaded => Some(format!("[proxy] deployed {n} forward(s) to {} and reloaded nginx", px.proxy_host)),
    }
}

/// Merge `listing` into the live proxy config and, if anything changed, write it and run
/// the reload (unless write-only). Idempotent: an unchanged config is neither written nor
/// reloaded; a listing where no provider answered writes nothing at all (error). `review`
/// prints the per-change lines and the write (the `up` poll loop); [`sync_proxy`] passes
/// `false` and reports in one line from the returned plan instead.
///
/// If another arena process rewrote the config between our read and our write
/// ([`ProxyChanged`]), the merge is redone against the file it wrote — a merge built from
/// the stale read would silently undo that writer's forwards.
async fn deploy_proxy(
    cfg: &Config,
    listing: &arena_core::proxy::Listing,
    review: bool,
) -> Result<(PreparedProxy, Written)> {
    const ATTEMPTS: usize = 3;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let prepared = prepare_proxy(cfg, listing).await?;
        if let Some(why) = &prepared.plan.abort {
            anyhow::bail!("not touching the proxy config: {why}");
        }
        if prepared.up_to_date() {
            return Ok((prepared, Written::Unchanged));
        }
        if review {
            print_proxy_review(&prepared, listing);
        }
        let written = match write_proxy(cfg, &prepared).await {
            Ok(w) => w,
            Err(e) if e.is::<ProxyChanged>() && attempt < ATTEMPTS => continue,
            Err(e) => return Err(e),
        };
        if review {
            if let Some(line) = written_line(&prepared.pxcfg, prepared.plan.forwards.len(), written) {
                println!("{line}");
            }
        }
        return Ok((prepared, written));
    }
}

/// What a post-lifecycle proxy sync did. Returned so a caller that *depends* on the proxy
/// (migrate cutover verifies through it, and reverts if it didn't land) can act on it;
/// everyone else ignores it — [`sync_proxy`] has already printed its line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProxySync {
    /// Not attempted: no proxy configured, or nowhere to deploy to (why).
    Skipped(String),
    /// Merged against a fresh fleet-wide listing.
    Synced {
        written: Written,
        counts: arena_core::proxy::ChangeCounts,
        /// Machines whose forward this sync dropped — the lockout-relevant change, so
        /// it's named rather than just counted.
        removed: Vec<String>,
        /// On-list pods listed with no SSH endpoint yet (not forwarded until a later sync).
        pending: usize,
        /// Providers whose listing failed: their forwards were kept as they were.
        failed_providers: Vec<String>,
        /// Forwards this sync routed to an endpoint the listing confirmed (not kept-stale
        /// ones) — what `pods replace` checks before terminating the pod it replaced.
        routed: Vec<arena_core::proxy::Forward>,
    },
    /// Listing, reading the live config, writing or reloading failed.
    Failed(String),
}

/// The single line a sync prints, e.g. `[proxy] after terminate: +0 ~0 -1 =0 (deployed;
/// removed arena8-apple)`. Pure, so every variant is tested. Error text is folded onto the
/// line: it often embeds a command's multi-line stderr (`nginx -t`'s `[emerg]` + "test
/// failed", ssh's host-key notice).
pub(crate) fn sync_line(why: &str, s: &ProxySync) -> String {
    let one_line = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
    match s {
        ProxySync::Skipped(reason) => format!("[proxy] after {why}: skipped — {}", one_line(reason)),
        ProxySync::Failed(e) => {
            format!("[proxy] after {why}: NOT synced — {} (retry: `arena proxy apply`)", one_line(e))
        }
        ProxySync::Synced { written, counts, removed, pending, failed_providers, .. } => {
            let mut notes = vec![match written {
                Written::Unchanged => "unchanged".to_string(),
                Written::WriteOnly => "written; write-only, nginx not reloaded".to_string(),
                Written::Reloaded => "deployed".to_string(),
            }];
            if !removed.is_empty() {
                const SHOWN: usize = 3;
                let mut names = removed.iter().take(SHOWN).cloned().collect::<Vec<_>>().join(", ");
                if removed.len() > SHOWN {
                    names.push_str(&format!(" +{} more", removed.len() - SHOWN));
                }
                notes.push(format!("removed {names}"));
            }
            if *pending > 0 {
                notes.push(format!("{pending} pod(s) not forwarded yet (no SSH endpoint)"));
            }
            if !failed_providers.is_empty() {
                notes.push(format!("{} failed to list — forwards kept", failed_providers.join(", ")));
            }
            format!("[proxy] after {why}: {} ({})", counts.compact(), notes.join("; "))
        }
    }
}

/// Best-effort proxy sync at the end of a lifecycle command (create/up/rename/reimage/
/// terminate/replace/migrate): skip with a note if there's no deployable proxy, else list
/// the fleet per provider, merge (the sticky 0.A rules), write + reload only if the config
/// changed, and print exactly one line. It never fails the command — the pods are already
/// created/renamed/terminated, and an error exit would invite re-running the mutation
/// itself; a failure is a warning naming `arena proxy apply`.
///
/// `fleet` must span every configured provider (the `build_fleet` provider the pods
/// commands get — `replace` / `migrate`, which act through the pod's own backend, are
/// handed it too): a single-provider listing would leave the others "not queried" and —
/// for legacy entries with no recorded owner — could even read as "every provider listed
/// OK without them", dropping their forwards.
///
/// A just-terminated pod can still be listed for a while (RunPod/Vast report it exited or
/// terminating), so the sync right after `terminate` may keep its forward (R1/R2). That's
/// harmless — its port belongs to that machine name alone — and the next sync (any
/// lifecycle command, `proxy apply`, or the `cron install --proxy` tick) removes it once
/// the provider stops listing the pod.
pub(crate) async fn sync_proxy(cfg: &Config, fleet: &dyn Provider, why: &str) -> ProxySync {
    let outcome = match proxy_deployable(cfg).await {
        Err(reason) => ProxySync::Skipped(reason),
        Ok(()) => proxy_sync_now(cfg, fleet).await,
    };
    let outcome = report_sync(why, outcome);
    for line in ssh_config_after(cfg, fleet, why).await {
        println!("{line}");
    }
    outcome
}

/// [`sync_proxy`] with its report line on stderr — for a caller whose stdout is a JSON
/// document (`pods reap run --json`).
pub(crate) async fn sync_proxy_to_stderr(cfg: &Config, fleet: &dyn Provider, why: &str) -> ProxySync {
    let outcome = match proxy_deployable(cfg).await {
        Err(reason) => ProxySync::Skipped(reason),
        Ok(()) => proxy_sync_now(cfg, fleet).await,
    };
    eprintln!("{}", sync_line(why, &outcome));
    for line in ssh_config_after(cfg, fleet, why).await {
        eprintln!("{line}");
    }
    outcome
}

/// [`sync_proxy`]'s merge + write, without its deployability probe or its report line — for
/// a caller that probed once and reports in its own words (`up`'s per-pod pipelines, which
/// also make sure no two of these run at once).
pub(crate) async fn proxy_sync_now(cfg: &Config, fleet: &dyn Provider) -> ProxySync {
    let listing = fleet_listing(fleet).await;
    match deploy_proxy(cfg, &listing, false).await {
        Ok((prepared, written)) => ProxySync::Synced {
            written,
            counts: prepared.plan.counts(),
            removed: prepared
                .plan
                .changes
                .iter()
                .filter(|c| matches!(c.kind, arena_core::proxy::ChangeKind::Removed { .. }))
                .map(|c| c.forward.name.clone())
                .collect(),
            pending: prepared.plan.pending.len(),
            failed_providers: listing.errors().iter().map(|(p, _)| p.to_string()).collect(),
            routed: prepared.plan.routed(),
        },
        Err(e) => ProxySync::Failed(format!("{e:#}")),
    }
}

/// Print a sync's line (failures to stderr) and hand the outcome back.
fn report_sync(why: &str, outcome: ProxySync) -> ProxySync {
    let line = sync_line(why, &outcome);
    if matches!(outcome, ProxySync::Failed(_)) {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
    outcome
}

/// Another arena process (the proxy cron, a lifecycle sync, an interactive apply) changed
/// the proxy config between our read and our write. Typed so [`deploy_proxy`] can redo
/// the merge against the new file instead of overwriting it with one built from a stale
/// `prev` — which would silently drop whatever that writer had kept or added.
#[derive(Debug)]
struct ProxyChanged;

impl std::fmt::Display for ProxyChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "the proxy config changed since it was read (another arena run wrote it) — not \
             overwriting it; re-run to merge against the new one",
        )
    }
}

impl std::error::Error for ProxyChanged {}

/// How long a proxy write waits for another arena process's write to finish. A writer
/// holds the lock only for re-read + write + reload (seconds), so this is generous.
const PROXY_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Serialize proxy writes between arena processes on this box — the `cron install
/// --proxy` tick, post-lifecycle syncs, an interactive `proxy apply` — with an exclusive
/// `flock` held across re-read, write and reload. Local proxy: on the config's
/// *directory*, which needs no write access and is the same inode for root's cron and a
/// user's shell. Remote proxy: on a lock file in the temp dir keyed by host (only
/// serializes writers on *this* control box). Best-effort: when no lock can be opened we
/// go on — [`write_proxy`]'s re-read-and-compare still refuses to overwrite a change.
async fn proxy_lock(pxcfg: &arena_core::proxy::ProxyConfig) -> Result<Option<std::fs::File>> {
    use std::path::Path;
    let file = if pxcfg.local {
        let path = PathBuf::from(expand_tilde(&pxcfg.nginx_path));
        let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        std::fs::File::open(dir).ok()
    } else {
        let host: String =
            pxcfg.proxy_host.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect();
        let path = std::env::temp_dir().join(format!("arena-proxy-{host}.lock"));
        // Open an existing lock read-only (flock doesn't need write access, and another
        // user may own it); create it world-readable otherwise so the next user can too.
        std::fs::File::open(&path).ok().or_else(|| {
            use std::os::unix::fs::PermissionsExt;
            let f = std::fs::OpenOptions::new().write(true).create(true).truncate(false).open(&path).ok()?;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));
            Some(f)
        })
    };
    let Some(file) = file else { return Ok(None) };
    let deadline = std::time::Instant::now() + PROXY_LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!(
                "another arena process has been writing the proxy config for over {}s — not writing",
                PROXY_LOCK_WAIT.as_secs()
            ),
            // A filesystem without flock: go on, the compare-before-write still guards.
            Err(std::fs::TryLockError::Error(_)) => return Ok(None),
        }
    }
}

/// Replace `path`'s contents so a concurrent reader (another arena's prepare, someone's
/// `nginx -t`) sees the old file or the new one, never a truncated one: write a hidden temp
/// sibling (the leading `.` keeps it out of an nginx `include …/*` glob), give it the old
/// file's mode and owner, and rename it over. A symlink is followed — its target is
/// replaced, the link kept. Where that isn't permitted (a directory we can't create files
/// in, a sticky one we can't rename within — the old in-place write only needed the file
/// itself to be writable) fall back to that in-place write rather than fail a deploy that
/// used to work; the lock + compare in [`write_proxy`] still keep arena writers apart.
fn replace_file(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let (Some(dir), Some(name)) = (real.parent(), real.file_name()) else {
        return std::fs::write(&real, text);
    };
    let tmp = dir.join(format!(".{}.arena-tmp-{}", name.to_string_lossy(), std::process::id()));
    let swapped = std::fs::write(&tmp, text).and_then(|()| {
        if let Ok(meta) = std::fs::metadata(&real) {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
            // Root's cron rewriting a user-owned file must leave it user-owned, or the
            // user's next interactive sync can't write it. (Fails harmlessly as non-root.)
            let _ = std::os::unix::fs::chown(&tmp, Some(meta.uid()), Some(meta.gid()));
        }
        std::fs::rename(&tmp, &real)
    });
    match swapped {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                std::fs::write(&real, text)
            } else {
                Err(e)
            }
        }
    }
}

/// A config path as a shell word: `~/x` → `"$HOME"/'x'` (still expands on the far side),
/// anything else single-quoted.
fn sh_path(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => format!("\"$HOME\"/{}", shell_quote(rest)),
        None => shell_quote(p),
    }
}

/// `<pid>-<nanos>`: unique enough for a temp file name no other process will pick.
fn unique_tag() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{}-{nanos}", std::process::id())
}

/// Exit codes of [`remote_install_script`].
const INSTALL_FAILED: i32 = 90;
const RELOAD_FAILED_RESTORED: i32 = 91;
const RELOAD_FAILED_NOT_RESTORED: i32 = 92;

/// The single remote command that installs an uploaded config: back up the live file,
/// copy the upload over it *in place* (keeps its owner and mode, and needs only write
/// access to the file, like the old direct scp), run the reload, and if the reload fails
/// put the backup back (or remove the file if there was none) — so the live file always
/// matches what nginx is running, and the next sync sees a difference and retries.
/// One SSH session after the upload, so a connection dropped between upload and install
/// leaves the live config untouched. `reload_cmd: None` = write-only. Pure (and plain
/// POSIX sh), so its behaviour is tested by running it locally.
fn remote_install_script(path: &str, upload: &str, reload_cmd: Option<&str>) -> String {
    let (p, n) = (sh_path(path), shell_quote(upload));
    let mut script = format!(
        "p={p}; n={n}; b=\"$n.prev\"; had=0\n\
         restore() {{ if [ \"$had\" = 1 ]; then cat \"$b\" > \"$p\"; else rm -f \"$p\"; fi; }}\n\
         if [ -e \"$p\" ]; then cat \"$p\" > \"$b\" || {{ rm -f \"$n\" \"$b\"; exit {INSTALL_FAILED}; }}; had=1; fi\n\
         if ! cat \"$n\" > \"$p\"; then restore; rm -f \"$n\" \"$b\"; exit {INSTALL_FAILED}; fi\n\
         rm -f \"$n\"\n"
    );
    match reload_cmd {
        None => script.push_str("rm -f \"$b\"\nexit 0\n"),
        Some(cmd) => script.push_str(&format!(
            // On its own lines, so a reload command ending in a `# comment` can't swallow
            // the rest of the script.
            "if (\n{cmd}\n); then rm -f \"$b\"; exit 0; fi\n\
             if restore; then rm -f \"$b\"; exit {RELOAD_FAILED_RESTORED}; fi\n\
             exit {RELOAD_FAILED_NOT_RESTORED}\n"
        )),
    }
    script
}

/// Removes a local temp file when dropped (the scp source), whatever path we leave by.
struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Write a prepared config to the proxy (locally, or scp + one SSH install over SSH) and
/// run the reload command unless write-only. This is the one place the tool touches the
/// proxy host. Silent: callers report (see [`written_line`], [`sync_line`]).
///
/// - Under [`proxy_lock`], the live config is re-read and must still be what `p` was
///   planned against, else [`ProxyChanged`] — never a blind overwrite.
/// - A failed reload puts the previous config back. Leaving the new file in place would
///   make every later sync see "already up to date" and never retry the reload, while
///   nginx kept serving the old routes.
async fn write_proxy(cfg: &Config, p: &PreparedProxy) -> Result<Written> {
    use arena_core::ssh::{self, SshTarget};

    // The planner's abort is also enforced here, so no caller can write around it.
    if let Some(why) = &p.plan.abort {
        anyhow::bail!("refusing to write the proxy config: {why}");
    }
    if p.up_to_date() {
        return Ok(Written::Unchanged);
    }
    let pxcfg = &p.pxcfg;
    let _lock = proxy_lock(pxcfg).await?;
    if read_current_proxy(cfg, pxcfg).await? != p.current {
        return Err(ProxyChanged.into());
    }

    // Local: this box IS the proxy — write the config + reload directly, no SSH.
    if pxcfg.local {
        let path = PathBuf::from(expand_tilde(&pxcfg.nginx_path));
        replace_file(&path, &p.rendered).with_context(|| format!("writing nginx config to {}", path.display()))?;
        if pxcfg.write_only() {
            return Ok(Written::WriteOnly);
        }
        let failure = match std::process::Command::new("sh").args(["-c", &pxcfg.reload_cmd]).output() {
            Ok(out) if out.status.success() => return Ok(Written::Reloaded),
            Ok(out) => format!("local nginx reload failed: {}", String::from_utf8_lossy(&out.stderr).trim()),
            Err(e) => format!("running the local reload command: {e}"),
        };
        let restored = match &p.current {
            Some(prev) => replace_file(&path, prev),
            None => std::fs::remove_file(&path).or_else(|e| match e.kind() {
                std::io::ErrorKind::NotFound => Ok(()),
                _ => Err(e),
            }),
        };
        match restored {
            Ok(()) => anyhow::bail!("{failure} — previous config put back, so the next sync retries"),
            Err(e) => anyhow::bail!(
                "{failure} — and putting the previous config back failed ({e}): {} now holds a config \
                 nginx hasn't loaded, which a re-run will see as up to date; fix the cause, then run \
                 `{}` by hand",
                path.display(),
                pxcfg.reload_cmd
            ),
        }
    }

    // Remote: upload to a unique temp file on the proxy host, then install + reload there
    // in one SSH session. (A fixed local temp name broke once another user had created it.)
    let target =
        SshTarget::for_host(&pxcfg.proxy_user, &pxcfg.proxy_host, 22, cfg.get("SHARED_SSH_KEY_PATH"));
    let tag = unique_tag();
    let local_tmp = RemoveOnDrop(std::env::temp_dir().join(format!("arena-proxy-{tag}.conf")));
    std::fs::write(&local_tmp.0, &p.rendered).context("writing rendered nginx config to a temp file")?;
    let upload = format!("/tmp/.arena-proxy-{tag}.conf");
    let scp = ssh::scp(&target, &local_tmp.0.to_string_lossy(), &upload).await?;
    if !scp.success {
        anyhow::bail!("scp to proxy {} failed: {}", pxcfg.proxy_host, scp.stderr.trim());
    }
    let reload = (!pxcfg.write_only()).then_some(pxcfg.reload_cmd.as_str());
    let out = ssh::run(&target, &remote_install_script(&pxcfg.nginx_path, &upload, reload)).await?;
    let (host, err) = (&pxcfg.proxy_host, out.stderr.trim());
    match out.code {
        Some(0) if reload.is_none() => Ok(Written::WriteOnly),
        Some(0) => Ok(Written::Reloaded),
        Some(INSTALL_FAILED) => anyhow::bail!("installing the config on {host} failed (live config left as it was): {err}"),
        Some(RELOAD_FAILED_RESTORED) => {
            anyhow::bail!("nginx reload on {host} failed: {err} — previous config put back, so the next sync retries")
        }
        Some(RELOAD_FAILED_NOT_RESTORED) => anyhow::bail!(
            "nginx reload on {host} failed: {err} — and putting the previous config back failed: {} \
             holds a config nginx hasn't loaded, which a re-run will see as up to date; fix the cause, \
             then run `{}` there by hand",
            pxcfg.nginx_path,
            pxcfg.reload_cmd
        ),
        code => anyhow::bail!(
            "installing/reloading on {host} failed ({}): {err} — the live config may not \
             have changed; `arena proxy apply --dry-run` shows where it stands",
            arena_core::ssh::exit_label(code)
        ),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sync_line_reports_each_outcome_on_one_line() {
        use super::{sync_line, ProxySync, Written};
        use arena_core::proxy::ChangeCounts;
        let synced = |written, counts, removed: &[&str], pending, failed: &[&str]| ProxySync::Synced {
            written,
            counts,
            removed: removed.iter().map(|s| s.to_string()).collect(),
            pending,
            failed_providers: failed.iter().map(|s| s.to_string()).collect(),
            routed: vec![],
        };
        let removed_one = ChangeCounts { removed: 1, unchanged: 4, ..Default::default() };
        let cases: Vec<(ProxySync, &str)> = vec![
            (
                synced(Written::Reloaded, removed_one, &["arena8-apple"], 0, &[]),
                "[proxy] after terminate: +0 ~0 -1 =0 (deployed; removed arena8-apple)",
            ),
            (
                synced(Written::Unchanged, ChangeCounts { unchanged: 5, ..Default::default() }, &[], 0, &[]),
                "[proxy] after terminate: +0 ~0 -0 =0 (unchanged)",
            ),
            (
                synced(Written::WriteOnly, ChangeCounts { added: 1, ..Default::default() }, &[], 2, &[]),
                "[proxy] after terminate: +1 ~0 -0 =0 (written; write-only, nginx not reloaded; \
                 2 pod(s) not forwarded yet (no SSH endpoint))",
            ),
            (
                synced(
                    Written::Reloaded,
                    ChangeCounts { removed: 5, kept: 2, ..Default::default() },
                    &["a", "b", "c", "d", "e"],
                    0,
                    &["vast"],
                ),
                "[proxy] after terminate: +0 ~0 -5 =2 (deployed; removed a, b, c +2 more; \
                 vast failed to list — forwards kept)",
            ),
            (
                ProxySync::Skipped("no proxy configured (SSH_PROXY_HOST unset)".into()),
                "[proxy] after terminate: skipped — no proxy configured (SSH_PROXY_HOST unset)",
            ),
            (
                ProxySync::Failed("not touching the proxy config: every provider failed".into()),
                "[proxy] after terminate: NOT synced — not touching the proxy config: every provider \
                 failed (retry: `arena proxy apply`)",
            ),
            // What a failing `nginx -t` really prints: several stderr lines, folded into one.
            (
                ProxySync::Failed(
                    "local nginx reload failed: nginx: [emerg] unexpected \"}\" in /p.conf:9\n\
                     nginx: configuration file /etc/nginx/nginx.conf test failed — previous config put back"
                        .into(),
                ),
                "[proxy] after terminate: NOT synced — local nginx reload failed: nginx: [emerg] \
                 unexpected \"}\" in /p.conf:9 nginx: configuration file /etc/nginx/nginx.conf test \
                 failed — previous config put back (retry: `arena proxy apply`)",
            ),
            (
                ProxySync::Failed(
                    "nginx reload on p.example failed: Warning: Permanently added 'p.example' (ED25519) \
                     to the list of known hosts.\r\nnginx: [emerg] bind() failed"
                        .into(),
                ),
                "[proxy] after terminate: NOT synced — nginx reload on p.example failed: Warning: \
                 Permanently added 'p.example' (ED25519) to the list of known hosts. nginx: [emerg] \
                 bind() failed (retry: `arena proxy apply`)",
            ),
        ];
        for (outcome, want) in cases {
            let got = sync_line("terminate", &outcome);
            assert_eq!(got, want);
            assert!(!got.contains('\n'), "one line: {got}");
        }
    }

    #[test]
    fn undeployable_only_without_a_proxy_or_without_nginx_unless_write_only() {
        use super::undeployable_reason;
        use arena_core::proxy::ProxyConfig;
        let px = |extra: &str| {
            ProxyConfig::from_config(&arena_core::Config::parse(&format!("SSH_PROXY_HOST=proxy.example.com\n{extra}")))
                .unwrap()
        };
        let local = px("");
        let remote = px("PROXY_LOCAL=false\n");
        let write_only = px("PROXY_LOCAL=false\nSSH_PROXY_RELOAD_CMD=\"\"\n");

        assert!(undeployable_reason(None, true).unwrap().contains("SSH_PROXY_HOST"));
        assert_eq!(undeployable_reason(Some(&local), true), None);
        assert!(undeployable_reason(Some(&local), false).unwrap().contains("not found on this host"));
        assert_eq!(undeployable_reason(Some(&remote), true), None);
        let why = undeployable_reason(Some(&remote), false).unwrap();
        assert!(why.contains("proxy.example.com") && why.contains("unreachable"), "{why}");
        // Write-only never runs nginx, so a missing nginx never blocks it.
        assert_eq!(undeployable_reason(Some(&write_only), false), None);
    }

    #[test]
    fn written_line_says_what_reached_the_proxy() {
        use super::{written_line, Written};
        use arena_core::proxy::ProxyConfig;
        let px = |extra: &str| {
            ProxyConfig::from_config(&arena_core::Config::parse(&format!(
                "SSH_PROXY_HOST=proxy.example.com\nSSH_PROXY_NGINX_CONFIG_PATH=/srv/p.conf\n{extra}"
            )))
            .unwrap()
        };
        let (local, remote) = (px(""), px("PROXY_LOCAL=false\n"));
        assert_eq!(written_line(&local, 3, Written::Unchanged), None);
        assert_eq!(
            written_line(&local, 3, Written::WriteOnly).as_deref(),
            Some("[proxy] wrote 3 forward(s) to /srv/p.conf (write-only: nginx not reloaded)")
        );
        assert_eq!(
            written_line(&local, 3, Written::Reloaded).as_deref(),
            Some("[proxy] deployed 3 forward(s) locally and reloaded nginx")
        );
        assert_eq!(
            written_line(&remote, 2, Written::WriteOnly).as_deref(),
            Some("[proxy] wrote 2 forward(s) to proxy.example.com:/srv/p.conf (write-only: nginx not reloaded)")
        );
        assert_eq!(
            written_line(&remote, 2, Written::Reloaded).as_deref(),
            Some("[proxy] deployed 2 forward(s) to proxy.example.com and reloaded nginx")
        );
    }

    #[test]
    fn sh_path_keeps_tilde_expansion_and_quotes_the_rest() {
        use super::sh_path;
        assert_eq!(sh_path("~/proxy.conf"), "\"$HOME\"/'proxy.conf'");
        assert_eq!(sh_path("/etc/nginx/streams-enabled/arena.conf"), "'/etc/nginx/streams-enabled/arena.conf'");
        assert_eq!(sh_path("/a b/it's"), "'/a b/it'\\''s'");
    }
}

/// End-to-end tests of the proxy deploy path (read current file → merge → write), against
/// a temp file in **write-only** mode (`SSH_PROXY_RELOAD_CMD=""`, loopback host = local)
/// — so they can never run nginx or SSH anywhere.
#[cfg(test)]
mod proxy_deploy_tests;
