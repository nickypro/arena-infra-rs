//! SSH command construction + execution against a pod.
//!
//! Both the backup flow (git over SSH) and the dashboard (`nvidia-smi` over SSH) need
//! to run a command on a pod, so this centralizes *how* we reach one. Whether a call
//! mutates anything is entirely a function of the remote command string — callers
//! gate mutation; this module just builds and runs the SSH invocation.
//!
//! The options are deliberately non-interactive and fail-fast: `BatchMode=yes` (never
//! prompt for a password/passphrase) and a short `ConnectTimeout` so a down or unreachable
//! pod errors quickly instead of hanging the whole fleet sweep.
//!
//! Host-key checking is turned OFF (`StrictHostKeyChecking=no` + `UserKnownHostsFile=/dev/null`):
//! pod IPs are recycled across the provider's pool constantly, so the *same* IP routinely
//! comes back with a *different* host key. `accept-new` would reject that as "host key
//! changed" and break provisioning of a freshly-created pod the moment it reuses an IP we've
//! seen before — and it would also pollute the operator's `~/.ssh/known_hosts`. The pods are
//! created by us via the provider API, so IP-pinned host-key verification buys little here.
//! `LogLevel=ERROR` suppresses the per-connection "Permanently added …" warning that
//! `/dev/null` known-hosts would otherwise print on every call.

use std::process::Stdio;

use tokio::process::Command;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::pod::Pod;

/// Everything needed to SSH into one pod.
#[derive(Debug, Clone)]
pub struct SshTarget {
    pub user: String,
    pub host: String,
    pub port: u16,
    /// Identity files to offer, in order — ssh tries each, so the tool gets in with
    /// whichever key a pod authorizes (handy for a mixed fleet across cohorts).
    pub key_paths: Vec<String>,
    pub connect_timeout_secs: u32,
}

impl SshTarget {
    /// Build from a pod's current SSH endpoint plus config. Offers every configured key
    /// (`SHARED_SSH_KEY_PATH`, `GIT_SSH_KEY_LOCAL`, and comma-separated `EXTRA_SSH_KEYS`)
    /// so a pod authorized by any of them is reachable. Errors if no endpoint yet.
    pub fn from_pod(pod: &Pod, cfg: &Config) -> Result<Self> {
        let host = pod
            .ssh_ip
            .clone()
            .ok_or_else(|| Error::provider(format!("{}: no SSH ip yet", pod.name)))?;
        let port = pod
            .ssh_port
            .ok_or_else(|| Error::provider(format!("{}: no SSH port yet", pod.name)))?;
        Ok(Self {
            user: cfg.get("SSH_USER").unwrap_or("root").to_string(),
            host,
            port,
            key_paths: config_ssh_keys(cfg),
            connect_timeout_secs: 10,
        })
    }

    /// Build a target for an arbitrary host (e.g. the proxy box), resolving the key the
    /// same way pods do (prefer a readable `~/.ssh/<name>` if the configured path isn't).
    pub fn for_host(user: &str, host: &str, port: u16, key_path: Option<&str>) -> Self {
        Self {
            user: user.to_string(),
            host: host.to_string(),
            port,
            key_paths: key_path.map(resolve_key_path).into_iter().collect(),
            connect_timeout_secs: 10,
        }
    }

    /// The `ssh` argv *before* the remote command — non-interactive, fail-fast.
    pub fn ssh_args(&self) -> Vec<String> {
        let mut a = vec![
            "-p".into(),
            self.port.to_string(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "StrictHostKeyChecking=no".into(),
            "-o".into(),
            "UserKnownHostsFile=/dev/null".into(),
            "-o".into(),
            "LogLevel=ERROR".into(),
            "-o".into(),
            format!("ConnectTimeout={}", self.connect_timeout_secs),
        ];
        for key in &self.key_paths {
            a.push("-i".into());
            a.push(key.clone());
        }
        a.push(format!("{}@{}", self.user, self.host));
        a
    }

    /// A human-readable rendering of the full command, for dry-run output. The remote
    /// command is single-quoted so what's printed is what would run.
    pub fn display_command(&self, remote_cmd: &str) -> String {
        format!("ssh {} '{}'", self.ssh_args().join(" "), remote_cmd.replace('\'', "'\\''"))
    }

    /// The `ssh …` invocation *without* the `user@host` — i.e. just the transport
    /// options (port, BatchMode, host-key policy, timeout, identity files). This is what
    /// `rsync -e` (and `scp -o ProxyCommand`, etc.) want: rsync appends the `user@host`
    /// itself, so handing it the full `ssh_args` (which end in `user@host`) would be
    /// wrong. Returns a single shell-ready string.
    pub fn rsh_command(&self) -> String {
        let mut parts = vec![
            "ssh".to_string(),
            "-p".into(),
            self.port.to_string(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "StrictHostKeyChecking=no".into(),
            "-o".into(),
            "UserKnownHostsFile=/dev/null".into(),
            "-o".into(),
            "LogLevel=ERROR".into(),
            "-o".into(),
            format!("ConnectTimeout={}", self.connect_timeout_secs),
        ];
        for key in &self.key_paths {
            parts.push("-i".into());
            parts.push(key.clone());
        }
        parts.join(" ")
    }

    /// The `scp` argv to copy `local` -> `host:remote`. scp uses `-P` for the port
    /// (capital, unlike ssh's `-p`) and the same non-interactive/fail-fast options.
    pub fn scp_args(&self, local: &str, remote: &str) -> Vec<String> {
        let mut a = vec![
            "-P".into(),
            self.port.to_string(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "StrictHostKeyChecking=no".into(),
            "-o".into(),
            "UserKnownHostsFile=/dev/null".into(),
            "-o".into(),
            "LogLevel=ERROR".into(),
            "-o".into(),
            format!("ConnectTimeout={}", self.connect_timeout_secs),
        ];
        for key in &self.key_paths {
            a.push("-i".into());
            a.push(key.clone());
        }
        a.push(local.to_string());
        a.push(format!("{}@{}:{}", self.user, self.host, remote));
        a
    }

    /// Human-readable `scp` command for dry-run output.
    pub fn display_scp(&self, local: &str, remote: &str) -> String {
        format!("scp {}", self.scp_args(local, remote).join(" "))
    }
}

/// Resolve the configured SSH key path to one the current user can actually read.
///
/// The shared `config.env` typically points at a key under `/root` (the prod layout),
/// but the tooling often runs as a less-privileged user that keeps a readable copy at
/// `~/.ssh/<same-name>`. Without this, the dashboard's `nvidia-smi`-over-SSH just fails
/// with "identity file not accessible". So: expand a leading `~`, and if the configured
/// path isn't readable but a key with the same file name exists under `$HOME/.ssh`,
/// prefer that. If neither is readable, keep the configured path so the resulting error
/// names what was actually tried. An explicit `SHARED_SSH_KEY_PATH` env override still
/// wins (it's applied before this, at config load) — this is only a fallback.
pub fn resolve_key_path(configured: &str) -> String {
    let home = std::env::var("HOME").ok();
    resolve_key_with(configured, home.as_deref(), |p| {
        std::fs::File::open(p).is_ok()
    })
}

/// The public half of a configured private key (for authorizing it on pods): resolves
/// the path, prefers an existing `<key>.pub`, else derives it with `ssh-keygen -y`.
pub fn public_key_for(private_path: &str) -> Option<String> {
    let resolved = resolve_key_path(private_path);
    let pubkey = std::fs::read_to_string(format!("{resolved}.pub")).ok().or_else(|| {
        std::process::Command::new("ssh-keygen")
            .args(["-y", "-f", &resolved])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    })?;
    let pubkey = pubkey.trim().to_string();
    (!pubkey.is_empty()).then_some(pubkey)
}

/// The public keys that should be authorized on every pod (the shared key + the git
/// deploy key), for `PUBLIC_KEY` injection at create and `setup`'s authorized_keys.
/// (The provider's own account key — e.g. `arena_admin` — is injected automatically.)
pub fn authorized_pubkeys(cfg: &Config) -> Vec<String> {
    let mut out = Vec::new();
    // SHARED_SSH_KEY_PATH = the cohort key (arena8), GIT_SSH_KEY_LOCAL = the persistent
    // deploy key (arena_infra), ADMIN_SSH_KEY_PATH = the ops/admin key. All three are
    // authorized for incoming SSH so the control plane, git, AND an admin can reach pods —
    // and (via the fleet ssh-config) pods can reach each other with the arena_infra key.
    for key in ["SHARED_SSH_KEY_PATH", "GIT_SSH_KEY_LOCAL", "ADMIN_SSH_KEY_PATH"] {
        if let Some(p) = cfg.get(key).filter(|s| !s.is_empty()) {
            if let Some(pk) = public_key_for(p) {
                if !out.contains(&pk) {
                    out.push(pk);
                }
            }
        }
    }
    out
}

/// The ordered, de-duplicated list of SSH identity files the tool should offer pods:
/// `SHARED_SSH_KEY_PATH`, `GIT_SSH_KEY_LOCAL`, then comma-separated `EXTRA_SSH_KEYS`
/// (e.g. a previous cohort's key like `arena7_key`). Each is resolved to a readable
/// copy; if none are readable, all are kept so the resulting error names what was tried.
pub fn config_ssh_keys(cfg: &Config) -> Vec<String> {
    let mut all: Vec<String> = Vec::new();
    let mut add = |raw: &str| {
        let r = resolve_key_path(raw);
        if !all.contains(&r) {
            all.push(r);
        }
    };
    if let Some(s) = cfg.get("SHARED_SSH_KEY_PATH").filter(|s| !s.is_empty()) {
        add(s);
    }
    if let Some(g) = cfg.get("GIT_SSH_KEY_LOCAL").filter(|s| !s.is_empty()) {
        add(g);
    }
    if let Some(extra) = cfg.get("EXTRA_SSH_KEYS") {
        for p in extra.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            add(p);
        }
    }
    let readable: Vec<String> = all.iter().filter(|p| std::fs::File::open(p).is_ok()).cloned().collect();
    if readable.is_empty() {
        all
    } else {
        readable
    }
}

/// The pure core of [`resolve_key_path`], with `$HOME` and the readability check
/// injected so it's testable without touching the real filesystem.
fn resolve_key_with<R: Fn(&str) -> bool>(configured: &str, home: Option<&str>, readable: R) -> String {
    // Expand a leading `~/` against $HOME.
    let expanded = match (configured.strip_prefix("~/"), home) {
        (Some(rest), Some(h)) => format!("{}/{rest}", h.trim_end_matches('/')),
        _ => configured.to_string(),
    };
    if readable(&expanded) {
        return expanded;
    }
    // Fall back to a same-named key under $HOME/.ssh, if that one is readable.
    if let Some(h) = home {
        if let Some(name) = std::path::Path::new(&expanded).file_name().and_then(|n| n.to_str()) {
            let candidate = format!("{}/.ssh/{name}", h.trim_end_matches('/'));
            if candidate != expanded && readable(&candidate) {
                return candidate;
            }
        }
    }
    expanded
}

/// Wrap a command so it runs with the participants' environment — the conda env active
/// and the broadcast-token exports present — instead of a bare non-interactive shell.
///
/// On the pods the login shell is **zsh**, and the participant setup lives in `~/.zshrc`:
/// conda's `conda init` block (defining the `conda` shell function and activating
/// `arena-env`) and the `setup`/`copy-keys` token exports (HF_TOKEN,
/// CLAUDE_CODE_OAUTH_TOKEN, …). A plain `ssh host 'cmd'` runs a non-interactive shell
/// that never reads `~/.zshrc`, so none of that is present (a bare `bash -ic` reads
/// `~/.bashrc`, which gets conda *base* but not the participants' `arena-env`). We
/// therefore source `~/.zshrc` explicitly and then `conda activate <env>` so `python` and
/// installed packages resolve to the participants' env (activation failure is left
/// visible on stderr but never aborts the command). Pass `conda_env: None` to source the
/// rc / tokens without an explicit activate. `~/.zshrc` has no non-interactive guard, so
/// its init runs in full; using `zsh -c` (not `zsh -ic`) avoids the p10k/gitstatus
/// chatter an interactive shell prints when it has no controlling TTY.
///
/// The inner command is single-quoted for `zsh -c`; history expansion doesn't apply to a
/// `-c` string, so a literal `!` is safe.
pub fn login_shell_wrap(cmd: &str, conda_env: Option<&str>) -> String {
    let inner = match conda_env.filter(|e| !e.is_empty()) {
        Some(env) => {
            format!("source ~/.zshrc 2>/dev/null; conda activate {env} >/dev/null 2>&1; {cmd}")
        }
        None => format!("source ~/.zshrc 2>/dev/null; {cmd}"),
    };
    format!("zsh -c '{}'", inner.replace('\'', "'\\''"))
}

/// Strip the harmless startup chatter an interactive shell prints when it has no
/// controlling TTY (we run `bash -ic` without requesting a PTY). Leaves real output and
/// errors untouched — only the two fixed job-control lines are removed.
pub fn strip_interactive_noise(s: &str) -> String {
    s.lines()
        .filter(|l| {
            let t = l.trim();
            !(t.contains("cannot set terminal process group")
                || t.contains("no job control in this shell"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Copy a local file to the target over scp.
pub async fn scp(target: &SshTarget, local: &str, remote: &str) -> Result<SshOutput> {
    let out = Command::new("scp")
        .args(target.scp_args(local, remote))
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| Error::provider(format!("spawning scp to {}: {e}", target.host)))?;
    Ok(SshOutput {
        success: out.status.success(),
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// The result of a remote command.
#[derive(Debug, Clone)]
pub struct SshOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Run `remote_cmd` on the target over SSH and capture its output. stdin is closed so
/// a remote prompt can't wedge the call.
pub async fn run(target: &SshTarget, remote_cmd: &str) -> Result<SshOutput> {
    let out = Command::new("ssh")
        .args(target.ssh_args())
        .arg(remote_cmd)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| Error::provider(format!("spawning ssh to {}: {e}", target.host)))?;
    Ok(SshOutput {
        success: out.status.success(),
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> SshTarget {
        SshTarget {
            user: "root".into(),
            host: "1.2.3.4".into(),
            port: 22001,
            key_paths: vec!["/root/.ssh/arena8_key".into()],
            connect_timeout_secs: 10,
        }
    }

    #[test]
    fn builds_noninteractive_failfast_args() {
        let a = target().ssh_args();
        let joined = a.join(" ");
        assert!(joined.contains("-p 22001"));
        assert!(joined.contains("BatchMode=yes"));
        assert!(joined.contains("StrictHostKeyChecking=no"));
        assert!(joined.contains("UserKnownHostsFile=/dev/null"));
        assert!(joined.contains("ConnectTimeout=10"));
        assert!(joined.contains("-i /root/.ssh/arena8_key"));
        assert!(joined.ends_with("root@1.2.3.4"));
    }

    #[test]
    fn offers_multiple_keys_or_none() {
        let mut t = target();
        t.key_paths = vec!["/a/key1".into(), "/a/key2".into()];
        let joined = t.ssh_args().join(" ");
        assert!(joined.contains("-i /a/key1"));
        assert!(joined.contains("-i /a/key2")); // tries each
        t.key_paths.clear();
        assert!(!t.ssh_args().join(" ").contains("-i "));
    }

    #[test]
    fn key_falls_back_to_home_ssh_when_root_unreadable() {
        // /root key unreadable; same-named key under $HOME/.ssh is readable.
        let got = resolve_key_with("/root/.ssh/arena8_key", Some("/home/dev"), |p| {
            p == "/home/dev/.ssh/arena8_key"
        });
        assert_eq!(got, "/home/dev/.ssh/arena8_key");
    }

    #[test]
    fn key_keeps_configured_path_when_readable() {
        let got = resolve_key_with("/root/.ssh/arena8_key", Some("/home/dev"), |_| true);
        assert_eq!(got, "/root/.ssh/arena8_key");
    }

    #[test]
    fn key_keeps_configured_path_when_no_fallback_exists() {
        // Nothing readable -> keep the original so the error names what was tried.
        let got = resolve_key_with("/root/.ssh/arena8_key", Some("/home/dev"), |_| false);
        assert_eq!(got, "/root/.ssh/arena8_key");
    }

    #[test]
    fn key_expands_leading_tilde() {
        let got = resolve_key_with("~/.ssh/k", Some("/home/dev"), |p| p == "/home/dev/.ssh/k");
        assert_eq!(got, "/home/dev/.ssh/k");
    }

    #[test]
    fn login_shell_wrap_activates_conda_and_quotes() {
        // Sources ~/.zshrc (conda + token exports), activates the env, and single-quotes
        // the whole inner command, escaping any embedded quotes.
        let w = login_shell_wrap("python -c 'import torch'", Some("arena-env"));
        assert_eq!(
            w,
            r#"zsh -c 'source ~/.zshrc 2>/dev/null; conda activate arena-env >/dev/null 2>&1; python -c '\''import torch'\'''"#
        );
    }

    #[test]
    fn login_shell_wrap_without_env_just_sources_rc() {
        // No conda env => still source ~/.zshrc (rc/tokens), but no explicit activation.
        let w = login_shell_wrap("echo hi", None);
        assert_eq!(w, r#"zsh -c 'source ~/.zshrc 2>/dev/null; echo hi'"#);
        // Empty string is treated the same as None.
        assert_eq!(login_shell_wrap("echo hi", Some("")), w);
    }

    #[test]
    fn strip_interactive_noise_drops_only_job_control_lines() {
        let raw = "bash: cannot set terminal process group (42): Inappropriate ioctl for device\n\
                   bash: no job control in this shell\n\
                   2.3.1";
        assert_eq!(strip_interactive_noise(raw), "2.3.1");
        // Real content with neither marker is untouched.
        assert_eq!(strip_interactive_noise("hello\nworld"), "hello\nworld");
    }

    #[test]
    fn display_command_is_quoted() {
        let s = target().display_command("nvidia-smi");
        assert!(s.starts_with("ssh -p 22001"));
        assert!(s.ends_with("'nvidia-smi'"));
    }
}
