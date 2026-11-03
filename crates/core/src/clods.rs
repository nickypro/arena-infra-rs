//! clods on the pods: every Claude Code account on each machine, one profile each
//! (`~/.clods/profiles/claudeN`), with the defaults spread across the fleet.
//!
//! **What clods is.** `clods/` at the repo's root: shell scripts that set up one Claude Code
//! config dir per account (`CLAUDE_CONFIG_DIR` picks it; the token is in its `settings.json`),
//! `clodN` and `clods` in the shells, and a default for plain `claude`. Its README says how; its
//! tests run in CI. The binary carries it ([`FILES`], built from those files), so pushing needs no
//! checkout of it here.
//!
//! **When.** Only when config's `CLODS_DIR` names a folder holding a `tokens` file (clods'
//! format, the one `arena claude` reads too). Unset, or no `tokens` there: nothing happens and
//! nothing is said ([`Clods::from_config`]). Setup, `up` and replace push to each pod they set up
//! ([`stage`]); `pods setup --clods-only` to running pods; `arena clods install` writes the scripts
//! into a local clods folder ([`install`]).
//!
//! **What a pod gets is clods' call.** Arena carries it: an installer ([`installer`]) holding the
//! scripts and the tokens file as it is, which runs clods' `setup.sh` on the pod with every
//! account (`--prune`: an account taken out of `tokens` loses its token there), a default by
//! spread (`--spread T`) unless the pod has one (`--keep-default`, so a re-push moves nobody),
//! and one line back (`--summary`: [`Summary`]). The numbering, the files and their modes, the
//! shell hook: all `setup.sh`'s, as on any other machine clods sets up.
//!
//! **Why not `push.sh --arena`.** That runs `arena pods cp` and `pods run` as child processes: a
//! second listing, a confirm or `--yes`, arena's output to read back, and the installer copied to
//! the pod as a file (left there if the run step never ran). In here, the installer goes on the
//! ssh command's stdin over arena's own [`Remote`] — the way `arena claude` sends its token —
//! which reaches Hetzner, RunPod and Vast alike: never in a command line, a log or a file here,
//! and on the pod only in a 0600 file in a 0700 temp dir that the installer removes when it ends.
//!
//! **The spread.** Each pod that has no default yet gets the next account in turn, from a counter
//! in the fleet's state dir ([`take_turns`]): `up -a 28` puts its pods on accounts 1, 2, …, N, 1, …
//! as they come up, one at a time or all at once. A failed push is the caller's warning, never a
//! failed setup: it isn't the machine's fault.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;

use crate::config::Config;
use crate::pod::Pod;
use crate::remote::{describe_error, Remote};
use crate::ssh::SshTarget;

/// The config key naming the clods folder (the one holding `tokens`).
pub const DIR_KEY: &str = "CLODS_DIR";

/// The tokens file in it.
pub const TOKENS: &str = "tokens";

/// How long one pod's push may take: a few python runs and small files.
pub const TIMEOUT: Duration = Duration::from_secs(90);

/// The spread's counter, in the fleet's state dir.
pub const SPREAD_FILE: &str = "clods-spread";

/// What a machine gets of clods, as `clods/` has them (setup.sh's `tool_files`; a test holds the
/// two lists together). Built from the files, so the binary can't carry another version.
pub const FILES: [(&str, &str); 6] = [
    ("setup.sh", include_str!("../../../clods/setup.sh")),
    ("push.sh", include_str!("../../../clods/push.sh")),
    ("clods.sh", include_str!("../../../clods/clods.sh")),
    ("README.md", include_str!("../../../clods/README.md")),
    ("fleet.example", include_str!("../../../clods/fleet.example")),
    (".gitignore", include_str!("../../../clods/.gitignore")),
];

/// The command that installs on the pod: bash, reading the installer on its stdin.
pub const COMMAND: &str = "bash -s";

// ---------------------------------------------------------------------------------------
// Config and the tokens file
// ---------------------------------------------------------------------------------------

/// A configured clods: `CLODS_DIR`, which holds a `tokens` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clods {
    pub dir: PathBuf,
}

/// The tokens file, read: its text as it is (sent so) and its accounts, `(number, email)`.
#[derive(Clone)]
pub struct Tokens {
    text: String,
    pub accounts: Vec<(u32, String)>,
    /// The lines that aren't an account, named without their tokens.
    pub skipped: Vec<String>,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens").field("text", &"<redacted>").field("accounts", &self.accounts).finish()
    }
}

impl Tokens {
    /// The file's text — only ever put into an [`installer`].
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// `CLODS_DIR` as config has it, `~/` expanded; `None` when it's unset or empty.
pub fn configured_dir(cfg: &Config) -> Option<PathBuf> {
    let raw = cfg.get(DIR_KEY).map(str::trim).filter(|v| !v.is_empty())?;
    Some(expand_home(raw))
}

/// `~/x` as `$HOME/x`, `~` as `$HOME`; anything else as it is.
pub fn expand_home(p: &str) -> PathBuf {
    let home = std::env::var_os("HOME");
    match (p, p.strip_prefix("~/"), home) {
        ("~", _, Some(h)) => PathBuf::from(h),
        (_, Some(rest), Some(h)) => PathBuf::from(h).join(rest),
        _ => PathBuf::from(p),
    }
}

impl Clods {
    /// From config: `CLODS_DIR` when it's set and has a `tokens` file. `None` otherwise — then
    /// nothing is pushed and nothing is said.
    pub fn from_config(cfg: &Config) -> Option<Self> {
        let dir = configured_dir(cfg)?;
        dir.join(TOKENS).exists().then_some(Clods { dir })
    }

    pub fn tokens_path(&self) -> PathBuf {
        self.dir.join(TOKENS)
    }

    /// Read the tokens file: refused when other users of this box can read it (as `arena claude`
    /// refuses its tokens file), when it can't be parsed, or when it holds no account (a push
    /// would take every token off the pods). Never a token in the message.
    pub fn read(&self) -> Result<Tokens, String> {
        let path = self.tokens_path();
        let shown = path.display();
        let meta = std::fs::metadata(&path).map_err(|e| format!("{shown}: {e}"))?;
        if !meta.is_file() {
            return Err(format!("{shown} isn't a file"));
        }
        if let Some(why) = crate::claude::mode_problem(&meta) {
            return Err(format!("{shown} {why} (fix: chmod 600 {shown})"));
        }
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{shown}: {e}"))?;
        let parsed = crate::claude::parse_tokens(&text).map_err(|e| format!("{shown}: {e}"))?;
        if parsed.accounts.is_empty() {
            return Err(format!("{shown} holds no account (one `email -> sk-ant-oat01-…` per line)"));
        }
        let accounts = parsed.accounts.iter().map(|a| (a.num, a.email.clone())).collect();
        Ok(Tokens { text, accounts, skipped: parsed.skipped })
    }
}

// ---------------------------------------------------------------------------------------
// The installer and what it says
// ---------------------------------------------------------------------------------------

/// The installer for one pod, for `bash -s` on its stdin: [`FILES`] and the tokens file into a
/// private temp dir, then clods' `setup.sh` with every account, `--prune`, the default by spread
/// (turn `turn`) unless the pod has one, and `--summary`. Everything sits in `main`, so bash has
/// read it all before anything runs (a dropped connection runs nothing). The heredocs' end word is
/// `eof`, which no line of any file may be ([`installer`] picks one). Pure.
pub fn installer_with(eof: &str, tokens: &str, turn: u64) -> String {
    let mut s = String::from(
        "#!/usr/bin/env bash\n\
         # Installs Claude Code profiles into ~/.clods (clods, sent by arena). It holds account tokens.\n\
         main() {\n\
         set -euo pipefail\n\
         umask 077\n\
         clod_tmp=$(mktemp -d)\n\
         trap 'rm -rf \"$clod_tmp\"' EXIT\n",
    );
    let mut file = |name: &str, text: &str| {
        s.push_str(&format!("cat > \"$clod_tmp/{name}\" <<'{eof}'\n{text}"));
        if !text.is_empty() && !text.ends_with('\n') {
            s.push('\n');
        }
        s.push_str(eof);
        s.push('\n');
    };
    for (name, text) in FILES {
        file(name, text);
    }
    file(TOKENS, tokens);
    s.push_str(&format!(
        "bash \"$clod_tmp/setup.sh\" --prune --keep-default --summary --spread {turn} \"$clod_tmp/{TOKENS}\" </dev/null\n}}\nmain \"$@\"\n"
    ));
    s
}

/// [`installer_with`] and an end word that no line of the scripts or the tokens file is.
pub fn installer(tokens: &str, turn: u64) -> String {
    let taken = |eof: &str| FILES.iter().map(|(_, t)| *t).chain([tokens]).any(|t| t.lines().any(|l| l == eof));
    let mut n = crate::update::sha256_hex(format!("{turn}{:?}", std::time::SystemTime::now()).as_bytes());
    loop {
        let eof = format!("__CLODS_{}__", &n[..12]);
        if !taken(&eof) {
            return installer_with(&eof, tokens, turn);
        }
        n = crate::update::sha256_hex(n.as_bytes());
    }
}

/// What a pod has after a push: `setup.sh --summary`'s line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    /// Its profiles with a token.
    pub accounts: u32,
    /// The profile plain `claude` uses in new shells (`None`: the normal login).
    pub default: Option<u32>,
    /// Whether the push changed anything there.
    pub changed: bool,
}

/// The last `clods: accounts=N default=K|none changed=0|1` line of `stdout`. Pure.
pub fn parse_summary(stdout: &str) -> Option<Summary> {
    let line = stdout.lines().rev().find(|l| l.starts_with("clods: "))?;
    let field = |k: &str| line.split_whitespace().find_map(|w| w.strip_prefix(k)?.strip_prefix('='));
    let default = match field("default")? {
        "none" => None,
        n => Some(n.parse().ok()?),
    };
    Some(Summary { accounts: field("accounts")?.parse().ok()?, default, changed: field("changed")? != "0" })
}

/// Push `installer` to one pod: on its stdin, to [`COMMAND`]. What it says, or why not — the
/// pod's last line on stderr (setup.sh's own `setup.sh: …`), any token masked.
pub async fn push(remote: &dyn Remote, t: &SshTarget, installer: &str, timeout: Duration) -> Result<Summary, String> {
    let mask = |s: &str| crate::claude::mask_tokens(s, None);
    match remote.exec_input(t, COMMAND, installer.as_bytes(), Some(timeout)).await {
        Ok(o) if o.success => parse_summary(&o.stdout).ok_or_else(|| {
            format!("its setup.sh said nothing arena can read ({})", mask(last_line(&o.stdout).unwrap_or("no output")))
        }),
        Ok(o) => Err(match last_line(&o.stderr) {
            Some(why) => format!("{}: {}", o.exit_label(), mask(why)),
            None => o.exit_label(),
        }),
        Err(e) => Err(mask(&describe_error(&e))),
    }
}

/// The last line with something on it, clipped to 200 characters.
fn last_line(s: &str) -> Option<&str> {
    let l = s.lines().rev().map(str::trim).find(|l| !l.is_empty())?;
    Some(l.char_indices().nth(200).map_or(l, |(i, _)| &l[..i]))
}

// ---------------------------------------------------------------------------------------
// The spread's counter
// ---------------------------------------------------------------------------------------

/// How long [`take_turns`] waits for the state dir's lock.
const TURN_LOCK_WAIT: Duration = Duration::from_secs(10);

/// Take `n` turns of the spread for this fleet: the first one's number. The counter
/// (`<state dir>/clods-spread`) is read and moved on under the state dir's lock, so runs at once
/// never take the same turn. A counter that isn't a number starts again at 0. `Err`: the state dir
/// can't be used (the caller then starts at 0: the spread is a nicety, never a reason not to push).
pub fn take_turns(cfg: &Config, n: u64) -> Result<u64, String> {
    let path = crate::snapshot::state_file_path_for(cfg, SPREAD_FILE)?;
    take_turns_at(&path, n)
}

/// [`take_turns`] with the counter at `path`.
pub fn take_turns_at(path: &Path, n: u64) -> Result<u64, String> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    crate::snapshot::create_private_dir(dir).map_err(|e| format!("can't make {}: {e}", dir.display()))?;
    let _lock = crate::snapshot::lock_dir_within(dir, TURN_LOCK_WAIT).map_err(|e| e.to_string())?;
    let first = std::fs::read_to_string(path).ok().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0);
    let next = first.saturating_add(n);
    crate::snapshot::write_atomic(path, format!("{next}\n").as_bytes(), 0o600).map_err(|e| format!("can't write {}: {e}", path.display()))?;
    Ok(first)
}

// ---------------------------------------------------------------------------------------
// The stage setup, `up` and replace run
// ---------------------------------------------------------------------------------------

/// One pod's push.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub name: String,
    pub result: Result<Summary, String>,
}

/// The command that pushes to `name` again.
pub fn retry(name: &str) -> String {
    format!("arena pods setup --clods-only {name}")
}

impl Outcome {
    /// The default line: `[NAME] claude: clods, 3 accounts`, or the warning with its fix.
    pub fn brief(&self) -> String {
        let name = crate::out::bold(&self.name);
        match &self.result {
            Ok(s) => format!("[{name}] claude: clods, {}", accounts(s.accounts)),
            Err(e) => crate::out::warn(format!("[{name}] claude: clods not pushed — {e} (retry: {})", retry(&self.name))),
        }
    }

    /// `-v`'s line: the default too, and whether anything changed.
    pub fn detail(&self) -> String {
        match &self.result {
            Ok(s) => {
                let default = s.default.map_or("none (normal login)".to_string(), |n| format!("claude{n}"));
                let changed = if s.changed { "updated" } else { "unchanged" };
                format!("[{}] claude: clods, {}, default {default} ({changed})", crate::out::bold(&self.name), accounts(s.accounts))
            }
            Err(_) => self.brief(),
        }
    }
}

/// `1 account`, `3 accounts`.
pub fn accounts(n: u32) -> String {
    format!("{n} account{}", if n == 1 { "" } else { "s" })
}

/// Drive `futs` side by side on this task, their outputs in `futs`' order (as `arena claude`'s
/// push does: futures that borrow `&dyn Remote`, which a `JoinSet` can't spawn).
async fn join_all<F: Future>(futs: Vec<F>) -> Vec<F::Output> {
    let mut slots: Vec<Option<Pin<Box<F>>>> = futs.into_iter().map(|f| Some(Box::pin(f))).collect();
    let mut out: Vec<Option<F::Output>> = slots.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        for (i, slot) in slots.iter_mut().enumerate() {
            if let Some(f) = slot {
                if let Poll::Ready(v) = f.as_mut().poll(cx) {
                    out[i] = Some(v);
                    *slot = None;
                }
            }
        }
        if slots.iter().all(Option::is_none) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    out.into_iter().map(|o| o.expect("every future finished")).collect()
}

/// What a push to some pods came to.
#[derive(Debug)]
pub enum Staged {
    /// The tokens file can't be used: nothing was pushed (one warning for the run).
    Unusable(String),
    /// Each pod's push, in the order of `pods`.
    Pushed(Vec<Outcome>),
}

/// Push clods to each of `pods` (each with its SSH target), side by side, each within [`TIMEOUT`]:
/// every account, and a default by spread for a pod that has none — the turns taken in name order,
/// so a fleet set up at once is spread as `push.sh --spread` would. `None`: clods isn't configured
/// (or there are no pods): nothing to say.
pub async fn stage(remote: &dyn Remote, cfg: &Config, pods: &[(Pod, SshTarget)]) -> Option<Staged> {
    let clods = Clods::from_config(cfg)?;
    if pods.is_empty() {
        return None;
    }
    let tokens = match clods.read() {
        Ok(t) => t,
        Err(e) => return Some(Staged::Unusable(e)),
    };
    let first = take_turns(cfg, pods.len() as u64).unwrap_or(0);
    let mut order: Vec<usize> = (0..pods.len()).collect();
    order.sort_by(|&a, &b| pods[a].0.name.cmp(&pods[b].0.name));
    let mut turns = vec![0; pods.len()];
    for (k, &i) in order.iter().enumerate() {
        turns[i] = first + k as u64;
    }
    let pushes: Vec<_> = pods
        .iter()
        .zip(&turns)
        .map(|((_, t), &turn)| {
            let script = installer(tokens.text(), turn);
            async move { push(remote, t, &script, TIMEOUT).await }
        })
        .collect();
    let results = join_all(pushes).await;
    Some(Staged::Pushed(pods.iter().zip(results).map(|((p, _), result)| Outcome { name: p.name.clone(), result }).collect()))
}

// ---------------------------------------------------------------------------------------
// A local clods folder: `arena clods install`
// ---------------------------------------------------------------------------------------

/// What [`install`] did to one of [`FILES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wrote {
    New,
    Updated,
    Same,
}

/// Write [`FILES`] into the clods folder `dir` (made 0700 if it isn't there), each only when it
/// differs, beside itself and renamed over (the scripts 0755, the rest 0644). Nothing else in
/// `dir` is touched: not `tokens`, `fleet` or `profiles/`. With `dry_run`, only says what it would
/// do.
pub fn install(dir: &Path, dry_run: bool) -> std::io::Result<Vec<(&'static str, Wrote)>> {
    if !dry_run && !dir.exists() {
        crate::snapshot::create_private_dir(dir)?;
    }
    let mut out = Vec::new();
    for (name, text) in FILES {
        let path = dir.join(name);
        let mode = if name.ends_with(".sh") { 0o755 } else { 0o644 };
        let wrote = match std::fs::read(&path) {
            Ok(b) if b == text.as_bytes() && runnable(&path, mode) => Wrote::Same,
            Ok(_) => Wrote::Updated,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Wrote::New,
            Err(e) => return Err(e),
        };
        if wrote != Wrote::Same && !dry_run {
            crate::snapshot::write_atomic(&path, text.as_bytes(), mode)?;
        }
        out.push((name, wrote));
    }
    Ok(out)
}

/// Whether the file at `path` runs as it should: a script (`mode` 0755) that can't be run is
/// written again. Off unix, always.
fn runnable(path: &Path, mode: u32) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        mode & 0o111 == 0 || std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o100 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        true
    }
}

#[cfg(test)]
#[path = "clods_tests.rs"]
mod tests;
