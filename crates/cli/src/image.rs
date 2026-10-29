//! `arena image build|list|prune`: pre-built Hetzner images (see `arena_core::image` for what
//! they are and what Hetzner's API says about them). This side does the I/O: the Hetzner calls
//! through [`ImageCloud`] (the fleet's own `HetznerProvider`), the bake VM over [`Remote`], the
//! repo's current requirements through a blob-less `git clone` with the deploy key.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use arena_core::image::build::{self as bake, BuildPlan, VmFate};
use arena_core::image::cloud::ImageCloud;
use arena_core::image::{self, Fit, ImagePrice, Resolved, Scope, Setting, Snapshot};
use arena_core::provider::hetzner::{HetznerOpts, HetznerProvider};
use arena_core::remote::Remote;
use arena_core::Config;

use super::{confirm, Interrupt};

/// `arena image …`.
#[derive(clap::Subcommand, Debug)]
pub(crate) enum ImageCmd {
    /// Bake an image: a VM set up, scrubbed, scanned, snapshotted, deleted
    ///
    /// Creates <prefix>-image-<yymmdd-hhmm> from Ubuntu (by default the smallest-disk type of
    /// HETZNER_SERVER_TYPE's architecture: a cx23, 40 GB, which fits cx23, cx33 and up), runs
    /// `pods setup`'s steps on it (no tokens), scrubs what belongs to that machine (deploy key,
    /// authorized keys, tokens, history, name, swap, machine-id, host keys, cloud-init state,
    /// /tmp, logs), scans the homes and /etc for secrets — refusing on any hit — then powers it
    /// off and snapshots it. The VM is deleted whatever happens, Ctrl+C included. ~20 min,
    /// about a cent of VM time; the image bills ~6 GB of snapshot storage a month.
    #[command(verbatim_doc_comment)]
    Build {
        /// The bake VM's server type (default: the smallest disk of HETZNER_SERVER_TYPE's arch)
        #[arg(long, value_name = "TYPE")]
        server_type: Option<String>,
        /// Preview: the plan, the type, the cost; creates nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// This cohort's arena images, and which one `arena` picks (read-only)
    ///
    /// ID, when built, age, size, cost a month, the smallest disk it boots on, the course commit
    /// it holds, whether the repo's requirements (install_uv.sh, requirements.txt) still match.
    #[command(verbatim_doc_comment)]
    List,
    /// Delete old arena-built images (and leftover build VMs)
    ///
    /// Keeps the newest --keep and the one HETZNER_IMAGE=arena resolves to now; only ever
    /// touches snapshots labelled arena-image with this cohort's prefix. Asks first.
    #[command(verbatim_doc_comment)]
    Prune {
        /// How many of the newest to keep
        #[arg(long, default_value_t = 1)]
        keep: usize,
        /// Show what would go; delete nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
}

/// How long the read-only lookups may take (`config check`, `image list`, `up`'s resolve).
pub(crate) const LOOKUP_TIMEOUT: Duration = Duration::from_secs(30);

/// The Hetzner provider the image commands talk through (`HETZNER_API_KEY`).
pub(crate) fn hetzner(cfg: &Config) -> Result<HetznerProvider> {
    let key = cfg.get("HETZNER_API_KEY").filter(|k| !k.trim().is_empty()).context("HETZNER_API_KEY isn't set (see `arena config check`)")?;
    Ok(HetznerProvider::new(key, HetznerOpts::from_config(cfg)))
}

pub(crate) async fn handle_image(cmd: ImageCmd, cfg: &Config, remote: Arc<dyn Remote>, yes: bool, interrupt: Interrupt) -> Result<()> {
    let cloud = hetzner(cfg)?;
    match cmd {
        ImageCmd::Build { server_type, dry_run } => build(&cloud, remote.as_ref(), cfg, server_type, dry_run, yes, interrupt).await,
        ImageCmd::List => list(&cloud, cfg).await,
        ImageCmd::Prune { keep, dry_run } => prune(&cloud, cfg, keep, dry_run, yes).await,
    }
}

/// The repo's current default-branch commit and requirements hash ([`image::req_hash`]), read
/// with the deploy key: a blob-less, depth-1 clone into a temp dir (only the two files' blobs
/// are fetched), github.com's host key taken on first use into that dir (never your
/// known_hosts). Err: why it couldn't (no key, no network, no access).
pub(crate) async fn repo_requirements(cfg: &Config) -> std::result::Result<(String, String), String> {
    let owner = cfg.get("ARENA_REPO_OWNER").filter(|s| !s.is_empty()).ok_or("ARENA_REPO_OWNER isn't set")?;
    let name = cfg.get("ARENA_REPO_NAME").filter(|s| !s.is_empty()).ok_or("ARENA_REPO_NAME isn't set")?;
    let key = cfg.get("GIT_SSH_KEY_LOCAL").filter(|s| !s.is_empty()).ok_or("GIT_SSH_KEY_LOCAL isn't set")?;
    let key = arena_core::ssh::resolve_key_path(key);
    let branch = cfg.get("DEFAULT_BRANCH").unwrap_or("main");
    let dir = private_temp_dir("arena-image-req")?;
    let result = async {
        let ssh = format!(
            "ssh -i {} -o IdentitiesOnly=yes -o BatchMode=yes -o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile={}",
            shell_word(&key),
            shell_word(&dir.join("known_hosts").display().to_string())
        );
        let repo = dir.join("r");
        let git = |args: &[&str]| {
            let mut c = tokio::process::Command::new("git");
            c.args(args).env("GIT_SSH_COMMAND", &ssh).env("GIT_TERMINAL_PROMPT", "0").kill_on_drop(true);
            c.stdin(std::process::Stdio::null());
            c
        };
        let url = format!("git@github.com:{owner}/{name}.git");
        let repo_s = repo.display().to_string();
        let out = git(&["clone", "--quiet", "--depth", "1", "--filter=blob:none", "--no-checkout", "--branch", branch, &url, &repo_s])
            .output()
            .await
            .map_err(|e| format!("running git: {e}"))?;
        if !out.status.success() {
            let why = String::from_utf8_lossy(&out.stderr);
            return Err(format!("git clone of {owner}/{name} failed: {}", why.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no message").trim()));
        }
        // Which of the files the commit has (from the tree, which the clone holds), then each one's
        // bytes (fetched now: the clone is blob-less) — a file there that can't be read is an
        // error, never "absent", which would make a different hash.
        let listed = git(&["-C", &repo_s, "ls-tree", "--name-only", "HEAD", "--", "install_uv.sh", "requirements.txt", "pyproject.toml"])
            .output()
            .await
            .map_err(|e| format!("running git: {e}"))?;
        if !listed.status.success() {
            return Err("git ls-tree failed on the clone".into());
        }
        let present: Vec<String> = String::from_utf8_lossy(&listed.stdout).lines().map(str::to_string).collect();
        let show = |file: &'static str| {
            let mut c = git(&["-C", &repo_s, "show", &format!("HEAD:{file}")]);
            let there = present.iter().any(|p| p == file);
            async move {
                if !there {
                    return Ok(None);
                }
                let o = c.output().await.map_err(|e| format!("running git: {e}"))?;
                if !o.status.success() {
                    return Err(format!("couldn't read {file} from the repo: {}", String::from_utf8_lossy(&o.stderr).trim()));
                }
                Ok::<_, String>(Some(o.stdout))
            }
        };
        let head = git(&["-C", &repo_s, "rev-parse", "--short=12", "HEAD"]).output().await.map_err(|e| format!("running git: {e}"))?;
        let head = String::from_utf8_lossy(&head.stdout).trim().to_string();
        let install = show("install_uv.sh").await?;
        let reqs = show("requirements.txt").await?;
        let pyproject = show("pyproject.toml").await?;
        Ok((head, image::req_hash(install.as_deref(), reqs.as_deref(), pyproject.as_deref())))
    };
    let r = match tokio::time::timeout(LOOKUP_TIMEOUT, result).await {
        Ok(r) => r,
        Err(_) => Err(format!("reading the repo took over {}s", LOOKUP_TIMEOUT.as_secs())),
    };
    let _ = std::fs::remove_dir_all(&dir);
    r
}

/// A new directory of this user's only (0700) in the temp dir, under a name nobody can guess
/// ahead (the clock's nanoseconds and the pid): made with `mkdir`, never an existing path or a
/// symlink someone left there — ssh writes its known_hosts into it, and git the repo's objects.
fn private_temp_dir(tag: &str) -> std::result::Result<std::path::PathBuf, String> {
    use std::os::unix::fs::DirBuilderExt;
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("{tag}-{}-{nanos}", std::process::id()));
    std::fs::DirBuilder::new().mode(0o700).create(&dir).map_err(|e| format!("making {}: {e}", dir.display()))?;
    Ok(dir)
}

/// One word for `GIT_SSH_COMMAND` (a shell string).
fn shell_word(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// What `arena` resolves to for `server_type` (the setting's own value when it isn't `arena`),
/// with the snapshots it looked at.
pub(crate) async fn resolve(cloud: &HetznerProvider, cfg: &Config, setting: &Setting, server_type: &str) -> Result<(Resolved, Vec<Snapshot>)> {
    let scope = Scope::from_config(cfg);
    let snaps = cloud.snapshots(Some(&scope.selector(image::LABEL))).await.context("listing the project's arena images")?;
    let types = cloud.server_types().await.context("listing Hetzner's server types")?;
    let fit = Fit::of(&types, server_type).unwrap_or(Fit { server_type: server_type.into(), arch: None, disk_gb: None });
    Ok((image::resolve(setting, &snaps, &scope, &fit), snaps))
}

/// `config check`'s and `image list`'s lines about what `HETZNER_IMAGE` resolves to: the
/// resolution, and a note when the image is old or its requirements differ from the repo's.
/// `repo`: [`repo_requirements`]' answer.
pub(crate) fn resolution_lines(resolved: &Resolved, now: u64, repo: &std::result::Result<(String, String), String>) -> Vec<String> {
    let mut out = vec![resolved.describe(now)];
    if let Resolved::Snapshot(s) = resolved {
        let req = repo.as_ref().ok().map(|(_, r)| r.as_str());
        out.extend(image::notes(s, now, req).into_iter().map(|n| format!("⚠ {n}")));
        if let Err(e) = repo {
            out.push(format!("(couldn't compare its requirements with the repo's: {e})"));
        }
    }
    out
}

async fn list(cloud: &HetznerProvider, cfg: &Config) -> Result<()> {
    let opts = HetznerOpts::from_config(cfg);
    let scope = Scope::from_config(cfg);
    let setting = Setting::parse(&opts.image);
    let now = arena_core::snapshot::unix_now();
    let (resolved, snaps) = resolve(cloud, cfg, &Setting::Arena, &opts.server_type).await?;
    let price = cloud.image_price().await.ok();
    let repo = repo_requirements(cfg).await;
    let ours: Vec<&Snapshot> = snaps.iter().filter(|s| s.ours(&scope)).collect();
    if ours.is_empty() {
        println!("No arena-built images in this Hetzner project for prefix `{}` — `arena image build` makes one.", scope.prefix);
    } else {
        let picked = match &resolved {
            Resolved::Snapshot(s) => Some(s.id),
            _ => None,
        };
        print!("{}", image::list_table(&snaps, &scope, now, price.as_ref(), repo.as_ref().ok().map(|(_, r)| r.as_str()), picked));
        let gb: f64 = ours.iter().filter_map(|s| s.size_gb).sum();
        let monthly = price.as_ref().map_or_else(|| "?".into(), |p| p.money(gb * p.per_gb_month));
        println!("\n{} image(s), {gb:.1} GB stored, ≈ {monthly}/month.", ours.len());
    }
    match &repo {
        Ok((commit, req)) => println!("The repo now: commit {commit}, requirements {req}."),
        Err(e) => println!("The repo now: couldn't read it ({e}) — REQ shows ?."),
    }
    let which = if setting == Setting::Arena { "HETZNER_IMAGE=arena".to_string() } else { format!("HETZNER_IMAGE={} (set to arena to use these)", opts.image) };
    println!("For {} ({which}):", opts.server_type);
    for l in resolution_lines(&resolved, now, &repo) {
        println!("  {l}");
    }
    leftover_note(cloud, &scope, now).await;
    Ok(())
}

/// A warning for every build VM older than [`image::LEFTOVER_SECS`] still in the project.
async fn leftover_note(cloud: &HetznerProvider, scope: &Scope, now: u64) {
    if let Ok(servers) = cloud.servers(&scope.selector(image::BUILD_LABEL)).await {
        for p in image::leftover_build_vms(&servers, now) {
            println!("⚠ a build VM was left behind and bills by the hour: {} (id {}) — `arena image prune` deletes it", p.name, p.id);
        }
    }
}

async fn prune(cloud: &HetznerProvider, cfg: &Config, keep: usize, dry_run: bool, yes: bool) -> Result<()> {
    let opts = HetznerOpts::from_config(cfg);
    let scope = Scope::from_config(cfg);
    let now = arena_core::snapshot::unix_now();
    let (resolved, snaps) = resolve(cloud, cfg, &Setting::Arena, &opts.server_type).await?;
    // Kept whatever --keep says (but 0): what `arena` picks now, and an image HETZNER_IMAGE names.
    let mut protect = Vec::new();
    if let Resolved::Snapshot(s) = &resolved {
        protect.push(s.id);
    }
    let configured = match Setting::parse(&opts.image) {
        Setting::Id(id) => Some(id),
        _ => None,
    };
    protect.extend(configured);
    let (kept, gone) = image::prune_plan(&snaps, &scope, keep, &protect);
    let servers = cloud.servers(&scope.selector(image::BUILD_LABEL)).await.context("listing build VMs")?;
    let leftovers = image::leftover_build_vms(&servers, now);
    let price = cloud.image_price().await.ok();
    for s in &kept {
        println!("  keep    {} ({})", s.id, image::about(s, now));
    }
    for s in &gone {
        let cost = match (&price, s.size_gb) {
            (Some(p), Some(g)) => format!(", {}/month", p.money(g * p.per_gb_month)),
            _ => String::new(),
        };
        println!("  delete  {} ({}{cost})", s.id, image::about(s, now));
    }
    for p in &leftovers {
        println!("  delete  build VM {} (id {}) — left behind by a bake", p.name, p.id);
    }
    if gone.is_empty() && leftovers.is_empty() {
        println!("Nothing to prune (keeping the newest {keep}).");
        return Ok(());
    }
    if let Some(id) = configured.filter(|id| gone.iter().any(|s| s.id == *id)) {
        println!("⚠ HETZNER_IMAGE={id} is one of these: creates from it fail once it's gone — set HETZNER_IMAGE first");
    }
    if dry_run {
        println!("Dry run — nothing deleted.");
        return Ok(());
    }
    if !confirm(yes, &format!("Delete {} image(s) and {} build VM(s)?", gone.len(), leftovers.len()))? {
        println!("aborted.");
        return Ok(());
    }
    let mut failed = 0;
    for s in &gone {
        match cloud.delete_image(s.id).await {
            Ok(()) => println!("✓ deleted image {}", s.id),
            Err(e) => {
                failed += 1;
                eprintln!("✗ image {}: {e}", s.id);
            }
        }
    }
    for p in &leftovers {
        match cloud.delete_server(&p.id).await {
            Ok(()) => println!("✓ deleted build VM {} (id {})", p.name, p.id),
            Err(e) => {
                failed += 1;
                eprintln!("✗ build VM {} (id {}): {e}", p.name, p.id);
            }
        }
    }
    if failed > 0 {
        bail!("{failed} delete(s) failed");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn build(
    cloud: &HetznerProvider,
    remote: &dyn Remote,
    cfg: &Config,
    server_type: Option<String>,
    dry_run: bool,
    yes: bool,
    interrupt: Interrupt,
) -> Result<()> {
    let opts = HetznerOpts::from_config(cfg);
    let scope = Scope::from_config(cfg);
    if scope.repo.is_empty() {
        bail!("set ARENA_REPO_OWNER and ARENA_REPO_NAME: the image holds that repo's checkout and env");
    }
    let types = cloud.server_types().await.context("listing Hetzner's server types")?;
    let ty = match server_type {
        Some(t) => super::sizing_flags("hetzner", None, None, Some(t))?.unwrap_or_default(),
        None => image::bake_type(&types, &opts.server_type).map_err(anyhow::Error::msg)?,
    };
    let offer = types.iter().find(|t| t.name == ty).with_context(|| format!("Hetzner has no server type `{ty}` on sale"))?;
    let (arch, disk) = (offer.architecture.clone().unwrap_or_else(|| "x86".into()), offer.disk_gb.unwrap_or(0));
    let price = cloud.image_price().await.ok();
    let fits: Vec<&str> = types.iter().filter(|t| t.architecture == offer.architecture && t.disk_gb.is_some_and(|d| d >= disk)).map(|t| t.name.as_str()).collect();
    let now = arena_core::snapshot::unix_now();
    let name = image::build_name(&scope.prefix, now);
    // `pods setup`'s steps for a Hetzner pod — without any token (the image must hold none) and
    // without swap: a swap file would hold pages of this VM's memory, keys among them (the VMs
    // made from the image make their own).
    let mut scfg = arena_core::setup::SetupConfig::from_config(cfg)?;
    scfg.broadcast_exports.clear();
    scfg.swap_gb = 0;
    let timeouts = arena_core::setup::SetupTimeouts::from_config(cfg, None)?;
    let script = super::stage_hetzner_script(true)?;
    let steps = arena_core::setup::provisioning_steps("hetzner", &scfg, &name, false, &script, &timeouts);

    let symbol = price.as_ref().map_or_else(|| "€".to_string(), |p| p.symbol().trim().to_string());
    let eur_h = offer.eur_per_hour.map_or_else(|| "?".into(), |p| format!("{symbol}{p:.4}/h"));
    let shown = if fits.len() > 6 { format!("{}, … ({} types)", fits[..6].join(", "), fits.len()) } else { fits.join(", ") };
    println!("Bake an arena image for {}/{} (prefix {}):", cfg.get("ARENA_REPO_OWNER").unwrap_or("?"), cfg.get("ARENA_REPO_NAME").unwrap_or("?"), scope.prefix);
    println!("  VM        {name}: {ty} ({arch}, {disk} GB disk, {eur_h}), from {}, deleted at the end", image::FALLBACK);
    println!("  setup     {}", steps.iter().map(|s| s.label()).collect::<Vec<_>>().join(" → "));
    println!("  scrub     deploy key, authorized keys, tokens, history, ~/.name, ~/.arena, swap, machine-id, SSH host keys, cloud-init, /tmp, logs");
    println!("  scan      the homes and /etc for the backup's secret patterns and private keys — any hit: no snapshot");
    println!("  snapshot  labelled arena-image, built, repo, commit, req (requirements hash), arch, disk; boots on {disk} GB disks and up: {shown}");
    if let Some(p) = &price {
        println!("  cost      VM ≈ 20 min of {eur_h}; the image ≈ 6 GB × {}/GB/month ≈ {}/month", per_gb(p), p.money(6.0 * p.per_gb_month));
    }
    if dry_run {
        println!("Dry run — nothing created.");
        return Ok(());
    }
    if !confirm(yes, "Create the VM and bake the image?")? {
        println!("aborted.");
        return Ok(());
    }

    let plan = BuildPlan {
        name: name.clone(),
        build_id: image::new_build_id(),
        server_type: ty.clone(),
        scope: scope.clone(),
        setup: steps,
        scrub: image::scrub_command(&scfg.repo_path, &scfg.key_remote, &name, now),
        arch,
        disk_gb: disk,
        built: now,
    };
    let target = |pod: &arena_core::Pod| arena_core::ssh::SshTarget::from_pod(pod, cfg);
    let started = std::time::Instant::now();
    let report = bake::build(cloud, remote, &plan, &target, interrupt.wait(), &|l| println!("{l}")).await;
    let _ = std::fs::remove_file(&script);
    let took = started.elapsed().as_secs();
    if let Some(why) = &report.orphan_image {
        eprintln!("⚠ {why}");
    }
    let left = match &report.vm {
        VmFate::Left { id, why } => {
            eprintln!(
                "\n!!! THE BUILD VM {name} (id {id}) WAS NOT DELETED: {why}\n!!! It bills by the hour until it is. Delete it: \
                 `arena image prune` (a leftover once it's 3 h old), or now in the Hetzner console.\n"
            );
            true
        }
        VmFate::Unsure { build_id, why } => {
            eprintln!(
                "\n!!! THE BUILD VM {name} MAY EXIST: its create was sent and no answer came ({why}).\n!!! If it turns up it bills by \
                 the hour: `arena teardown --check` lists it, `arena image prune` deletes it once it's 3 h old, or delete the server \
                 labelled {}={build_id} in the Hetzner console.\n",
                image::BUILD_ID_LABEL
            );
            true
        }
        _ => false,
    };
    match report.result {
        Ok(snap) => {
            let size = snap.size_gb.map_or("?".into(), |g| format!("{g:.1} GB"));
            let monthly = match (&price, snap.size_gb) {
                (Some(p), Some(g)) => format!(", ≈ {}/month", p.money(g * p.per_gb_month)),
                _ => String::new(),
            };
            let secs = |d: Option<Duration>| d.map_or("?".into(), |d| format!("{}m{:02}s", d.as_secs() / 60, d.as_secs() % 60));
            println!(
                "\n✓ image {} ({name}): {size}{monthly}, boots on {disk} GB disks and up. Took {}m{:02}s (setup {}, snapshot {}).",
                snap.id,
                took / 60,
                took % 60,
                secs(report.setup_took),
                secs(report.snapshot_took)
            );
            println!("  Use it: HETZNER_IMAGE=arena in the config (the newest that fits), or `--image arena` on pods create/up.");
            if left {
                bail!("the image is made, but the build VM is still there (above)");
            }
            Ok(())
        }
        Err(e) => bail!("image build failed: {e}"),
    }
}

/// `price` as `$0.0199`.
fn per_gb(p: &ImagePrice) -> String {
    format!("{}{:.4}", p.symbol(), p.per_gb_month)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The repo read's scratch dir is new, this user's only, and never a path that was there.
    #[test]
    fn the_temp_dir_is_private_and_never_reused() {
        use std::os::unix::fs::PermissionsExt;
        let d = private_temp_dir("arena-test").unwrap();
        assert_eq!(std::fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o700);
        let again = private_temp_dir("arena-test").unwrap();
        assert_ne!(d, again);
        std::fs::remove_dir(&d).unwrap();
        std::fs::remove_dir(&again).unwrap();
    }

    /// `--image` is the VM's image on Hetzner (`arena`, an id, a name: one word) and the container
    /// image anywhere else, as before.
    #[test]
    fn the_image_flag_is_the_vm_image_on_hetzner_only() {
        let flag = |p: &str, i: Option<&str>| crate::image_flag(p, i.map(String::from)).map_err(|e| e.to_string());
        assert_eq!(flag("hetzner", Some("arena")), Ok((None, Some("arena".into()))));
        assert_eq!(flag("hetzner", Some(" 449622487 ")), Ok((None, Some("449622487".into()))));
        assert_eq!(flag("hetzner", Some("ubuntu-24.04")), Ok((None, Some("ubuntu-24.04".into()))));
        assert_eq!(flag("hetzner", None), Ok((None, None)));
        assert!(flag("hetzner", Some("ubuntu 24.04; rm")).unwrap_err().contains("takes `arena`"));
        assert_eq!(flag("runpod", Some("ghcr.io/x/y:1")), Ok((Some("ghcr.io/x/y:1".into()), None)));
    }

    /// What `config check` and `image list` say about the resolution: the snapshot and its age,
    /// a ⚠ per note, and why the requirements couldn't be compared.
    #[test]
    fn the_resolution_lines_carry_its_notes() {
        let now = 1_793_300_000;
        let snap = image::parse_snapshot(&serde_json::json!({
            "id": 7, "description": "devtest-image-1", "status": "available", "disk_size": 40,
            "labels": {"arena-image": "1", "arena-prefix": "devtest", "repo": "o.r", "built": (now - 20 * 86_400).to_string(), "req": "r1", "commit": "c"}
        }))
        .unwrap();
        let r = Resolved::Snapshot(Box::new(snap));
        let lines = resolution_lines(&r, now, &Ok(("c2".into(), "r2".into())));
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[0].starts_with("image arena → snapshot 7 (devtest-image-1, built ") && lines[0].ends_with("20d old, commit c)"), "{lines:?}");
        assert!(lines[1].starts_with("⚠ built 20 days ago") && lines[2].starts_with("⚠ the repo's requirements"), "{lines:?}");
        let lines = resolution_lines(&r, now, &Err("git clone failed".into()));
        assert_eq!(lines.last().unwrap(), "(couldn't compare its requirements with the repo's: git clone failed)");
        let none = Resolved::Fallback { why: "no arena-built snapshot in this Hetzner project".into() };
        assert_eq!(resolution_lines(&none, now, &Ok(("c".into(), "r".into()))).len(), 1);
    }
}
