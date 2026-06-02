//! SSH command construction + execution against a pod.
//!
//! Both the backup flow (git over SSH) and the dashboard (`nvidia-smi` over SSH) need
//! to run a command on a pod, so this centralizes *how* we reach one. Whether a call
//! mutates anything is entirely a function of the remote command string — callers
//! gate mutation; this module just builds and runs the SSH invocation.
//!
//! The options are deliberately non-interactive and fail-fast: `BatchMode=yes` (never
//! prompt for a password/passphrase), `StrictHostKeyChecking=accept-new` (trust a new
//! host once, but error if a known key changed), and a short `ConnectTimeout` so a
//! down or unreachable pod errors quickly instead of hanging the whole fleet sweep.

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
    pub key_path: Option<String>,
    pub connect_timeout_secs: u32,
}

impl SshTarget {
    /// Build from a pod's current SSH endpoint plus config (`SSH_USER`,
    /// `SHARED_SSH_KEY_PATH`). Errors if the pod has no endpoint yet.
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
            key_path: cfg.get("SHARED_SSH_KEY_PATH").map(resolve_key_path),
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
            key_path: key_path.map(resolve_key_path),
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
            "StrictHostKeyChecking=accept-new".into(),
            "-o".into(),
            format!("ConnectTimeout={}", self.connect_timeout_secs),
        ];
        if let Some(key) = &self.key_path {
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

    /// The `scp` argv to copy `local` -> `host:remote`. scp uses `-P` for the port
    /// (capital, unlike ssh's `-p`) and the same non-interactive/fail-fast options.
    pub fn scp_args(&self, local: &str, remote: &str) -> Vec<String> {
        let mut a = vec![
            "-P".into(),
            self.port.to_string(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "StrictHostKeyChecking=accept-new".into(),
            "-o".into(),
            format!("ConnectTimeout={}", self.connect_timeout_secs),
        ];
        if let Some(key) = &self.key_path {
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
fn resolve_key_path(configured: &str) -> String {
    let home = std::env::var("HOME").ok();
    resolve_key_with(configured, home.as_deref(), |p| {
        std::fs::File::open(p).is_ok()
    })
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
            key_path: Some("/root/.ssh/arena8_key".into()),
            connect_timeout_secs: 10,
        }
    }

    #[test]
    fn builds_noninteractive_failfast_args() {
        let a = target().ssh_args();
        let joined = a.join(" ");
        assert!(joined.contains("-p 22001"));
        assert!(joined.contains("BatchMode=yes"));
        assert!(joined.contains("StrictHostKeyChecking=accept-new"));
        assert!(joined.contains("ConnectTimeout=10"));
        assert!(joined.contains("-i /root/.ssh/arena8_key"));
        assert!(joined.ends_with("root@1.2.3.4"));
    }

    #[test]
    fn omits_identity_flag_without_key() {
        let mut t = target();
        t.key_path = None;
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
    fn display_command_is_quoted() {
        let s = target().display_command("nvidia-smi");
        assert!(s.starts_with("ssh -p 22001"));
        assert!(s.ends_with("'nvidia-smi'"));
    }
}
