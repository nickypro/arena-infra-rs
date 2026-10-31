//! `arena keys` (the cohort's OpenRouter keys: gen, rotate, revoke, list) and `pods copy-keys`
//! (API keys and fleet SSH into each pod's shell), with the keys CSV they share.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use arena_core::provider::Provider;
use arena_core::remote::Remote;
use arena_core::selector::{Naming, SelectArgs, Selector};
use arena_core::Config;
use clap::Subcommand;

use crate::cli::Select;
use crate::pods::list::{enrich_best_effort, ENRICH_TIMEOUT};

mod checked;
pub(crate) use checked::{check_csv_keys, list_for_check, mint_missing, plan_mints};
use crate::pods::selection::{all_stopped_error, stopped_skip, Selected};
use crate::{archive, confirm, key_limits, shell_quote, spend};

#[derive(Subcommand)]
pub(crate) enum KeysCmd {
    /// Mint an OpenRouter key per machine (skips existing ones)
    ///
    /// Keys are named `<prefix>-<machine>`, capped at --limit USD and saved to
    /// keys/openrouter_api_keys.csv. Targets may be MACHINE_NAME_LIST names with no pod yet;
    /// --all = every current pod. --copy also puts them on the pods. `rotate` replaces existing
    /// keys.
    #[command(verbatim_doc_comment)]
    Gen {
        /// USD credit cap per key (default: config OPENROUTER_KEY_LIMIT, else 5).
        #[arg(long)]
        limit: Option<f64>,
        /// After generating, copy the key(s) onto the pod(s) (runs copy-keys for them).
        #[arg(long)]
        copy: bool,
        /// Preview only: show what would be created, change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        #[command(flatten)]
        sel: Select,
    },
    /// List the cohort's OpenRouter keys (limit, usage)
    List {
        /// Every key on the account, not just this cohort's `<prefix>-*` keys
        #[arg(long)]
        all: bool,
    },
    /// Show the local keys file and which pods have keys
    Which,
    /// Replace machines' keys with fresh ones (e.g. after a leak)
    ///
    /// Deletes each key, mints a new one and updates the CSV; --copy re-pushes to the pods.
    /// Targets as for `gen`, or --all.
    #[command(verbatim_doc_comment)]
    Rotate {
        /// USD credit cap for the new key (default: config OPENROUTER_KEY_LIMIT, else 5).
        #[arg(long)]
        limit: Option<f64>,
        /// After rotating, copy the new key(s) onto the pod(s).
        #[arg(long)]
        copy: bool,
        /// Preview only: change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        #[command(flatten)]
        sel: Select,
    },
    /// Delete machines' keys without minting new ones
    ///
    /// Targets as for `gen`, or --all.
    Revoke {
        /// Preview only: change nothing.
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
        #[command(flatten)]
        sel: Select,
    },
    /// Raise or set machines' key caps in place (`limit apple +5`)
    Limit(key_limits::LimitArgs),
    /// Pause machines' keys: their AI calls fail until `resume`
    #[command(visible_alias = "disable")]
    Pause(key_limits::PauseArgs),
    /// Resume paused keys
    #[command(visible_alias = "enable")]
    Resume(key_limits::PauseArgs),
}

/// Drop `host`'s row from the keys CSV in `dir` (its key was deleted, none replaced it):
/// whether a row went (`Ok(false)`: no CSV, or no row for it). The caller holds the keys folder
/// ([`lock_keys_dir`]).
pub(crate) fn drop_key_row_held(dir: &std::path::Path, host: &str) -> std::io::Result<bool> {
    let csv = dir.join(OPENROUTER_KEYS_LINK);
    let text = match std::fs::read_to_string(&csv) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    match arena_core::apikeys::remove_csv_host(&text, host) {
        Some(rest) => write_secret_file(&csv, &rest).map(|()| true),
        None => Ok(false),
    }
}

/// How long one pod gets for the post-setup API-key + fleet-SSH write (`copy-keys`): a
/// few greps/appends to rc files and `~/.ssh/config` — seconds on a healthy pod, so a
/// minute means a wedged one, which must not hang the command (see `handle_copy_keys`).
pub(crate) const COPY_KEYS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Where `keys gen` writes, and `setup` / `copy-keys` read, the per-host API-key CSVs: the
/// fleet's `keys/`, next to the config (else `./keys`, warned: [`crate::fleet_files::keys_dir`]).
pub(crate) use crate::fleet_files::keys_dir;

/// Where a machine's OpenRouter key lives, for the lifecycle commands that move or drop it
/// (`rename`, `terminate --revoke-key`): the local CSV `copy-keys` distributes from, and —
/// when `OPENROUTER_PROVISIONING_KEY` is set — the provisioning API, where keys are found by
/// name (`keys gen` names each after its machine, i.e. the pod's name). A struct so tests
/// point it at a temp file and a fake API.
pub(crate) struct KeyBook<'a> {
    /// `keys/openrouter_api_keys.csv` — a symlink to the per-cohort file
    /// (`ensure_cohort_keys_file`); reads and writes go through it to that file.
    pub csv: PathBuf,
    pub api: Option<&'a dyn arena_core::openrouter::KeyApi>,
}

impl<'a> KeyBook<'a> {
    pub(crate) fn new<K: arena_core::openrouter::KeyApi + 'a>(csv: impl Into<PathBuf>, api: Option<&'a K>) -> Self {
        Self { csv: csv.into(), api: api.map(|k| k as &dyn arena_core::openrouter::KeyApi) }
    }

    /// The CSV's text, or `None` when there is no keys file (nothing to move or drop).
    pub(crate) fn read_csv(&self) -> Option<String> {
        std::fs::read_to_string(&self.csv).ok()
    }

    /// Replace the CSV atomically, through the symlink (the per-cohort file it points at is
    /// rewritten, the link kept), owner-only: it holds secrets ([`write_secret_file`]).
    pub(crate) fn write_csv(&self, text: &str) -> std::io::Result<()> {
        write_secret_file(&self.csv, text)
    }

    /// Hold the CSV's folder ([`lock_keys_dir`]) for one read-change-write of it.
    pub(crate) fn lock(&self) -> std::io::Result<Option<std::fs::File>> {
        match self.csv.parent().filter(|d| !d.as_os_str().is_empty()) {
            Some(d) => lock_keys_dir(d),
            None => lock_keys_dir(std::path::Path::new(".")),
        }
    }
}

/// The provisioning-API client, when `OPENROUTER_PROVISIONING_KEY` is set (else `None`:
/// key names on OpenRouter are left alone, and `--revoke-key` is refused).
pub(crate) fn openrouter_client(cfg: &Config) -> Option<arena_core::openrouter::OpenRouter> {
    arena_core::openrouter::OpenRouter::from_config(cfg)
}

/// A pod in `listed` that is named `name` but isn't one of `leaving` (by id) — i.e. one that
/// will still be there, and still using that machine's key, after `leaving` are terminated.
pub(crate) fn shared_name_holder<'a>(listed: &'a [arena_core::Pod], leaving: &[&str], name: &str) -> Option<&'a arena_core::Pod> {
    listed.iter().find(|p| p.name == name && !leaving.contains(&p.id.as_str()))
}

/// The OpenRouter key names that are `machine`'s: the name `keys gen` gives it
/// ([`arena_core::openrouter::key_name`] — the pod name for a cohort pod, but
/// `{prefix}-vast-777` for a pod off the prefix, like Vast's fallback name) and, when that
/// differs, a key named exactly after the pod (minted under its own prefix). Looking only at
/// the raw pod name missed the first kind: revoke then found "no key", dropped the CSV row
/// and left the key working.
pub(crate) fn machine_key_names(naming: &Naming, machine: &str) -> Vec<String> {
    let minted = arena_core::openrouter::key_name(naming.prefix, naming.list, machine);
    if minted == machine {
        vec![minted]
    } else {
        vec![machine.to_string(), minted]
    }
}

/// Build the remote command that wires up *fleet* SSH on a pod: authorize every fleet
/// pubkey for incoming SSH (arena8 + arena_infra + admin), and write the fleet host map
/// into a managed block in `~/.ssh/config` so the pod can `ssh <prefix>-<peer>` any other
/// pod (using its arena_infra key, `id_ed25519`). Idempotent — re-running refreshes the
/// block and skips already-present authorized keys, and it preserves other `~/.ssh/config`
/// blocks (e.g. the github.com deploy-key block `setup` writes).
fn fleet_ssh_command(pubkeys: &[String], rendered_config: &str) -> String {
    let mut cmd = String::from(
        "mkdir -p \"$HOME/.ssh\" && chmod 700 \"$HOME/.ssh\" && \
         touch \"$HOME/.ssh/authorized_keys\" && chmod 600 \"$HOME/.ssh/authorized_keys\"",
    );
    for pk in pubkeys {
        let q = shell_quote(pk.trim());
        cmd.push_str(&format!(
            "; (grep -qxF {q} \"$HOME/.ssh/authorized_keys\" || echo {q} >> \"$HOME/.ssh/authorized_keys\")"
        ));
    }
    cmd.push_str("; touch \"$HOME/.ssh/config\" && chmod 600 \"$HOME/.ssh/config\"");
    cmd.push_str(
        "; sed -i '/^# BEGIN arena-infra fleet/,/^# END arena-infra fleet/d' \"$HOME/.ssh/config\"",
    );
    cmd.push_str("; cat >> \"$HOME/.ssh/config\" <<'ARENAFLEETCFG'\n# BEGIN arena-infra fleet\n");
    cmd.push_str(rendered_config);
    if !rendered_config.ends_with('\n') {
        cmd.push('\n');
    }
    cmd.push_str("# END arena-infra fleet\nARENAFLEETCFG");
    cmd
}

/// The API keys `copy-keys` (and `up`'s per-pod keys stage) hand out: per-host vars from
/// the `<provider>_api_keys.csv` files in a keys dir, and the broadcast tokens (`--hf-token`
/// over config) that every targeted pod gets. (Claude Code's token is each pod's own: the
/// Claude Code stage, `crate::claude`.)
pub(crate) struct KeySources {
    /// host (exact pod name) -> [(ENV_NAME, value), …].
    pub per_host: std::collections::HashMap<String, Vec<(String, String)>>,
    pub broadcast: Vec<(String, String)>,
    /// `OpenAI (3 hosts)` per CSV with rows — empty = no per-host keys at all.
    pub csv: Vec<String>,
    /// Display names of the broadcast tokens that are set (`Hugging Face`, …).
    broadcast_names: Vec<&'static str>,
}

impl KeySources {
    pub(crate) fn load(cfg: &Config, keys_dir: &str, hf_token: Option<String>) -> Self {
        use arena_core::apikeys;
        let mut per_host: std::collections::HashMap<String, Vec<(String, String)>> = Default::default();
        let mut csv = Vec::new();
        for (base, display, env_names) in apikeys::PROVIDERS {
            let path = format!("{}/{base}_api_keys.csv", keys_dir.trim_end_matches('/'));
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let rows = apikeys::parse_csv(&text);
            if rows.is_empty() {
                continue;
            }
            csv.push(format!("{display} ({} host{})", rows.len(), if rows.len() == 1 { "" } else { "s" }));
            for (host, key) in rows {
                let entry = per_host.entry(host).or_default();
                for env in *env_names {
                    entry.push((env.to_string(), key.clone()));
                }
            }
        }
        // Broadcast tokens (HF): a flag overrides the config value.
        let token_value = |k: &str| -> Option<String> {
            let flag = match k {
                "HF_TOKEN" => hf_token.clone(),
                _ => None,
            };
            flag.or_else(|| cfg.get(k).filter(|s| !s.is_empty()).map(String::from))
        };
        let broadcast = apikeys::broadcast_env_vars(token_value);
        let broadcast_names =
            apikeys::BROADCAST_TOKENS.iter().filter(|(key, _, _)| token_value(key).is_some()).map(|(_, d, _)| *d).collect();
        Self { per_host, broadcast, csv, broadcast_names }
    }

    /// One pod's vars — the broadcast tokens plus its per-host keys — and whether it matched
    /// a per-host row at all (on its exact name).
    pub(crate) fn vars_for(&self, name: &str) -> (Vec<(String, String)>, bool) {
        let mut vars = self.broadcast.clone();
        let matched = self.per_host.get(name).map(|h| vars.extend(h.iter().cloned())).is_some();
        (vars, matched)
    }

    /// The OpenRouter key the keys CSV holds for `host` (its exact name), if any.
    pub(crate) fn openrouter_secret(&self, host: &str) -> Option<&str> {
        self.per_host.get(host)?.iter().find(|(env, _)| env == checked::OPENROUTER_ENV).map(|(_, v)| v.as_str())
    }

    /// Leave `host`'s OpenRouter key out of what's copied (its CSV row holds a key OpenRouter no
    /// longer has): its other keys stay, and the sources line counts one host fewer.
    pub(crate) fn leave_out_openrouter(&mut self, host: &str) {
        let Some(vars) = self.per_host.get_mut(host) else { return };
        let before = vars.len();
        vars.retain(|(env, _)| env != checked::OPENROUTER_ENV);
        if vars.len() == before {
            return;
        }
        if vars.is_empty() {
            self.per_host.remove(host);
        }
        let left = self.per_host.values().filter(|v| v.iter().any(|(env, _)| env == checked::OPENROUTER_ENV)).count();
        let label = |n: usize| format!("OpenRouter ({n} host{})", if n == 1 { "" } else { "s" });
        match self.csv.iter().position(|s| s.starts_with("OpenRouter (")) {
            Some(i) if left == 0 => {
                self.csv.remove(i);
            }
            Some(i) => self.csv[i] = label(left),
            None => {}
        }
    }
}


/// The fleet-SSH half of a key write, and how many fleet keys it authorizes: authorize the
/// fleet pubkeys (arena8 + arena_infra + admin) and write the host map into `~/.ssh/config`
/// so pods can ssh each other with the arena_infra key. The stable proxy layout (survives
/// restarts) when a proxy is configured, else the live endpoints of `pods`.
pub(crate) fn fleet_ssh_for(cfg: &Config, pods: &[arena_core::Pod]) -> (String, usize) {
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    let fleet_pubkeys = arena_core::ssh::authorized_pubkeys(cfg);
    let ssh_user = cfg.get("SSH_USER").unwrap_or("root");
    let pod_identity = cfg.get("GIT_SSH_KEY_REMOTE").unwrap_or("/root/.ssh/id_ed25519");
    let fleet_cfg = match arena_core::proxy::ProxyConfig::from_config(cfg) {
        Ok(px) if !cfg.machine_names.is_empty() => arena_core::sshconfig::render_proxy(
            prefix, ssh_user, pod_identity, &px.proxy_host, px.starting_port, &cfg.machine_names,
        ),
        _ => arena_core::sshconfig::render_manual(prefix, ssh_user, pod_identity, pods),
    };
    // The backup archive (when this user can render it): its own managed block and its pinned
    // host key, after the fleet's. When it can't (no archive, or state it can't read) nothing is
    // sent — a pod's existing archive block is never removed (AD4). The read key itself goes by
    // scp ([`archive::install_read_key`]).
    let mut cmd = fleet_ssh_command(&fleet_pubkeys, &fleet_cfg);
    if let archive::PodArchive::Ready(a) = archive::pod_archive(cfg) {
        cmd.push_str(&archive::pod_archive_command(&a));
    }
    (cmd, fleet_pubkeys.len())
}

/// What `up`'s keys stage copies to each pod once it's set up (`None`: no keys stage): the key
/// files' per-host keys and the broadcast tokens when there are key files — and, with a backup
/// archive this user can render, the stage runs without any too: it writes each pod's archive
/// access (W7 live A-ckl9: a cohort without key files never got it).
pub(crate) fn up_keys_stage(cfg: &Config, keys_dir: &str, setup: bool) -> Option<KeySources> {
    if !setup {
        return None;
    }
    let keys = KeySources::load(cfg, keys_dir, None);
    (!keys.csv.is_empty() || archive::pod_archive(cfg).ready().is_some()).then_some(keys)
}

/// One pod's key write: the token export lines + the fleet SSH (authorized_keys +
/// `~/.ssh/config`), in one round-trip.
pub(crate) fn copy_keys_command(vars: &[(String, String)], fleet_ssh: &str) -> String {
    format!("{}; {}", arena_core::apikeys::remote_export_command(vars), fleet_ssh)
}

/// `pods copy-keys`: distribute API keys to each pod's shell. Per-host keys come from
/// `<keys_dir>/<provider>_api_keys.csv`; a Hugging Face token (from `--hf-token` or
/// config `HF_TOKEN`) is broadcast to every pod (for gated repos like Llama 3); each pod gets its
/// own Claude Code account's token (`crate::claude::stage`: one by spread if it has none; `cc_token`,
/// `--cc-token`, puts that one token on every pod instead). With a backup
/// archive this user can render, every reachable pod also gets its access (the ssh block, the
/// pinned key, the read key) — keys or not: a cohort without key files still reads the archive
/// from its pods (W7 live A-ckl9). And every reachable pod gets the fleet's ssh, keys or not
/// ([`fleet_ssh_for`]: every machine's keys authorized, the admin key's among them, and the
/// host map): `copy-keys` is how machines that already exist come to let in a key added since —
/// staff boxes with no per-host key included. With no key and no archive, that's all it writes.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_copy_keys(
    // How we reach pods: `SshRemote` for real, `FakeRemote` in tests. Each pod's write is
    // bounded by COPY_KEYS_TIMEOUT, so one wedged pod can't hang the command.
    remote: std::sync::Arc<dyn arena_core::remote::Remote>,
    cfg: &Config,
    keys_dir: &str,
    hf_token: Option<String>,
    cc_token: Option<String>,
    // The pods to write to, resolved by the caller (the operator's selection, or the pods
    // `setup` / `keys --copy` just handled).
    sel: &Selected,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    use arena_core::ssh::SshTarget;

    let mut keys = KeySources::load(cfg, keys_dir, hf_token);
    // The selected pods' OpenRouter rows, checked against OpenRouter's listing first: a key
    // deleted elsewhere is left out and said, not handed on (`checked`). Listed only when one
    // of them has such a row.
    let hosts: Vec<String> = sel.pods.iter().map(|p| p.name.clone()).collect();
    if hosts.iter().any(|h| keys.openrouter_secret(h).is_some()) {
        let or = openrouter_client(cfg);
        let listed = list_for_check(or.as_ref().map(|a| a as &dyn arena_core::openrouter::KeyApi)).await;
        for line in check_csv_keys(&mut keys, &hosts, listed.as_ref(), &Naming::from_config(cfg)) {
            eprintln!("{line}");
        }
    }
    let mut sources = keys.csv.clone();
    // "broadcast" = same value on every *selected* pod (vs per-host CSV keys) — not "all
    // pods", which misleadingly implies the whole fleet when the selection is narrower.
    for display in &keys.broadcast_names {
        sources.push(format!("{display} (broadcast to every selected pod)"));
    }
    // Claude Code: each pod its own account's token. A pool that can't be read is said, and fails
    // the run once the rest is written.
    let claude_plan = crate::claude::plan_line(cfg, cc_token.as_deref());
    match &claude_plan {
        Ok(Some(line)) => sources.push(line.clone()),
        Ok(None) => {}
        Err(e) => eprintln!("⚠ Claude Code: {e} — no Claude Code token goes out"),
    }
    let claude_on = matches!(claude_plan, Ok(Some(_)));

    // The backup archive's access goes to every pod it reaches, whatever the keys (A-ckl9).
    let pod_archive = archive::pod_archive(cfg);
    let archive_access = pod_archive.ready().cloned();
    let no_keys = sources.is_empty();
    if no_keys && archive_access.is_none() {
        println!(
            "Key sources: none — writing only the fleet's ssh (API keys: put `<provider>_api_keys.csv` in {keys_dir}/ \
             (openai/anthropic/openrouter), set HF_TOKEN / CLAUDE_TOKENS_FILE, or pass --hf-token)\n"
        );
    } else if no_keys {
        println!("Key sources: none — writing only the fleet's ssh and the backup archive's access\n");
    } else {
        println!("Key sources: {}\n", sources.join(", "));
    }

    // Build the per-pod var set (broadcast HF merged into every reachable selected pod).
    let pods = &sel.pods;
    /// One pod's write: its name, how to reach it, and the `(name, value)` vars it gets.
    type KeyJob = (String, SshTarget, Vec<(String, String)>);
    let mut jobs: Vec<KeyJob> = Vec::new();
    // The same pods, for the Claude Code stage (which needs each pod, not only its name).
    let mut claude_jobs: Vec<(arena_core::Pod, SshTarget)> = Vec::new();
    // Reachable pods that matched NO per-host key — they'd get broadcast tokens only.
    // Worth flagging loudly: a stale/misnamed CSV (e.g. last cohort's hosts) otherwise
    // hides behind a per-pod ✓, so nobody notices the per-host keys never landed.
    let mut broadcast_only: Vec<String> = Vec::new();
    let mut unreachable = 0;
    let mut stopped: Vec<&str> = Vec::new();
    for pod in pods {
        // A pod with no key of its own still gets the fleet's ssh (every machine's keys).
        let (vars, matched_per_host) = keys.vars_for(&pod.name);
        if arena_core::status::is_stopped(&pod.status) {
            eprintln!("{}", stopped_skip(pod));
            unreachable += 1;
            stopped.push(pod.name.as_str());
            continue;
        }
        match SshTarget::from_pod(pod, cfg) {
            Ok(t) => {
                if !matched_per_host && !keys.per_host.is_empty() {
                    broadcast_only.push(pod.name.clone());
                }
                claude_jobs.push((pod.clone(), t.clone()));
                jobs.push((pod.name.clone(), t, vars));
            }
            Err(_) => {
                let why = arena_core::fleet::no_endpoint_reason(pod).unwrap_or_else(|| "no SSH endpoint yet".into());
                eprintln!("skip {} — {why}", pod.name);
                unreachable += 1;
            }
        }
    }
    if jobs.is_empty() {
        // Pods the operator named, all without an endpoint: a failure, not "nothing to do" —
        // said as "stopped" when that's why, as every SSH command says it.
        if sel.named && unreachable > 0 {
            if stopped.len() == unreachable {
                anyhow::bail!("{}", all_stopped_error(&stopped));
            }
            anyhow::bail!("none of the selected pods ({}) has an SSH endpoint yet — check `arena pods list`", sel.names());
        }
        println!("(no reachable pods)");
        return Ok(());
    }
    if !broadcast_only.is_empty() {
        eprintln!(
            "⚠ {} reachable pod(s) matched NO per-host key — broadcast tokens only: {}\n  \
             (per-host keys come from <provider>_api_keys.csv, matched on exact pod name; \
             check that file actually lists these hosts)\n",
            broadcast_only.len(),
            broadcast_only.join(", ")
        );
    }

    // Also wire fleet SSH on every reachable pod (see `fleet_ssh_for`). Built once — same
    // on every pod.
    let (fleet_ssh, fleet_keys) = fleet_ssh_for(cfg, pods);
    println!(
        "Also on each target ({} pod(s)): authorize {} fleet key(s) + write that pod's own \
         ~/.ssh/config (the fleet host map, so it can ssh the others).",
        jobs.len(),
        fleet_keys
    );

    if let Some(w) = pod_archive.warning() {
        eprintln!("{w}");
    }
    if let Some(a) = &archive_access {
        println!(
            "Also the backup archive's access on each: the `{}` ssh block, its pinned host key, and the read key \
             (copied by scp to ~/{}).",
            a.block.lines().next().unwrap_or("Host archive").trim_start_matches("Host "),
            arena_core::archive::render::POD_KEY
        );
    }

    if dry_run {
        println!("Dry-run — would set on {} pod(s) (values redacted):\n", jobs.len());
        let claude_pods: Vec<arena_core::Pod> = claude_jobs.iter().map(|(p, _)| p.clone()).collect();
        let claude_would = if claude_on { crate::claude::preview(cfg, &claude_pods, cc_token.as_deref()).unwrap_or_default() } else { vec![] };
        for (name, _, vars) in &jobs {
            let mut names: Vec<String> = vars.iter().map(|(k, _)| k.clone()).collect();
            if let Some((_, what)) = claude_would.iter().find(|(n, _)| n == name) {
                names.push(what.clone());
            }
            let what = match (names.is_empty(), archive_access.is_some()) {
                (false, _) => names.join(", "),
                (true, true) => "(no keys — the fleet's ssh and the backup archive's access)".to_string(),
                (true, false) => "(no keys — the fleet's ssh)".to_string(),
            };
            println!("  {name:<22} {what}");
        }
        println!("\nPreview only — run without --dry-run to write to ~/.bashrc & ~/.zshrc.");
        return Ok(());
    }
    let names = jobs.iter().map(|(n, _, _)| n.as_str()).collect::<Vec<_>>().join(", ");
    let what = if no_keys && archive_access.is_some() {
        "authorize the fleet keys and write ~/.ssh/config (the fleet map and the backup archive's block)"
    } else if no_keys {
        "authorize the fleet keys and write ~/.ssh/config (the fleet map)"
    } else if claude_on {
        "export API keys into ~/.bashrc & ~/.zshrc (each pod its own Claude Code account's token, replacing any there), \
         authorize the fleet keys, and write ~/.ssh/config"
    } else {
        "export API keys into ~/.bashrc & ~/.zshrc, authorize the fleet keys, and write ~/.ssh/config"
    };
    if !confirm(yes, &format!("Will {what} on {} pod(s): {names}", jobs.len()))? {
        println!("aborted.");
        return Ok(());
    }

    let total = jobs.len();
    let mut set = tokio::task::JoinSet::new();
    for (name, t, vars) in jobs {
        let cmd = copy_keys_command(&vars, &fleet_ssh);
        let remote = remote.clone();
        let access = archive_access.clone();
        set.spawn(async move {
            let res = remote.exec(&t, &cmd, Some(COPY_KEYS_TIMEOUT)).await;
            // The archive's read key, by scp, once the keys write (which makes ~/.ssh) went through.
            let res = match (res, access) {
                (Ok(o), Some(a)) if o.success => match archive::install_read_key(remote.as_ref(), &t, &a, COPY_KEYS_TIMEOUT).await {
                    Ok(()) => Ok(o),
                    Err(e) => Ok(arena_core::ssh::SshOutput { success: false, code: None, stdout: o.stdout, stderr: e }),
                },
                (res, _) => res,
            };
            (name, res)
        });
    }
    let (mut ok, mut failed, mut done) = (0, 0, 0);
    while let Some(joined) = set.join_next().await {
        done += 1;
        let Ok((name, res)) = joined else { continue };
        match res {
            Ok(o) if o.success => {
                println!("[{done}/{total}] ✓ {name}");
                ok += 1;
            }
            Ok(o) => {
                println!("[{done}/{total}] ✗ {name}: {}", o.stderr.trim());
                failed += 1;
            }
            Err(e) => {
                println!("[{done}/{total}] ✗ {name}: {e}");
                failed += 1;
            }
        }
    }
    println!("\nDone: {ok} updated, {failed} failed.");
    // Each pod's own Claude Code account's token (one by spread for a pod with none), on ssh's
    // stdin — never in the command above.
    // With no pool too: a pod that's off still gets a pending removal (review r3).
    let claude_failed = crate::claude::stage(remote.as_ref(), cfg, &claude_jobs, cc_token.as_deref()).await;
    if failed > 0 {
        anyhow::bail!("{failed} pod(s) failed");
    }
    if claude_failed > 0 {
        anyhow::bail!("{claude_failed} pod(s) didn't get their Claude Code token (`arena claude push` retries)");
    }
    Ok(())
}

/// Where generated OpenRouter keys are persisted (also where `copy-keys` reads them): the
/// [`OPENROUTER_KEYS_LINK`] in the fleet's [`keys_dir`].
pub(crate) fn openrouter_keys_csv(cfg: &Config) -> PathBuf {
    PathBuf::from(keys_dir(cfg)).join(OPENROUTER_KEYS_LINK)
}
/// The OpenRouter keys CSV's file name, inside whichever keys dir is in use.
pub(crate) use arena_core::config::OPENROUTER_KEYS_LINK;

/// Resolve which machines a `keys` action targets, with the shared selector. A key belongs
/// to a *machine name*, which can exist before its pod (mint keys, then `up`) or after it
/// (revoke a terminated pod's key) — so names and ranges resolve against the current pods
/// **plus** every MACHINE_NAME_LIST name without a pod **plus** `known`: machines that
/// still have a key (an OpenRouter key under this cohort's naming, or a row in the keys
/// CSV — see [`key_holders`]), so a lingering key whose name left the list can still be
/// revoked by name. A name in none of them is still a typo error (with the closest of all
/// of them suggested). `--all` keeps its meaning: every *current* pod. Returns the full
/// machine names, and the selection of those that are pods (what `--copy` can reach).
pub(crate) async fn keys_targets(
    provider: &dyn Provider,
    cfg: &Config,
    sel: &SelectArgs,
    known: &[String],
) -> Result<(Vec<String>, Selected)> {
    let naming = Naming::from_config(cfg);
    let selector = Selector::parse(sel, &naming)?;
    if selector.is_unscoped() {
        anyhow::bail!("name the machines (names or a range like apple..mayor), or pass --all");
    }
    let policy = arena_core::retry::RetryPolicy::default();
    let mut pods = arena_core::retry::retrying(&policy, || provider.list_pods()).await.context("listing pods")?;
    // As in `select`: a listing may omit GPU counts, and an unknown count never matches —
    // without this, `--gpus N` would silently skip such pods.
    if selector.gpus.is_some() {
        if let Some(warning) = enrich_best_effort(provider, &mut pods, ENRICH_TIMEOUT).await {
            eprintln!("{warning}");
        }
    }
    let mut universe = pods.clone();
    if !selector.all {
        // Spare list names and key holders, as pod-less stand-ins (no id, no provider:
        // `--on`/`--gpus` skip them).
        let spare = cfg.machine_names.iter().map(|e| arena_core::naming::qualify(naming.prefix, e));
        for name in spare.chain(known.iter().cloned()) {
            if !universe.iter().any(|p| p.name == name) {
                universe.push(arena_core::Pod { name, ..Default::default() });
            }
        }
    }
    let picked = selector.resolve(&naming, &universe)?;
    let names = picked.iter().map(|&i| universe[i].name.clone()).collect();
    let live = picked.into_iter().filter(|&i| i < pods.len()).map(|i| pods[i].clone()).collect();
    Ok((names, Selected { pods: live, named: true }))
}

/// Machines that still hold a key, for [`keys_targets`]: every host in the keys CSV, and
/// every OpenRouter key whose name is its own [`key_name`](arena_core::openrouter::key_name)
/// — i.e. named under this cohort's prefix (or an absolute list entry), as `keys gen` would
/// name it; another cohort's `arena7-x` is left out (under this prefix it would be looked
/// up as `{prefix}-arena7-x`, a key that doesn't exist).
pub(crate) fn key_holders(naming: &Naming, keys: &[arena_core::openrouter::KeyInfo], csv: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let rows = csv.map(arena_core::apikeys::parse_csv).unwrap_or_default();
    let named = keys
        .iter()
        .filter_map(|k| k.name.clone())
        .filter(|n| arena_core::openrouter::key_name(naming.prefix, naming.list, n) == *n);
    for name in rows.into_iter().map(|(h, _)| h).chain(named) {
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// `keys gen|rotate --copy`: push the fresh keys to the targeted machines that are pods
/// (a name without a pod has nowhere to go yet — `setup`/`copy-keys` deliver it later).
async fn copy_new_keys(remote: Arc<dyn Remote>, cfg: &Config, dir: &std::path::Path, live: &Selected, yes: bool) -> Result<()> {
    if live.pods.is_empty() {
        println!("(--copy: none of these machines has a pod yet — run `arena pods copy-keys` once they're up)");
        return Ok(());
    }
    handle_copy_keys(remote, cfg, &dir.display().to_string(), None, None, live, false, yes).await
}

/// Persist one machine's freshly minted OpenRouter key into the per-host CSV in `dir`
/// (upsert), holding the keys folder for it. A new file is seeded with a header naming the arena
/// iteration (`prefix`) so the CSV is self-documenting about which cohort the keys belong to. The
/// commands hold the folder from their check to their save and write with
/// [`write_openrouter_key_held`]; this is for tests.
#[cfg(test)]
pub(crate) fn write_openrouter_key(dir: &std::path::Path, host: &str, secret: &str, prefix: &str) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let _held = lock_keys_dir(dir).with_context(|| format!("locking {}", dir.display()))?;
    write_openrouter_key_held(dir, host, secret, prefix)
}

/// [`write_openrouter_key`], for a caller already holding the keys folder ([`lock_keys_dir`]) —
/// one that checks what OpenRouter has and mints under it, so two arenas never both mint a key
/// for one name.
pub(crate) fn write_openrouter_key_held(dir: &std::path::Path, host: &str, secret: &str, prefix: &str) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let target = ensure_cohort_keys_file(dir, prefix)?;
    let mut existing = std::fs::read_to_string(&target).unwrap_or_default();
    if existing.trim().is_empty() {
        existing = cohort_keys_header(prefix);
    }
    let updated = arena_core::apikeys::upsert_csv(&existing, host, secret);
    write_secret_file(&target, &updated).with_context(|| format!("writing {}", target.display()))?;
    Ok(())
}

/// How long a change to the keys CSV waits for another arena's. A write holds it for one read and
/// one atomic write; a mint (`pods up`, `keys gen|rotate`, a handover) for its listing and its
/// creates — seconds per key.
const KEYS_CSV_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// An exclusive lock on the keys folder `dir`: for one read-change-write of its CSV, or for a mint
/// from its check of OpenRouter's listing to its saved secret. Two `pods up` minting at once (an
/// organiser's parallel terminals) each read the CSV, added their row and wrote it back — the
/// second write dropped the first's row, and that key's secret (OpenRouter shows it once) with it;
/// and an `up` waiting at its prompt while `keys gen` minted the same name minted a second key.
/// `Ok(None)` where the folder can't be locked (as before: the atomic write alone); `Err` when
/// another arena held it past [`KEYS_CSV_LOCK_WAIT`].
pub(crate) fn lock_keys_dir(dir: &std::path::Path) -> std::io::Result<Option<std::fs::File>> {
    arena_core::snapshot::lock_dir_within(dir, KEYS_CSV_LOCK_WAIT)
}

/// Write a file of secrets (the OpenRouter keys CSV) atomically, readable by its owner
/// only. The text goes into a sibling temp file created 0600, is synced, renamed over the
/// target, and the folder synced: a failure part-way leaves the old file whole, and a crash
/// after success can't undo the rename (OpenRouter shows a key's secret once). The target
/// ends up 0600 whatever it was — an older file, created 0664 by a plain `fs::write`, is
/// tightened. A symlink is followed as `replace_file` follows one (its target replaced, the
/// link kept, even before the target exists): arena's own link points inside its folder,
/// an operator's may point elsewhere, and refusing it would lose keys already minted
/// (review round 3). Whoever can write the keys folder can read the keys anyway. The owner
/// is kept: the old file's, or for a new file the folder's (a `sudo arena keys gen` leaves
/// it the operator's). Needs a writable folder (the temp file lives there).
pub(crate) fn write_secret_file(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let folder = |p: &std::path::Path| match p.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => std::path::PathBuf::from("."),
    };
    let real = std::fs::canonicalize(path).unwrap_or_else(|_| match std::fs::read_link(path) {
        Ok(t) if t.is_absolute() => t,
        Ok(t) => folder(path).join(t),
        Err(_) => path.to_path_buf(),
    });
    let (dir, Some(name)) = (folder(&real), real.file_name()) else {
        return Err(std::io::Error::other(format!("{}: no file name", real.display())));
    };
    let tmp = dir.join(format!(".{}.arena-tmp-{}", name.to_string_lossy(), std::process::id()));
    let _ = std::fs::remove_file(&tmp); // this process's own leftover, if any
    let written = (|| {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        let owner = std::fs::metadata(&real).or_else(|_| std::fs::metadata(&dir)).ok();
        if let Some(meta) = owner {
            let _ = std::os::unix::fs::fchown(&f, Some(meta.uid()), Some(meta.gid()));
        }
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &real)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return written;
    }
    // The rename is done (the new text is what's there); a folder that can't be synced is
    // said, not returned as a failure — a caller would take that for "not written".
    if let Err(e) = std::fs::File::open(&dir).and_then(|d| d.sync_all()) {
        eprintln!("warning: wrote {}, but its folder couldn't be synced ({e}): a crash now could undo it", real.display());
    }
    Ok(())
}

/// The first lines of a new cohort keys file.
pub(crate) fn cohort_keys_header(prefix: &str) -> String {
    format!("# OpenRouter API keys — arena iteration: {prefix}\n# host,key (one runtime key per machine; managed by `arena keys`)\n")
}

/// The per-cohort keys file's name for `prefix` (what the `openrouter_api_keys.csv` link
/// points at while that prefix is current).
pub(crate) fn cohort_keys_file(prefix: &str) -> String {
    format!("{prefix}_openrouter_keys.csv")
}

/// Keys live per cohort in `<dir>/<prefix>_openrouter_keys.csv`; the canonical
/// `<dir>/openrouter_api_keys.csv` (what copy-keys reads) is a symlink to the current one.
/// Repoints the symlink when the prefix changes, so a new cohort's keys never get written
/// into the previous cohort's file. A legacy regular file is moved aside to `.bak`.
pub(crate) fn ensure_cohort_keys_file(dir: &std::path::Path, prefix: &str) -> Result<PathBuf> {
    let file = cohort_keys_file(prefix);
    let target = dir.join(&file);
    let link = dir.join(OPENROUTER_KEYS_LINK);
    match std::fs::symlink_metadata(&link) {
        Ok(m) if m.file_type().is_symlink() => {
            if std::fs::read_link(&link)?.file_name() != Some(std::ffi::OsStr::new(&file)) {
                std::fs::remove_file(&link)?;
                std::os::unix::fs::symlink(&file, &link)?;
                println!("(keys) {} now → {file}", link.display());
            }
        }
        Ok(_) => {
            let bak = PathBuf::from(format!("{}.pre-{prefix}.bak", link.display()));
            anyhow::ensure!(!bak.exists(), "{} already exists — sort out {} by hand", bak.display(), link.display());
            std::fs::rename(&link, &bak)?;
            println!("(keys) moved old {} aside to {}", link.display(), bak.display());
            std::os::unix::fs::symlink(&file, &link)?;
        }
        Err(_) => std::os::unix::fs::symlink(&file, &link)?,
    }
    Ok(target)
}

/// `arena keys`: manage OpenRouter runtime keys (generate / list / rotate / revoke) via
/// the provisioning API. Secrets are saved to `keys/openrouter_api_keys.csv` (which
/// `pods copy-keys` then distributes); rotation/revocation find a key by its name
/// (`<prefix>-<machine>`), so no local hash bookkeeping is needed.
pub(crate) async fn handle_keys(
    cmd: KeysCmd,
    provider: &dyn Provider,
    // How `--copy` reaches pods (via `copy-keys`): `SshRemote` for real.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    yes: bool,
) -> Result<()> {
    use arena_core::openrouter::OpenRouter;

    // `which` just reads the local CSV — no provisioning key / network needed.
    if let KeysCmd::Which = cmd {
        let csv = openrouter_keys_csv(cfg);
        let abs = std::fs::canonicalize(&csv)
            .or_else(|_| std::path::absolute(&csv))
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| csv.display().to_string());
        let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
        if let Some(note) = csv.parent().and_then(|dir| arena_core::config::keys_link_note(dir, prefix)) {
            println!("⚠ {note}");
        }
        match std::fs::read_to_string(&csv) {
            Ok(text) => {
                let rows = arena_core::apikeys::parse_csv(&text);
                println!("OpenRouter keys file: {abs}");
                println!("  ✓ exists · {} key(s) for: {}", rows.len(),
                    rows.iter().map(|(h, _)| h.as_str()).collect::<Vec<_>>().join(", "));
                println!("\n(distribute with `arena pods copy-keys`; values not shown)");
            }
            Err(_) => {
                println!("OpenRouter keys file: {abs}");
                println!("  · not created yet — run `arena keys gen --all` to mint keys");
            }
        }
        return Ok(());
    }

    let prov_key = cfg
        .get("OPENROUTER_PROVISIONING_KEY")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "set OPENROUTER_PROVISIONING_KEY first (openrouter.ai → Settings → \
                 Management Keys), e.g. `arena config set OPENROUTER_PROVISIONING_KEY <key>`"
            )
        })?;
    let or = OpenRouter::new(prov_key).with_workspace(cfg.get("OPENROUTER_WORKSPACE"));
    keys_with(cmd, provider, remote, cfg, &or, std::path::Path::new(&keys_dir(cfg)), yes).await
}

/// `keys list`'s table: each key's cap, what it spent in the cap's period, today and this
/// month (UTC) and all told, then a total row. Pure, for testing.
fn keys_table(keys: &[&arena_core::openrouter::KeyInfo]) -> String {
    use arena_core::fleet::fmt_money;
    use std::fmt::Write as _;
    let money = |x: Option<f64>| x.map(|x| fmt_money("$", x)).unwrap_or_else(|| "-".into());
    let mut out = format!("{:<28} {:>9} {:>9} {:>9} {:>9} {:>9}  HASH\n", "NAME", "LIMIT", "SPENT", "TODAY", "MONTH", "ALL");
    let (mut today, mut month, mut all) = (0.0, 0.0, 0.0);
    for k in keys {
        let u = arena_core::openrouter::KeyUsage::of(k);
        // A cap as it was set (`$0.05`, not the hourly prices' `$0.050`).
        let lim = match (k.limit, u.reset.as_deref()) {
            (Some(l), Some(r)) => format!("{}/{}", arena_core::openrouter::exact_money(l), &r[..1]),
            (Some(l), None) => arena_core::openrouter::exact_money(l),
            (None, _) => "-".into(),
        };
        let short = &k.hash[..k.hash.len().min(12)];
        let dis = if k.disabled { "  (disabled)" } else { "" };
        let _ = writeln!(
            out,
            "{:<28} {lim:>9} {:>9} {:>9} {:>9} {:>9}  {short}{dis}",
            k.name.clone().unwrap_or_default(),
            money(u.spent),
            money(u.today),
            money(u.month),
            money(u.total),
        );
        today += u.today.unwrap_or(0.0);
        month += u.month.unwrap_or(0.0);
        all += u.total.unwrap_or(0.0);
    }
    let _ = writeln!(out, "{:<28} {:>9} {:>9} {:>9} {:>9} {:>9}", format!("total ({} keys)", keys.len()), "", "", fmt_money("$", today), fmt_money("$", month), fmt_money("$", all));
    out
}

/// What a `keys` dry run says it would do with one machine's key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeysAction {
    Gen,
    Rotate,
    Revoke,
}

/// The `keys gen|rotate|revoke --dry-run` preview (pure, so tested): one line per resolved
/// machine — the key name it goes by, and what happens to it given `existing` (how many
/// keys already go by that name; disabled ones for `gen`'s skip excluded).
pub(crate) fn keys_preview(action: KeysAction, limit: f64, rows: &[(String, String, usize)]) -> Vec<String> {
    let n = rows.len();
    let mut out = vec![match action {
        KeysAction::Gen => format!("Dry-run — would mint a ${limit:.2}-cap key for {n} machine(s):"),
        KeysAction::Rotate => format!("Dry-run — would delete + re-mint a key (cap ${limit:.2}) for {n} machine(s):"),
        KeysAction::Revoke => format!("Dry-run — would revoke the key for {n} machine(s):"),
    }];
    for (host, kn, existing) in rows {
        let note = match (action, existing) {
            (KeysAction::Gen, 0) => String::new(),
            (KeysAction::Gen, _) => format!(" — already exists, would skip (`arena keys rotate {host}` replaces it)"),
            (KeysAction::Rotate, 0) => " — none yet, would just mint".into(),
            (KeysAction::Rotate, k) => format!(" — deletes {k} existing"),
            (KeysAction::Revoke, 0) => " — no such key, nothing to revoke".into(),
            (KeysAction::Revoke, k) => format!(" — deletes {k}"),
        };
        out.push(format!("  {kn}{note}"));
    }
    out
}

/// `arena keys list|gen|rotate|revoke` against `api` (the OpenRouter provisioning API for
/// real; a fake in tests), keeping the secrets in the per-cohort CSV in `dir` (`./keys`; a
/// temp dir in tests). One full key listing up front serves the target resolution (machines
/// that still hold a key are valid targets), the dry run, and the skip/delete decisions.
pub(crate) async fn keys_with(
    cmd: KeysCmd,
    provider: &dyn Provider,
    // How `--copy` reaches pods (via `copy-keys`): `SshRemote` for real.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    api: &dyn arena_core::openrouter::KeyApi,
    dir: &std::path::Path,
    yes: bool,
) -> Result<()> {
    use arena_core::openrouter::key_name;
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena").to_string();
    let default_limit = cfg.get_parsed::<f64>("OPENROUTER_KEY_LIMIT").unwrap_or(5.0);
    let naming = Naming::from_config(cfg);
    let csv_path = dir.join(OPENROUTER_KEYS_LINK);
    let (action, sel, limit, copy, dry_run) = match cmd {
        KeysCmd::Which => unreachable!("handled before the provisioning-key check"),
        KeysCmd::List { all } => {
            let mut keys = api.list_keys().await.context("listing OpenRouter keys")?;
            keys.sort_by(|a, b| a.name.cmp(&b.name));
            if keys.is_empty() {
                println!("(no provisioned OpenRouter keys)");
                return Ok(());
            }
            // Default to this iteration's keys (named `<prefix>-…`); count the rest.
            let needle = format!("{prefix}-");
            let total = keys.len();
            let shown: Vec<_> = if all {
                keys.iter().collect()
            } else {
                keys.iter()
                    .filter(|k| k.name.as_deref().map(|n| n.starts_with(&needle)).unwrap_or(false))
                    .collect()
            };
            let excluded = total - shown.len();

            if shown.is_empty() {
                println!("(no {prefix}-* keys; {excluded} other key(s) hidden — use --all to show all)");
                return Ok(());
            }
            print!("{}", keys_table(&shown));
            if !all && excluded > 0 {
                println!("\n(excluded {excluded} non-{prefix} key(s) — use --all to show all)");
            }
            return Ok(());
        }
        KeysCmd::Gen { sel, limit, copy, dry_run } => (KeysAction::Gen, sel, limit, copy, dry_run),
        KeysCmd::Rotate { sel, limit, copy, dry_run } => (KeysAction::Rotate, sel, limit, copy, dry_run),
        KeysCmd::Revoke { sel, dry_run } => (KeysAction::Revoke, sel, None, false, dry_run),
        KeysCmd::Limit(a) => return key_limits::limit(a, provider, cfg, api, dir, yes).await,
        KeysCmd::Pause(a) => return key_limits::pause(a, false, provider, cfg, api, dir, yes).await,
        KeysCmd::Resume(a) => return key_limits::pause(a, true, provider, cfg, api, dir, yes).await,
    };
    let limit = limit.unwrap_or(default_limit);
    let existing = api.list_keys().await.context("listing OpenRouter keys")?;
    let known = key_holders(&naming, &existing, std::fs::read_to_string(&csv_path).ok().as_deref());
    let (names, live) = keys_targets(provider, cfg, &sel.args(), &known).await?;
    if names.is_empty() {
        println!("(no target machines)");
        return Ok(());
    }
    // Per machine: the name its new key gets, and the existing keys that are its (gen skips
    // only for an enabled one; rotate/revoke delete them all — a leaked duplicate must die).
    type Plan<'k> = Vec<(String, String, Vec<&'k arena_core::openrouter::KeyInfo>)>;
    let plan_from = |existing: &[arena_core::openrouter::KeyInfo]| -> Vec<(String, String, Vec<usize>)> {
        names
            .iter()
            .map(|host| {
                let theirs = machine_key_names(&naming, host);
                let keys = existing
                    .iter()
                    .enumerate()
                    .filter(|(_, k)| k.name.as_deref().is_some_and(|n| theirs.iter().any(|t| t == n)))
                    .filter(|(_, k)| action != KeysAction::Gen || !k.disabled)
                    .map(|(i, _)| i)
                    .collect();
                (host.clone(), key_name(&prefix, &cfg.machine_names, host), keys)
            })
            .collect()
    };
    fn resolve<'k>(existing: &'k [arena_core::openrouter::KeyInfo], rows: Vec<(String, String, Vec<usize>)>) -> Plan<'k> {
        rows.into_iter().map(|(h, kn, ix)| (h, kn, ix.into_iter().map(|i| &existing[i]).collect())).collect()
    }
    let plan = resolve(&existing, plan_from(&existing));
    if dry_run {
        let rows: Vec<(String, String, usize)> = plan.iter().map(|(h, kn, ks)| (h.clone(), kn.clone(), ks.len())).collect();
        for line in keys_preview(action, limit, &rows) {
            println!("{line}");
        }
        return Ok(());
    }
    let what = match action {
        KeysAction::Gen => format!("Will mint {} OpenRouter key(s) (cap ${limit:.2} each) and write {}", names.len(), csv_path.display()),
        KeysAction::Rotate => format!("Will DELETE + re-mint {} OpenRouter key(s) (cap ${limit:.2})", names.len()),
        KeysAction::Revoke => format!("Will DELETE {} OpenRouter key(s) — no regenerate", names.len()),
    };
    if !confirm(yes, &format!("{what}: {}", names.join(", ")))? {
        println!("aborted.");
        return Ok(());
    }
    // From here to the last saved secret the keys folder is held, and what OpenRouter has is read
    // again under it: a key another arena minted while this one waited at its prompt (`pods up`,
    // another `keys gen`) is seen — `gen` skips it rather than mint a second one for the name.
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let held = lock_keys_dir(dir).with_context(|| format!("locking {}", dir.display()))?;
    let listed_at = arena_core::snapshot::unix_now();
    let existing = api.list_keys().await.context("listing OpenRouter keys again")?;
    let plan = resolve(&existing, plan_from(&existing));
    // Each key's usage, recorded before anything is deleted: a rotated or revoked key's
    // spend stays with whoever it was minted for (`arena spend`).
    let tracking = spend::SpendCtx::from_config(cfg);
    tracking.keys(&existing, listed_at);
    let (mut ok, mut skipped, mut failed) = (0, 0, 0);
    for (host, kn, theirs) in &plan {
        if action == KeysAction::Gen && !theirs.is_empty() {
            println!("= {host}: '{kn}' already exists — use `arena keys rotate {host}` to replace");
            skipped += 1;
            continue;
        }
        if action == KeysAction::Revoke {
            if theirs.is_empty() {
                println!("= {host}: no key named '{kn}'");
                skipped += 1;
                continue;
            }
            let (deleted, errs) = delete_keys(api, theirs).await;
            tracking.keys_deleted(theirs, &deleted);
            if errs.is_empty() {
                println!("✓ {host}: revoked");
                ok += 1;
            } else {
                eprintln!("✗ {host}: delete: {}", errs.join("; "));
                failed += 1;
            }
            continue;
        }
        match rotate_key(api, kn, theirs, limit).await {
            Ok((deleted, ck)) => {
                tracking.keys_deleted(theirs, &deleted);
                tracking.key_minted(&ck.hash, host, None);
                write_openrouter_key_held(dir, host, &ck.secret, &prefix)?;
                println!("✓ {host}: {} {}", if action == KeysAction::Gen { "minted" } else { "rotated" }, &ck.hash[..ck.hash.len().min(12)]);
                ok += 1;
            }
            Err((deleted, why)) => {
                tracking.keys_deleted(theirs, &deleted);
                eprintln!("✗ {host}: {why}");
                failed += 1;
            }
        }
    }
    drop(held);
    match action {
        KeysAction::Gen => println!("\nGenerated {ok}, skipped {skipped}, failed {failed} → {}", csv_path.display()),
        KeysAction::Rotate => println!("\nRotated {ok}, failed {failed}."),
        KeysAction::Revoke => {
            println!("\nRevoked {ok}, none-found {skipped}, failed {failed}. (The CSV is left as-is; rotate to refresh.)")
        }
    }
    if copy && ok > 0 {
        println!();
        copy_new_keys(remote.clone(), cfg, dir, &live, yes).await?;
    }
    Ok(())
}

/// Delete `keys` one by one: the hashes deleted, and the errors.
async fn delete_keys(api: &dyn arena_core::openrouter::KeyApi, keys: &[&arena_core::openrouter::KeyInfo]) -> (Vec<String>, Vec<String>) {
    let (mut deleted, mut errs) = (Vec::new(), Vec::new());
    for k in keys {
        match api.delete_key(&k.hash).await {
            Ok(()) => deleted.push(k.hash.clone()),
            Err(e) => errs.push(e.to_string()),
        }
    }
    (deleted, errs)
}

/// `keys rotate` (and `gen`, with nothing to delete) for one machine — and a handover's: delete
/// `theirs` (its existing keys), then mint `kn` with a `limit` cap (the caller saves its
/// secret). Nothing is minted while an old key is still alive. Either way, the hashes
/// deleted; `Err` says which step failed.
pub(crate) async fn rotate_key(
    api: &dyn arena_core::openrouter::KeyApi,
    kn: &str,
    theirs: &[&arena_core::openrouter::KeyInfo],
    limit: f64,
) -> std::result::Result<(Vec<String>, arena_core::openrouter::CreatedKey), (Vec<String>, String)> {
    let (deleted, errs) = delete_keys(api, theirs).await;
    if !errs.is_empty() {
        return Err((deleted, format!("delete: {}", errs.join("; "))));
    }
    match api.create_key(kn, Some(limit)).await {
        Ok(ck) => Ok((deleted, ck)),
        Err(e) => Err((deleted, format!("create: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    /// Keys minted at once (several `pods up` in parallel terminals) all land in the CSV: each
    /// read-change-write holds the keys folder, so none drops another's row (and its secret).
    #[test]
    fn keys_written_at_once_all_land() {
        let dir = std::env::temp_dir().join(format!("arena-keys-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let hosts: Vec<String> = (0..12).map(|i| format!("devtest-m{i}")).collect();
        std::thread::scope(|s| {
            for h in &hosts {
                let dir = &dir;
                s.spawn(move || super::write_openrouter_key(dir, h, &format!("sk-or-v1-{h}"), "devtest").unwrap());
            }
        });
        let text = std::fs::read_to_string(dir.join("devtest_openrouter_keys.csv")).unwrap();
        for h in &hosts {
            assert!(text.contains(&format!("{h},sk-or-v1-{h}\n")), "{h} lost:\n{text}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn minted_keys_are_written_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("arena-keys-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        super::write_openrouter_key(&dir, "devtest-apple", "sk-or-v1-A", "devtest").unwrap();
        let file = dir.join("devtest_openrouter_keys.csv");
        let link = dir.join("openrouter_api_keys.csv");
        assert_eq!(mode(&file), 0o600);
        // An older file at 0664 is tightened on its next write.
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o664)).unwrap();
        super::write_openrouter_key(&dir, "devtest-bloom", "sk-or-v1-B", "devtest").unwrap();
        assert_eq!(mode(&file), 0o600);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("devtest-apple,sk-or-v1-A") && text.contains("devtest-bloom,sk-or-v1-B"), "{text}");
        // Through the link: the file is replaced, the link kept.
        super::write_secret_file(&link, "x\n").unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!((std::fs::read_to_string(&file).unwrap().as_str(), mode(&file)), ("x\n", 0o600));
        // A write that can't finish leaves the old file whole (here, as root too: a folder
        // sits where the temp file would go).
        let blocker = dir.join(format!(".devtest_openrouter_keys.csv.arena-tmp-{}", std::process::id()));
        std::fs::create_dir(&blocker).unwrap();
        assert!(super::write_secret_file(&link, "lost\n").is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "x\n");
        std::fs::remove_dir(&blocker).unwrap();
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2, "no temp file left");
        // A link to another folder (an operator's secrets folder) is followed, as before.
        let elsewhere = std::env::temp_dir().join(format!("arena-keys-elsewhere-{}", std::process::id()));
        std::fs::create_dir_all(&elsewhere).unwrap();
        let target = elsewhere.join("keys.csv");
        std::fs::write(&target, "old\n").unwrap();
        let away = dir.join("away_link.csv");
        std::os::unix::fs::symlink(&target, &away).unwrap();
        super::write_secret_file(&away, "new\n").unwrap();
        assert!(std::fs::symlink_metadata(&away).unwrap().file_type().is_symlink());
        assert_eq!((std::fs::read_to_string(&target).unwrap().as_str(), mode(&target)), ("new\n", 0o600));
        let _ = std::fs::remove_dir_all(&elsewhere);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keys_table_shows_spend_per_period_and_a_total() {
        use arena_core::openrouter::KeyInfo;
        let a = KeyInfo {
            hash: "0123456789abcdef".into(),
            name: Some("devtest-apple".into()),
            limit: Some(5.0),
            limit_remaining: Some(3.77),
            limit_reset: Some("monthly".into()),
            usage: Some(4.56),
            usage_daily: Some(0.2),
            usage_monthly: Some(1.23),
            ..Default::default()
        };
        let b = KeyInfo { hash: "ff".into(), name: Some("devtest-bloom".into()), disabled: true, usage: Some(0.5), ..Default::default() };
        let t = super::keys_table(&[&a, &b]);
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines[0], "NAME                             LIMIT     SPENT     TODAY     MONTH       ALL  HASH");
        assert_eq!(lines[1], "devtest-apple                  $5.00/m     $1.23     $0.20     $1.23     $4.56  0123456789ab");
        assert_eq!(lines[2], "devtest-bloom                        -     $0.50         -         -     $0.50  ff  (disabled)");
        assert_eq!(lines[3], "total (2 keys)                                       $0.20     $1.23     $5.06");
    }
}
