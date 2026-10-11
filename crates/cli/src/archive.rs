//! `arena archive …`: the backup archive VM (see `arena_core::archive` for the design). This side
//! does the I/O — the Hetzner calls through [`ArchiveCloud`], the SSH to the VM through
//! [`Remote`] (pinned host key, one dedicated key per role), the pushes through `rsync` — and
//! the core plans, renders and judges.
//!
//! Who touches what:
//! - `up`, `resize`, `down` change the Hetzner project, each after one fresh inventory and a
//!   confirmation; any listing error stops them before the first mutation (S8).
//! - `status` lists, reads `/pricing` and runs the read-only probe — nothing else (S16).
//! - `sync` (the cron path) takes **no** [`ArchiveCloud`] and no token at all (S13): it reads
//!   local files and `archive.json`, and pushes with the push key.
//! - `access`, `ssh-config`, `ssh` only touch the VM (or nothing).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use arena_core::archive::cloud::{is_placement_error, server_body, volume_body, CloudServer};
use arena_core::archive::render::{self, ReadmeInputs, SetupInputs, RRSYNC_PLACEHOLDER};
use arena_core::archive::report::{self, judge_status, StatusInputs};
use arena_core::archive::state::{self, ArchiveHost, ArchivePaths, BigState, ErrRecord, LabelRecord, RunRecord, SnapState};
use arena_core::archive::sync::{self as push, PushJob, PushTransport, SnapPass};
use arena_core::archive::{
    self, plan_down, plan_resize, plan_up, ArchiveCloud, ArchiveNames, ArchiveSettings, DownStep, PushFailure, SizeFlags, Tier,
    TokenSource, UpPlan, UpStep,
};
use arena_core::remote::{describe_error, Remote};
use arena_core::ssh::{HostKeyPolicy, SshTarget};
use arena_core::Config;

use super::confirm;

/// `arena archive …`.
#[derive(clap::Subcommand, Debug)]
pub(crate) enum ArchiveCmd {
    /// Create or converge the archive (idempotent): its own SSH-key object, the server (delete +
    /// rebuild protected), the two volumes (ext4 by Hetzner, protected), the pinned host key, the
    /// setup script. Re-run it any time — after a server loss it builds a new server for the
    /// surviving volumes, never formatting them. Participant access stays as it was (OFF at first).
    Up {
        /// The snap volume's size at creation (default ARCHIVE_SNAP_GB, else 50).
        #[arg(long)]
        snap_gb: Option<u32>,
        /// The big volume's size at creation (default ARCHIVE_BIG_GB, else 200).
        #[arg(long)]
        big_gb: Option<u32>,
        /// Accept the server's current host key although it isn't the pinned one (a rebuilt
        /// server). Check its fingerprint in the Hetzner console.
        #[arg(long)]
        accept_new_host_key: bool,
        /// Preview: the plan, live prices and local sizes; changes nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Read-only health: the server and volumes (API), mounts, read-only binds, space and inodes,
    /// the key audit (probe over SSH), the last pushes, retention and cost. Exit non-zero on any
    /// FAIL or UNKNOWN.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Push the backup dir to the archive (additive only — never deletes there). Snap first: every
    /// dated label that isn't final yet, hard-linked to the previous one; then big. What cron runs.
    Sync {
        /// `snap`, `big` or `both` (default).
        #[arg(long, default_value = "both")]
        tier: String,
        /// Push this snapshot label even if final, or a custom one (`pods pull --label …`). Repeatable.
        #[arg(long = "label")]
        labels: Vec<String>,
        /// Push from this directory instead of LOCAL_BACKUP_DIR.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Print the rsync commands; push nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Grow a volume (online): the API resize, then resize2fs on the server. Never shrinks.
    Resize {
        #[arg(long, conflicts_with = "big_gb", required_unless_present = "big_gb")]
        snap_gb: Option<u32>,
        #[arg(long)]
        big_gb: Option<u32>,
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
    /// Let participants read (`on`: the cohort key from laptops, the read key from pods) or stop
    /// them (`off`: instant, and open sessions are ended). Off until you turn it on.
    Access {
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// Print the participants' laptop ssh-config block (or, with --pod, the one `copy-keys` puts on pods).
    SshConfig {
        #[arg(long)]
        pod: bool,
    },
    /// A root shell (or one command) on the archive: the admin key, the pinned host key.
    Ssh {
        /// The command to run (after `--`); none = an interactive shell.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        cmd: Vec<String>,
    },
    /// Delete the server (its volumes are detached and KEPT, still protected). With
    /// `--delete-volumes --confirm <prefix>-archive`: also the volumes and every backup on them.
    Down {
        #[arg(long)]
        delete_volumes: bool,
        /// Required with --delete-volumes: the archive's name typed out (`-y` is not enough).
        #[arg(long)]
        confirm: Option<String>,
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
}

/// What `up` runs to see that SSH works (a distinctive no-op).
const READY_CMD: &str = "echo arena-archive-ready";

/// Budgets.
const SSH_WAIT: Duration = Duration::from_secs(300);
const SSH_RETRY: Duration = Duration::from_secs(5);
const SSH_ATTEMPT: Duration = Duration::from_secs(20);
const SETUP_TIMEOUT: Duration = Duration::from_secs(600);
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);
const ACCESS_TIMEOUT: Duration = Duration::from_secs(60);
const QUIESCE_TIMEOUT: Duration = Duration::from_secs(70);
const RESIZE_TIMEOUT: Duration = Duration::from_secs(300);
const ACTION_LIMIT: Duration = Duration::from_secs(300);
const SERVER_ACTION_LIMIT: Duration = Duration::from_secs(600);

/// What every handler works with.
pub(crate) struct Env<'a> {
    pub cfg: &'a Config,
    pub paths: ArchivePaths,
    pub names: ArchiveNames,
    pub remote: &'a dyn Remote,
    pub yes: bool,
}

impl<'a> Env<'a> {
    pub fn new(cfg: &'a Config, remote: &'a dyn Remote, yes: bool) -> Result<Self> {
        let paths = ArchivePaths::for_config(cfg).map_err(anyhow::Error::msg)?;
        let names = ArchiveNames::from_config(cfg).map_err(anyhow::Error::msg)?;
        Ok(Self { cfg, paths, names, remote, yes })
    }

    /// The admin SSH target (root, admin key only).
    fn admin(&self, ip: &str, policy: HostKeyPolicy) -> SshTarget {
        target("root", ip, &self.paths.admin_key(), policy)
    }

    fn pinned(&self) -> HostKeyPolicy {
        HostKeyPolicy::Pinned { known_hosts: self.paths.known_hosts().display().to_string() }
    }

    fn host(&self) -> Result<Option<ArchiveHost>> {
        state::load_host(&self.paths.host_file()).map_err(anyhow::Error::msg)
    }

    /// `archive.json` with a server — or the error every VM-touching command gives.
    fn host_up(&self) -> Result<ArchiveHost> {
        match self.host()? {
            None => bail!(
                "no archive.json in {} for this user — run `arena archive up` as this user first (or set ARENA_STATE_DIR to \
                 the state dir it used)",
                self.paths.dir.display()
            ),
            Some(h) if !h.is_up() => bail!("the archive's server is down (`arena archive down`) — `arena archive up` brings it back"),
            Some(h) => Ok(h),
        }
    }
}

/// An archive SSH target: port 22, ONE identity, the given host-key policy (S3).
fn target(user: &str, ip: &str, key: &Path, policy: HostKeyPolicy) -> SshTarget {
    SshTarget { user: user.into(), host: ip.into(), port: 22, key_paths: vec![key.display().to_string()], connect_timeout_secs: 10, host_keys: policy }
}

/// rsync's transport for a push: `arena-push` with the push key, pinned.
fn push_transport(paths: &ArchivePaths, ip: &str) -> PushTransport {
    let t = target(archive::PUSH_USER, ip, &paths.push_key(), HostKeyPolicy::Pinned { known_hosts: paths.known_hosts().display().to_string() });
    PushTransport { rsh: t.rsh_command(), dest: format!("{}@{ip}", archive::PUSH_USER) }
}

/// The public keys that must never be authorized on the archive: the fleet's (cohort, deploy,
/// admin — `ssh::authorized_pubkeys`) and every `EXTRA_SSH_KEYS` key.
fn fleet_pubkeys(cfg: &Config) -> Vec<String> {
    let mut v = arena_core::ssh::authorized_pubkeys(cfg);
    for p in cfg.get("EXTRA_SSH_KEYS").unwrap_or("").split(',').map(str::trim).filter(|s| !s.is_empty()) {
        v.extend(arena_core::ssh::public_key_for(p));
    }
    v
}

/// The keys the read user (`backups`) must never hold: the deploy key (every pod of every
/// cohort holds its private half), `ADMIN_SSH_KEY_PATH`, every `EXTRA_SSH_KEYS` key, and the
/// archive's own admin and push keys. A cohort key that IS one of them (a config mix-up) is
/// refused by its blob rather than published to everyone holding it.
fn forbidden_for_readers(cfg: &Config, paths: &ArchivePaths) -> Vec<String> {
    let mut v = Vec::new();
    for key in ["GIT_SSH_KEY_LOCAL", "ADMIN_SSH_KEY_PATH"] {
        if let Some(p) = cfg.get(key).filter(|s| !s.is_empty()) {
            v.extend(arena_core::ssh::public_key_for(p));
        }
    }
    for p in cfg.get("EXTRA_SSH_KEYS").unwrap_or("").split(',').map(str::trim).filter(|s| !s.is_empty()) {
        v.extend(arena_core::ssh::public_key_for(p));
    }
    v.extend(state::read_pub(&paths.admin_key()));
    v.extend(state::read_pub(&paths.push_key()));
    v
}

/// The cohort key's public half (`SHARED_SSH_KEY_PATH`).
fn cohort_pub(cfg: &Config) -> Option<String> {
    cfg.get("SHARED_SSH_KEY_PATH").filter(|s| !s.is_empty()).and_then(arena_core::ssh::public_key_for)
}

/// The `backups` key file `access` (and setup) should write.
fn backups_keys(env: &Env<'_>, access_on: bool) -> std::result::Result<String, String> {
    let read = state::read_pub(&env.paths.read_key()).ok_or("the read key (read_ed25519.pub) is missing — run `arena archive up`")?;
    let cohort = cohort_pub(env.cfg);
    render::render_backups_keys(cohort.as_deref(), &read, access_on, &forbidden_for_readers(env.cfg, &env.paths))
}

fn days_today() -> i64 {
    arena_core::schedule::days_from_unix(arena_core::snapshot::unix_now())
}

/// `€X.XX/month`.
fn eur(x: f64) -> String {
    format!("€{x:.2}/month")
}

// =========================================================================================
// up
// =========================================================================================

/// `archive up`'s flags.
#[derive(Debug, Clone, Default)]
pub(crate) struct UpOpts {
    pub snap_gb: Option<u32>,
    pub big_gb: Option<u32>,
    pub accept_new_host_key: bool,
    pub dry_run: bool,
}

/// `archive up` (spec §6.1).
pub(crate) async fn archive_up(env: &Env<'_>, cloud: &dyn ArchiveCloud, source: TokenSource, o: UpOpts) -> Result<()> {
    let settings = ArchiveSettings::from_config(env.cfg).map_err(anyhow::Error::msg)?;
    env.paths.check_plain().map_err(anyhow::Error::msg)?;
    if source == TokenSource::Fleet {
        eprintln!("{}", archive::FLEET_TOKEN_WARNING);
    }
    let names = &env.names;
    if cohort_pub(env.cfg).is_none() {
        println!("warning: the cohort key's public half (SHARED_SSH_KEY_PATH) can't be read — participant access stays off");
    }

    // 2. Keys (generated once, in the state dir).
    let keys = [(env.paths.admin_key(), "arena-archive-admin"), (env.paths.push_key(), "arena-archive-push"), (env.paths.read_key(), "arena-archive-read")];
    for (key, comment) in &keys {
        if key.exists() {
            continue;
        }
        if o.dry_run {
            println!("would generate {}", key.display());
        } else {
            env.paths.ensure_dir().with_context(|| format!("creating {}", env.paths.dir.display()))?;
            state::ensure_keypair(key, comment).with_context(|| format!("generating {}", key.display()))?;
            println!("generated {}", key.display());
        }
    }
    // The archive's own keys are never fleet keys (no pod may hold the admin key; the read key
    // is per cohort).
    let fleet = fleet_pubkeys(env.cfg);
    for (key, _) in &keys {
        if let Some(pubk) = state::read_pub(key) {
            if fleet.iter().any(|f| archive::key_blob(f) == archive::key_blob(&pubk)) {
                bail!("{} is also a fleet key (authorized on pods) — the archive needs its own keys; remove it and re-run", key.display());
            }
        }
    }

    // 3. Discover (fail closed), 4. plan.
    let inv = cloud.inventory(Some(&names.prefix)).await.context("listing the archive's Hetzner resources — nothing was changed")?;
    let admin_pub = state::read_pub(&env.paths.admin_key());
    let flags = SizeFlags { snap_gb: o.snap_gb, big_gb: o.big_gb };
    let (plan, notes) = plan_up(&inv, names, &settings, &env.cfg.machine_names, flags, admin_pub.as_deref());
    let steps = match plan {
        UpPlan::Refuse(why) => bail!("archive up refused — nothing was changed: {why}"),
        UpPlan::Steps(s) => s,
    };

    // 5. Show the plan with live prices.
    println!("Backup archive {} — plan:", names.server());
    for s in &steps {
        println!("  • {}", s.describe(names));
    }
    for n in &notes {
        println!("  note: {n}");
    }
    let existing = inv.server(names);
    let location = steps
        .iter()
        .find_map(|s| match s {
            UpStep::CreateServer { location } => Some(location.clone()),
            _ => None,
        })
        .or_else(|| existing.map(|s| s.location.clone()))
        .unwrap_or_else(|| settings.location.clone());
    let server_type = existing.map(|s| s.server_type.clone()).unwrap_or_else(|| settings.server_types[0].clone());
    let gb: u32 = Tier::ALL
        .iter()
        .map(|t| inv.volume(names, *t).map(|v| v.size_gb).unwrap_or_else(|| flags.of(*t).unwrap_or_else(|| settings.size_of(*t))))
        .sum();
    match cloud.prices(&server_type, &location).await {
        Ok(p) => println!(
            "  cost: ≈ {} ({server_type} {}, IPv4 {}, {gb} GB of volumes at €{:.4}/GB) — excl. VAT",
            eur(p.monthly(true, gb)),
            p.server_month.map_or("?".into(), eur),
            p.ipv4_month.map_or("?".into(), eur),
            p.volume_gb_month
        ),
        Err(e) => println!("  cost: live prices unavailable ({e})"),
    }
    println!("  participant access: {}", if env.host()?.is_some_and(|h| h.access_on) { "on (kept)" } else { "OFF until `arena archive access on`" });
    if o.dry_run {
        print_local_sizes(env.cfg, &inv, names, &settings, flags);
        println!("\nPreview only — run without --dry-run to apply.");
        return Ok(());
    }
    if !confirm(env.yes, &format!("\nApply this plan to {}?", names.server()))? {
        println!("aborted.");
        return Ok(());
    }

    // 6–9. Execute, in order; a failure names its step (a re-run converges).
    let mut server: Option<CloudServer> = existing.cloned();
    let mut created = false;
    for step in &steps {
        let what = step.describe(names);
        run_up_step(env, cloud, &settings, step, &mut server, &mut created, &o)
            .await
            .with_context(|| format!("archive up stopped at: {what} — fix the cause and re-run `arena archive up` (it picks up where it stopped)"))?;
    }
    Ok(())
}

/// One step of `up`.
async fn run_up_step(
    env: &Env<'_>,
    cloud: &dyn ArchiveCloud,
    settings: &ArchiveSettings,
    step: &UpStep,
    server: &mut Option<CloudServer>,
    created: &mut bool,
    o: &UpOpts,
) -> Result<()> {
    let names = &env.names;
    let fresh_inventory = || async { cloud.inventory(Some(&names.prefix)).await.context("listing again before creating") };
    match step {
        UpStep::CreateSshKey => {
            let inv = fresh_inventory().await?;
            if !inv.owned_by(names).ssh_keys.is_empty() {
                println!("= {} exists (made meanwhile)", names.ssh_key());
                return Ok(());
            }
            let pubk = state::read_pub(&env.paths.admin_key()).context("the admin key's .pub is missing")?;
            let k = cloud.create_ssh_key(&names.ssh_key(), &pubk, &names.labels()).await?;
            println!("✓ SSH-key object {} (id {})", k.name, k.id);
        }
        UpStep::CreateServer { location } => {
            let inv = fresh_inventory().await?;
            if let Some(s) = inv.server(names) {
                println!("= {} exists (made meanwhile)", s.name);
                *server = Some(s.clone());
                return Ok(());
            }
            let key = inv.owned_by(names).ssh_keys.first().map(|k| k.id).context("the archive's SSH-key object is missing")?;
            let mut why = Vec::new();
            for ty in &settings.server_types {
                match cloud.create_server(&server_body(names, ty, location, key)).await {
                    Ok((s, action)) => {
                        cloud.wait_action(action, SERVER_ACTION_LIMIT).await.context("waiting for the server")?;
                        println!("✓ server {} ({ty}, {location}, id {})", s.name, s.id);
                        *server = Some(s);
                        *created = true;
                        return Ok(());
                    }
                    Err(e) if is_placement_error(&e) => {
                        println!("  {ty} unavailable in {location} — trying the next ARCHIVE_SERVER_TYPES entry");
                        why.push(format!("{ty}: {e}"));
                    }
                    Err(e) => return Err(e).context("creating the server"),
                }
            }
            bail!("no type in ARCHIVE_SERVER_TYPES could be placed in {location} — nothing is billed ({})", why.join("; "));
        }
        UpStep::ProtectServer => {
            let s = server.as_ref().context("no server to protect")?;
            let a = cloud.protect_server(s.id, true).await?;
            cloud.wait_action(a, ACTION_LIMIT).await?;
            println!("✓ {} protected (delete + rebuild)", s.name);
        }
        UpStep::CreateVolume { tier, size_gb } => {
            let inv = fresh_inventory().await?;
            if inv.volume(names, *tier).is_some() {
                println!("= {} exists (made meanwhile)", names.volume(*tier));
                return Ok(());
            }
            let s = server.as_ref().context("no server to attach the volume to")?;
            let (v, actions) = cloud.create_volume(&volume_body(names, *tier, *size_gb, s.id)).await?;
            for a in actions {
                cloud.wait_action(a, ACTION_LIMIT).await?;
            }
            let a = cloud.protect_volume(v.id, true).await?;
            cloud.wait_action(a, ACTION_LIMIT).await?;
            println!("✓ volume {} ({} GB, ext4, attached, protected)", v.name, v.size_gb);
        }
        UpStep::AttachVolume { tier, id } => {
            let s = server.as_ref().context("no server to attach the volume to")?;
            let a = cloud.attach(*id, s.id).await?;
            cloud.wait_action(a, ACTION_LIMIT).await?;
            println!("✓ {} attached (its data untouched)", names.volume(*tier));
        }
        UpStep::ProtectVolume { tier, id } => {
            let a = cloud.protect_volume(*id, true).await?;
            cloud.wait_action(a, ACTION_LIMIT).await?;
            println!("✓ {} protected", names.volume(*tier));
        }
        UpStep::PinHostKey { fresh } => {
            let ip = server_ip(env, cloud, server).await?;
            pin_host_key(env, &ip, *fresh || *created, o.accept_new_host_key).await?;
        }
        UpStep::RunSetup => run_setup(env, cloud, settings).await?,
        UpStep::WriteState => {} // written by RunSetup, once the VM is known good
    }
    Ok(())
}

/// The server's IPv4 (a just-created server reports it in its create answer; else list again).
async fn server_ip(env: &Env<'_>, cloud: &dyn ArchiveCloud, server: &mut Option<CloudServer>) -> Result<String> {
    if let Some(ip) = server.as_ref().and_then(|s| s.ipv4.clone()) {
        return Ok(ip);
    }
    let inv = cloud.inventory(Some(&env.names.prefix)).await?;
    let s = inv.server(&env.names).context("the server is gone")?;
    *server = Some(s.clone());
    s.ipv4.clone().context("the server has no IPv4")
}

/// Wait for SSH (≤ 5 min) and pin the host key (§6.1 step 7): a server this run created (or a
/// state dir that has never pinned one — no known_hosts, or one with no key in it, as a first
/// `up` that stopped before SSH answered leaves) is trusted on first use into an EMPTY
/// known_hosts; a changed pin needs `--accept-new-host-key`.
async fn pin_host_key(env: &Env<'_>, ip: &str, fresh: bool, accept_new: bool) -> Result<()> {
    let kh = env.paths.known_hosts();
    let never_pinned = std::fs::read_to_string(&kh).map_or(true, |t| render::parse_known_hosts(&t).is_empty());
    let mut policy = if fresh || never_pinned {
        state::save_text(&kh, "").with_context(|| format!("resetting {}", kh.display()))?;
        HostKeyPolicy::PinFirstUse { known_hosts: kh.display().to_string() }
    } else {
        env.pinned()
    };
    let deadline = tokio::time::Instant::now() + SSH_WAIT;
    let mut last: String;
    loop {
        let t = env.admin(ip, policy.clone());
        match env.remote.exec(&t, READY_CMD, Some(SSH_ATTEMPT)).await {
            Ok(o) if o.success => break,
            Ok(o) if matches!(push::classify_push(o.code, &o.stderr, false), Err(PushFailure::HostKey)) => {
                if matches!(policy, HostKeyPolicy::Pinned { .. }) && accept_new {
                    println!("  the host key isn't the pinned one — re-pinning (--accept-new-host-key)");
                    state::save_text(&kh, "")?;
                    policy = HostKeyPolicy::PinFirstUse { known_hosts: kh.display().to_string() };
                    continue;
                }
                bail!(
                    "the archive's host key isn't the one pinned in {} (or none is pinned for {ip}) — if you know why (a rebuilt \
                     server), re-run with --accept-new-host-key and compare the fingerprint with the Hetzner console",
                    kh.display()
                );
            }
            Ok(o) => last = o.stderr.trim().to_string(),
            Err(e) => last = describe_error(&e),
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("the archive didn't answer SSH as root within {}s (last: {last})", SSH_WAIT.as_secs());
        }
        tokio::time::sleep(SSH_RETRY).await;
    }
    println!("✓ SSH as root, host key pinned in {}", kh.display());
    Ok(())
}

/// `ED25519 SHA256:…` of the pinned key (`ssh-keygen -l`), best effort.
fn fingerprint(known_hosts: &Path) -> Option<String> {
    let out = std::process::Command::new("ssh-keygen").arg("-l").arg("-f").arg(known_hosts).output().ok()?;
    let line = String::from_utf8_lossy(&out.stdout).lines().next()?.to_string();
    let words: Vec<&str> = line.split_whitespace().collect();
    let fp = words.iter().find(|w| w.starts_with("SHA256:"))?;
    let ty = words.last().map(|w| w.trim_matches(|c| c == '(' || c == ')')).unwrap_or("");
    Some(format!("{ty} {fp}").trim().to_string())
}

/// §6.1 steps 8–9: run setup once, judge its report, write `archive.json`, print what's next.
async fn run_setup(env: &Env<'_>, cloud: &dyn ArchiveCloud, settings: &ArchiveSettings) -> Result<()> {
    let names = &env.names;
    let inv = cloud.inventory(Some(&names.prefix)).await.context("listing the volumes' devices")?;
    let server = inv.server(names).context("the server is gone")?;
    let ip = server.ipv4.clone().context("the server has no IPv4")?;
    let vol = |t: Tier| inv.volume(names, t).with_context(|| format!("the {t} volume is gone"));
    let (snap, big) = (vol(Tier::Snap)?, vol(Tier::Big)?);
    for v in [snap, big] {
        if !render::plain_device(&v.linux_device) {
            bail!("{} reports the device path {:?}, which arena won't put in fstab", v.name, v.linux_device);
        }
    }
    let previous = env.host()?;
    let mut access_on = previous.as_ref().is_some_and(|h| h.access_on);
    let mut backups = String::new();
    if access_on {
        match backups_keys(env, true) {
            Ok(k) => backups = k,
            Err(e) => {
                println!("warning: participant access turned OFF: {e}");
                access_on = false;
            }
        }
    }
    let push_pub = state::read_pub(&env.paths.push_key()).context("push_ed25519.pub is missing")?;
    let inputs = SetupInputs {
        snap_dev: snap.linux_device.clone(),
        big_dev: big.linux_device.clone(),
        push_keys: render::render_push_key_line(RRSYNC_PLACEHOLDER, &push_pub).map_err(anyhow::Error::msg)?,
        backups_keys: backups,
        readme: render::render_readme(&ReadmeInputs {
            prefix: names.prefix.clone(),
            alias: names.alias(),
            keep_until: settings.keep_until_text(),
            snap_max_size: settings.snap_max_size.clone(),
            extra_excludes: settings.extra_excludes.clone(),
        }),
        setup_version: archive::SETUP_VERSION,
    };
    println!("  running the setup script on {ip} (≤ {} min)…", SETUP_TIMEOUT.as_secs() / 60);
    let out = env.remote.exec(&env.admin(&ip, env.pinned()), &render::setup_command(&inputs), Some(SETUP_TIMEOUT)).await.context("running the setup script")?;
    let rep = report::parse_setup_report(&out.stdout);
    if rep.sshd_failed() {
        let why = rep.get("why_sshd").unwrap_or("no reason given");
        bail!("the sshd config failed its checks and was NOT installed — config unchanged, root access kept: {why}");
    }
    let fails = rep.failures();
    if !fails.is_empty() {
        let list: Vec<String> = fails.iter().map(|(s, w)| format!("{s} ({w})")).collect();
        bail!("setup failed: {}", list.join("; "));
    }
    if !rep.finished() || !out.success {
        bail!("setup didn't finish ({}): {}", out.exit_label(), out.stderr.lines().last().unwrap_or("no message"));
    }
    println!(
        "✓ setup: mounts, read-only binds, users, key files and sshd in place (rrsync at {}; sshd reload: {})",
        rep.rrsync().unwrap_or("?"),
        rep.get("sshd_reload").unwrap_or("?")
    );
    let host = ArchiveHost {
        version: state::STATE_VERSION,
        prefix: names.prefix.clone(),
        server_id: server.id,
        ipv4: ip.clone(),
        location: server.location.clone(),
        snap_device: snap.linux_device.clone(),
        big_device: big.linux_device.clone(),
        access_on,
        setup_version: archive::SETUP_VERSION,
        snap_gb: snap.size_gb,
        big_gb: big.size_gb,
        host_key_fingerprint: fingerprint(&env.paths.known_hosts()).or_else(|| previous.and_then(|p| p.host_key_fingerprint)),
    };
    state::save(&env.paths.host_file(), &host).context("writing archive.json")?;
    println!("✓ wrote {}", env.paths.host_file().display());
    println!("\nThe participants' laptop block (`arena ssh-config` appends it):\n");
    print!("{}", laptop_block(env.cfg, &host));
    println!(
        "\nNext:\n  arena cron install --archive            # push snap every 15 min, big hourly (needs ARENA_START_DATE)\n  \
         arena pods copy-keys --all             # the pods' block, pinned host key and read key\n  \
         arena archive access on                # when you're ready to let participants read\n\
         Back up {} (keys + pin): losing it stops pushes and admin access.",
        env.paths.dir.display()
    );
    Ok(())
}

/// `up --dry-run`'s local sizes: the newest snapshot label and `big/`, against the volumes.
fn print_local_sizes(cfg: &Config, inv: &archive::Inventory, names: &ArchiveNames, settings: &ArchiveSettings, flags: SizeFlags) {
    let dir = super::local_backup_dir(cfg);
    let base = Path::new(&dir);
    let gb_of = |t: Tier| inv.volume(names, t).map(|v| v.size_gb).unwrap_or_else(|| flags.of(t).unwrap_or_else(|| settings.size_of(t)));
    let newest = push::local_labels(base).ok().and_then(|l| l.last().cloned());
    let sizes = [
        (Tier::Snap, newest.as_ref().map(|l| base.join(l)), newest.clone().unwrap_or_default()),
        (Tier::Big, Some(base.join(arena_core::restore::BIG)), "big/".into()),
    ];
    println!("  local sizes ({dir}):");
    for (tier, path, what) in sizes {
        let Some(path) = path else {
            println!("    {tier}: no snapshot label yet");
            continue;
        };
        let Ok(d) = arena_core::pull::disk_use(&path) else {
            println!("    {tier}: {what} not there yet");
            continue;
        };
        let have = u64::from(gb_of(tier)) << 30;
        let warn = if have < 2 * d.bytes { format!("  ⚠ the {tier} volume ({} GB) is under 2× this", gb_of(tier)) } else { String::new() };
        println!("    {tier}: {what} holds {} in {} files{warn}", arena_core::pull::human_bytes(d.bytes), d.files);
    }
}

// =========================================================================================
// status
// =========================================================================================

/// `archive status` (read-only; S16).
pub(crate) async fn archive_status(env: &Env<'_>, cloud: Option<&dyn ArchiveCloud>, source: Option<TokenSource>, json: bool, now: u64) -> Result<()> {
    let names = &env.names;
    let inventory = match cloud {
        None => None,
        Some(c) => Some(match tokio::time::timeout(arena_core::provider::LIST_TIMEOUT, c.inventory(Some(&names.prefix))).await {
            Ok(Ok(inv)) => Ok(inv),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err(format!("timed out after {}s", arena_core::provider::LIST_TIMEOUT.as_secs())),
        }),
    };
    let host = env.host()?;
    let probe = match host.as_ref().filter(|h| h.is_up()) {
        Some(h) if state::readable(&env.paths.admin_key()) && state::readable(&env.paths.known_hosts()) => {
            let cmd = render::probe_command(&h.snap_device, &h.big_device);
            Some(match env.remote.exec(&env.admin(&h.ipv4, env.pinned()), &cmd, Some(PROBE_TIMEOUT)).await {
                Ok(o) => report::parse_probe(&o.stdout).map_err(|e| format!("{e} ({}: {})", o.exit_label(), o.stderr.trim())),
                Err(e) => Err(describe_error(&e)),
            })
        }
        Some(_) => Some(Err("the admin key or known_hosts isn't readable by this user".into())),
        None => None,
    };
    let snap = state::load_snap(&env.paths.sync_state(Tier::Snap)).map_err(anyhow::Error::msg)?;
    let big = state::load_big(&env.paths.sync_state(Tier::Big)).map_err(anyhow::Error::msg)?;
    let dir = PathBuf::from(super::local_backup_dir(env.cfg));
    let local = push::local_labels(&dir).unwrap_or_default();
    let unsealed: Vec<String> = match start_days(env.cfg) {
        Ok(start) => local.iter().filter(|l| !push::sealed(l, &snap, start)).cloned().collect(),
        Err(_) => local.clone(),
    };
    let custom = push::custom_labels(&dir);
    let allowed = push::allowed_pod_dirs(&names.prefix, &env.cfg.machine_names);
    let only_local = push::control_box_only(&dir, &local, &allowed);
    let prices = match (cloud, inventory.as_ref().and_then(|i| i.as_ref().ok()).and_then(|i| i.server(names))) {
        (Some(c), Some(s)) => c.prices(&s.server_type, &s.location).await.ok(),
        _ => None,
    };
    let expected = host.as_ref().and_then(|h| backups_keys(env, h.access_on).ok());
    let admin_pub = state::read_pub(&env.paths.admin_key());
    let push_pub = state::read_pub(&env.paths.push_key());
    let inv_ref = inventory.as_ref().map(|r| r.as_ref().map_err(String::clone));
    let r = judge_status(&StatusInputs {
        names,
        inventory: inv_ref,
        host: host.as_ref(),
        probe,
        admin_pub: admin_pub.as_deref(),
        push_pub: push_pub.as_deref(),
        expected_backups: expected.as_deref(),
        snap: &snap,
        big: &big,
        unsealed: &unsealed,
        custom_labels: &custom,
        control_box_only: &only_local,
        now,
        keep_until_days: archive::keep_until_from_config(env.cfg).unwrap_or(None),
        today_days: days_today(),
        token_warning: (source == Some(TokenSource::Fleet)).then_some(archive::FLEET_TOKEN_WARNING),
        prices,
    });
    if json {
        println!("{}", serde_json::to_string_pretty(&r)?);
    } else {
        println!("Backup archive {} (read-only check)\n", names.server());
        print!("{}", r.render());
    }
    if !r.ok() {
        bail!("archive status: {} fail, {} unknown", r.count(report::Level::Fail), r.count(report::Level::Unknown));
    }
    Ok(())
}

fn start_days(cfg: &Config) -> Result<i64> {
    let (y, m, d) = cfg
        .get("ARENA_START_DATE")
        .and_then(arena_core::schedule::parse_ymd)
        .context("the snap push needs ARENA_START_DATE=YYYY-MM-DD to know which day each wNdM label is (as `pods pull` does)")?;
    Ok(arena_core::schedule::days_from_civil(y, m, d))
}

// =========================================================================================
// sync (the cron path: no Hetzner, no token — S13)
// =========================================================================================

/// `archive sync`'s flags.
#[derive(Debug, Clone, Default)]
pub(crate) struct SyncOpts {
    pub tier: String,
    pub labels: Vec<String>,
    pub dir: Option<PathBuf>,
    pub dry_run: bool,
}

/// A held sync lock (`flock`, released when dropped).
struct Lock(#[allow(dead_code)] std::fs::File);

/// Take a tier's lock without waiting: `None` = another sync of that tier holds it.
fn try_lock(path: &Path) -> Result<Option<Lock>> {
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let f = opts.open(path).with_context(|| format!("opening {}", path.display()))?;
    match f.try_lock() {
        Ok(()) => Ok(Some(Lock(f))),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e).with_context(|| format!("locking {}", path.display())),
    }
}

/// `archive sync` (§7.5). `rsync` is the program (a recording stub in tests); `clock` the time
/// each push starts and ends (sealing reads it). Takes no `ArchiveCloud`: S13.
pub(crate) async fn archive_sync(cfg: &Config, paths: &ArchivePaths, o: SyncOpts, rsync: &str, clock: &dyn Fn() -> u64) -> Result<()> {
    let tiers: Vec<Tier> = match o.tier.as_str() {
        "both" => Tier::ALL.to_vec(),
        t => vec![Tier::parse(t).with_context(|| format!("--tier {t}: snap, big or both"))?],
    };
    let settings = ArchiveSettings::from_config(cfg).map_err(anyhow::Error::msg)?;
    let names = ArchiveNames::from_config(cfg).map_err(anyhow::Error::msg)?;
    paths.check_plain().map_err(anyhow::Error::msg)?;
    for l in &o.labels {
        if arena_core::restore::parse_label(l).is_none() && !push::valid_custom_label(l) {
            bail!("--label {l}: a snapshot label (w1d3) or a plain name (letters, digits, `._-`)");
        }
    }
    // Before anything is spawned: the state this user needs.
    let host = match state::load_host(&paths.host_file()).map_err(anyhow::Error::msg)? {
        None => bail!(
            "no archive.json in {} for this user — run `arena archive up` as this user first (or set ARENA_STATE_DIR to the \
             state dir it used)",
            paths.dir.display()
        ),
        Some(h) if !h.is_up() => bail!("the archive's server is down (`arena archive down`) — `arena archive up` brings it back"),
        Some(h) => h,
    };
    for f in [paths.push_key(), paths.known_hosts()] {
        if !state::readable(&f) {
            bail!("{} isn't readable by this user — run `arena archive up` as this user (or share ARENA_STATE_DIR)", f.display());
        }
    }
    let raw_dir = o.dir.clone().unwrap_or_else(|| PathBuf::from(super::local_backup_dir(cfg)));
    let dir = std::fs::canonicalize(&raw_dir).with_context(|| format!("the backup dir {} doesn't exist (LOCAL_BACKUP_DIR / --dir)", raw_dir.display()))?;
    println!("Pushing {} → {} ({}){}", dir.display(), host.ipv4, names.server(), if o.dry_run { " — dry run" } else { "" });
    let allowed = push::allowed_pod_dirs(&names.prefix, &cfg.machine_names);
    let transport = push_transport(paths, &host.ipv4);
    let mut failed = 0;
    for tier in tiers {
        let r = match tier {
            Tier::Snap => sync_snap(cfg, paths, &dir, &o, &allowed, &transport, &settings, &host, rsync, clock).await,
            Tier::Big => sync_big(paths, &dir, &o, &allowed, &transport, &settings, &host, rsync, clock).await,
        };
        match r {
            Ok(n) => failed += n,
            Err(e) => {
                println!("✗ {tier}: {e:#}");
                failed += 1;
            }
        }
    }
    let local = push::local_labels(&dir).unwrap_or_default();
    let only_local = push::control_box_only(&dir, &local, &allowed);
    if !only_local.is_empty() {
        println!("kept on the control box only: {}", only_local.join(", "));
    }
    if failed > 0 {
        bail!("{failed} archive push(es) failed");
    }
    Ok(())
}

/// The outcome of one label's (or big's) push: stats, or the failure and its last line.
type PushOutcome = std::result::Result<(Option<String>, Option<arena_core::pull::RsyncStats>), (PushFailure, String)>;

/// Run `jobs` one after the other within one budget, classifying each (§7.5).
async fn run_push(rsync: &str, jobs: &[PushJob], transport: &PushTransport, settings: &ArchiveSettings, budget: Duration) -> PushOutcome {
    let deadline = tokio::time::Instant::now() + budget;
    let mut notes = Vec::new();
    let mut stats: Option<arena_core::pull::RsyncStats> = None;
    for job in jobs {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now()).max(Duration::from_secs(1));
        let args = push::push_args(job, transport, settings);
        let what = format!("archive push {}", job.label.as_deref().unwrap_or(job.tier.as_str()));
        match arena_core::remote::run_local(rsync, &args, &what, Some(left)).await {
            Err(arena_core::Error::Timeout { after, .. }) => return Err((PushFailure::Timeout, format!("ran out of its {}s budget", after.as_secs()))),
            Err(e) => return Err((PushFailure::Other, e.to_string())),
            Ok(out) => match push::classify_push(out.code, &out.stderr, false) {
                Ok(note) => {
                    notes.extend(note);
                    let s = arena_core::pull::parse_rsync_stats(&out.stdout);
                    stats = match (stats, s) {
                        (Some(a), Some(b)) => Some(a.add(b)),
                        (a, b) => a.or(b),
                    };
                }
                Err(f) => {
                    let line = out.stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
                    return Err((f, format!("{} — {line}", out.exit_label())));
                }
            },
        }
    }
    Ok(((!notes.is_empty()).then(|| notes.join("; ")), stats))
}

/// The snap tier: every label `plan_snap` names, each two passes, state saved after each.
#[allow(clippy::too_many_arguments)]
async fn sync_snap(
    cfg: &Config,
    paths: &ArchivePaths,
    dir: &Path,
    o: &SyncOpts,
    allowed: &[String],
    transport: &PushTransport,
    settings: &ArchiveSettings,
    host: &ArchiveHost,
    rsync: &str,
    clock: &dyn Fn() -> u64,
) -> Result<usize> {
    let Some(_lock) = try_lock(&paths.sync_lock(Tier::Snap))? else {
        println!("= snap: another `archive sync` is pushing it — skipped");
        return Ok(0);
    };
    let start = start_days(cfg)?;
    let state_path = paths.sync_state(Tier::Snap);
    let mut st: SnapState = state::load_snap(&state_path).map_err(anyhow::Error::msg)?;
    let local = push::local_labels(dir).with_context(|| format!("reading {}", dir.display()))?;
    let todo = push::plan_snap(&local, &o.labels, &st, start);
    if todo.is_empty() {
        println!("= snap: nothing to push (every label is final)");
        return Ok(0);
    }
    let run_started = clock();
    let mut failed = 0;
    for label in &todo {
        let src = dir.join(label);
        if !src.is_dir() {
            println!("✗ snap {label}: no such label in {}", dir.display());
            failed += 1;
            continue;
        }
        let present = push::subdirs(&src).with_context(|| format!("reading {}", src.display()))?;
        let (pods, _) = push::split_pod_dirs(&present, allowed);
        let link = push::link_dest_for(label, &st);
        let jobs: Vec<PushJob> = [SnapPass::Files, SnapPass::Git]
            .into_iter()
            .map(|pass| PushJob { tier: Tier::Snap, pass, label: Some(label.clone()), link_dest: link.clone(), pods: pods.clone(), src: src.display().to_string() })
            .collect();
        if o.dry_run {
            for j in &jobs {
                println!("[snap {label}{}] {}", if j.pass == SnapPass::Git { " .git" } else { "" }, arena_core::pull::display_argv(rsync, &push::push_args(j, transport, settings)));
            }
            continue;
        }
        let started = clock();
        let outcome = run_push(rsync, &jobs, transport, settings, settings.snap_timeout).await;
        let finished = clock();
        match outcome {
            Ok((note, stats)) => {
                let s = stats.unwrap_or_default();
                st.labels.insert(label.clone(), LabelRecord { ok_started: started, ok_finished: finished.max(1), files: s.files_sent, bytes: s.bytes_sent });
                let sealed = if push::sealed(label, &st, start) { ", final" } else { "" };
                let link = link.as_deref().map(|l| format!(", linked to {l}")).unwrap_or_default();
                println!("✓ snap {label} ({}{link}{sealed}){}", s.summary(), note.map(|n| format!(" — note: {n}")).unwrap_or_default());
            }
            Err((class, msg)) => {
                failed += 1;
                st.last_error = Some(ErrRecord { at: finished, label: Some(label.clone()), class, message: msg.clone() });
                let hint = push::failure_hint(class, Tier::Snap, host.snap_gb);
                let hint = if hint.is_empty() { String::new() } else { format!("{hint}; ") };
                println!("✗ snap {label}: {class} — {hint}{msg}");
            }
        }
        state::save(&state_path, &st).context("saving sync-snap.json")?;
    }
    if !o.dry_run {
        st.last_run = Some(RunRecord { started: run_started, finished: clock(), ok: failed == 0 });
        state::save(&state_path, &st).context("saving sync-snap.json")?;
    }
    Ok(failed)
}

/// The big tier: one push of `<dir>/big/`.
#[allow(clippy::too_many_arguments)]
async fn sync_big(
    paths: &ArchivePaths,
    dir: &Path,
    o: &SyncOpts,
    allowed: &[String],
    transport: &PushTransport,
    settings: &ArchiveSettings,
    host: &ArchiveHost,
    rsync: &str,
    clock: &dyn Fn() -> u64,
) -> Result<usize> {
    let Some(_lock) = try_lock(&paths.sync_lock(Tier::Big))? else {
        println!("= big: another `archive sync` is pushing it — skipped");
        return Ok(0);
    };
    let src = dir.join(arena_core::restore::BIG);
    if !src.is_dir() {
        println!("= big: nothing to push yet (no {})", src.display());
        return Ok(0);
    }
    let present = push::subdirs(&src).with_context(|| format!("reading {}", src.display()))?;
    let (pods, _) = push::split_pod_dirs(&present, allowed);
    let job = PushJob { tier: Tier::Big, pass: SnapPass::Files, label: None, link_dest: None, pods, src: src.display().to_string() };
    if o.dry_run {
        println!("[big] {}", arena_core::pull::display_argv(rsync, &push::push_args(&job, transport, settings)));
        return Ok(0);
    }
    let state_path = paths.sync_state(Tier::Big);
    let mut st: BigState = state::load_big(&state_path).map_err(anyhow::Error::msg)?;
    let started = clock();
    let outcome = run_push(rsync, std::slice::from_ref(&job), transport, settings, settings.big_timeout).await;
    let finished = clock();
    let run = RunRecord { started, finished, ok: outcome.is_ok() };
    st.last_run = Some(run);
    let failed = match outcome {
        Ok((note, stats)) => {
            st.last_ok = Some(run);
            println!("✓ big ({}){}", stats.unwrap_or_default().summary(), note.map(|n| format!(" — note: {n}")).unwrap_or_default());
            0
        }
        Err((class, msg)) => {
            st.last_error = Some(ErrRecord { at: finished, label: None, class, message: msg.clone() });
            let hint = push::failure_hint(class, Tier::Big, host.big_gb);
            println!("✗ big: {class} — {}{msg}", if hint.is_empty() { String::new() } else { format!("{hint}; ") });
            1
        }
    };
    state::save(&state_path, &st).context("saving sync-big.json")?;
    Ok(failed)
}

// =========================================================================================
// resize, access, ssh-config, ssh
// =========================================================================================

/// `archive resize` (grow only, online).
pub(crate) async fn archive_resize(env: &Env<'_>, cloud: &dyn ArchiveCloud, tier: Tier, to_gb: u32, dry_run: bool) -> Result<()> {
    let mut host = env.host_up()?;
    let inv = cloud.inventory(Some(&env.names.prefix)).await.context("listing the archive's Hetzner resources — nothing was changed")?;
    let plan = plan_resize(&inv, &env.names, tier, to_gb).map_err(anyhow::Error::msg)?;
    let delta = match inv.server(&env.names) {
        Some(s) => cloud.prices(&s.server_type, &s.location).await.ok().map(|p| f64::from(plan.to_gb - plan.from_gb) * p.volume_gb_month),
        None => None,
    };
    println!(
        "Grow {} from {} GB to {} GB (online: the API resize, then resize2fs on the server){}",
        env.names.volume(tier),
        plan.from_gb,
        plan.to_gb,
        delta.map(|d| format!(" — +{}", eur(d))).unwrap_or_default()
    );
    if dry_run {
        println!("Preview only — run without --dry-run to resize.");
        return Ok(());
    }
    if !confirm(env.yes, "Resize it? (a volume can't shrink back)")? {
        println!("aborted.");
        return Ok(());
    }
    let a = cloud.resize(plan.volume_id, plan.to_gb).await.context("resizing the volume")?;
    cloud.wait_action(a, ACTION_LIMIT).await.context("waiting for the resize")?;
    let out = env
        .remote
        .exec(&env.admin(&host.ipv4, env.pinned()), &render::resize_command(&plan.linux_device, tier), Some(RESIZE_TIMEOUT))
        .await
        .context("growing the filesystem")?;
    if !out.success {
        bail!("the volume is {} GB but resize2fs failed ({}): {} — re-run `arena archive resize` or `arena archive ssh -- resize2fs {}`", plan.to_gb, out.exit_label(), out.stderr.trim(), plan.linux_device);
    }
    let bytes: Option<u64> = out.stdout.trim().lines().last().and_then(|l| l.trim().parse().ok());
    match tier {
        Tier::Snap => host.snap_gb = plan.to_gb,
        Tier::Big => host.big_gb = plan.to_gb,
    }
    state::save(&env.paths.host_file(), &host).context("writing archive.json")?;
    println!("✓ {} is {} GB; the filesystem now holds {}", env.names.volume(tier), plan.to_gb, bytes.map_or("?".into(), arena_core::pull::human_bytes));
    Ok(())
}

/// What `access on` says before it opens the archive.
pub(crate) const PRIVACY_STATEMENT: &str = "With access ON, everyone holding the cohort key — every participant — can read EVERY pod's \
     backups on the archive (sftp, read-only), not just their own; pods can too, with the read key. That is the same \
     exposure as today (everyone is root on every pod), and the README on the archive says so. Secret files (rc files, \
     histories, .env, keys, .git/config, .claude*) and staff boxes are never copied there.";

/// `archive access on|off` (persisted in `archive.json`; `up` re-applies it).
pub(crate) async fn archive_access(env: &Env<'_>, on: bool) -> Result<()> {
    let mut host = env.host_up()?;
    let keys = backups_keys(env, on).map_err(anyhow::Error::msg)?;
    if on {
        println!("{PRIVACY_STATEMENT}\n");
        if !confirm(env.yes, &format!("Let the cohort read {}?", env.names.server()))? {
            println!("aborted — access unchanged ({}).", if host.access_on { "on" } else { "off" });
            return Ok(());
        }
    }
    let out = env
        .remote
        .exec(&env.admin(&host.ipv4, env.pinned()), &render::access_command(&keys, on), Some(ACCESS_TIMEOUT))
        .await
        .context("updating the read user's keys")?;
    if !out.success {
        bail!("updating the read user's keys failed ({}): {}", out.exit_label(), out.stderr.trim());
    }
    host.access_on = on;
    state::save(&env.paths.host_file(), &host).context("writing archive.json")?;
    if on {
        println!("✓ access ON: laptops (cohort key) and pods (`arena pods copy-keys`) can read via `sftp {}`", env.names.alias());
    } else {
        println!("✓ access OFF: no participant key is authorized, open sessions were ended (pushes and admin unaffected)");
    }
    Ok(())
}

/// The host participants and pods connect to: `ARCHIVE_HOST`, else the pinned IP.
fn public_host(cfg: &Config, host: &ArchiveHost) -> String {
    match cfg.get("ARCHIVE_HOST").map(str::trim).filter(|h| !h.is_empty()) {
        // Validated like `ArchiveSettings` does (it goes into ssh configs and known_hosts).
        Some(h) if !h.starts_with('-') && h.chars().all(|c| c.is_ascii_alphanumeric() || ".-:".contains(c)) => h.to_string(),
        _ => host.ipv4.clone(),
    }
}

/// The laptop block for this config's archive.
fn laptop_block(cfg: &Config, host: &ArchiveHost) -> String {
    let names = ArchiveNames { prefix: host.prefix.clone() };
    // The identity as the PARTICIPANT names it (the configured value, verbatim — as `ssh-config`).
    let identity = cfg.get("SHARED_SSH_KEY_PATH").unwrap_or("~/.ssh/shared_infra_key");
    render::render_laptop_block(&names.alias(), &public_host(cfg, host), identity, host.host_key_fingerprint.as_deref())
}

/// The laptop block `arena ssh-config` appends — only when this user has an `archive.json` with
/// a server.
pub(crate) fn laptop_block_for(cfg: &Config) -> Option<String> {
    let paths = ArchivePaths::for_config(cfg).ok()?;
    let host = state::load_host(&paths.host_file()).ok()??;
    host.is_up().then(|| laptop_block(cfg, &host))
}

/// `archive ssh-config [--pod]`.
pub(crate) fn archive_ssh_config(env: &Env<'_>, pod: bool) -> Result<String> {
    let host = env.host_up()?;
    Ok(if pod { render::render_pod_block(&env.names.alias(), &public_host(env.cfg, &host)) } else { laptop_block(env.cfg, &host) })
}

/// `archive ssh [-- CMD…]`: the operator's root shell, pinned, admin key only.
pub(crate) fn archive_ssh(env: &Env<'_>, cmd: &[String]) -> Result<()> {
    let host = env.host_up()?;
    if !state::readable(&env.paths.known_hosts()) {
        bail!("no pinned host key ({}) — run `arena archive up`", env.paths.known_hosts().display());
    }
    let t = env.admin(&host.ipv4, env.pinned());
    let mut c = std::process::Command::new("ssh");
    if cmd.is_empty() {
        c.arg("-t");
    }
    c.args(t.ssh_args());
    if !cmd.is_empty() {
        c.arg(cmd.join(" "));
    }
    let status = c.status().context("running ssh")?;
    if !status.success() {
        bail!("ssh to the archive: {}", arena_core::ssh::exit_label(status.code()));
    }
    Ok(())
}

// =========================================================================================
// down
// =========================================================================================

/// `archive down`'s flags.
#[derive(Debug, Clone, Default)]
pub(crate) struct DownOpts {
    pub delete_volumes: bool,
    pub confirm: Option<String>,
    pub dry_run: bool,
}

/// `archive down` (§6.2): server only (volumes kept, protected), or with `--delete-volumes
/// --confirm <prefix>-archive` everything. The typed name is the confirmation; `-y` never is.
pub(crate) async fn archive_down(env: &Env<'_>, cloud: &dyn ArchiveCloud, o: DownOpts) -> Result<()> {
    let names = &env.names;
    let inv = cloud.inventory(Some(&names.prefix)).await.context("listing the archive's Hetzner resources — nothing was changed")?;
    let steps = plan_down(&inv, names, o.delete_volumes, o.confirm.as_deref()).map_err(anyhow::Error::msg)?;
    if steps.is_empty() {
        println!("Nothing to take down: no {} (its volumes, if any, are kept — `--delete-volumes --confirm {}` deletes them).", names.server(), names.server());
        return Ok(());
    }
    let host = env.host()?;
    let ours = inv.owned_by(names);
    if o.delete_volumes {
        println!("About to DELETE the archive's volumes — every backup on them, with no other copy on Hetzner:");
        for v in &ours.volumes {
            println!("  {} — {} GB", v.name, v.size_gb);
        }
        // What's on them, if the server can still be asked (the read-only probe).
        if let Some(h) = host.as_ref().filter(|h| h.is_up() && state::readable(&env.paths.known_hosts())) {
            let cmd = render::probe_command(&h.snap_device, &h.big_device);
            match env.remote.exec(&env.admin(&h.ipv4, env.pinned()), &cmd, Some(PROBE_TIMEOUT)).await.ok().and_then(|o| report::parse_probe(&o.stdout).ok()) {
                Some(p) => {
                    for t in Tier::ALL {
                        let tp = p.tier(t);
                        let used = tp.bytes_used.map_or("?".into(), arena_core::pull::human_bytes);
                        println!("  {t}: {used} used{}", if tp.mounted { "" } else { " (not mounted now)" });
                    }
                }
                None => println!("  (couldn't ask the server how much is used)"),
            }
        }
        let snap = state::load_snap(&env.paths.sync_state(Tier::Snap)).unwrap_or_default();
        let big = state::load_big(&env.paths.sync_state(Tier::Big)).unwrap_or_default();
        let at = |t: Option<u64>| t.map_or("never".to_string(), |t| arena_core::snapshot::rfc3339(t));
        println!("  last OK push: snap {}, big {}", at(snap.last_ok()), at(big.last_ok.map(|r| r.finished)));
        if let Ok(start) = start_days(env.cfg) {
            let local = push::local_labels(Path::new(&super::local_backup_dir(env.cfg))).unwrap_or_default();
            let open: Vec<&String> = local.iter().filter(|l| !push::sealed(l, &snap, start)).collect();
            if !open.is_empty() {
                println!("  labels never finalised on the archive: {}", open.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "));
            }
        }
    }
    println!("Plan:");
    for s in &steps {
        println!("  • {}", s.describe(names));
    }
    let kept_gb: u32 = ours.volumes.iter().map(|v| v.size_gb).sum();
    if o.dry_run {
        println!("Preview only — run without --dry-run to apply.");
        return Ok(());
    }
    // The typed `--confirm` is the confirmation for a volume deletion; the server alone asks.
    if !o.delete_volumes && !confirm(env.yes, &format!("Delete the server {} (its volumes are kept)?", names.server()))? {
        println!("aborted.");
        return Ok(());
    }
    // No push of this user's may start (or be mid-way) while the server goes. (No state dir: no
    // sync of this user's can run at all.)
    let mut held = Vec::new();
    if env.paths.dir.is_dir() {
        for t in Tier::ALL {
            match try_lock(&env.paths.sync_lock(t))? {
                Some(l) => held.push(l),
                None => bail!("an `arena archive sync` ({t}) is running — wait for it (or remove the cron lines) and re-run; nothing was changed"),
            }
        }
    }
    for step in &steps {
        run_down_step(env, cloud, step, host.as_ref()).await.with_context(|| format!("archive down stopped at: {}", step.describe(names)))?;
    }
    if !o.delete_volumes {
        if let Some(mut h) = host {
            h.server_id = 0;
            h.ipv4 = String::new();
            state::save(&env.paths.host_file(), &h).context("writing archive.json")?;
        }
        let price = cloud.prices("cx23", &ours.volumes.first().map(|v| v.location.clone()).unwrap_or_else(|| "nbg1".into())).await.ok();
        println!(
            "Done. The volumes are kept ({kept_gb} GB{}, still delete-protected): `arena archive up` builds a new server for them; \
             `arena archive down --delete-volumes --confirm {}` deletes them.",
            price.map(|p| format!(", ≈ {}", eur(p.monthly(false, kept_gb)))).unwrap_or_default(),
            names.server()
        );
    } else {
        println!("Done. The archive is gone; the keys stay in {} (delete them by hand if you're sure).", env.paths.dir.display());
    }
    Ok(())
}

async fn run_down_step(env: &Env<'_>, cloud: &dyn ArchiveCloud, step: &DownStep, host: Option<&ArchiveHost>) -> Result<()> {
    match step {
        DownStep::Quiesce => match host.filter(|h| h.is_up()) {
            Some(h) if state::readable(&env.paths.known_hosts()) => {
                match env.remote.exec(&env.admin(&h.ipv4, env.pinned()), &render::quiesce_command(), Some(QUIESCE_TIMEOUT)).await {
                    Ok(o) if o.success => println!("✓ sessions ended, volumes unmounted"),
                    Ok(o) => println!("  (quiesce: {} — going on)", o.stderr.trim()),
                    Err(e) => println!("  (quiesce: {} — going on)", describe_error(&e)),
                }
            }
            _ => println!("  (no archive.json/pin for this user: not quiescing — going on)"),
        },
        DownStep::UnprotectServer => {
            let s = cloud.inventory(Some(&env.names.prefix)).await?.server(&env.names).cloned().context("the server is gone")?;
            let a = cloud.protect_server(s.id, false).await?;
            cloud.wait_action(a, ACTION_LIMIT).await?;
        }
        DownStep::DetachVolume(t, id) => {
            let a = cloud.detach(*id).await?;
            cloud.wait_action(a, ACTION_LIMIT).await?;
            println!("✓ {} detached", env.names.volume(*t));
        }
        DownStep::DeleteServer(id) => {
            cloud.delete_server(*id).await?;
            println!("✓ server {} deleted", env.names.server());
        }
        DownStep::UnprotectVolume(_, id) => {
            let a = cloud.protect_volume(*id, false).await?;
            cloud.wait_action(a, ACTION_LIMIT).await?;
        }
        DownStep::DeleteVolume(t, id) => {
            cloud.delete_volume(*id).await?;
            println!("✓ volume {} deleted", env.names.volume(*t));
        }
        DownStep::DeleteSshKey(id) => {
            cloud.delete_ssh_key(*id).await?;
            println!("✓ SSH-key object {} deleted", env.names.ssh_key());
        }
        DownStep::RemoveLocalState => {
            for f in [env.paths.host_file(), env.paths.sync_state(Tier::Snap), env.paths.sync_state(Tier::Big)] {
                match std::fs::remove_file(&f) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e).with_context(|| format!("removing {}", f.display())),
                }
            }
            println!("✓ archive.json and the sync state removed");
        }
    }
    Ok(())
}

// =========================================================================================
// pods: the archive block, its pinned key and the read key (copy-keys)
// =========================================================================================

/// What `copy-keys` puts on a pod when this user has an archive: the `Host archive-<prefix>` block
/// (inside the fleet's managed `~/.ssh/config` block), the pinned host key(s) and the read key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PodAccess {
    pub block: String,
    pub known_hosts: String,
    pub read_key: PathBuf,
}

/// `Some` when `archive.json` (with a server), the read key and a pinned host key are all here.
pub(crate) fn pod_access(cfg: &Config) -> Option<PodAccess> {
    let paths = ArchivePaths::for_config(cfg).ok()?;
    let host = state::load_host(&paths.host_file()).ok()??;
    if !host.is_up() || !state::readable(&paths.read_key()) {
        return None;
    }
    let keys = render::parse_known_hosts(&std::fs::read_to_string(paths.known_hosts()).ok()?);
    if keys.is_empty() {
        return None;
    }
    let names = ArchiveNames { prefix: host.prefix.clone() };
    let public = public_host(cfg, &host);
    let named = (public != host.ipv4).then_some(public.as_str());
    Some(PodAccess {
        block: render::render_pod_block(&names.alias(), &public),
        known_hosts: render::render_known_hosts(&host.ipv4, named, &keys),
        read_key: paths.read_key(),
    })
}

/// The shell lines that write the pod's pinned archive key (a here-doc, after the fleet block).
pub(crate) fn pod_known_hosts_command(a: &PodAccess) -> String {
    format!(
        "\numask 077; mkdir -p \"$HOME/.ssh\" && cat > \"$HOME/{kh}\" <<'ARENAARCHIVEKH'\n{}ARENAARCHIVEKH\nchmod 600 \"$HOME/{kh}\"",
        a.known_hosts,
        kh = render::POD_KNOWN_HOSTS
    )
}

/// Copy the read key onto a pod — by `scp` (`Remote::copy`), never inside a command, so the key
/// never appears in an argv or a dry run — and make it 0600.
pub(crate) async fn install_read_key(remote: &dyn Remote, t: &SshTarget, a: &PodAccess, timeout: Duration) -> std::result::Result<(), String> {
    let o = remote.copy(t, &a.read_key.display().to_string(), render::POD_KEY, Some(timeout)).await.map_err(|e| describe_error(&e))?;
    if !o.success {
        return Err(format!("copying the archive read key: {}", o.stderr.trim()));
    }
    let o = remote.exec(t, &format!("chmod 600 \"$HOME/{}\"", render::POD_KEY), Some(timeout)).await.map_err(|e| describe_error(&e))?;
    if !o.success {
        return Err(format!("chmod of the archive read key: {}", o.stderr.trim()));
    }
    Ok(())
}

// =========================================================================================
// cron and config check
// =========================================================================================

/// Why `cron install --archive` can't install its lines for this user, if it can't (§7.1).
pub(crate) fn cron_refusals(cfg: &Config, start_date: bool) -> Vec<String> {
    let mut out = Vec::new();
    let dir = super::local_backup_dir(cfg);
    if !Path::new(&dir).is_absolute() {
        out.push(format!("LOCAL_BACKUP_DIR must be an absolute path for the archive's cron lines (is `{dir}`; cron runs in $HOME)"));
    }
    match ArchivePaths::for_config(cfg) {
        Err(e) => out.push(e),
        Ok(p) => {
            for (f, what) in [(p.host_file(), "archive.json"), (p.push_key(), "the push key"), (p.known_hosts(), "the pinned host key")] {
                if !state::readable(&f) {
                    out.push(format!(
                        "{what} ({}) is missing or unreadable for this user — run `arena archive up` as the user whose crontab this is \
                         (prod: root, with ARENA_STATE_DIR in config)",
                        f.display()
                    ));
                }
            }
        }
    }
    if !start_date && cfg.get("ARENA_START_DATE").and_then(arena_core::schedule::parse_ymd).is_none() {
        out.push("the snap push needs ARENA_START_DATE: pass --start-date YYYY-MM-DD or set it in config".into());
    }
    out
}

/// `config check`'s Archive section.
pub(crate) fn config_check_lines(cfg: &Config) -> Vec<String> {
    let mut out = Vec::new();
    let set = |k: &str| cfg.get(k).is_some_and(|v| !v.trim().is_empty());
    match archive::archive_token(cfg) {
        Some((_, TokenSource::Archive)) => out.push(format!("  ✓ {:<28} (set)", "ARCHIVE_HETZNER_API_KEY")),
        Some((_, TokenSource::Fleet)) => {
            out.push(format!("  · {:<28} (missing — falls back to HETZNER_API_KEY)", "ARCHIVE_HETZNER_API_KEY"));
            out.push(format!("  {}", archive::FLEET_TOKEN_WARNING));
        }
        None => out.push(format!("  · {:<28} (missing; no HETZNER_API_KEY either — `arena archive` can't reach Hetzner)", "ARCHIVE_HETZNER_API_KEY")),
    }
    for k in [
        "ARCHIVE_LOCATION",
        "ARCHIVE_SERVER_TYPES",
        "ARCHIVE_SNAP_GB",
        "ARCHIVE_BIG_GB",
        "ARCHIVE_KEEP_UNTIL",
        "ARCHIVE_HOST",
        "ARCHIVE_BWLIMIT",
        "ARCHIVE_SNAP_TIMEOUT_SECS",
        "ARCHIVE_BIG_TIMEOUT_SECS",
        "ARCHIVE_EXTRA_EXCLUDES",
    ] {
        let shown = if set(k) { cfg.get(k).unwrap_or("").to_string() } else { "(default)".into() };
        out.push(format!("  {} {k:<28} {shown}", if set(k) { "✓" } else { "·" }));
    }
    match ArchiveSettings::from_config(cfg) {
        Ok(s) => out.push(format!(
            "  ✓ settings                     {} in {}, {}+{} GB at creation, bwlimit {}, retention: {}",
            s.server_types.join("/"),
            s.location,
            s.snap_gb,
            s.big_gb,
            s.bwlimit.as_deref().unwrap_or("none"),
            s.keep_until_text()
        )),
        Err(e) => out.push(format!("  ✗ settings                     {e}")),
    }
    match ArchivePaths::for_config(cfg) {
        Err(e) => out.push(format!("  ✗ state dir                    {e}")),
        Ok(p) => {
            out.push(format!("  · state dir                    {}", p.dir.display()));
            for (f, what) in [(p.host_file(), "archive.json"), (p.push_key(), "push key"), (p.admin_key(), "admin key"), (p.known_hosts(), "pinned host key")] {
                let (mark, note) = if state::readable(&f) {
                    ("✓", "readable")
                } else if f.exists() {
                    ("✗", "NOT readable by this user")
                } else {
                    ("·", "absent (`arena archive up`)")
                };
                out.push(format!("  {mark} {what:<28} {note}"));
            }
        }
    }
    let dir = super::local_backup_dir(cfg);
    if Path::new(&dir).is_absolute() {
        out.push(format!("  ✓ LOCAL_BACKUP_DIR absolute     {dir}"));
    } else {
        out.push(format!("  · LOCAL_BACKUP_DIR absolute     no (`{dir}`) — `cron install --archive` needs an absolute path"));
    }
    out
}

/// `arena archive …` from `main`: the real Hetzner client (only for the commands that need
/// one), the real ssh, the real rsync and clock.
pub(crate) async fn handle_archive(cmd: ArchiveCmd, cfg: &Config, remote: &dyn Remote, yes: bool) -> Result<()> {
    if let ArchiveCmd::Sync { tier, labels, dir, dry_run } = cmd {
        // The cron path: no token, no Hetzner client built at all (S13).
        let paths = ArchivePaths::for_config(cfg).map_err(anyhow::Error::msg)?;
        return archive_sync(cfg, &paths, SyncOpts { tier, labels, dir, dry_run }, super::RSYNC, &arena_core::snapshot::unix_now).await;
    }
    let env = Env::new(cfg, remote, yes)?;
    let token = archive::archive_token(cfg);
    let cloud = token.as_ref().map(|(t, _)| arena_core::archive::HetznerArchiveCloud::new(t.clone()));
    let need = || -> Result<(&dyn ArchiveCloud, TokenSource)> {
        match (&cloud, &token) {
            (Some(c), Some((_, src))) => Ok((c as &dyn ArchiveCloud, *src)),
            _ => bail!("set ARCHIVE_HETZNER_API_KEY (recommended: its own Hetzner project) or HETZNER_API_KEY"),
        }
    };
    match cmd {
        ArchiveCmd::Up { snap_gb, big_gb, accept_new_host_key, dry_run } => {
            let (c, src) = need()?;
            archive_up(&env, c, src, UpOpts { snap_gb, big_gb, accept_new_host_key, dry_run }).await
        }
        ArchiveCmd::Status { json } => {
            archive_status(&env, cloud.as_ref().map(|c| c as &dyn ArchiveCloud), token.map(|t| t.1), json, arena_core::snapshot::unix_now()).await
        }
        ArchiveCmd::Resize { snap_gb, big_gb, dry_run } => {
            let (c, _) = need()?;
            let (tier, gb) = match (snap_gb, big_gb) {
                (Some(g), None) => (Tier::Snap, g),
                (None, Some(g)) => (Tier::Big, g),
                _ => bail!("give one of --snap-gb / --big-gb"),
            };
            archive_resize(&env, c, tier, gb, dry_run).await
        }
        ArchiveCmd::Access { state } => archive_access(&env, state == "on").await,
        ArchiveCmd::SshConfig { pod } => {
            print!("{}", archive_ssh_config(&env, pod)?);
            Ok(())
        }
        ArchiveCmd::Ssh { cmd } => archive_ssh(&env, &cmd),
        ArchiveCmd::Down { delete_volumes, confirm, dry_run } => {
            let (c, _) = need()?;
            archive_down(&env, c, DownOpts { delete_volumes, confirm, dry_run }).await
        }
        ArchiveCmd::Sync { .. } => unreachable!("handled above"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use arena_core::archive::cloud::{CloudCall, FakeArchiveCloud};
    use arena_core::remote::{FakeRemote, RemoteCall};
    use arena_core::ssh::SshOutput;

    use super::*;

    /// The archive VM, scripted by what a command contains (whatever the host), recording every
    /// call WITH its target — so each call's key and host-key policy can be checked (S3).
    #[derive(Default)]
    struct VmRemote {
        rules: Mutex<Vec<(String, SshOutput)>>,
        calls: Mutex<Vec<(SshTarget, String)>>,
        /// The server's key changed: every pinned connection fails host-key verification until
        /// a trust-on-first-use connection re-pins it.
        key_changed: std::sync::atomic::AtomicBool,
    }

    fn out(code: i32, stdout: &str, stderr: &str) -> SshOutput {
        SshOutput { success: code == 0, code: Some(code), stdout: stdout.into(), stderr: stderr.into() }
    }

    impl VmRemote {
        /// Commands containing `needle` answer `o` (the first matching rule wins; later rules
        /// for the same needle are put in front, so a test can change an answer).
        fn on(&self, needle: &str, o: SshOutput) {
            self.rules.lock().unwrap().insert(0, (needle.to_string(), o));
        }
        fn calls(&self) -> Vec<(SshTarget, String)> {
            self.calls.lock().unwrap().clone()
        }
        fn cmds(&self) -> Vec<String> {
            self.calls().into_iter().map(|(_, c)| c).collect()
        }
        fn clear(&self) {
            self.calls.lock().unwrap().clear();
        }
        fn answer(&self, cmd: &str) -> SshOutput {
            self.rules.lock().unwrap().iter().find(|(n, _)| cmd.contains(n.as_str())).map(|(_, o)| o.clone()).unwrap_or_else(|| out(0, "", ""))
        }
    }

    #[async_trait::async_trait]
    impl Remote for VmRemote {
        async fn exec(&self, t: &SshTarget, cmd: &str, _timeout: Option<Duration>) -> arena_core::Result<SshOutput> {
            self.calls.lock().unwrap().push((t.clone(), cmd.to_string()));
            if self.key_changed.load(Ordering::SeqCst) {
                match t.host_keys {
                    HostKeyPolicy::Pinned { .. } => {
                        return Ok(out(255, "", "@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @\nHost key verification failed."))
                    }
                    _ => self.key_changed.store(false, Ordering::SeqCst),
                }
            }
            Ok(self.answer(cmd))
        }
        async fn copy(&self, t: &SshTarget, local: &str, remote: &str, _timeout: Option<Duration>) -> arena_core::Result<SshOutput> {
            self.calls.lock().unwrap().push((t.clone(), format!("scp {local} {remote}")));
            Ok(out(0, "", ""))
        }
        async fn copy_recursive(&self, t: &SshTarget, local: &str, remote: &str, timeout: Option<Duration>) -> arena_core::Result<SshOutput> {
            self.copy(t, local, remote, timeout).await
        }
    }

    const SETUP_OK: &str = "ok_os=1\nok_rsync=1\nrrsync=/usr/bin/rrsync\nok_sshd=1\nsetup_version=1\nsetup_done=1\n";

    /// A temp dir with a cohort key, a config pointing everything into it, a fake project and a
    /// scripted VM.
    struct World {
        dir: PathBuf,
        cfg: Config,
        remote: VmRemote,
        cloud: FakeArchiveCloud,
    }

    impl Drop for World {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn have(program: &str) -> bool {
        std::process::Command::new(program).arg("-V").output().is_ok()
    }

    fn world(tag: &str) -> World {
        world_with(tag, "")
    }

    fn world_with(tag: &str, extra: &str) -> World {
        let dir = std::env::temp_dir().join(format!("arena-archive-cli-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("backup")).unwrap();
        state::ensure_keypair(&dir.join("cohort_key"), "devtest").unwrap();
        let d = dir.display();
        let cfg = Config::parse(&format!(
            "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(alpha bravo @staff-box)\nARENA_STATE_DIR={d}/state\n\
             LOCAL_BACKUP_DIR={d}/backup\nARENA_START_DATE=2026-10-01\nSHARED_SSH_KEY_PATH={d}/cohort_key\n\
             ARCHIVE_SNAP_GB=10\nARCHIVE_BIG_GB=10\nHF_TOKEN=hf_x\n{extra}"
        ));
        let remote = VmRemote::default();
        remote.on("arena-archive-setup", out(0, SETUP_OK, ""));
        World { dir, cfg, remote, cloud: FakeArchiveCloud::new() }
    }

    impl World {
        fn env(&self) -> Env<'_> {
            Env::new(&self.cfg, &self.remote, true).unwrap()
        }
        async fn up(&self) -> Result<()> {
            self.up_with(UpOpts::default()).await
        }
        async fn up_with(&self, o: UpOpts) -> Result<()> {
            archive_up(&self.env(), &self.cloud, TokenSource::Archive, o).await
        }
        fn host(&self) -> ArchiveHost {
            state::load_host(&self.env().paths.host_file()).unwrap().expect("archive.json")
        }
    }

    fn mutations(c: &FakeArchiveCloud) -> Vec<CloudCall> {
        c.mutations()
    }

    /// §14 fake: `up` from scratch — key object, server (exactly the admin key), protection, two
    /// volumes each made attached and protected; a fresh inventory before every create; the
    /// first SSH trusts a new key into an emptied pin, setup runs once on the pinned key; the
    /// state is written with participant access OFF.
    #[tokio::test]
    async fn up_from_scratch_creates_in_order_pins_runs_setup_once_and_writes_state() {
        let w = world("scratch");
        w.up().await.unwrap();
        let calls = w.cloud.calls();
        let m = mutations(&w.cloud);
        let kinds: Vec<&str> = m
            .iter()
            .map(|c| match c {
                CloudCall::CreateSshKey(_) => "key",
                CloudCall::CreateServer(_) => "server",
                CloudCall::ProtectServer(_, true) => "protect server",
                CloudCall::CreateVolume(v) if v["labels"]["arena-archive-tier"] == "snap" => "snap",
                CloudCall::CreateVolume(_) => "big",
                CloudCall::ProtectVolume(_, true) => "protect volume",
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(kinds, ["key", "server", "protect server", "snap", "protect volume", "big", "protect volume"]);
        for (i, c) in calls.iter().enumerate() {
            if matches!(c, CloudCall::CreateSshKey(_) | CloudCall::CreateServer(_) | CloudCall::CreateVolume(_)) {
                assert!(matches!(calls[i - 1], CloudCall::Inventory(Some(ref p)) if p == "devtest"), "a fresh inventory before {c:?}: {calls:?}");
            }
        }
        let key_id = w.cloud.state().ssh_keys[0].id;
        let server = calls.iter().find_map(|c| if let CloudCall::CreateServer(b) = c { Some(b.clone()) } else { None }).unwrap();
        assert_eq!(server["ssh_keys"], serde_json::json!([key_id]));
        assert!(server.get("user_data").is_none());
        let vols = w.cloud.state().volumes;
        assert!(vols.iter().all(|v| v.size_gb == 10 && v.delete_protected && v.format.as_deref() == Some("ext4")), "{vols:?}");

        let vm = w.remote.calls();
        assert_eq!(vm.len(), 2, "{:?}", w.remote.cmds());
        assert_eq!(vm[0].1, READY_CMD);
        assert!(matches!(vm[0].0.host_keys, HostKeyPolicy::PinFirstUse { .. }), "a new server: trust on first use");
        assert!(vm[1].1.contains("arena-archive-setup"));
        assert!(matches!(vm[1].0.host_keys, HostKeyPolicy::Pinned { .. }));
        let h = w.host();
        assert!(!h.access_on, "participant access starts OFF");
        assert!(h.is_up() && h.snap_device.ends_with(&vols.iter().find(|v| v.tier() == Some(Tier::Snap)).unwrap().id.to_string()));
        assert_eq!((h.snap_gb, h.big_gb), (10, 10));
        // The keys exist, private and generated once.
        let paths = w.env().paths;
        for k in [paths.admin_key(), paths.push_key(), paths.read_key()] {
            assert!(k.exists() && ArchivePaths::public(&k).exists(), "{}", k.display());
        }
    }

    /// S3: every SSH arena makes to the archive (setup, probe, access, resize, quiesce) is pinned,
    /// as root with the ONE admin key — and every push goes as arena-push with the ONE push key.
    #[tokio::test]
    async fn every_archive_ssh_is_pinned_with_exactly_one_key() {
        let w = world("pinned");
        w.up().await.unwrap();
        let env = w.env();
        archive_access(&env, true).await.unwrap();
        archive_access(&env, false).await.unwrap();
        let _ = archive_status(&env, Some(&w.cloud), Some(TokenSource::Archive), true, 0).await;
        archive_resize(&env, &w.cloud, Tier::Big, 20, false).await.unwrap();
        archive_down(&env, &w.cloud, DownOpts::default()).await.unwrap();
        let calls = w.remote.calls();
        assert!(calls.len() >= 7, "{:?}", w.remote.cmds());
        for (i, (t, cmd)) in calls.iter().enumerate() {
            assert_eq!(t.key_paths, [env.paths.admin_key().display().to_string()], "{cmd}");
            assert_eq!(t.user, "root");
            let first = i == 0;
            match &t.host_keys {
                HostKeyPolicy::PinFirstUse { known_hosts } => assert!(first && known_hosts.ends_with("known_hosts"), "accept-new only on up's first connect: {cmd}"),
                HostKeyPolicy::Pinned { known_hosts } => assert_eq!(known_hosts, &env.paths.known_hosts().display().to_string()),
                HostKeyPolicy::Unchecked => panic!("unchecked: {cmd}"),
            }
            let argv = t.ssh_args().join(" ");
            assert!(argv.contains("IdentitiesOnly=yes") && argv.contains("GlobalKnownHostsFile=/dev/null"), "{argv}");
        }
        let push = push_transport(&env.paths, "192.0.2.1");
        assert_eq!(push.dest, "arena-push@192.0.2.1");
        assert!(push.rsh.contains("StrictHostKeyChecking=yes") && push.rsh.contains("IdentitiesOnly=yes"), "{}", push.rsh);
        assert_eq!(push.rsh.matches(" -i ").count(), 1);
        assert!(push.rsh.contains(&format!("-i {}", env.paths.push_key().display())));
    }

    /// Idempotent: a second `up` only lists, connects on the pinned key and re-runs setup.
    #[tokio::test]
    async fn up_again_only_lists_and_reuses_the_pin() {
        let w = world("again");
        w.up().await.unwrap();
        std::fs::write(w.env().paths.known_hosts(), "203.0.113.102 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHostHostHostHostHostHostHostHostHostHos5\n").unwrap();
        w.cloud.clear_calls();
        w.remote.clear();
        w.up().await.unwrap();
        assert!(mutations(&w.cloud).is_empty(), "{:?}", w.cloud.calls());
        let vm = w.remote.calls();
        assert!(matches!(vm[0].0.host_keys, HostKeyPolicy::Pinned { .. }), "the existing pin is kept");
        assert!(std::fs::read_to_string(w.env().paths.known_hosts()).unwrap().contains("HostHost"), "the pin wasn't reset");
        assert_eq!(w.remote.cmds().iter().filter(|c| c.contains("arena-archive-setup")).count(), 1);
    }

    #[tokio::test]
    async fn up_falls_back_to_the_next_server_type_only_on_placement_errors() {
        let w = world("fallback");
        w.cloud.unavailable(&["cx23"]);
        w.up().await.unwrap();
        let tried: Vec<String> = w
            .cloud
            .calls()
            .iter()
            .filter_map(|c| if let CloudCall::CreateServer(b) = c { b["server_type"].as_str().map(String::from) } else { None })
            .collect();
        assert_eq!(tried, ["cx23", "cax11"]);
        assert_eq!(w.cloud.state().servers[0].server_type, "cax11");
        // All unavailable: nothing billed, no volume made.
        let w = world("fallback-none");
        w.cloud.unavailable(&["cx23", "cax11"]);
        let e = format!("{:#}", w.up().await.unwrap_err());
        assert!(e.contains("nothing is billed"), "{e}");
        assert!(w.cloud.state().servers.is_empty() && w.cloud.state().volumes.is_empty());
    }

    /// A 500 on the second volume stops `up` naming the step; the re-run makes only what's missing.
    #[tokio::test]
    async fn a_failed_volume_create_stops_up_and_a_rerun_converges_without_duplicates() {
        let w = world("converge");
        w.cloud.fail_volume_create(2);
        let e = format!("{:#}", w.up().await.unwrap_err());
        assert!(e.contains("archive up stopped at: create the volume devtest-archive-big"), "{e}");
        assert_eq!(w.cloud.state().volumes.len(), 1);
        assert!(w.host_file_absent());
        w.cloud.clear_calls();
        w.up().await.unwrap();
        let creates: Vec<CloudCall> = mutations(&w.cloud).into_iter().filter(|c| matches!(c, CloudCall::CreateVolume(_) | CloudCall::CreateServer(_) | CloudCall::CreateSshKey(_))).collect();
        assert_eq!(creates.len(), 1, "{creates:?}");
        assert!(matches!(&creates[0], CloudCall::CreateVolume(v) if v["name"] == "devtest-archive-big"));
        assert_eq!(w.cloud.state().volumes.len(), 2);
    }

    impl World {
        fn host_file_absent(&self) -> bool {
            !self.env().paths.host_file().exists()
        }
    }

    /// S8: a failed listing → `up`, `down`, `resize` change nothing; `status` is UNKNOWN (non-zero).
    #[tokio::test]
    async fn a_failed_listing_changes_nothing_and_status_says_unknown() {
        let w = world("listfail");
        w.up().await.unwrap();
        w.cloud.clear_calls();
        w.cloud.fail_inventory(Some("hetzner archive list servers HTTP 503"));
        let env = w.env();
        assert!(w.up().await.is_err());
        assert!(archive_down(&env, &w.cloud, DownOpts { delete_volumes: true, confirm: Some("devtest-archive".into()), dry_run: false }).await.is_err());
        assert!(archive_resize(&env, &w.cloud, Tier::Snap, 20, false).await.is_err());
        assert!(mutations(&w.cloud).is_empty(), "{:?}", w.cloud.calls());
        let e = archive_status(&env, Some(&w.cloud), Some(TokenSource::Archive), false, 0).await.unwrap_err();
        assert!(e.to_string().contains("unknown"), "{e}");
    }

    /// Server lost, volumes kept: a new server in their location, both attached (never a new or
    /// formatted volume), the old pin dropped for the new key.
    #[tokio::test]
    async fn after_a_server_loss_up_reattaches_the_volumes_and_repins() {
        let w = world("loss");
        w.up().await.unwrap();
        let old = w.cloud.state().servers[0].id;
        w.cloud.protect_server(old, false).await.unwrap();
        w.cloud.delete_server(old).await.unwrap();
        std::fs::write(w.env().paths.known_hosts(), "203.0.113.1 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOldOldOldOldOldOldOldOldOldOldOldOldOl\n").unwrap();
        w.cloud.clear_calls();
        w.remote.clear();
        w.up().await.unwrap();
        let m = mutations(&w.cloud);
        assert!(m.iter().any(|c| matches!(c, CloudCall::CreateServer(_))));
        assert_eq!(m.iter().filter(|c| matches!(c, CloudCall::Attach(_, _))).count(), 2, "{m:?}");
        assert!(!m.iter().any(|c| matches!(c, CloudCall::CreateVolume(_))), "{m:?}");
        assert!(matches!(w.remote.calls()[0].0.host_keys, HostKeyPolicy::PinFirstUse { .. }));
        assert_eq!(std::fs::read_to_string(w.env().paths.known_hosts()).unwrap(), "", "the old pin was dropped (the fake ssh pins nothing)");
        let new = w.cloud.state().servers[0].clone();
        assert_eq!(w.host().server_id, new.id);
        assert!(w.cloud.state().volumes.iter().all(|v| v.server == Some(new.id)));
    }

    /// Without --accept-new-host-key, a changed host key stops `up` before setup; with it, the pin
    /// is reset and the new key trusted on first use, then setup runs on it.
    #[tokio::test]
    async fn a_changed_host_key_needs_the_flag() {
        let w = world("hostkey");
        w.up().await.unwrap();
        let pin = "203.0.113.102 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOldOldOldOldOldOldOldOldOldOldOldOldOl\n";
        std::fs::write(w.env().paths.known_hosts(), pin).unwrap();
        w.remote.clear();
        w.remote.key_changed.store(true, Ordering::SeqCst);
        let e = format!("{:#}", w.up().await.unwrap_err());
        assert!(e.contains("--accept-new-host-key"), "{e}");
        assert!(!w.remote.cmds().iter().any(|c| c.contains("arena-archive-setup")), "no setup on an unverified host");
        assert_eq!(std::fs::read_to_string(w.env().paths.known_hosts()).unwrap(), pin, "the pin is kept");
        w.remote.clear();
        w.up_with(UpOpts { accept_new_host_key: true, ..Default::default() }).await.unwrap();
        let vm = w.remote.calls();
        assert!(matches!(vm[0].0.host_keys, HostKeyPolicy::Pinned { .. }) && matches!(vm[1].0.host_keys, HostKeyPolicy::PinFirstUse { .. }), "{vm:?}");
        assert!(vm.last().unwrap().1.contains("arena-archive-setup"));
    }

    /// S14: a setup report with `ok_sshd=0` fails `up` — "config unchanged" — and writes no state.
    #[tokio::test]
    async fn a_failed_sshd_check_fails_up_with_config_unchanged() {
        let w = world("sshd");
        w.remote.on(
            "arena-archive-setup",
            out(1, "ok_os=1\nok_sshd=0\nwhy_sshd=sshd -T for user backups lacks \"chrootdirectory /srv/arena/view\" — config unchanged\nsetup_done=1\n", ""),
        );
        let e = format!("{:#}", w.up().await.unwrap_err());
        assert!(e.contains("config unchanged") && e.contains("NOT installed"), "{e}");
        assert!(w.host_file_absent());
        assert_eq!(w.remote.cmds().iter().filter(|c| c.contains("arena-archive-setup")).count(), 1, "sent once");
        // Another failed step is named.
        let w = world("setupfail");
        w.remote.on("arena-archive-setup", out(1, "ok_fs_big=0\nwhy_fs_big=/dev/sdc is xfs\nsetup_done=1\n", ""));
        let e = format!("{:#}", w.up().await.unwrap_err());
        assert!(e.contains("fs_big (/dev/sdc is xfs)"), "{e}");
        // A report cut short.
        let w = world("setupcut");
        w.remote.on("arena-archive-setup", out(255, "ok_os=1\n", "Connection reset"));
        assert!(format!("{:#}", w.up().await.unwrap_err()).contains("didn't finish"));
    }

    fn decode_setup(cmd: &str) -> String {
        let b64 = cmd.split_whitespace().nth(4).unwrap();
        let mut child = std::process::Command::new("base64")
            .arg("-d")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(b64.as_bytes()).unwrap();
        String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap()
    }

    /// `access on` writes {cohort, read} (after the privacy statement), `off` an empty file and
    /// ends sessions; the choice persists and `up` re-applies it.
    #[tokio::test]
    async fn access_is_persisted_and_reapplied_and_off_ends_sessions() {
        let w = world("access");
        w.up().await.unwrap();
        let setup = w.remote.cmds().into_iter().find(|c| c.contains("arena-archive-setup")).unwrap();
        assert!(decode_setup(&setup).contains("ARENA_BACKUPS_KEYS=''\n"), "up starts with access off");
        let env = w.env();
        w.remote.clear();
        archive_access(&env, true).await.unwrap();
        let on = w.remote.cmds().pop().unwrap();
        let cohort = state::read_pub(&w.dir.join("cohort_key")).unwrap();
        let read = state::read_pub(&env.paths.read_key()).unwrap();
        for k in [&cohort, &read] {
            assert!(on.contains(archive::key_blob(k).unwrap()), "{on}");
        }
        assert!(!on.contains(archive::key_blob(&state::read_pub(&env.paths.admin_key()).unwrap()).unwrap()));
        assert!(!on.contains("pkill"));
        assert!(w.host().access_on);
        // `up` re-applies it.
        w.remote.clear();
        w.up().await.unwrap();
        let setup = w.remote.cmds().into_iter().find(|c| c.contains("arena-archive-setup")).unwrap();
        assert!(decode_setup(&setup).contains(&format!("restrict {}", cohort.split_whitespace().take(2).collect::<Vec<_>>().join(" "))));
        // Off: an empty file, open sessions ended.
        w.remote.clear();
        archive_access(&env, false).await.unwrap();
        let off = w.remote.cmds().pop().unwrap();
        assert!(off.contains("printf '%s' ''") && off.contains("pkill -u backups"), "{off}");
        assert!(!w.host().access_on);
    }

    /// `access on` refuses a cohort key that is also the deploy key (it would open the archive to
    /// every past cohort's pods).
    #[tokio::test]
    async fn access_refuses_a_cohort_key_that_is_the_deploy_key() {
        // The cohort key file of this world, named as the deploy key too.
        let d = std::env::temp_dir().join(format!("arena-archive-cli-{}-deploy", std::process::id()));
        let w = world_with("deploy", &format!("GIT_SSH_KEY_LOCAL={}/cohort_key\n", d.display()));
        w.up().await.unwrap();
        let e = archive_access(&w.env(), true).await.unwrap_err();
        assert!(e.to_string().contains("refusing"), "{e}");
        assert!(!w.host().access_on);
    }

    fn cfg_text(w: &World) -> String {
        let d = w.dir.display();
        format!(
            "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(alpha bravo @staff-box)\nARENA_STATE_DIR={d}/state\n\
             LOCAL_BACKUP_DIR={d}/backup\nARENA_START_DATE=2026-10-01\nSHARED_SSH_KEY_PATH={d}/cohort_key\n"
        )
    }

    /// S7 + §14 down: server only keeps the volumes; volumes need the typed name (`-y` never
    /// suffices); the order; the keys stay.
    #[tokio::test]
    async fn down_keeps_volumes_unless_the_typed_name_is_given() {
        let w = world("down");
        w.up().await.unwrap();
        let env = w.env();
        w.cloud.clear_calls();
        for confirm in [None, Some("devtest"), Some("devtest-archive ")] {
            let e = archive_down(&env, &w.cloud, DownOpts { delete_volumes: true, confirm: confirm.map(String::from), dry_run: false }).await.unwrap_err();
            assert!(e.to_string().contains("--confirm devtest-archive"), "{e}");
        }
        assert!(mutations(&w.cloud).is_empty());
        archive_down(&env, &w.cloud, DownOpts { delete_volumes: false, confirm: None, dry_run: true }).await.unwrap();
        assert!(mutations(&w.cloud).is_empty(), "dry run");
        archive_down(&env, &w.cloud, DownOpts::default()).await.unwrap();
        let m = mutations(&w.cloud);
        let order: Vec<&str> = m
            .iter()
            .map(|c| match c {
                CloudCall::ProtectServer(_, false) => "unprotect server",
                CloudCall::Detach(_) => "detach",
                CloudCall::DeleteServer(_) => "delete server",
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(order, ["unprotect server", "detach", "detach", "delete server"]);
        assert!(w.remote.cmds().iter().any(|c| c.contains("umount")), "quiesced first");
        assert_eq!(w.cloud.state().volumes.len(), 2, "kept");
        assert!(w.cloud.state().volumes.iter().all(|v| v.delete_protected && v.server.is_none()));
        assert!(!w.host().is_up(), "archive.json says down");
        // Pushes now refuse before spawning anything.
        let e = archive_sync(&w.cfg, &env.paths, SyncOpts { tier: "both".into(), ..Default::default() }, "/nonexistent/rsync", &|| 1).await.unwrap_err();
        assert!(e.to_string().contains("down"), "{e}");
        // Everything, typed.
        w.cloud.clear_calls();
        archive_down(&env, &w.cloud, DownOpts { delete_volumes: true, confirm: Some("devtest-archive".into()), dry_run: false }).await.unwrap();
        let m = mutations(&w.cloud);
        assert!(matches!(m[0], CloudCall::ProtectVolume(_, false)) && matches!(m[1], CloudCall::DeleteVolume(_)), "{m:?}");
        assert!(matches!(m.last(), Some(CloudCall::DeleteSshKey(_))), "{m:?}");
        assert!(w.cloud.state().is_empty());
        assert!(w.host_file_absent() && env.paths.admin_key().exists(), "state gone, keys kept");
    }

    /// §14 resize: the API first, then resize2fs on the volume's device, then the size is recorded.
    #[tokio::test]
    async fn resize_grows_through_the_api_then_resize2fs_on_the_device() {
        let w = world("resize");
        w.up().await.unwrap();
        let env = w.env();
        let big = w.cloud.state().volumes.into_iter().find(|v| v.tier() == Some(Tier::Big)).unwrap();
        w.cloud.clear_calls();
        w.remote.clear();
        w.remote.on("resize2fs", out(0, "21474836480\n", ""));
        assert!(archive_resize(&env, &w.cloud, Tier::Big, 10, false).await.unwrap_err().to_string().contains("only grow"));
        archive_resize(&env, &w.cloud, Tier::Big, 20, false).await.unwrap();
        assert_eq!(mutations(&w.cloud), [CloudCall::Resize(big.id, 20)]);
        let cmd = w.remote.cmds().pop().unwrap();
        assert!(cmd.contains(&format!("resize2fs '{}'", big.linux_device)), "{cmd}");
        assert_eq!(w.host().big_gb, 20);
    }

    fn healthy_probe(env: &Env<'_>, host: &ArchiveHost) -> String {
        use arena_core::archive::render::render_push_key_line;
        let b64 = |s: &str| {
            let mut child = std::process::Command::new("base64").arg("-w0").stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
            use std::io::Write;
            child.stdin.take().unwrap().write_all(s.as_bytes()).unwrap();
            String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap()
        };
        let mut s = String::new();
        for (t, dev) in [("snap", &host.snap_device), ("big", &host.big_device)] {
            s.push_str(&format!(
                "dev_{t}={dev}\nmounted_{t}=1\nsource_{t}={dev}\nfstype_{t}=ext4\nbytes_total_{t}=100\nbytes_used_{t}=1\ninodes_total_{t}=100\n\
                 inodes_used_{t}=1\nbind_{t}=1\nbind_source_{t}={dev}\nbind_ro_{t}=1\n"
            ));
        }
        let admin = state::read_pub(&env.paths.admin_key()).unwrap();
        let push = render_push_key_line("/usr/bin/rrsync", &state::read_pub(&env.paths.push_key()).unwrap()).unwrap();
        s.push_str(&format!(
            "root_keys_b64={}\npush_keys_b64={}\nbackups_keys_b64=\nprivate_keys=0\nssh_service=active\nunattended_upgrades=active\n\
             rrsync=/usr/bin/rrsync\nsetup_version=1\nsshd_dropin=1\nprobe_done=1\n",
            b64(&admin),
            b64(&push)
        ));
        s
    }

    /// S16: `status` and `teardown --check` make no mutating API call, and `status` runs only
    /// the read-only probe on the VM.
    #[tokio::test]
    async fn status_only_reads_and_only_runs_the_probe() {
        let w = world("status");
        w.up().await.unwrap();
        let env = w.env();
        w.remote.on("arena-archive-probe", out(0, &healthy_probe(&env, &w.host()), ""));
        w.cloud.clear_calls();
        w.remote.clear();
        // Never pushed yet → warnings only; healthy otherwise.
        archive_status(&env, Some(&w.cloud), Some(TokenSource::Archive), false, 1_800_000_000).await.unwrap();
        assert!(mutations(&w.cloud).is_empty(), "{:?}", w.cloud.calls());
        let cmds = w.remote.cmds();
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].contains("arena-archive-probe") && !cmds[0].contains("arena-archive-setup"));
        // The teardown read lists once, with any prefix.
        w.cloud.clear_calls();
        let _ = crate::teardown::read_archive(Some(&w.cloud)).await;
        assert_eq!(w.cloud.calls(), [CloudCall::Inventory(None)]);
    }

    // ---- sync -------------------------------------------------------------------------------

    /// A recording rsync: appends its argv to `rsync-calls`; fails a tier when `fail-<tier>`
    /// exists (its text on stderr, exit `fail-<tier>.code` or 11); else prints stats.
    fn rsync_stub(dir: &Path) -> String {
        let stub = dir.join("rsync-stub");
        std::fs::write(
            &stub,
            "#!/bin/sh\nd=$(dirname \"$0\")\nfor a; do last=$a; done\nprintf '%s\\n' \"$*\" >> \"$d/rsync-calls\"\n\
             case \"$last\" in *:big/) f=\"$d/fail-big\" ;; *) f=\"$d/fail-snap\" ;; esac\n\
             if [ -f \"$f\" ]; then cat \"$f\" >&2; exit \"$(cat \"$f.code\" 2>/dev/null || echo 11)\"; fi\n\
             printf 'Number of files: 5 (reg: 3, dir: 2)\\nNumber of regular files transferred: 2\\nTotal file size: 300 bytes\\nTotal transferred file size: 200 bytes\\n'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        stub.display().to_string()
    }

    fn rsync_calls(dir: &Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("rsync-calls")).unwrap_or_default().lines().map(String::from).collect()
    }

    /// The archive state a sync needs, without an `up`.
    fn seed(w: &World) {
        let p = w.env().paths;
        p.ensure_dir().unwrap();
        state::ensure_keypair(&p.push_key(), "push").unwrap();
        std::fs::write(p.known_hosts(), "192.0.2.1 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHostHostHostHostHostHostHostHostHostHos5\n").unwrap();
        let h = ArchiveHost {
            version: state::STATE_VERSION,
            prefix: "devtest".into(),
            server_id: 7,
            ipv4: "192.0.2.1".into(),
            location: "nbg1".into(),
            snap_device: "/dev/disk/by-id/scsi-0HC_Volume_1".into(),
            big_device: "/dev/disk/by-id/scsi-0HC_Volume_2".into(),
            access_on: false,
            setup_version: 1,
            snap_gb: 10,
            big_gb: 10,
            host_key_fingerprint: None,
        };
        state::save(&p.host_file(), &h).unwrap();
        for f in ["w0d1/devtest-alpha/a.py", "w0d2/devtest-alpha/a.py", "w0d2/staff-box/s.txt", "w0d2/devtest-ghost/g", "big/devtest-alpha/ckpt.pt", "mylabel/devtest-alpha/x"] {
            let path = w.dir.join("backup").join(f);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "x").unwrap();
        }
    }

    /// Day boundaries for ARENA_START_DATE=2026-10-01 (w0d1).
    fn day_end(label: &str) -> u64 {
        let (y, m, d) = arena_core::schedule::parse_ymd("2026-10-01").unwrap();
        push::label_end(label, arena_core::schedule::days_from_civil(y, m, d)).unwrap()
    }

    fn clock(start: u64) -> impl Fn() -> u64 {
        let t = AtomicU64::new(start);
        move || t.fetch_add(1, Ordering::SeqCst)
    }

    fn sync_opts(tier: &str) -> SyncOpts {
        SyncOpts { tier: tier.into(), ..Default::default() }
    }

    /// §14: snap before big, two passes per label, link-dest to the previous label, only the
    /// allowlisted pods; state per label; sealing by the clock; the next run pushes only what isn't
    /// final.
    #[tokio::test]
    async fn sync_pushes_snap_before_big_links_days_and_seals_by_the_clock() {
        let w = world("sync");
        seed(&w);
        let stub = rsync_stub(&w.dir);
        let paths = w.env().paths;
        // Pushed 3 h after w0d1 ended: w0d1 becomes final, w0d2 (today) doesn't.
        let now = day_end("w0d1") + 3 * 3600;
        archive_sync(&w.cfg, &paths, sync_opts("both"), &stub, &clock(now)).await.unwrap();
        let calls = rsync_calls(&w.dir);
        assert_eq!(calls.len(), 5, "{calls:#?}");
        assert!(calls[0].ends_with(":snap/w0d1/") && !calls[0].contains("--link-dest"), "{}", calls[0]);
        assert!(calls[1].contains("--include=**/.git/**") && calls[1].ends_with(":snap/w0d1/"));
        assert!(calls[2].contains("--link-dest=/snap/w0d1/") && calls[2].ends_with(":snap/w0d2/"));
        assert!(calls[4].ends_with(":big/"), "big last: {}", calls[4]);
        for c in &calls {
            assert!(c.contains("--include=/devtest-alpha/") && !c.contains("staff-box") && !c.contains("devtest-ghost"), "{c}");
            assert!(c.contains("arena-push@192.0.2.1:"));
            assert!(arena_core::archive::sync::forbidden_flag(&c.split(' ').map(String::from).collect::<Vec<_>>()).is_none(), "{c}");
        }
        let st = state::load_snap(&paths.sync_state(Tier::Snap)).unwrap();
        assert!(st.labels.contains_key("w0d1") && st.labels.contains_key("w0d2"));
        assert!(!st.labels.contains_key("mylabel"), "custom labels only with --label");
        let big = state::load_big(&paths.sync_state(Tier::Big)).unwrap();
        assert!(big.last_ok.is_some());
        // The next tick: w0d1 is final, only w0d2 (+ big).
        std::fs::remove_file(w.dir.join("rsync-calls")).unwrap();
        archive_sync(&w.cfg, &paths, sync_opts("both"), &stub, &clock(now + 900)).await.unwrap();
        let calls = rsync_calls(&w.dir);
        assert_eq!(calls.len(), 3, "{calls:#?}");
        assert!(calls[0].ends_with(":snap/w0d2/") && calls[0].contains("--link-dest=/snap/w0d1/"));
        // --label: a final label again, and a custom one (no link-dest).
        std::fs::remove_file(w.dir.join("rsync-calls")).unwrap();
        let o = SyncOpts { tier: "snap".into(), labels: vec!["w0d1".into(), "mylabel".into()], ..Default::default() };
        archive_sync(&w.cfg, &paths, o, &stub, &clock(now + 1800)).await.unwrap();
        let calls = rsync_calls(&w.dir);
        assert!(calls.iter().any(|c| c.ends_with(":snap/mylabel/") && !c.contains("--link-dest")), "{calls:#?}");
        assert!(calls.iter().any(|c| c.ends_with(":snap/w0d1/")));
    }

    /// S10: a full big volume fails big (exit ≠ 0) but leaves the snap pushes and sync-snap.json
    /// alone; each tier has its own state and lock.
    #[tokio::test]
    async fn a_big_failure_never_touches_snap() {
        let w = world("tiers");
        seed(&w);
        let stub = rsync_stub(&w.dir);
        let paths = w.env().paths;
        std::fs::write(w.dir.join("fail-big"), "rsync: [Receiver] write failed on \"/srv/arena/push/big/devtest-alpha/ckpt.pt\": No space left on device (28)\n").unwrap();
        let e = archive_sync(&w.cfg, &paths, sync_opts("both"), &stub, &clock(day_end("w0d1") + 60)).await.unwrap_err();
        assert!(e.to_string().contains("1 archive push(es) failed"), "{e}");
        let st = state::load_snap(&paths.sync_state(Tier::Snap)).unwrap();
        assert_eq!(st.labels.len(), 2, "snap went through");
        assert!(st.last_error.is_none());
        let big = state::load_big(&paths.sync_state(Tier::Big)).unwrap();
        assert_eq!(big.last_error.as_ref().map(|e| e.class), Some(PushFailure::NoSpace));
        assert!(big.last_ok.is_none());
        assert_ne!(paths.sync_lock(Tier::Snap), paths.sync_lock(Tier::Big));
        assert_ne!(paths.sync_state(Tier::Snap), paths.sync_state(Tier::Big));
        // A snap timeout leaves big pushing: budget expiry of a hanging rsync.
        std::fs::remove_file(w.dir.join("fail-big")).unwrap();
        std::fs::write(w.dir.join("fail-snap"), "rsync error: timeout in data send/receive (code 30)\n").unwrap();
        std::fs::write(w.dir.join("fail-snap.code"), "30").unwrap();
        let before = state::load_snap(&paths.sync_state(Tier::Snap)).unwrap().labels;
        assert!(archive_sync(&w.cfg, &paths, sync_opts("both"), &stub, &clock(day_end("w0d1") + 120)).await.is_err());
        let after = state::load_snap(&paths.sync_state(Tier::Snap)).unwrap();
        assert_eq!(after.labels, before, "a failed label keeps its last good record");
        assert_eq!(after.last_error.as_ref().map(|e| e.class), Some(PushFailure::Timeout));
        assert!(state::load_big(&paths.sync_state(Tier::Big)).unwrap().last_ok.is_some(), "big still pushed");
    }

    /// A held lock skips only that tier (exit 0); the other tier pushes.
    #[tokio::test]
    async fn a_held_lock_skips_only_its_tier() {
        let w = world("lock");
        seed(&w);
        let stub = rsync_stub(&w.dir);
        let paths = w.env().paths;
        let held = try_lock(&paths.sync_lock(Tier::Snap)).unwrap().expect("free");
        archive_sync(&w.cfg, &paths, sync_opts("both"), &stub, &clock(day_end("w0d1"))).await.unwrap();
        let calls = rsync_calls(&w.dir);
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(calls[0].ends_with(":big/"));
        assert!(try_lock(&paths.sync_lock(Tier::Snap)).unwrap().is_none(), "still held");
        drop(held);
        assert!(try_lock(&paths.sync_lock(Tier::Snap)).unwrap().is_some());
    }

    /// No state for this user / no start date / a dry run: nothing is spawned.
    #[tokio::test]
    async fn sync_refuses_before_spawning_and_a_dry_run_spawns_nothing() {
        let w = world("nostate");
        let stub = rsync_stub(&w.dir);
        let paths = w.env().paths;
        let e = archive_sync(&w.cfg, &paths, sync_opts("both"), &stub, &clock(1)).await.unwrap_err();
        assert!(e.to_string().contains("run `arena archive up` as this user"), "{e}");
        assert!(rsync_calls(&w.dir).is_empty());
        seed(&w);
        archive_sync(&w.cfg, &paths, SyncOpts { dry_run: true, ..sync_opts("both") }, &stub, &clock(1)).await.unwrap();
        assert!(rsync_calls(&w.dir).is_empty());
        assert!(!paths.sync_state(Tier::Snap).exists() && !paths.sync_state(Tier::Big).exists());
        // Without a start date, snap errors and big still pushes.
        let nodate = Config::parse(&cfg_text(&w).replace("ARENA_START_DATE=2026-10-01\n", ""));
        let e = archive_sync(&nodate, &paths, sync_opts("both"), &stub, &clock(1)).await.unwrap_err();
        assert!(e.to_string().contains("1 archive push(es) failed"), "{e}");
        assert_eq!(rsync_calls(&w.dir).len(), 1, "big only");
        // A bad --label is refused up front.
        let e = archive_sync(&w.cfg, &paths, SyncOpts { labels: vec!["../x".into()], ..sync_opts("snap") }, &stub, &clock(1)).await.unwrap_err();
        assert!(e.to_string().contains("--label"), "{e}");
    }

    /// S13 (compile-level + review): the sync path takes no `ArchiveCloud` and builds none —
    /// `archive_sync`'s section of this file never names the cloud.
    #[test]
    fn the_sync_path_never_touches_hetzner() {
        let src = include_str!("archive.rs");
        let start = src.find("// sync (the cron path").unwrap();
        let end = src.find("// resize, access, ssh-config, ssh").unwrap();
        // The code only (the doc comments say "no ArchiveCloud" in words).
        let section: String = src[start..end].lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n");
        for needle in ["ArchiveCloud", "cloud.", "HetznerArchiveCloud", "archive_token"] {
            assert!(!section.contains(needle), "{needle} in the sync path");
        }
        let main_arm: String = src[src.find("if let ArchiveCmd::Sync").unwrap()..src.find("let env = Env::new(cfg, remote, yes)?;").unwrap()]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!main_arm.contains("cloud") && !main_arm.contains("token"), "{main_arm}");
    }

    // ---- pods, ssh-config, cron --------------------------------------------------------------

    fn pod(name: &str) -> arena_core::Pod {
        arena_core::Pod {
            id: format!("id-{name}"),
            name: name.into(),
            provider: "runpod".into(),
            status: "RUNNING".into(),
            ssh_ip: Some("10.0.0.1".into()),
            ssh_port: Some(22001),
            ..Default::default()
        }
    }

    /// §14 FakeRemote: with an archive, copy-keys writes the block and the pinned key in its one
    /// exec and copies the read key by scp (never inside a command); without one, nothing changes.
    #[tokio::test]
    async fn copy_keys_installs_the_archive_block_and_copies_the_read_key_by_scp() {
        let w = world("copykeys");
        let sel = crate::Selected { pods: vec![pod("devtest-alpha")], named: true };
        let keys_dir = w.dir.join("keys").display().to_string();
        // No archive: one exec, nothing about it.
        let fake = Arc::new(FakeRemote::new());
        crate::handle_copy_keys(fake.clone(), &w.cfg, &keys_dir, None, None, &sel, false, true).await.unwrap();
        let calls = fake.calls();
        assert_eq!(calls.len(), 1);
        assert!(matches!(&calls[0], RemoteCall::Exec { cmd, .. } if !cmd.contains("archive")));
        let before = match &calls[0] {
            RemoteCall::Exec { cmd, .. } => cmd.clone(),
            _ => unreachable!(),
        };
        // With one.
        w.up().await.unwrap();
        std::fs::write(w.env().paths.known_hosts(), "203.0.113.102 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHostHostHostHostHostHostHostHostHostHos5\n").unwrap();
        let fake = Arc::new(FakeRemote::new());
        crate::handle_copy_keys(fake.clone(), &w.cfg, &keys_dir, None, None, &sel, false, true).await.unwrap();
        let calls = fake.calls();
        assert_eq!(calls.len(), 3, "{calls:#?}");
        let RemoteCall::Exec { cmd, .. } = &calls[0] else { panic!("{calls:?}") };
        assert!(cmd.starts_with(before.split("# END arena-infra fleet").next().unwrap()), "the fleet part is as before");
        assert!(cmd.contains("Host archive-devtest\n    HostName 203.0.113.102\n    User backups\n    IdentityFile ~/.ssh/arena_archive_key"), "{cmd}");
        assert!(cmd.contains("# END arena-infra fleet") && cmd.contains(".ssh/arena_archive_known_hosts") && cmd.contains("HostHost"));
        let private = std::fs::read_to_string(w.env().paths.read_key()).unwrap();
        assert!(!cmd.contains(private.lines().nth(1).unwrap()), "the read key never travels in a command");
        assert!(
            matches!(&calls[1], RemoteCall::Copy { local, remote, recursive: false, .. } if *local == w.env().paths.read_key().display().to_string() && remote == ".ssh/arena_archive_key"),
            "{calls:?}"
        );
        assert!(matches!(&calls[2], RemoteCall::Exec { cmd, .. } if cmd.contains("chmod 600") && cmd.contains("arena_archive_key")));
    }

    /// `arena ssh-config` + the laptop block, read back by the real `ssh -G`: the archive alias
    /// gets user backups, its host and the cohort identity — none of the fleet wildcard's.
    #[tokio::test]
    async fn ssh_g_resolves_the_archive_alias_past_the_fleet_wildcard() {
        if !have("ssh") {
            return;
        }
        let w = world("sshg");
        w.up().await.unwrap();
        let fleet = arena_core::sshconfig::render_proxy("devtest", "root", "~/.ssh/devtest_key", "proxy.example.org", 9500, &w.cfg.machine_names);
        let block = laptop_block_for(&w.cfg).expect("an archive");
        let file = w.dir.join("ssh_config");
        std::fs::write(&file, format!("{fleet}\n{block}")).unwrap();
        let g = |host: &str| {
            let o = std::process::Command::new("ssh").arg("-G").arg("-F").arg(&file).arg(host).output().unwrap();
            String::from_utf8_lossy(&o.stdout).into_owned()
        };
        let a = g("archive-devtest");
        let identity = w.dir.join("cohort_key").display().to_string();
        assert!(a.lines().any(|l| l == "user backups"), "{a}");
        assert!(a.lines().any(|l| l == format!("hostname {}", w.host().ipv4)), "{a}");
        assert!(a.lines().any(|l| l == format!("identityfile {identity}")), "{a}");
        assert!(a.lines().any(|l| l == "identitiesonly yes"), "{a}");
        assert!(!a.lines().any(|l| l == "hostname proxy.example.org"), "{a}");
        let pod = g("devtest-alpha");
        assert!(pod.lines().any(|l| l == "user root") && pod.lines().any(|l| l == "hostname proxy.example.org"), "{pod}");
    }

    /// §14.11: `cron install --archive` adds the two lines (start date on both) and keeps every
    /// other line byte for byte; `remove --archive` drops just them; refusals come first.
    #[cfg(unix)]
    #[tokio::test]
    async fn cron_archive_lines_merge_and_refuse() {
        use crate::{handle_cron, Cli, Cmd, CronKind};
        use clap::Parser;
        use std::os::unix::fs::PermissionsExt;
        let w = world("cron");
        let stub = w.dir.join("crontab");
        std::fs::write(
            &stub,
            "#!/bin/sh\ntab=\"$(dirname \"$0\")/tab\"\ncase \"$1\" in\n  -l) [ -f \"$tab\" ] && exec cat \"$tab\"; exit 1;;\n  -) cat > \"$tab\";;\nesac\n",
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let tab = w.dir.join("tab");
        let config = w.dir.join("config.env");
        std::fs::write(&config, "").unwrap();
        let run = |cfg: Config, args: Vec<&'static str>| {
            let (stub, config) = (stub.display().to_string(), config.clone());
            async move {
                let argv = ["arena", "cron"].into_iter().chain(args);
                let Cmd::Cron(c) = Cli::try_parse_from(argv).unwrap().cmd else { unreachable!() };
                handle_cron(c, &cfg, &config, true, &stub).await
            }
        };
        let lines = || crate::arena_cron_lines(&std::fs::read_to_string(&tab).unwrap_or_default()).managed;
        // Refused without this user's archive state.
        let e = run(w.cfg.clone(), vec!["install", "--archive"]).await.unwrap_err();
        assert!(e.to_string().contains("archive.json"), "{e}");
        assert!(lines().is_empty());
        w.up().await.unwrap();
        std::fs::write(w.env().paths.known_hosts(), "203.0.113.102 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHostHostHostHostHostHostHostHostHostHos5\n").unwrap();
        // A relative backup dir, and no start date: both refused, by name.
        let rel = Config::parse(&format!("{}LOCAL_BACKUP_DIR=./backup\n", cfg_text(&w).replace(&format!("LOCAL_BACKUP_DIR={}/backup\n", w.dir.display()), "").replace("ARENA_START_DATE=2026-10-01\n", "")));
        let e = run(rel, vec!["install", "--archive"]).await.unwrap_err().to_string();
        assert!(e.contains("absolute") && e.contains("ARENA_START_DATE"), "{e}");
        // The backup and proxy lines first, then the archive's: they stay byte for byte.
        run(w.cfg.clone(), vec!["install", "--pull", "--start-date", "2026-10-01", "--proxy"]).await.unwrap();
        let first = lines();
        run(w.cfg.clone(), vec!["install", "--archive"]).await.unwrap();
        let all = lines();
        assert_eq!(all.len(), 4, "{all:#?}");
        assert_eq!(all[..2], first[..]);
        assert_eq!(CronKind::of(&all[2]), Some(CronKind::ArchiveSnap));
        assert_eq!(CronKind::of(&all[3]), Some(CronKind::ArchiveBig));
        assert!(all[2].starts_with("7,22,37,52 * * * * ARENA_START_DATE=2026-10-01 nice -n 10 ") && all[2].contains(" archive sync --tier snap >> "), "{}", all[2]);
        assert!(all[3].starts_with("40 * * * * ARENA_START_DATE=2026-10-01 nice -n 10 ") && all[3].ends_with("/arena-archive-cron.log 2>&1"), "{}", all[3]);
        assert_eq!(CronKind::of(&all[0]), Some(CronKind::Backup));
        // Re-installing is a no-op; remove --archive drops just those two.
        run(w.cfg.clone(), vec!["install", "--archive"]).await.unwrap();
        assert_eq!(lines(), all);
        run(w.cfg.clone(), vec!["remove", "--archive"]).await.unwrap();
        assert_eq!(lines(), first);
        // An archive line is never mistaken for the snapshot line (or vice versa).
        assert_eq!(CronKind::of("*/2 * * * * flock -n /h/.arena-snapshot-cron.lock /a --config /c snapshot --public --out /w/fleet.json"), Some(CronKind::Snapshot));
        assert_eq!(CronKind::of("# 7 * * * * /a archive sync --tier snap"), None);
    }
}
