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
    /// How the server's host key is checked. Every pod path uses [`HostKeyPolicy::Unchecked`]
    /// (see the module doc: pod IPs are recycled); only the backup archive pins its key.
    pub host_keys: HostKeyPolicy,
}

/// How an [`SshTarget`] treats the server's host key.
///
/// Pods churn host keys constantly, so their checking is off ([`HostKeyPolicy::Unchecked`],
/// the default — every pod caller's argv is exactly what it always was). The backup archive
/// (`arena archive`) is one long-lived VM holding every participant's files, reached with keys
/// that can write to it or administer it: a MITM there would get the cohort's data, so its key
/// is **pinned** in arena's own `known_hosts` file. The pinned modes also make the connection
/// hermetic: `-F /dev/null` (no `~/.ssh/config` or `/etc/ssh/ssh_config` — no stray `Host *`
/// identity, `HashKnownHosts` or proxy), `IdentitiesOnly=yes` (only the one `-i` key is offered,
/// never an agent's or a default identity) and `GlobalKnownHostsFile=/dev/null` (only arena's
/// pin counts). Keep-alives, because a push can sit on one big file for minutes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum HostKeyPolicy {
    /// `StrictHostKeyChecking=no` + `UserKnownHostsFile=/dev/null` (pods).
    #[default]
    Unchecked,
    /// Trust on first use into `known_hosts` (`StrictHostKeyChecking=accept-new`): only for the
    /// first connection to a server this run just created, or when the operator asked to
    /// re-pin (`archive up --accept-new-host-key`).
    PinFirstUse { known_hosts: String },
    /// Only the key already in `known_hosts` is accepted (`StrictHostKeyChecking=yes`).
    Pinned { known_hosts: String },
}

impl HostKeyPolicy {
    /// The `-F`/`-o` options this policy puts between `-p <port>` and the `-i` keys (the
    /// connect timeout last, as it always was).
    fn options(&self, connect_timeout_secs: u32) -> Vec<String> {
        let mut a: Vec<String> = Vec::new();
        let mut opt = |s: String| {
            a.push("-o".into());
            a.push(s);
        };
        match self {
            HostKeyPolicy::Unchecked => {
                opt("BatchMode=yes".into());
                opt("StrictHostKeyChecking=no".into());
                opt("UserKnownHostsFile=/dev/null".into());
            }
            HostKeyPolicy::PinFirstUse { known_hosts } | HostKeyPolicy::Pinned { known_hosts } => {
                let strict = if matches!(self, HostKeyPolicy::Pinned { .. }) { "yes" } else { "accept-new" };
                opt("BatchMode=yes".into());
                opt(format!("StrictHostKeyChecking={strict}"));
                opt(format!("UserKnownHostsFile={known_hosts}"));
                opt("GlobalKnownHostsFile=/dev/null".into());
                opt("IdentitiesOnly=yes".into());
                opt("ServerAliveInterval=30".into());
                opt("ServerAliveCountMax=4".into());
            }
        }
        opt("LogLevel=ERROR".into());
        opt(format!("ConnectTimeout={connect_timeout_secs}"));
        if !matches!(self, HostKeyPolicy::Unchecked) {
            a.splice(0..0, ["-F".to_string(), "/dev/null".to_string()]);
        }
        a
    }
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
            host_keys: HostKeyPolicy::Unchecked,
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
            host_keys: HostKeyPolicy::Unchecked,
        }
    }

    /// The `ssh` argv *before* the remote command — non-interactive, fail-fast.
    pub fn ssh_args(&self) -> Vec<String> {
        let mut a = vec!["-p".into(), self.port.to_string()];
        a.extend(self.host_keys.options(self.connect_timeout_secs));
        for key in &self.key_paths {
            a.push("-i".into());
            a.push(key.clone());
        }
        a.push(format!("{}@{}", self.user, self.host));
        a
    }

    /// A human-readable rendering of the full command, for dry-run output — shell-quoted
    /// ([`shell_word`]: the remote command always, any other argument that needs it), so what's
    /// printed pastes into a shell as exactly what would run. The remote command is shown as
    /// written, not as [`for_any_login_shell`] sends it: the same where the login shell is a
    /// POSIX one (not on a fish box).
    pub fn display_command(&self, remote_cmd: &str) -> String {
        let args: Vec<String> = self.ssh_args().iter().map(|a| shell_word(a)).collect();
        format!("ssh {} '{}'", args.join(" "), remote_cmd.replace('\'', "'\\''"))
    }

    /// The `ssh …` invocation *without* the `user@host` — i.e. just the transport
    /// options (port, BatchMode, host-key policy, timeout, identity files). This is what
    /// `rsync -e` (and `scp -o ProxyCommand`, etc.) want: rsync appends the `user@host`
    /// itself, so handing it the full `ssh_args` (which end in `user@host`) would be
    /// wrong. Returns a single shell-ready string.
    pub fn rsh_command(&self) -> String {
        let mut parts = vec!["ssh".to_string(), "-p".into(), self.port.to_string()];
        parts.extend(self.host_keys.options(self.connect_timeout_secs));
        for key in &self.key_paths {
            parts.push("-i".into());
            parts.push(key.clone());
        }
        parts.join(" ")
    }

    /// The `scp` argv to copy `local` -> `host:remote`. scp uses `-P` for the port
    /// (capital, unlike ssh's `-p`) and the same non-interactive/fail-fast options.
    pub fn scp_args(&self, local: &str, remote: &str) -> Vec<String> {
        let mut a = vec!["-P".into(), self.port.to_string()];
        a.extend(self.host_keys.options(self.connect_timeout_secs));
        for key in &self.key_paths {
            a.push("-i".into());
            a.push(key.clone());
        }
        a.push(local.to_string());
        a.push(format!("{}@{}:{}", self.user, self.host, remote));
        a
    }

    /// Human-readable `scp` command for dry-run output, shell-quoted like [`Self::display_command`].
    pub fn display_scp(&self, local: &str, remote: &str) -> String {
        let args: Vec<String> = self.scp_args(local, remote).iter().map(|a| shell_word(a)).collect();
        format!("scp {}", args.join(" "))
    }
}

/// One argument as a POSIX shell reads it back unchanged: as is when it's only characters no
/// shell treats specially (`-avz`, `--timeout=300`, `root@1.2.3.4:`, `/a/b`), else
/// single-quoted (`'**/.git/**'`, `'ssh -p 22'`, `''`) with any `'` as `'\''`. For the commands
/// a dry run prints: an unquoted glob (`--include **/.git/**`) pasted into a shell would expand
/// against the operator's current directory (live finding).
pub fn shell_word(s: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "@%+=:,./_-".contains(c);
    if !s.is_empty() && s.chars().all(safe) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// What sshd is sent for `cmd`, a command written for bash, zsh and sh, so that a box whose user's
/// login shell is **fish** runs it too, and a POSIX login shell still runs it itself.
///
/// sshd hands the command to the login shell (`$SHELL -c '…'`). fish reads POSIX code its own way:
/// an `if …; then`, a `$?` or a `${x}` doesn't parse (live: the dashboard's probe failed on a fish
/// box), and in single quotes it reads `\\` and `\'` as escapes, so a command quoted for sh can
/// arrive changed with no error at all (live: `'a\\b'` came out `a\b`).
///
/// It goes as one line both kinds of shell parse, each running only its own part. They disagree on
/// whether a `\'` inside single quotes ends the quote, so a quoted stretch for one is code for the
/// other:
///
/// ```text
/// set -- __arena_p '<cmd>'; true '\''; <fish part>; true '; <POSIX part> #'
/// ```
/// - **Both** shells run `set`, which puts the command in `$2` (POSIX) or `$__arena_p` (fish). It is
///   [`printf_format`]-encoded: no `'`, and no backslash but before three octal digits, so both
///   read it alike.
/// - **bash, zsh, sh, ksh and ash** run the POSIX part; the fish part is a quoted argument to
///   `true`. The POSIX part decodes the command with `printf` (a builtin), clears the positional
///   parameters and `eval`s it in the login shell itself. Its startup runs once, and its dialect,
///   `$0` and exit status are the bare command's. Through `eval`, three things differ from a bare
///   command, none of which arena relies on: positional parameters a startup file set are cleared,
///   an `ERR` trap a startup file set fires once more when the command fails, and the shell forks
///   for the last command instead of `exec`ing it.
/// - **fish** runs the fish part; the POSIX part is quoted, and for POSIX the `#` starts a comment.
///   The fish part `exec`s `/bin/sh` to decode the command and `exec` bash with it, else
///   `/bin/sh`. arena's commands are tested under bash, and the process, signals and exit status
///   stay the command's own.
///
/// Other login shells (csh, nu) read neither part and fail, as they did before.
///
/// **Too big to encode** (the line would pass Linux's 128 KiB limit for one argument; encoding
/// grows a script by about a tenth): `true '\''; <refusal>; true '; <cmd>`. POSIX shells run
/// the bare command after a `true`, exactly as before. fish parses the whole line before running
/// any of it, so it either fails to parse the bare command or runs the refusal first (exit 126):
/// it never runs the command misread. Pure.
pub fn for_any_login_shell(cmd: &str) -> String {
    let line = format!("set -- __arena_p '{}'; true '\\''; {FISH_PART}; true '; {POSIX_PART} #'", printf_format(cmd));
    if line.len() < MAX_ARG_LEN {
        return line;
    }
    format!("true '\\''; {FISH_TOO_BIG}; true '; {cmd}")
}

/// What fish runs for a command too big to encode: it says so and exits 126. It sits in POSIX's
/// single quotes: no `'`.
const FISH_TOO_BIG: &str =
    r#"echo "arena: this command is too big to send to a fish login shell (over 128 KiB encoded)" >&2; exit 126"#;

/// Linux's limit on one argument (`MAX_ARG_STRLEN`, 128 KiB with its NUL): sshd passes the
/// command to the login shell as one.
const MAX_ARG_LEN: usize = 128 * 1024;

/// The part of [`for_any_login_shell`]'s line that bash, zsh and sh run: decode `$2`, clear the
/// positional parameters, unset its own variable and `eval` the command (the `printf x` keeps
/// trailing newlines). It sits in fish's single quotes: no `'`, no backslash.
const POSIX_PART: &str = r#"__arena_c=$(printf "$2"; printf x); set --; eval "unset __arena_c; ${__arena_c%x}""#;

/// The part fish runs: `/bin/sh` decodes `$__arena_p` and `exec`s bash with it, else `/bin/sh`. It
/// sits in POSIX's single quotes: no `'`. (fish's double quotes: `\$` and `\"` are `$` and `"`.)
const FISH_PART: &str = concat!(
    r#"exec /bin/sh -c "c=\$(printf \"\$1\"; printf x); c=\${c%x}; "#,
    r#"command -v bash >/dev/null 2>&1 && exec bash -c \"\$c\" bash; exec /bin/sh -c \"\$c\" sh" arena $__arena_p"#,
);

/// `s` as a `printf` format that prints `s`, readable by every shell inside single quotes: `%` is
/// `%%`; a backslash, a single quote, `!`, a control character and a leading `-` (which
/// `printf` would take for an option) are 3-digit octal escapes (`\047`) — `printf` reads at most
/// three digits, so a digit after one is never taken into it. Pure.
fn printf_format(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + s.len() / 8);
    for (i, c) in s.chars().enumerate() {
        match c {
            '%' => out.push_str("%%"),
            '\\' | '\'' | '!' => out.push_str(&format!("\\{:03o}", c as u32)),
            '-' if i == 0 => out.push_str("\\055"),
            c if c.is_ascii_control() => out.push_str(&format!("\\{:03o}", c as u32)),
            c => out.push(c),
        }
    }
    out
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

/// Whether a [`login_shell_wrap`]ped command failed because the pod has no zsh at all — a
/// bare VM (Hetzner) before setup installs it: exit 127 with the outer shell's `zsh: command
/// not found` (bash) or `zsh: not found` (dash). The command itself never ran, so its exit
/// says nothing about it; a retry finds the same, and setup is the fix. Running it under
/// bash instead would hide that the participants' own shell is missing (and with it their
/// conda env and tokens), so the wrap stays zsh-only and the caller says this instead. Pure.
pub fn login_shell_missing(code: Option<i32>, stderr: &str) -> bool {
    code == Some(127)
        && stderr.lines().map(str::trim).any(|l| l.ends_with("zsh: command not found") || l.ends_with("zsh: not found"))
}

/// What to say when [`login_shell_missing`] on `pod` — short, as it lands in a NOTES cell.
pub fn login_shell_missing_hint(pod: &str) -> String {
    format!("no zsh on the pod (setup installs it) — not set up yet? `arena pods setup {pod}`")
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

/// Copy a local file to the target over scp. No time budget: pod paths — the CLI's and
/// the dashboard's — use [`crate::remote::Remote`] instead (timeouts, fakes); this stays
/// for the proxy host.
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

impl SshOutput {
    /// How it ended, for a message: `exit 1`, or `killed by a signal` ([`exit_label`]).
    pub fn exit_label(&self) -> String {
        exit_label(self.code)
    }
}

/// A process's exit status as a person reads it: `exit 1` — never Rust's `Some(1)` — or
/// `killed by a signal` when there is no code (the process was signalled: a timeout's kill, OOM).
pub fn exit_label(code: Option<i32>) -> String {
    match code {
        Some(c) => format!("exit {c}"),
        None => "killed by a signal".to_string(),
    }
}

/// Run `remote_cmd` on the target over SSH and capture its output. stdin is closed so
/// a remote prompt can't wedge the call. No time budget — see [`scp`] for who still uses
/// this rather than [`crate::remote::Remote`].
pub async fn run(target: &SshTarget, remote_cmd: &str) -> Result<SshOutput> {
    let out = Command::new("ssh")
        .args(target.ssh_args())
        .arg(for_any_login_shell(remote_cmd))
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
            host_keys: Default::default(),
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

    /// Regression for every existing caller (spec §14 item 12): the default policy's argv is
    /// byte for byte what `SshTarget` built before host-key policies existed.
    #[test]
    fn unchecked_targets_build_exactly_the_old_argv() {
        let t = target();
        assert_eq!(t.host_keys, HostKeyPolicy::Unchecked);
        let old_opts = [
            "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null", "-o",
            "LogLevel=ERROR", "-o", "ConnectTimeout=10",
        ];
        let mut want: Vec<String> = ["-p", "22001"].iter().chain(old_opts.iter()).map(|s| s.to_string()).collect();
        want.extend(["-i".to_string(), "/root/.ssh/arena8_key".into(), "root@1.2.3.4".into()]);
        assert_eq!(t.ssh_args(), want);
        assert_eq!(
            t.rsh_command(),
            format!("ssh -p 22001 {} -i /root/.ssh/arena8_key", old_opts.join(" "))
        );
        let mut want_scp: Vec<String> = ["-P", "22001"].iter().chain(old_opts.iter()).map(|s| s.to_string()).collect();
        want_scp.extend(["-i".to_string(), "/root/.ssh/arena8_key".into(), "/a".into(), "root@1.2.3.4:/b".into()]);
        assert_eq!(t.scp_args("/a", "/b"), want_scp);
        // `for_host` / `from_pod` start unchecked too.
        assert_eq!(SshTarget::for_host("root", "h", 22, None).host_keys, HostKeyPolicy::Unchecked);
    }

    /// The archive's modes (S3): pinned to arena's own known_hosts, hermetic (no ssh config, no
    /// global known_hosts), only the given key offered; first use only accepts a NEW key.
    #[test]
    fn pinned_policies_are_strict_hermetic_and_offer_one_key() {
        let mut t = target();
        for (policy, strict) in [
            (HostKeyPolicy::Pinned { known_hosts: "/st/known_hosts".into() }, "StrictHostKeyChecking=yes"),
            (HostKeyPolicy::PinFirstUse { known_hosts: "/st/known_hosts".into() }, "StrictHostKeyChecking=accept-new"),
        ] {
            t.host_keys = policy;
            for argv in [t.ssh_args(), t.scp_args("/a", "/b"), t.rsh_command().split(' ').map(String::from).collect()] {
                let has = |o: &str| argv.windows(2).any(|w| w[0] == "-o" && w[1] == o);
                assert!(has(strict), "{argv:?}");
                for o in ["UserKnownHostsFile=/st/known_hosts", "GlobalKnownHostsFile=/dev/null", "IdentitiesOnly=yes", "BatchMode=yes"] {
                    assert!(has(o), "{o}: {argv:?}");
                }
                assert!(!argv.iter().any(|a| a.contains("StrictHostKeyChecking=no") || a == "UserKnownHostsFile=/dev/null"), "{argv:?}");
                assert!(argv.windows(2).any(|w| w[0] == "-F" && w[1] == "/dev/null"), "{argv:?}");
                assert_eq!(argv.iter().filter(|a| *a == "-i").count(), 1, "exactly one identity: {argv:?}");
            }
        }
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

    /// Live BPF F1/S9: a bare Hetzner VM has no zsh until setup, so every wrapped command
    /// ends `exit 127: bash: line 1: zsh: command not found`. Only the outer shell's own
    /// "no zsh" counts — a command zsh couldn't find, or another exit, is the command's.
    #[test]
    fn login_shell_missing_only_for_the_outer_shell_finding_no_zsh() {
        for (code, stderr, missing) in [
            (Some(127), "bash: line 1: zsh: command not found\n", true),
            (Some(127), "sh: 1: zsh: not found", true),
            (Some(127), "Warning: Permanently added '1.2.3.4' (ED25519) to the list of known hosts.\nbash: zsh: command not found", true),
            (Some(127), "zsh:1: command not found: pytest", false), // zsh ran; the command is missing
            (Some(1), "bash: line 1: zsh: command not found", false),
            (None, "bash: line 1: zsh: command not found", false),
            (Some(127), "", false),
        ] {
            assert_eq!(login_shell_missing(code, stderr), missing, "{code:?} {stderr:?}");
        }
        let hint = login_shell_missing_hint("devtest-hz2");
        assert!(hint.contains("no zsh") && hint.contains("`arena pods setup devtest-hz2`"), "{hint}");
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

    #[test]
    fn shell_words_quote_only_what_a_shell_would_change() {
        for (raw, want) in [
            ("-avz", "-avz"),
            ("--timeout=300", "--timeout=300"),
            ("root@1.2.3.4:", "root@1.2.3.4:"),
            ("/root/.ssh/arena8_key", "/root/.ssh/arena8_key"),
            ("**/.git/**", "'**/.git/**'"),
            ("models--*/", "'models--*/'"),
            ("ssh -p 22", "'ssh -p 22'"),
            ("it's", "'it'\\''s'"),
            ("~/x", "'~/x'"),
            ("$HOME", "'$HOME'"),
            ("", "''"),
        ] {
            assert_eq!(shell_word(raw), want, "{raw:?}");
        }
    }

    /// What a dry run prints is what a shell runs: every argument comes back intact — a key
    /// path with a space included — from a real bash.
    #[cfg(unix)]
    #[test]
    fn displayed_ssh_and_scp_commands_paste_back_as_their_argv() {
        let mut t = target();
        t.key_paths = vec!["/keys/my key".into(), "/keys/k2".into()];
        let echo = |line: &str, program: &str| {
            let rest = line.strip_prefix(&format!("{program} ")).expect("program first");
            let o = std::process::Command::new("bash").arg("-c").arg(format!("printf '%s\\n' {rest}")).output().unwrap();
            String::from_utf8_lossy(&o.stdout).lines().map(String::from).collect::<Vec<_>>()
        };
        let mut want = t.ssh_args();
        want.push("echo 'hi' $HOME".into());
        assert_eq!(echo(&t.display_command("echo 'hi' $HOME"), "ssh"), want);
        assert_eq!(echo(&t.display_scp("/tmp/a b", "/root/x"), "scp"), t.scp_args("/tmp/a b", "/root/x"));
    }
    /// A command full of what shells read differently: quotes of both kinds, backslashes, `%`, `!`,
    /// `$(…)`, `${x}`, `$?`, `$#`, `$0`, `if`/`for`, a here-doc, UTF-8, an exit status — and which
    /// shell ran it.
    const GNARLY: &str = "printf '%s|' \"a b\" 'it'\\''s' \"back\\\\slash\" '100%' '!bang' \"$((6*7))\" '\\047' \"n=$#\" \"z=${0##*/}\"; echo
x=$(echo sub); if [ \"${x}\" = sub ]; then echo \"if-ok $?\"; fi
for i in 1 2; do printf \"$i\"; done; echo; echo ${ZSH_VERSION:+zsh}${BASH_VERSION:+bash}.
cat <<'EOF'
here $HOME \\n %d
EOF
echo \"été ✓\"
(exit 3)
";

    /// The line's parts keep to what the other kind of shell reads them in (fish's single quotes:
    /// no `'`, no backslash; POSIX's: no `'`); the command is encoded with no quote, newline or `!`,
    /// and a backslash only before three octal digits. A command too big to encode within one
    /// argument goes bare behind fish's refusal.
    #[test]
    fn the_line_keeps_each_part_inside_the_other_shells_quotes() {
        assert!(!POSIX_PART.contains('\'') && !POSIX_PART.contains('\\'), "{POSIX_PART}");
        assert!(!FISH_PART.contains('\''), "{FISH_PART}");
        let w = for_any_login_shell(GNARLY);
        let enc = printf_format(GNARLY);
        assert_eq!(w, format!("set -- __arena_p '{enc}'; true '\\''; {FISH_PART}; true '; {POSIX_PART} #'"));
        assert!(!enc.contains(['\'', '\n', '!']), "{enc}");
        let b = enc.as_bytes();
        for (i, _) in enc.match_indices('\\') {
            assert!(b.len() > i + 3 && b[i + 1..i + 4].iter().all(|d| (b'0'..=b'7').contains(d)), "{}", &enc[i..]);
        }
        assert_eq!(printf_format("-x 5%"), "\\055x 5%%");
        assert_eq!(printf_format("a\\b'c!\n\t"), "a\\134b\\047c\\041\\012\\011");
        assert_eq!(printf_format("\\0471"), "\\1340471", "a digit after an escape stays a digit");
        assert!(!FISH_TOO_BIG.contains('\''), "{FISH_TOO_BIG}");
        let huge = ":\n".repeat(30_000);
        assert_eq!(for_any_login_shell(&huge), format!("true '\\''; {FISH_TOO_BIG}; true '; {huge}"), "4× in encoding would pass 128 KiB");
        // The biggest script arena sends fits, with room.
        let resolve = for_any_login_shell(include_str!("resolve.sh"));
        assert!(resolve.starts_with("set -- __arena_p") && resolve.len() < 100 * 1024, "{}", resolve.len());
    }

    /// The paths of the shells installed here, by name; `busybox` as its `sh`. Not installed:
    /// said, and left out — a failure where `ARENA_TEST_ALL_SHELLS` is set (CI installs them all).
    #[cfg(target_os = "linux")]
    fn installed(names: &[&'static str]) -> Vec<(&'static str, Vec<String>)> {
        let mut out = Vec::new();
        for &sh in names {
            let o = std::process::Command::new("sh").arg("-c").arg(format!("command -v {sh}")).output().unwrap();
            let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !o.status.success() || !p.starts_with('/') {
                assert!(std::env::var_os("ARENA_TEST_ALL_SHELLS").is_none(), "{sh} isn't installed, and ARENA_TEST_ALL_SHELLS is set");
                eprintln!("{sh} isn't installed here: not tried as the login shell");
                continue;
            }
            out.push((sh, if sh == "busybox" { vec![p, "sh".into()] } else { vec![p] }));
        }
        out
    }

    /// Run `cmd` as sshd runs it: `<login shell> -c <cmd>`, in an empty home, with `env`.
    #[cfg(target_os = "linux")]
    fn as_sshd(shell: &[String], home: &std::path::Path, cmd: &str, env: &[(&str, &str)]) -> (String, Option<i32>, String) {
        let o = std::process::Command::new(&shell[0])
            .args(&shell[1..])
            .arg("-c")
            .arg(cmd)
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("HOME", home)
            .envs(env.iter().copied())
            .output()
            .unwrap();
        (String::from_utf8_lossy(&o.stdout).into_owned(), o.status.code(), String::from_utf8_lossy(&o.stderr).into_owned())
    }

    /// Under each login shell installed here, sent as [`for_any_login_shell`] makes it: bash, zsh,
    /// dash and busybox's sh run the command themselves — what each prints sent bare, its `$0`,
    /// no positional parameters — and fish (which gets it wrong bare) runs it under bash. Exit 3
    /// everywhere, nothing on stderr.
    #[cfg(target_os = "linux")]
    #[test]
    fn each_login_shell_runs_the_command_as_sent_bare_and_fish_as_bash() {
        let home = std::env::temp_dir().join(format!("arena-any-shell-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let shells = installed(&["bash", "zsh", "dash", "busybox", "fish"]);
        assert!(shells.iter().any(|(n, _)| *n == "bash"), "bash is needed");
        let bash = &shells.iter().find(|(n, _)| *n == "bash").unwrap().1;
        for (name, shell) in &shells {
            let bare = if *name == "fish" { as_sshd(bash, &home, GNARLY, &[]) } else { as_sshd(shell, &home, GNARLY, &[]) };
            assert_eq!(bare.1, Some(3), "{name} bare: {}", bare.2);
            let ran = match *name {
                "fish" => "z=bash|".to_string(),
                "busybox" => "z=sh|".to_string(),
                n => format!("z={n}|"),
            };
            assert!(bare.0.contains(&ran) && bare.0.contains("n=0|") && bare.0.contains("a b|it's|back\\slash|100%|!bang|42|\\047|"), "{name}: {}", bare.0);
            let sent = as_sshd(shell, &home, &for_any_login_shell(GNARLY), &[]);
            assert_eq!(sent, (bare.0.clone(), Some(3), String::new()), "{name}");
            if *name == "fish" {
                assert_ne!(as_sshd(shell, &home, GNARLY, &[]).0, bare.0, "fish reads the bare command its own way");
            }
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A command too big to encode (over 128 KiB as a line): a POSIX login shell runs it bare, as
    /// before; fish runs none of it — not a line misread — and fails.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_command_too_big_to_encode_runs_bare_under_posix_and_not_at_all_under_fish() {
        let home = std::env::temp_dir().join(format!("arena-too-big-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let mark = home.join("ran");
        // 40,000 backslashes in quotes (fish would halve them), then a mark that it ran.
        let cmd = format!("printf '%s' '{}' | wc -c; : > {}\n{}", "\\".repeat(40_000), mark.display(), ":\n".repeat(20_000));
        let sent = for_any_login_shell(&cmd);
        assert!(sent.starts_with("true '\\'';") && sent.ends_with(&cmd));
        for (name, shell) in installed(&["bash", "zsh", "dash", "fish"]) {
            let _ = std::fs::remove_file(&mark);
            let (out, code, err) = as_sshd(&shell, &home, &sent, &[]);
            if name == "fish" {
                assert!(!mark.exists() && out.is_empty() && code.is_some_and(|c| c != 0), "fish: {code:?} {out:?} {err}");
            } else {
                assert_eq!((out.trim(), code, mark.exists()), ("40000", Some(0), true), "{name}: {err}");
            }
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The login shell's startup runs once, as for a bare command: bash's `~/.bashrc` (sourced over
    /// ssh) and zsh's `~/.zshenv` each append one line, and a function `~/.bashrc` defines is there
    /// for the command (review ai-review codex #1: an earlier design ran the shell again, and its
    /// startup twice).
    #[cfg(target_os = "linux")]
    #[test]
    fn the_login_shells_startup_runs_once() {
        let home = std::env::temp_dir().join(format!("arena-rc-once-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let log = home.join("rc.log");
        std::fs::write(home.join(".bashrc"), format!("echo bashrc >> {}\nrcfn() {{ echo fn-ok; }}\n", log.display())).unwrap();
        std::fs::write(home.join(".zshenv"), format!("echo zshenv >> {}\n", log.display())).unwrap();
        for (name, shell) in installed(&["bash", "zsh"]) {
            let _ = std::fs::remove_file(&log);
            let cmd = if name == "bash" { "rcfn; [ -n \"$BASH_VERSION\" ] && echo in-bash" } else { "echo \"in-${ZSH_NAME}\"" };
            let (out, code, err) = as_sshd(&shell, &home, &for_any_login_shell(cmd), &[("SSH_CLIENT", "10.0.0.1 50000 22")]);
            let rc = std::fs::read_to_string(&log).unwrap_or_default();
            if name == "bash" {
                // A bash built without SSH_SOURCE_BASHRC reads none (and so has no `rcfn`): once
                // or not at all, as for a bare command — never twice.
                let bare = as_sshd(&shell, &home, cmd, &[("SSH_CLIENT", "10.0.0.1 50000 22")]);
                let _ = std::fs::remove_file(&log);
                let sent = as_sshd(&shell, &home, &for_any_login_shell(cmd), &[("SSH_CLIENT", "10.0.0.1 50000 22")]);
                assert_eq!((sent.0.as_str(), sent.1), (bare.0.as_str(), bare.1), "{err}");
                let once = std::fs::read_to_string(&log).unwrap_or_default();
                assert!(once == "bashrc\n" || (once.is_empty() && bare.0 == "in-bash\n"), "{once:?}");
                assert!(rc == once, "{rc:?} then {once:?}");
            } else {
                assert_eq!(code, Some(0), "{name}: {err}");
                assert_eq!((out.as_str(), rc.as_str()), ("in-zsh\n", "zshenv\n"), "{err}");
            }
        }
        let _ = std::fs::remove_dir_all(&home);
    }
}
