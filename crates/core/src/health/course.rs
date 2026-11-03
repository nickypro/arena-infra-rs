//! The course's half of the deep check (and all of `pods test --course`): what a participant uses
//! beside the machine. Their way in (the proxy port), their backups (the deploy key, the last git
//! backup), the course repo's branch, Claude Code, the notebook kernel of the course env, and uv.
//!
//! **Why.** Each of these failed a participant while every machine check passed: a proxy route
//! left on an old pod after a replace, a deploy key GitHub refuses, a pod left on `main` (no
//! backup commits there) or in the middle of a rebase, a Claude Code without its token, a
//! notebook kernel that can't import torch, `uv` off PATH. The check script measures them (the
//! `repo.*`, `push.*`, `claude.*`, `kernel.*`, `uv.*` and `id.*` facts: [`course_fact`]); here
//! they are judged against what the control box knows ([`CourseBook`]: the proxy's ports and what
//! each one reached, the backups' record, today's branch, `arena claude`'s pushes), each check
//! saying what to run.
//!
//! **Read-only.** The repo is read through git's metadata (refs, config, the operation markers)
//! with no optional locks, never a file of the work. The push is a dry run. The kernel runs with
//! its runtime and IPython dirs in the script's temp dir and is stopped before the script ends.
//! The proxy is only connected through; nothing is written to it.
//!
//! **Not the machine.** None of these is the machine's own fault: a FAIL is
//! [`super::FailureKind::Course`] — `up --check` never replaces a machine for it, never retries it
//! and never remembers its host as bad — but for the kernel's: a course env whose kernel can't
//! import torch is the image's or setup's ([`super::FailureKind::Software`]), the same on a new
//! machine. Unknown is never a FAIL: facts an older script didn't report read as not reported.

use std::collections::BTreeMap;

use serde::Serialize;

use super::{Check, DeepFacts, PodHealth, Status};
use crate::backups::{Cell as BackupCell, Health as BackupHealth};
use crate::config::Config;
use crate::pod::Pod;
use crate::snapshot::{cache_key, ProxyState};

/// The proxy port's check: it reaches this machine.
pub const PROXY: &str = "proxy";
/// The deploy key pushes, and the last git backup's age.
pub const BACKUP: &str = "backup";
/// The course repo: there, origin right, on today's branch, no operation in progress.
pub const REPO: &str = "repo";
/// Claude Code runs, and which auth it would use.
pub const CLAUDE: &str = "claude";
/// The course env's notebook kernel starts and imports torch.
pub const KERNEL: &str = "kernel";
/// `uv` on PATH in a login shell.
pub const UV: &str = "uv";

/// The course checks, in the order they're listed.
pub const COURSE_CHECKS: [&str; 6] = [PROXY, BACKUP, REPO, CLAUDE, KERNEL, UV];

/// Whether the check named `name` is one of the course's ([`COURSE_CHECKS`]), never the machine's.
pub fn is_course_check(name: &str) -> bool {
    COURSE_CHECKS.contains(&name)
}

/// The machine's identity, printed by [`IDENTITY_SH`]: what it says through the proxy port is
/// held to what it says directly. The same text runs in the check script (bash) and through the
/// port (the participant's login shell, whatever its locale): POSIX only, the sort in the C locale,
/// no single quotes (it goes inside them).
pub const IDENTITY_SH: &str = r#"arena_identity() {
  arena_n=$(head -n 1 "$HOME/.name" 2>/dev/null | tr -d "\r")
  printf "id.name=%s\n" "${arena_n:-none}"
  printf "id.hostname=%s\n" "$(cat /proc/sys/kernel/hostname 2>/dev/null || hostname 2>/dev/null)"
  arena_k=$(cat /etc/ssh/ssh_host_*_key.pub 2>/dev/null | LC_ALL=C sort | { sha256sum 2>/dev/null || shasum -a 256 2>/dev/null; } | cut -c1-12)
  printf "id.hostkey=%s\n" "${arena_k:-unknown}"
}"#;

/// The command the proxy check runs through a machine's proxy port: its identity
/// ([`IDENTITY_SH`]) between the deep check's header and trailer, so [`super::parse_deep`] reads it
/// — under `sh`, whatever the login shell (zsh's failed globs abort a command).
pub fn identity_command() -> String {
    format!("sh -c '{IDENTITY_SH}\necho deep_check=3; arena_identity; echo deep_check_end=1'")
}

/// How long the proxy check's connection through a port may take: an ssh connect and a few
/// reads.
pub const PROXY_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// The script's helper that reads which auth Claude Code would use (kinds and fingerprints).
pub(super) const CLAUDE_PY: &str = include_str!("claude_auth.py");
/// The script's helper that starts the course env's notebook kernel.
pub(super) const KERNEL_PY: &str = include_str!("kernel.py");

/// What the script needs for the course facts, per machine: the repo, where the course env may
/// be, the deploy key, and whether to make the paid Claude call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckArgs {
    pub repo: String,
    /// The course env's candidates, most likely first ([`crate::agent_notes::env_candidates`]).
    pub envs: Vec<String>,
    /// `GIT_SSH_KEY_REMOTE`: the deploy key the backup pushes with (`None`: ssh's default).
    pub git_key: Option<String>,
    /// `pods test --claude-call`: one tiny Claude request per machine, on its account.
    pub claude_call: bool,
    /// The cohort repo (`owner/name`, [`crate::backup::configured_repo`]): the dry-run push goes
    /// to its SSH URL, never to origin's (which could carry a credential). `None`: no push tried.
    pub origin: Option<String>,
}

impl CheckArgs {
    /// From config, for `pod`: the repo `pods backup` backs up, the env the agent notes name.
    pub fn for_pod(cfg: &Config, pod: &Pod) -> Self {
        let repo = crate::backup::repo_path(cfg);
        let bare = crate::setup::bare_vm(&pod.provider).is_some();
        let envs = crate::agent_notes::env_candidates(cfg.get("CONDA_ENV").filter(|e| !e.is_empty()).unwrap_or("arena-env"), &repo, bare);
        let git_key = cfg.get("GIT_SSH_KEY_REMOTE").map(str::trim).filter(|k| !k.is_empty()).map(String::from);
        Self { repo, envs, git_key, claude_call: false, origin: crate::backup::configured_repo(cfg) }
    }

    /// The script's arguments after its mode, shell-quoted.
    pub(super) fn words(&self) -> String {
        let q = crate::setup::shell_quote;
        format!(
            " {} {} {} {} {}",
            q(&self.repo),
            q(&self.envs.join(":")),
            q(self.git_key.as_deref().unwrap_or("")),
            if self.claude_call { "1" } else { "0" },
            q(self.origin.as_deref().unwrap_or(""))
        )
    }
}

// ---------------------------------------------------------------------------------------
// Facts
// ---------------------------------------------------------------------------------------

/// `id.*`: who the machine says it is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Identity {
    /// `~/.name` (`None`: none there).
    pub name: Option<String>,
    pub hostname: Option<String>,
    /// The first 12 hex of the SHA-256 of its sshd host keys.
    pub hostkey: Option<String>,
}

impl Identity {
    fn known(&self) -> bool {
        self.hostname.is_some() || self.hostkey.is_some()
    }

    /// Whether `other` is the same machine: every part both know agrees, and they share one.
    fn same(&self, other: &Identity) -> bool {
        let parts = [(&self.name, &other.name), (&self.hostname, &other.hostname), (&self.hostkey, &other.hostkey)];
        let shared: Vec<_> = parts.iter().filter(|(a, b)| a.is_some() && b.is_some()).collect();
        !shared.is_empty() && shared.iter().all(|(a, b)| a == b)
    }
}

/// `repo.*`: the course repo's git metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RepoFacts {
    pub path: Option<String>,
    /// `ok`, `missing`, or `error: …`.
    pub state: Option<String>,
    pub branch: Option<String>,
    /// HEAD's short sha when it's on no branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detached: Option<String>,
    /// [`crate::backup::BUSY_OP_SH`]'s line: `rebase`, `merge unmerged`, … (`None`: none).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,
    /// origin's URL, credentials taken out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// Every URL origin fetches from or pushes to, as `host/owner/name` (lowercased).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub origin_keys: Vec<String>,
}

/// `push.*`: the dry-run push with the deploy key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PushFacts {
    /// `set` (the configured key), `default` (none configured: ssh's own), `missing`, `none` (no
    /// cohort repo configured to push to: not tried).
    pub key: Option<String>,
    pub exit: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `claude.*`: Claude Code and the auth it would use — never a secret: its fingerprint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ClaudeFacts {
    /// Where `claude` is on root's PATH, or `missing`.
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_error: Option<String>,
    /// `bearer`, `api_key`, `helper`, `oauth`, `login`, `cloud`, `none`.
    pub auth: Option<String>,
    /// The other kinds that are there, which the first one beats.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub also: Vec<String>,
    /// `shell` (exported: `arena claude`'s, copy-keys'), `settings`, `clods` (a clods profile's).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The clods profile `CLAUDE_CONFIG_DIR` is (its number).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_host: Option<String>,
    /// `openrouter`: the token is the machine's OpenRouter key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub same_as: Option<String>,
    /// The call made: `models` (free), `openrouter` (free), `messages` (`--claude-call`), `none`,
    /// `skipped` (`--claude-call` for an auth it can't ask about: a /login, a helper, a cloud).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The helper couldn't run (no python3), or broke.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_error: Option<String>,
}

/// `kernel.*`: the course env's notebook kernel.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct KernelFacts {
    /// The course env found (`none`: no candidate has a `bin/python`).
    pub env: Option<String>,
    /// `yes`, or why ipykernel doesn't import.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipykernel: Option<String>,
    /// The kernels that run the env's python (`none`), or why they couldn't be listed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub specs: Option<String>,
    /// Those with a kernel.json on disk (`none`: only ipykernel's built-in one).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registered: Option<String>,
    /// Kernels for the env with arguments or a launcher of their own: never started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub own_args: Option<String>,
    /// The one started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Why it didn't start (`timed out after 20s`, `error: …`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_secs: Option<f64>,
    /// `import torch` in it: `ok`, `timed out …`, `error: …`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub torch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub torch_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub torch_secs: Option<f64>,
    /// The env the kernel actually runs (`sys.prefix`), when it isn't the course env: a spec
    /// pointing elsewhere.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// The helper itself failed or timed out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `uv.*`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct UvFacts {
    /// Where a login shell finds it, or `missing`.
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// One course fact into `f` (`false`: not one of them). `unknown`, an empty value or an
/// unparseable one leaves a field unknown.
pub(super) fn course_fact(f: &mut DeepFacts, key: &str, value: &str) -> bool {
    let text = || (!value.is_empty() && value != "unknown").then(|| value.to_string());
    let num = || value.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0);
    let list = || value.split([',', ' ']).filter(|s| !s.is_empty()).map(String::from).collect::<Vec<_>>();
    let Some((group, field)) = key.split_once('.') else { return false };
    match group {
        "id" => {
            let id = f.identity.get_or_insert_with(Identity::default);
            match field {
                // setup writes it as `export MACHINE_NAME=devtest-apple`: the name is the news.
                "name" => {
                    let bare = value.strip_prefix("export ").unwrap_or(value);
                    let bare = bare.strip_prefix("MACHINE_NAME=").unwrap_or(bare).trim_matches(['"', '\'']);
                    id.name = Some(bare.to_string()).filter(|v| !v.is_empty() && v != "none" && v != "unknown");
                }
                "hostname" => id.hostname = text(),
                "hostkey" => id.hostkey = text().filter(|v| v.len() == 12 && v.bytes().all(|b| b.is_ascii_hexdigit())),
                _ => return false,
            }
        }
        "repo" => {
            let r = f.repo.get_or_insert_with(RepoFacts::default);
            match field {
                "path" => r.path = text(),
                "state" => r.state = text(),
                "branch" => r.branch = text(),
                "detached" => r.detached = text(),
                "op" => r.op = text().filter(|v| v != "none"),
                "origin" => r.origin = text(),
                "origin_keys" => {
                    r.origin_keys = list();
                    let mut seen = std::collections::BTreeSet::new();
                    r.origin_keys.retain(|k| seen.insert(k.clone()));
                }
                _ => return false,
            }
        }
        "push" => {
            let p = f.push.get_or_insert_with(PushFacts::default);
            match field {
                "key" => p.key = text(),
                "exit" => p.exit = value.parse().ok(),
                "error" => p.error = text(),
                _ => return false,
            }
        }
        "claude" => {
            let c = f.claude.get_or_insert_with(ClaudeFacts::default);
            match field {
                "path" => c.path = text(),
                "version" => c.version = text(),
                "version_error" => c.version_error = text(),
                "auth" => c.auth = text(),
                "also" => c.also = list(),
                "source" => c.source = text(),
                "profile" => c.profile = text(),
                "config_dir" => {}
                "fingerprint" => {
                    c.fingerprint = Some(value.to_ascii_lowercase()).filter(|v| v.len() == 12 && v.bytes().all(|b| b.is_ascii_hexdigit()))
                }
                "base_host" => c.base_host = text(),
                "same_as" => c.same_as = text(),
                "call" => c.call = text(),
                "http" => c.http = value.parse().ok(),
                "error" => c.error = text(),
                "auth_error" => c.auth_error = text(),
                _ => return false,
            }
        }
        "kernel" => {
            let k = f.kernel.get_or_insert_with(KernelFacts::default);
            match field {
                "env" => k.env = text(),
                "ipykernel" => k.ipykernel = text(),
                "specs" => k.specs = text(),
                "registered" => k.registered = text(),
                "own_args" => k.own_args = text(),
                "name" => k.name = text(),
                "start" => k.start = text(),
                "start_secs" => k.start_secs = num(),
                "torch" => k.torch = text(),
                "torch_version" => k.torch_version = text(),
                "torch_secs" => k.torch_secs = num(),
                "prefix" => k.prefix = text(),
                "error" => k.error = text(),
                "done" | "jupyter_client" => {}
                _ => return false,
            }
        }
        "uv" => {
            let u = f.uv.get_or_insert_with(UvFacts::default);
            match field {
                "path" => u.path = text(),
                "version" => u.version = text(),
                _ => return false,
            }
        }
        _ => return false,
    }
    true
}

// ---------------------------------------------------------------------------------------
// What the control box knows
// ---------------------------------------------------------------------------------------

/// One machine's proxy port, as the control box found it.
#[derive(Debug, Clone, PartialEq)]
pub struct Port {
    /// Its stable public port (`None`: the name has none).
    pub port: Option<u16>,
    /// The proxy config's forward for the name, held to the machine's endpoint
    /// ([`crate::snapshot::proxy_state`]).
    pub state: ProxyState,
    /// What the port reached ([`identity_command`] through it): its identity, or why nothing
    /// answered. `None`: not tried.
    pub reached: Option<Result<Identity, String>>,
}

/// What the course checks hold a machine to. `Default`: nothing known (each check then says
/// what it found, or that it wasn't compared).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CourseBook {
    /// `MACHINE_NAME_PREFIX`: commands take the machine without it.
    pub prefix: String,
    /// When the check runs (UNIX s): the last backup's age.
    pub now: u64,
    /// Today's iteration week and day (`ARENA_START_DATE`): the branch `init-branches` gives each
    /// machine. `None`: no start date to say.
    pub day: Option<(u32, u32)>,
    /// The repo every checkout's `origin` must be (`owner/name`, [`crate::backup::configured_repo`]).
    pub origin: Option<String>,
    /// `None`: no proxy configured (`SSH_PROXY_HOST`).
    pub proxy: Option<ProxyBook>,
    /// Each machine's last git backup by name, as `arena backups` judges it; `Err`: why the
    /// backups' state couldn't be read.
    pub backups: Option<Result<BTreeMap<String, BackupCell>, String>>,
    /// What `arena claude` put on each machine.
    pub claude: ClaudeBook,
}

/// The proxy, as the control box sees it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProxyBook {
    /// `SSH_PROXY_RELOAD_CMD` is empty: the config is written, nginx never reloaded (a sandbox).
    pub write_only: bool,
    /// By [`cache_key`].
    pub ports: BTreeMap<String, Port>,
}

/// `arena claude`'s record: by [`cache_key`], the account last pushed to the machine and its
/// token's fingerprint; and the pool's accounts by fingerprint.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClaudeBook {
    pub pushed: BTreeMap<String, (Option<String>, Option<String>)>,
    pub pool: BTreeMap<String, String>,
}

impl ClaudeBook {
    /// From `arena claude`'s state file and pool: an unreadable one is an empty book.
    pub fn load(cfg: &Config) -> Self {
        let state = crate::claude::state_path(cfg).ok().and_then(|p| crate::claude::load(&p).ok()).unwrap_or_default();
        let pushed = state
            .machines
            .iter()
            .filter_map(|m| m.pushed.as_ref().map(|p| (cache_key(&m.provider, &m.id), (p.account.clone(), p.fp.clone()))))
            .collect();
        let pool = crate::claude::load_pool(cfg)
            .map(|p| p.accounts.iter().map(|a| (a.fingerprint(), a.email.clone())).collect())
            .unwrap_or_default();
        Self { pushed, pool }
    }
}

impl CourseBook {
    /// From config alone: today's branch and the configured repo — no state read.
    pub fn from_config(cfg: &Config, now: u64) -> Self {
        let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena").to_string();
        let day = cfg.get("ARENA_START_DATE").and_then(crate::schedule::parse_ymd).map(|(y, m, d)| {
            crate::schedule::week_day(crate::schedule::days_from_civil(y, m, d), crate::schedule::days_from_unix(now))
        });
        Self { prefix, now, day, origin: crate::backup::configured_repo(cfg), ..Self::default() }
    }

    /// [`Self::from_config`] with the box's own state: the backups' record (judged as `arena
    /// backups` does, for `pods`, with `crontab` as `crontab -l` read it) and `arena claude`'s
    /// pushes. The proxy's ports are the caller's to fill in ([`probe_ports`]): they take a
    /// connection each.
    pub fn load(cfg: &Config, pods: &[Pod], now: u64, crontab: Result<String, String>) -> Self {
        Self { backups: Some(backup_cells(cfg, pods, now, crontab)), claude: ClaudeBook::load(cfg), ..Self::from_config(cfg, now) }
    }

    /// The whole book for `pods`, as `pods test`, `up --check` and the dashboard's deep check hold
    /// them: [`Self::load`] (with `crontab -l`), or [`Self::from_config`] alone when `read_state` is
    /// off (a crate's unit tests, which must never read the developer's own state or crontab),
    /// and what each pod's proxy port reaches from here ([`probe_ports`]).
    pub async fn gather(remote: &std::sync::Arc<dyn crate::remote::Remote>, cfg: &Config, pods: &[Pod], read_state: bool) -> Self {
        let now = crate::snapshot::unix_now();
        let book = match read_state {
            true => Self::load(cfg, pods, now, crate::backups::read_crontab()),
            false => Self::from_config(cfg, now),
        };
        Self { proxy: probe_ports(remote, cfg, pods).await, ..book }
    }

    /// `apple` for `devtest-apple`.
    fn short<'a>(&self, name: &'a str) -> &'a str {
        name.strip_prefix(self.prefix.as_str()).and_then(|s| s.strip_prefix('-')).filter(|s| !s.is_empty()).unwrap_or(name)
    }

    /// The branch `init-branches` gives `name` today.
    fn todays_branch(&self, name: &str) -> Option<String> {
        let (week, day) = self.day?;
        let b = crate::backup::BackupConfig {
            repo_path: String::new(),
            prefix: self.prefix.clone(),
            week,
            day,
            git_ssh_key: None,
            origin: None,
        };
        Some(b.branch_for(name))
    }
}

/// Each machine's stable proxy port as its name's place in `MACHINE_NAME_LIST` gives it (the rule
/// `proxy plan` follows: the first occurrence of a name wins) — for a proxy whose config this box
/// can't read.
fn name_port(cfg: &Config, px: &crate::proxy::ProxyConfig, name: &str) -> Option<u16> {
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    let idx = cfg.machine_names.iter().position(|e| crate::naming::qualify(prefix, e) == name)?;
    u16::try_from(px.starting_port as u32 + idx as u32).ok()
}

/// Why a connection through a proxy port got no identity, in a few words.
fn probe_why(out: &crate::ssh::SshOutput) -> String {
    let err = out.stderr.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let lower = err.to_ascii_lowercase();
    if lower.contains("connection refused") {
        "connection refused".into()
    } else if lower.contains("timed out") {
        "timed out".into()
    } else if lower.contains("permission denied") {
        "SSH refused the key".into()
    } else if lower.contains("closed") || lower.contains("reset") {
        "connection closed".into()
    } else if err.is_empty() {
        format!("no identity ({})", out.exit_label())
    } else {
        clip_why(err.rsplit(": ").next().unwrap_or(err), 40)
    }
}

/// What each pod's proxy port reaches, from this box: its port and the proxy config's forward
/// ([`crate::snapshot::proxy_state`]: the local config file, else the name's slot), then one
/// connection through the port ([`identity_command`], bounded by [`PROXY_PROBE_TIMEOUT`]) for
/// each pod that's up, all at once. `None` when no proxy is configured. Connects only: nothing is
/// written on the proxy or reloaded.
pub async fn probe_ports(remote: &std::sync::Arc<dyn crate::remote::Remote>, cfg: &Config, pods: &[Pod]) -> Option<ProxyBook> {
    let px = crate::proxy::ProxyConfig::from_config(cfg).ok()?;
    let forwards = crate::snapshot::local_proxy_text(cfg).0.map(|t| crate::proxy::parse_nginx(&t));
    let mut book = ProxyBook { write_only: px.write_only(), ports: BTreeMap::new() };
    let mut probes = tokio::task::JoinSet::new();
    for pod in pods {
        let key = cache_key(&pod.provider, &pod.id);
        let (mut port, state) = crate::snapshot::proxy_state(pod, forwards.as_deref());
        if state == ProxyState::Unknown {
            port = name_port(cfg, &px, &pod.name);
        }
        book.ports.insert(key.clone(), Port { port, state, reached: None });
        let up = !crate::status::is_stopped(&pod.status);
        let (Some(n), true, Ok(direct)) = (port, up, crate::ssh::SshTarget::from_pod(pod, cfg)) else { continue };
        let target = crate::ssh::SshTarget { host: px.proxy_host.clone(), port: n, ..direct };
        let remote = remote.clone();
        probes.spawn(async move {
            let reached = match remote.exec(&target, &identity_command(), Some(PROXY_PROBE_TIMEOUT)).await {
                Err(e) => Err(crate::remote::describe_error(&e)),
                Ok(out) => match super::parse_deep(&out.stdout).identity.filter(|i| i.known()) {
                    Some(id) => Ok(id),
                    None => Err(probe_why(&out)),
                },
            };
            (key, reached)
        });
    }
    while let Some(done) = probes.join_next().await {
        if let Ok((key, reached)) = done {
            if let Some(p) = book.ports.get_mut(&key) {
                p.reached = Some(reached);
            }
        }
    }
    Some(book)
}

/// Each machine's git cell from the backups' status ([`crate::backups::judge`], as `arena backups`
/// shows it), by name.
fn backup_cells(cfg: &Config, pods: &[Pod], now: u64, crontab: Result<String, String>) -> Result<BTreeMap<String, BackupCell>, String> {
    let naming = crate::selector::Naming::from_config(cfg);
    let ins: Vec<crate::backups::PodIn> = pods
        .iter()
        .map(|p| crate::backups::PodIn {
            name: p.name.clone(),
            cohort: naming.is_cohort(&p.name),
            up: Some(!crate::status::is_stopped(&p.status)),
            last_commit: None,
        })
        .collect();
    let status = crate::backups::judge(&crate::backups::read(cfg, None, crontab, ins, now));
    if let Some(why) = status.jobs.iter().find(|j| j.job == crate::backups::Job::Git).and_then(|j| j.last_unknown.clone()) {
        if status.pods.iter().all(|p| p.git.health == BackupHealth::Unknown) && why.contains("can't be read") {
            return Err(why);
        }
    }
    Ok(status.pods.into_iter().map(|p| (p.name, p.git)).collect())
}

// ---------------------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------------------

/// Facts from a script that didn't say (older than these checks, or cut off before them).
const NOT_REPORTED: &str = "not reported (a check from before course checks, or cut off)";

/// The course checks of one machine, from the facts its script reported, held to `book`. Pure.
pub fn course_checks(f: &DeepFacts, pod: &Pod, book: &CourseBook) -> Vec<Check> {
    vec![
        proxy_check(f, pod, book),
        backup_check(f, pod, book),
        repo_check(f, pod, book),
        claude_check(f, pod, book),
        kernel_check(f, pod, book),
        uv_check(f, pod, book),
    ]
}

fn clip_why(s: &str, n: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > n {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    } else {
        s
    }
}

fn proxy_check(f: &DeepFacts, pod: &Pod, book: &CourseBook) -> Check {
    use Status::*;
    let name = PROXY;
    let Some(px) = &book.proxy else { return Check::new(name, Skip, "no proxy configured (SSH_PROXY_HOST)") };
    let Some(port) = px.ports.get(&cache_key(&pod.provider, &pod.id)) else {
        return Check::new(name, Skip, "not checked here");
    };
    let apply = "`arena proxy apply`";
    let Some(n) = port.port else {
        return match port.state {
            ProxyState::None => Check::new(name, Warn, format!("no forward on the proxy for it — {apply}")),
            _ => Check::new(name, Skip, "no proxy port for this name (not in MACHINE_NAME_LIST)"),
        };
    };
    let stale = port.state == ProxyState::Stale;
    let direct = f.identity.as_ref().filter(|i| i.known());
    match &port.reached {
        Some(Ok(there)) => match direct {
            Some(here) if here.same(there) && stale => {
                Check::new(name, Warn, format!("port {n} reaches it, but the proxy file forwards to an old endpoint — {apply}"))
            }
            Some(here) if here.same(there) => Check::new(name, Pass, format!("port {n} reaches it")),
            Some(_) => {
                let who = there.name.as_deref().or(there.hostname.as_deref()).unwrap_or("?");
                Check::new(name, Fail, format!("port {n} reaches another machine ({}) — {apply}", clip_why(who, 24)))
            }
            None => Check::new(name, Warn, format!("port {n} answers; not compared (no identity reported) — `arena pods test --deep {}`", book.short(&pod.name))),
        },
        // The file forwards it to an old endpoint: wrong whether or not nginx runs it.
        Some(Err(why)) if stale => Check::new(name, Fail, format!("port {n} forwards to an old endpoint ({}) — {apply}", clip_why(why, 30))),
        Some(Err(why)) if px.write_only => {
            Check::new(name, Warn, format!("port {n}: {} — write-only proxy here: SSH_PROXY_RELOAD_CMD reloads it", clip_why(why, 40)))
        }
        Some(Err(why)) => Check::new(name, Fail, format!("port {n} doesn't reach it: {} — {apply}", clip_why(why, 40))),
        None if stale => Check::new(name, Fail, format!("port {n} forwards to an old endpoint — {apply}")),
        None => Check::new(name, Skip, format!("port {n} not tried")),
    }
}

/// A push GitHub turned down for the key (not the network): its words.
fn refused(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    ["denied", "read only", "read-only", "not found", "permission", "could not read from remote", "host key verification"]
        .iter()
        .any(|w| e.contains(w))
}

fn backup_check(f: &DeepFacts, pod: &Pod, book: &CourseBook) -> Check {
    use Status::*;
    let name = BACKUP;
    let s = book.short(&pod.name);
    if f.repo.as_ref().and_then(|r| r.state.as_deref()).is_some_and(|st| st != "ok") {
        return Check::new(name, Skip, "needs the course repo (see repo)");
    }
    let Some(p) = f.push.as_ref().filter(|p| p.key.is_some()) else { return Check::new(name, Skip, NOT_REPORTED) };
    if p.key.as_deref() == Some("missing") {
        return Check::new(name, Fail, format!("no deploy key on it (GIT_SSH_KEY_REMOTE) — `arena pods setup {s}`"));
    }
    if p.key.as_deref() == Some("none") {
        return Check::new(name, Warn, "push not checked: no cohort repo to push to — set ARENA_REPO_OWNER and ARENA_REPO_NAME");
    }
    if p.exit != Some(0) {
        let why = p.error.clone().unwrap_or_else(|| format!("git push exit {}", p.exit.map_or("?".into(), |e| e.to_string())));
        let e = why.to_ascii_lowercase();
        return if e.contains("read only") || e.contains("read-only") {
            Check::new(name, Fail, "GitHub refuses a push: its deploy key is read-only — allow write access for it on GitHub")
        } else if e.contains("host key verification") {
            Check::new(name, Fail, format!("github.com's host key isn't known there — `arena pods setup {s}`"))
        } else if refused(&why) {
            Check::new(name, Fail, format!("GitHub refuses a push: {} — `arena pods setup {s}`", clip_why(&why, 48)))
        } else {
            Check::new(name, Warn, format!("push not checked: {} — `arena pods test --course {s}`", clip_why(&why, 38)))
        };
    }
    let cell = match book.backups.as_ref() {
        None => None,
        Some(Err(why)) => return Check::new(name, Warn, format!("pushes; the backups' record can't be read: {} — `arena backups`", clip_why(why, 40))),
        Some(Ok(cells)) => cells.get(&pod.name),
    };
    let ago = |t: u64| crate::backups::ago(book.now, t);
    match cell {
        None => Check::new(name, Pass, "push ok"),
        Some(c) => match c.health {
            BackupHealth::Fail => Check::new(
                name,
                Fail,
                format!("pushes; its last backup failed: {} — `arena pods backup {s}`", clip_why(c.why.as_deref().unwrap_or("?"), 32)),
            ),
            // Late: as `arena backups` judges it (twice the backup's gap gone by).
            BackupHealth::Warn if c.text != "on main" => Check::new(
                name,
                Warn,
                format!("{} — `arena pods backup {s}`", clip_why(c.why.as_deref().unwrap_or("not backed up lately"), 60)),
            ),
            _ => match c.ok_at {
                Some(t) => Check::new(name, Pass, format!("push ok · last backup {}", ago(t))),
                None => Check::new(name, Pass, "push ok · no backup recorded yet"),
            },
        },
    }
}

fn repo_check(f: &DeepFacts, pod: &Pod, book: &CourseBook) -> Check {
    use Status::*;
    let name = REPO;
    let s = book.short(&pod.name);
    let Some(r) = f.repo.as_ref().filter(|r| r.state.is_some()) else { return Check::new(name, Skip, NOT_REPORTED) };
    let path = r.path.as_deref().unwrap_or("?");
    match r.state.as_deref() {
        Some("ok") => {}
        Some("missing") => return Check::new(name, Fail, format!("no course repo at {path} — `arena pods setup {s}`")),
        Some(e) => {
            return Check::new(name, Warn, format!("can't read {path}: {} — `arena pods ask {s}`", clip_why(e.strip_prefix("error: ").unwrap_or(e), 40)))
        }
        None => unreachable!(),
    }
    if let Some(want) = book.origin.as_deref() {
        let key = format!("github.com/{want}").to_ascii_lowercase();
        if r.origin_keys.is_empty() || r.origin_keys.iter().any(|k| *k != key) {
            let shown = r.origin.as_deref().filter(|o| !o.is_empty()).unwrap_or("none");
            return Check::new(name, Fail, format!("origin isn't {want}: no backups — `arena pods setup {s}` (it's {shown})"));
        }
    }
    let branch = r.branch.as_deref();
    if let Some(op) = &r.op {
        return Check::new(name, Warn, busy_line(op));
    }
    let init = format!("`arena pods init-branches {s}`");
    let Some(branch) = branch else {
        let at = r.detached.as_deref().unwrap_or("?");
        return Check::new(name, Warn, format!("detached HEAD (at {at}): no backups — {init}"));
    };
    if matches!(branch, "main" | "master") {
        return Check::new(name, Warn, format!("on {branch}: no backups — {init}"));
    }
    match book.todays_branch(&pod.name) {
        Some(want) if want != branch => {
            let other = branch.starts_with(&format!("autocommit-{}-", book.prefix));
            let what = if other { "an older day's" } else { "not today's" };
            Check::new(name, Warn, format!("on {} ({what}) — {init}", clip_why(branch, 40)))
        }
        Some(_) => Check::new(name, Pass, format!("on {branch} (today's)")),
        None => Check::new(name, Pass, format!("on {branch}")),
    }
}

/// An operation in progress ([`crate::backup::BUSY_OP_SH`]'s line: `rebase`, `merge unmerged`,
/// `conflicts`), with how it ends on the pod — short enough for the NOTES cell; `arena backups`
/// and the backup's own line say it at length ([`crate::backup::busy_advice`]).
fn busy_line(op: &str) -> String {
    let (what, unmerged) = match op.split_once(' ') {
        Some((w, rest)) => (w, rest.contains("unmerged")),
        None => (op, false),
    };
    let (it, ends) = match what {
        "merge" => ("a merge", "`git commit` or `git merge --abort`"),
        "cherry-pick" => ("a cherry-pick", "`git cherry-pick --continue` or `--abort`"),
        "revert" => ("a revert", "`git revert --continue` or `--abort`"),
        "am" => ("a `git am`", "`git am --continue` or `--abort`"),
        "rebase" => ("a rebase", "`git rebase --continue` or `--abort`"),
        "sequence" => ("a sequence", "`git cherry-pick --continue` or `--abort`"),
        "conflicts" => return "unresolved conflicts: no backups — there, resolve and `git add` them, or `git reset --merge`".into(),
        other => return format!("a git {other} in progress: no backups — there, finish it or abort it"),
    };
    let conflicts = if unmerged { " (conflicts)" } else { "" };
    format!("{it} in progress{conflicts}: no backups — there, {ends}")
}

/// `2.1.30` from `2.1.30 (Claude Code)`.
fn short_version(v: &str) -> String {
    v.split_whitespace().next().unwrap_or(v).to_string()
}

fn claude_check(f: &DeepFacts, pod: &Pod, book: &CourseBook) -> Check {
    use Status::*;
    let name = CLAUDE;
    let s = book.short(&pod.name);
    let Some(c) = f.claude.as_ref().filter(|c| c.path.is_some()) else { return Check::new(name, Skip, NOT_REPORTED) };
    if c.path.as_deref() == Some("missing") {
        return Check::new(name, Fail, format!("not installed (`claude` isn't on root's PATH) — `arena pods setup {s}`"));
    }
    if let Some(e) = &c.version_error {
        return Check::new(name, Fail, format!("`claude --version` fails: {} — `arena pods setup {s}`", clip_why(e, 40)));
    }
    let v = c.version.as_deref().map(short_version).unwrap_or_else(|| "?".into());
    let fp = c.fingerprint.as_deref().unwrap_or("?");
    let pushed = book.claude.pushed.get(&cache_key(&pod.provider, &pod.id));
    let push = format!("`arena claude push {s}`");
    let pool_account = c.fingerprint.as_ref().and_then(|f| book.claude.pool.get(f));
    // Which auth, in words.
    let what = match c.auth.as_deref() {
        None => {
            let why = clip_why(c.auth_error.as_deref().unwrap_or("?"), 28);
            return Check::new(name, Warn, format!("v{v}; its auth couldn't be read: {why} — `arena pods setup {s}`"));
        }
        Some("none") => {
            return match pushed {
                Some((Some(acct), _)) => Check::new(name, Warn, format!("v{v}: no token, though arena claude pushed one — {push} ({acct})")),
                _ => Check::new(name, Skip, format!("v{v}, no auth (no arena claude token, clods profile or API key)")),
            };
        }
        Some("oauth") => match (c.source.as_deref(), c.profile.as_deref()) {
            (Some("clods"), Some(n)) => format!("clods profile {n}, token {fp}"),
            (Some("settings"), _) => format!("a token in settings.json, {fp}"),
            _ => match (pushed, pool_account) {
                (Some((Some(acct), Some(want))), _) if Some(want.as_str()) == c.fingerprint.as_deref() => format!("arena claude's {acct}"),
                (Some((Some(acct), _)), _) => {
                    return Check::new(name, Warn, format!("v{v}: not the token arena claude pushed — {push} ({acct})"));
                }
                (_, Some(acct)) => format!("arena claude's {acct}"),
                _ => format!("a token {fp}, not arena claude's"),
            },
        },
        Some("bearer") if c.same_as.as_deref() == Some("openrouter") => "OpenRouter: the AI key".to_string(),
        Some("bearer") => format!("ANTHROPIC_AUTH_TOKEN to {}, {fp}", c.base_host.as_deref().unwrap_or("?")),
        Some("api_key") => format!("ANTHROPIC_API_KEY {fp}"),
        Some("helper") => "an apiKeyHelper".to_string(),
        Some("login") => "a /login".to_string(),
        Some("cloud") => "a cloud provider".to_string(),
        Some(other) => other.to_string(),
    };
    // A problem's line names the auth in a word, then the fix, then (after it, where the NOTES
    // cell may cut) which one exactly.
    let (short, fix) = match c.auth.as_deref() {
        Some("api_key") => ("ANTHROPIC_API_KEY", format!("`arena pods copy-keys {s}`")),
        Some("bearer") if c.same_as.as_deref() == Some("openrouter") => ("the AI key", format!("`arena pods test --keys {s}` says why")),
        Some("bearer") => ("ANTHROPIC_AUTH_TOKEN", "a working one there".to_string()),
        Some("oauth") if c.source.as_deref() == Some("clods") => ("its clods token", "re-push its clods profile".to_string()),
        Some("oauth") => ("its token", format!("renew it, then {push}")),
        Some(other) => (other, format!("`arena pods test --course {s}` later")),
        None => ("?", String::new()),
    };
    // Which one exactly, after the fix (where the NOTES cell may cut): the parts `short` didn't say.
    let which = match c.auth.as_deref() {
        Some("api_key") => fp.to_string(),
        Some("bearer") if c.same_as.as_deref() == Some("openrouter") => "OpenRouter".to_string(),
        Some("bearer") => format!("{}, {fp}", c.base_host.as_deref().unwrap_or("?")),
        _ => what.clone(),
    };
    let what = &what;
    let answered = |h: u16| (200..300).contains(&h);
    // A subscription token answers a plain Messages call with this, past its authentication:
    // only that, and only for one (a gateway's "not permitted to use Claude Code" is a refusal).
    let claude_code_only = c.auth.as_deref() == Some("oauth")
        && c.error.as_deref().is_some_and(|e| e.contains("only authorized for use with Claude Code"));
    let again = format!("`arena pods test --course {s}`");
    match (c.call.as_deref(), c.http) {
        (Some("skipped"), _) => Check::new(
            name,
            Warn,
            format!("v{v}, {what}: not asked (--claude-call can't for it) — there, `claude -p hi` (uses the account)"),
        ),
        (Some("models" | "openrouter" | "messages"), Some(h)) if answered(h) => Check::new(name, Pass, format!("v{v}, {what}: accepted")),
        (Some("messages"), Some(400 | 403)) if claude_code_only => Check::new(name, Pass, format!("v{v}, {what}: accepted (for Claude Code only)")),
        (Some(_), Some(h @ (401 | 403))) => Check::new(name, Fail, format!("v{v}: {short} rejected (HTTP {h}) — {fix} ({which})")),
        (Some(_), Some(429)) if c.auth.as_deref() == Some("oauth") => {
            Check::new(name, Warn, format!("v{v}: {short} at its limit (HTTP 429) — wait, or `arena claude assign {s} N` ({which})"))
        }
        (Some(_), Some(429)) => Check::new(name, Warn, format!("v{v}: {short} at its limit (HTTP 429) — wait for it to reset ({which})")),
        (Some(_), Some(0)) | (Some("models" | "openrouter" | "messages"), None) => Check::new(
            name,
            Warn,
            format!("v{v}: {short} not checked ({}) — {again} ({which})", clip_why(c.error.as_deref().unwrap_or("no answer"), 16)),
        ),
        (Some(_), Some(h)) => {
            Check::new(name, Warn, format!("v{v}: {short} answered HTTP {h} — {again} ({which}: {})", clip_why(c.error.as_deref().unwrap_or("?"), 40)))
        }
        _ => Check::new(name, Pass, format!("v{v}, {what}")),
    }
}

/// How to install `pkg` into `env`: uv's when the machine has it, else the env's pip (as the
/// agent notes say).
fn install(f: &DeepFacts, env: &str, pkg: &str) -> String {
    match f.uv.as_ref().and_then(|u| u.path.as_deref()).filter(|p| *p != "missing") {
        Some(_) => format!("uv pip install --python {env}/bin/python {pkg}"),
        None => format!("{env}/bin/python -m pip install {pkg}"),
    }
}

fn kernel_check(f: &DeepFacts, pod: &Pod, book: &CourseBook) -> Check {
    use Status::*;
    let name = KERNEL;
    let s = book.short(&pod.name);
    let Some(k) = f.kernel.as_ref().filter(|k| k.env.is_some()) else { return Check::new(name, Skip, NOT_REPORTED) };
    let env = k.env.as_deref().unwrap_or("?");
    if env == "none" {
        return Check::new(name, Fail, format!("no course env (none of the agent notes' places) — `arena pods setup {s}`"));
    }
    let run = |cmd: String| format!("`arena pods run {s} '{cmd}'`");
    match k.ipykernel.as_deref() {
        Some("yes") => {}
        Some(e) => {
            let why = if e.contains("ModuleNotFoundError") { "no ipykernel".to_string() } else { clip_why(e, 32) };
            return Check::new(name, Warn, format!("{} ({why} in {env}: notebooks can't use it)", run(install(f, env, "ipykernel"))));
        }
        None => {
            let why = k.error.as_deref().unwrap_or("the kernel check didn't report");
            return Check::new(name, Warn, format!("not checked: {} — `arena pods test --course {s}`", clip_why(why, 48)));
        }
    }
    let register = run(format!("{env}/bin/python -m ipykernel install --user"));
    // Kernels for the env, but each with arguments (or a launcher) of its own: not started, since
    // they could reach the participant's profile.
    if let (Some("none"), Some(own)) = (k.specs.as_deref(), k.own_args.as_deref()) {
        let own = clip_why(own, 24);
        return Check::new(name, Warn, format!("not started: {own} has its own arguments — `arena pods ask {s}`"));
    }
    match k.specs.as_deref() {
        Some("none") => return Check::new(name, Warn, format!("{register} (no Jupyter kernel runs {env})")),
        Some(e) if e.starts_with("error") => {
            return Check::new(name, Warn, format!("its kernels can't be listed: {} — `arena pods setup {s}`", clip_why(e, 40)))
        }
        _ => {}
    }
    // A kernel's name is the participant's (or an image's) to choose: cut, so the fix still fits.
    let kname = clip_why(k.name.as_deref().unwrap_or("?"), 12);
    if let Some(why) = &k.start {
        return Check::new(name, Warn, format!("the {kname} kernel didn't start: {} — `arena pods test --course {s}`", clip_why(why, 22)));
    }
    let secs = k.start_secs.unwrap_or(0.0) + k.torch_secs.unwrap_or(0.0);
    match k.torch.as_deref() {
        Some("ok") => {}
        Some(t) if t.starts_with("timed out") => {
            let after = t.strip_prefix("timed out ").unwrap_or(t);
            return Check::new(name, Warn, format!("the {kname} kernel: import torch timed out {after} — `arena pods test --course {s}`"));
        }
        Some(t) => {
            let t = t.strip_prefix("error: ").unwrap_or(t);
            // `ModuleNotFoundError: No module named 'torch'`: the module is the news.
            let why = clip_why(t.find("No module named").map_or(t, |i| &t[i..]), 32);
            return Check::new(name, Fail, format!("can't import torch in {kname}: {why} — `arena pods setup {s}`"));
        }
        None => {
            let why = k.error.as_deref().unwrap_or("no answer");
            return Check::new(name, Warn, format!("the {kname} kernel: not checked ({}) — `arena pods test --course {s}`", clip_why(why, 20)));
        }
    }
    let torch = k.torch_version.as_deref().unwrap_or("?");
    // The kernel picked runs another env (its spec points there): a pass would be that env's.
    if let Some(other) = &k.prefix {
        return Check::new(name, Warn, format!("{register} (the {kname} kernel runs {other}, not {env})"));
    }
    if k.registered.as_deref().is_none_or(|r| r == "none") {
        return Check::new(name, Warn, format!("{register} (works, torch {torch}, but no kernel is registered for {env})"));
    }
    Check::new(name, Pass, format!("{kname}: torch {torch} in {secs:.1}s"))
}

fn uv_check(f: &DeepFacts, pod: &Pod, book: &CourseBook) -> Check {
    use Status::*;
    let name = UV;
    let Some(u) = f.uv.as_ref().filter(|u| u.path.is_some()) else { return Check::new(name, Skip, NOT_REPORTED) };
    match u.path.as_deref() {
        Some("missing") => Check::new(name, Warn, format!("not on PATH in a login shell — `arena pods setup {} --notes-only`", book.short(&pod.name))),
        Some(p) => match u.version.as_deref() {
            Some(v) => Check::new(name, Pass, format!("{p} ({v})")),
            None => Check::new(name, Pass, p.to_string()),
        },
        None => unreachable!(),
    }
}

// ---------------------------------------------------------------------------------------
// Cells: `pods test --course`'s table
// ---------------------------------------------------------------------------------------

/// A course check's verdict on `h`, when it was judged (not a Skip).
fn judged<'a>(h: &'a PodHealth, name: &str) -> Option<&'a Check> {
    h.checks.iter().find(|c| c.name == name && c.status != Status::Skip)
}

/// The PROXY cell: `:9500` (it reaches the machine), `:9500 old` (stale), `:9500 other` (another
/// machine), `:9500 down` (no answer), `none` (no forward) — `-` when not judged.
pub fn proxy_cell(h: &PodHealth) -> String {
    let Some(c) = judged(h, PROXY) else { return "-".into() };
    let port = c.detail.split_whitespace().skip_while(|w| *w != "port").nth(1).map(|p| format!(":{}", p.trim_end_matches(':')));
    let Some(port) = port else { return if c.detail.starts_with("no forward") { "none".into() } else { "?".into() } };
    match c.status {
        Status::Pass => port,
        _ if c.detail.contains("another machine") => format!("{port} other"),
        _ if c.detail.contains("old endpoint") => format!("{port} old"),
        _ if c.detail.contains("answers") => format!("{port} ?"),
        _ => format!("{port} down"),
    }
}

/// The BACKUP cell: the last backup's age (`12 min ago`), `ok` (none recorded), `failed`, `late`,
/// `no push` (GitHub refused it), `no key`, `?`.
pub fn backup_cell(h: &PodHealth) -> String {
    let Some(c) = judged(h, BACKUP) else { return "-".into() };
    let d = c.detail.as_str();
    match c.status {
        Status::Pass => d.split(" · last backup ").nth(1).map(String::from).unwrap_or_else(|| "ok".into()),
        _ if d.starts_with("no deploy key") => "no key".into(),
        _ if d.starts_with("GitHub refuses") || d.starts_with("github.com") => "no push".into(),
        _ if d.contains("last backup failed") => "failed".into(),
        _ if d.starts_with("push not checked") => "?".into(),
        Status::Warn => "late".into(),
        _ => "?".into(),
    }
}

/// The REPO cell: `today` (today's branch), `ok` (no day to compare), `main`, `detached`, `old
/// day`, `other`, the operation (`rebase`), `origin` (the wrong one), `missing`.
pub fn repo_cell(h: &PodHealth) -> String {
    let Some(c) = judged(h, REPO) else { return "-".into() };
    let d = c.detail.as_str();
    let r = h.facts.as_ref().and_then(|f| f.repo.as_ref());
    match c.status {
        Status::Pass if d.ends_with("(today's)") => "today".into(),
        Status::Pass => "ok".into(),
        _ if d.starts_with("no course repo") => "missing".into(),
        _ if d.starts_with("origin is") => "origin".into(),
        _ if r.and_then(|r| r.op.as_deref()).is_some() => r.and_then(|r| r.op.as_deref()).map(|o| o.split(' ').next().unwrap_or(o).to_string()).unwrap_or_default(),
        _ if d.starts_with("detached") => "detached".into(),
        _ if d.starts_with("on main") || d.starts_with("on master") => "main".into(),
        _ if d.contains("older day") => "old day".into(),
        _ if d.contains("not today's") => "other".into(),
        _ => "?".into(),
    }
}

/// The CLAUDE cell: the auth it would use (`oauth`, `clods 2`, `api key`, `openrouter`, `login`,
/// `none`), or `missing`, `rejected`, `?`.
pub fn claude_cell(h: &PodHealth) -> String {
    let Some(c) = h.checks.iter().find(|c| c.name == CLAUDE) else { return "-".into() };
    let Some(f) = h.facts.as_ref().and_then(|f| f.claude.as_ref()) else { return "-".into() };
    if f.path.as_deref() == Some("missing") {
        return "missing".into();
    }
    if c.status == Status::Fail {
        return if c.detail.contains("rejected") { "rejected".into() } else { "broken".into() };
    }
    match (f.auth.as_deref(), f.profile.as_deref()) {
        (Some("oauth"), Some(n)) if f.source.as_deref() == Some("clods") => format!("clods {n}"),
        (Some("bearer"), _) if f.same_as.as_deref() == Some("openrouter") || f.base_host.as_deref().is_some_and(|h| h.ends_with("openrouter.ai")) => {
            "openrouter".into()
        }
        (Some("api_key"), _) => "api key".into(),
        (Some(a), _) => a.into(),
        (None, _) => "?".into(),
    }
}

/// The KERNEL cell: `4.2s` (started and imported torch), `no torch`, `no ipykernel`, `slow`, `no
/// env`, `unreg` (works, not registered), `?`.
pub fn kernel_cell(h: &PodHealth) -> String {
    let Some(c) = judged(h, KERNEL) else { return "-".into() };
    let k = h.facts.as_ref().and_then(|f| f.kernel.as_ref());
    let secs = k.map(|k| k.start_secs.unwrap_or(0.0) + k.torch_secs.unwrap_or(0.0)).unwrap_or(0.0);
    let d = c.detail.as_str();
    match c.status {
        Status::Pass => format!("{secs:.1}s"),
        _ if d.starts_with("no course env") => "no env".into(),
        _ if d.contains("can't import torch") => "no torch".into(),
        _ if d.contains("notebooks can't use it") => "no ipykernel".into(),
        _ if d.contains("no kernel is registered") || d.contains("no Jupyter kernel") => "unreg".into(),
        _ if d.contains("timed out") => "slow".into(),
        _ => "?".into(),
    }
}

#[cfg(test)]
#[path = "course_tests.rs"]
mod tests;
