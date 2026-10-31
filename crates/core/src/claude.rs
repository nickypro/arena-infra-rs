//! Claude Code accounts per machine (`arena claude`): a pool of accounts, each machine on one of
//! them, and each machine given its own account's token only.
//!
//! **Why.** `CLAUDE_CODE_OAUTH_TOKEN` used to be one token broadcast to every pod: the whole
//! cohort on one account's usage limits (one heavy user throttled everyone), a leaked token meant
//! rotating every machine, and nothing said which machine used what.
//!
//! **The pool** ([`load_pool`]): the tokens file `CLAUDE_TOKENS_FILE` names, in the format of the
//! human's `clods` tool — `[N] email -> sk-ant-oat01-…` per line, `#` comments ([`parse_tokens`]) —
//! so one file serves both. It must be owner-only: a file other users can read is refused. Without
//! one, config's single `CLAUDE_CODE_OAUTH_TOKEN` is a pool of one ([`CONFIG_ACCOUNT`]), so a config
//! that worked before works the same. An account is known by its email: clods numbers the lines in
//! order, so a number shifts when a line above it goes; an email doesn't.
//!
//! **Assignments** ([`State`]: `claude.json` in the fleet's state dir, 0600): one per machine
//! *instance* — provider + id, as spend's owners — so a name created again is a new machine and
//! inherits nothing. A machine is on an account by **spread** ([`assign`]: the account with the
//! fewest people, where a spend owner counts once however many machines they have and a machine
//! with no owner counts as one; a person's next machine joins the account their others are on), by
//! a **pin** (`claude assign <pods> <account>`), or by its **owner's** pin (`claude assign --owner`).
//! `none` (`claude off`) is a machine with no token. A machine gets one when it's set up (setup,
//! `up`, `copy-keys`); one whose account has left the pool gets a new one by spread at its next push.
//!
//! **The push** ([`push_command`], [`push`]): the machine's own token only, as
//! `export CLAUDE_CODE_OAUTH_TOKEN="…"` in `~/.bashrc` and `~/.zshrc` — the channel copy-keys
//! always used — with every earlier such line taken out, so a moved machine has the new token and
//! nothing else; `none` takes the lines out. The token travels on the ssh command's stdin
//! ([`Remote::exec_input`]): never in a command line (so never in `ps` here or on the pod, or in a
//! log or a dry run), and never in a file, here or there, that a dropped connection could leave
//! behind. The rc files are rewritten beside themselves and renamed over, so a failure leaves the
//! old file whole. Pushes to one machine take its lock and read what it should get afresh, so two
//! that meet run one after the other and the record says what the last one put there. What went is
//! recorded ([`Pushed`]: the account and a fingerprint of its token), so a moved machine or a
//! rotated token shows as out of date until it's pushed again ([`out_of_date`]). Machines a
//! complete listing no longer shows, or that arena terminated, are forgotten
//! ([`record_listing`], [`record_terminated`]), so they stop counting as load.
//!
//! Usage per account isn't shown: a `claude setup-token` token is inference-only, and Anthropic
//! publishes no usage API for subscription tokens (checked 2026-10-31).
//!
//! Layout as spend's: the parser, the planners and the views are pure and table-tested; the state
//! file's I/O ([`record`]) and the push ([`push`], [`stage`]) come last.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::pod::Pod;
use crate::proxy::{Listing, ListingStatus};
use crate::remote::{describe_error, Remote};
use crate::ssh::SshTarget;

// ---------------------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------------------

/// The state file's name in the fleet's state directory.
pub const STATE_FILE: &str = "claude.json";

/// The state file's format.
pub const STATE_VERSION: u32 = 1;

/// The config key naming the tokens file.
pub const TOKENS_FILE_KEY: &str = "CLAUDE_TOKENS_FILE";

/// The config key of the single token (a pool of one), and the variable the pods get.
pub const TOKEN_KEY: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// What config's single token is called as an account.
pub const CONFIG_ACCOUNT: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// What a token given with `--cc-token` is recorded as, on a machine it was pushed to.
pub const OVERRIDE_ACCOUNT: &str = "--cc-token";

/// How long one machine's push may take (an scp and one command): seconds on a healthy pod.
pub const PUSH_TIMEOUT: Duration = Duration::from_secs(60);

/// How long [`record`] waits for the state directory's lock (as spend's).
pub const RECORD_LOCK_WAIT: Duration = Duration::from_secs(10);

/// Every token starts so (`claude setup-token`'s `sk-ant-oat01-…`).
const TOKEN_PREFIX: &str = "sk-ant-";

// ---------------------------------------------------------------------------------------
// The pool
// ---------------------------------------------------------------------------------------

/// One account of the pool. Its token is never printed: `Debug` leaves it out.
#[derive(Clone, PartialEq, Eq)]
pub struct Account {
    /// clods' profile number: the Nth token, or the number its line pins.
    pub num: u32,
    /// What its line calls it (an email, or any word): the account's identity here.
    pub email: String,
    token: String,
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account").field("num", &self.num).field("email", &self.email).field("token", &"<redacted>").finish()
    }
}

impl Account {
    pub fn new(num: u32, email: impl Into<String>, token: impl Into<String>) -> Self {
        Account { num, email: email.into(), token: token.into() }
    }

    /// The token itself — only ever sent on a push's stdin ([`push`]).
    pub fn token(&self) -> &str {
        &self.token
    }

    /// [`fingerprint`] of its token.
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.token)
    }

    /// `#2 bob@x`.
    pub fn label(&self) -> String {
        format!("#{} {}", self.num, self.email)
    }
}

/// The first 12 hex digits of a token's SHA-256: enough to tell a rotated token from the one
/// pushed before, and nothing a token can be got back from.
pub fn fingerprint(token: &str) -> String {
    crate::update::sha256_hex(token.as_bytes())[..12].to_string()
}

/// A token's characters: letters, digits, `-` and `_` (what `claude setup-token` makes) — so it
/// goes into `export NAME="…"` as it is.
fn token_chars_ok(t: &str) -> bool {
    !t.is_empty() && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// What a tokens file holds: its accounts in file order, and a line for each line that isn't one
/// (never with its token).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parsed {
    pub accounts: Vec<Account>,
    pub skipped: Vec<String>,
}

/// Parse a tokens file in clods' format: `email -> sk-ant-…` per line (`=` or plain spaces work
/// as well as `->`), optionally led by the account's number (`4 someone@x -> sk-ant-…`); a line
/// without one is the number after the line before. `#` lines and blank lines are skipped; a line
/// that can't be read is skipped and named ([`Parsed::skipped`], as clods warns). A number twice,
/// a number 0 or an email twice is an error: one of them would be the wrong account. Messages
/// never carry a token.
pub fn parse_tokens(text: &str) -> Result<Parsed, String> {
    let mut out = Parsed::default();
    let mut n: u32 = 0;
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let lineno = i + 1;
        let Some((num, email, token)) = parse_line(line) else {
            out.skipped.push(format!(
                "line {lineno} ({}) isn't `email -> token` or `N email -> token` — skipped",
                shown_start(line)
            ));
            continue;
        };
        n = match num {
            Some(p) => p,
            None => n.checked_add(1).ok_or_else(|| format!("line {lineno}: too many accounts"))?,
        };
        if n == 0 {
            return Err(format!("line {lineno}: account numbers start at 1 ({email})"));
        }
        if out.accounts.iter().any(|a| a.num == n) {
            return Err(format!("line {lineno}: account {n} twice ({email})"));
        }
        if out.accounts.iter().any(|a| a.email.eq_ignore_ascii_case(&email)) {
            return Err(format!("line {lineno}: {email} twice — one token per account"));
        }
        out.accounts.push(Account::new(n, email, token));
    }
    Ok(out)
}

/// One line's `(number, email, token)`, or `None` when it isn't one.
fn parse_line(line: &str) -> Option<(Option<u32>, String, String)> {
    let at = line.find(TOKEN_PREFIX)?;
    let token = line[at..].trim_end();
    if token.len() <= TOKEN_PREFIX.len() || !token_chars_ok(token) {
        return None;
    }
    let mut head = line[..at].trim_end();
    if let Some(h) = head.strip_suffix("->").or_else(|| head.strip_suffix('=')) {
        head = h.trim_end();
    }
    let words: Vec<&str> = head.split_whitespace().collect();
    let (num, email) = match words.as_slice() {
        [email] => (None, *email),
        [num, email] if num.chars().all(|c| c.is_ascii_digit()) => (Some(num.parse::<u32>().ok()?), *email),
        _ => return None,
    };
    if email.contains(['=', '>']) {
        return None;
    }
    Some((num, email.to_string(), token.to_string()))
}

/// How a line that couldn't be read is named: its first word, unless that could be (part of) a
/// token — then nothing of it.
fn shown_start(line: &str) -> String {
    let first = line.split_whitespace().next().unwrap_or("");
    if first.contains("sk-") || first.len() > 64 {
        "its start isn't shown".to_string()
    } else {
        format!("`{first} …`")
    }
}

/// Where the pool came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// `CLAUDE_TOKENS_FILE`.
    File(PathBuf),
    /// Config's single `CLAUDE_CODE_OAUTH_TOKEN`.
    Config,
    /// `--cc-token`.
    Flag,
    /// Neither is set.
    Unset,
}

/// The accounts machines can be put on.
#[derive(Debug, Clone)]
pub struct Pool {
    pub accounts: Vec<Account>,
    pub source: Source,
    /// Worth saying, never a refusal: lines skipped, a `CLAUDE_CODE_OAUTH_TOKEN` the file replaces.
    pub notes: Vec<String>,
}

impl Pool {
    /// No accounts.
    pub fn empty() -> Self {
        Pool { accounts: Vec::new(), source: Source::Unset, notes: Vec::new() }
    }

    /// These accounts, from a tokens file (tests, and a parsed file).
    pub fn of(accounts: Vec<Account>, source: Source) -> Self {
        Pool { accounts, source, notes: Vec::new() }
    }

    /// `--cc-token`'s: that token alone ([`OVERRIDE_ACCOUNT`]).
    pub fn flag(token: &str) -> Result<Self, String> {
        if !token_chars_ok(token) {
            return Err("--cc-token isn't a token (letters, digits, `-` and `_` only)".into());
        }
        Ok(Pool::of(vec![Account::new(1, OVERRIDE_ACCOUNT, token)], Source::Flag))
    }

    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    /// The account known as `email`.
    pub fn get(&self, email: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.email == email)
    }

    /// The account the operator means: its number (`2`, `#2`) or its email (any case).
    pub fn find(&self, which: &str) -> Option<&Account> {
        let w = which.trim();
        let n = w.strip_prefix('#').unwrap_or(w);
        if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
            return n.parse::<u32>().ok().and_then(|n| self.accounts.iter().find(|a| a.num == n));
        }
        self.accounts.iter().find(|a| a.email.eq_ignore_ascii_case(w))
    }

    /// `#1 alice@x, #2 bob@y` — for a refusal that names what there is.
    pub fn listed(&self) -> String {
        self.accounts.iter().map(Account::label).collect::<Vec<_>>().join(", ")
    }

    /// `3 accounts in /path/tokens`, `1 account: config's CLAUDE_CODE_OAUTH_TOKEN`, `none`.
    pub fn describe(&self) -> String {
        let n = self.accounts.len();
        let accounts = format!("{n} account{}", if n == 1 { "" } else { "s" });
        match &self.source {
            Source::File(p) => format!("{accounts} in {}", p.display()),
            Source::Config => format!("{accounts}: config's {TOKEN_KEY} (a pool of one; {TOKENS_FILE_KEY} names more)"),
            Source::Flag => "--cc-token's, for this run".to_string(),
            Source::Unset => format!("none ({TOKENS_FILE_KEY} and {TOKEN_KEY} are unset)"),
        }
    }
}

/// The pool config sets: the tokens file `CLAUDE_TOKENS_FILE` names, else `CLAUDE_CODE_OAUTH_TOKEN`
/// as a pool of one, else none. `Err`: a tokens file that's missing, readable by other users,
/// unreadable or holds no token, or a single token that isn't one — never with a token in it.
pub fn load_pool(cfg: &Config) -> Result<Pool, String> {
    let set = |k: &str| cfg.get(k).map(str::trim).filter(|v| !v.is_empty()).map(String::from);
    if let Some(p) = set(TOKENS_FILE_KEY) {
        // A relative path is beside the config, as the fleet's other files are (`keys/`); one
        // parsed from text (no file) keeps it as given.
        let path = match (expand_home(&p), cfg.source().and_then(Path::parent)) {
            (rel, Some(dir)) if rel.is_relative() && !dir.as_os_str().is_empty() => dir.join(rel),
            (path, _) => path,
        };
        let parsed = read_tokens_file(&path)?;
        let mut pool = Pool::of(parsed.accounts, Source::File(path));
        pool.notes = parsed.skipped;
        if set(TOKEN_KEY).is_some() {
            pool.notes.push(format!("{TOKEN_KEY} is set too, and unused: {TOKENS_FILE_KEY}'s accounts replace it"));
        }
        return Ok(pool);
    }
    match set(TOKEN_KEY) {
        Some(t) if !token_chars_ok(&t) => Err(format!("{TOKEN_KEY} isn't a token (letters, digits, `-` and `_` only)")),
        Some(t) => Ok(Pool::of(vec![Account::new(1, CONFIG_ACCOUNT, t)], Source::Config)),
        None => Ok(Pool::empty()),
    }
}

/// `~/x` as `$HOME/x`; anything else as it is.
fn expand_home(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(p),
    }
}

/// Read and parse the tokens file at `path`, refusing one other users of this box can read (as
/// ssh refuses such a key: the tokens are the accounts) and one with no token.
pub fn read_tokens_file(path: &Path) -> Result<Parsed, String> {
    let shown = path.display();
    let meta = std::fs::metadata(path).map_err(|e| format!("{TOKENS_FILE_KEY} {shown}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{TOKENS_FILE_KEY} {shown} isn't a file"));
    }
    if let Some(why) = mode_problem(&meta) {
        return Err(format!("{TOKENS_FILE_KEY} {shown} {why} — chmod 600 {shown}"));
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("{TOKENS_FILE_KEY} {shown}: {e}"))?;
    let parsed = parse_tokens(&text).map_err(|e| format!("{TOKENS_FILE_KEY} {shown}: {e}"))?;
    if parsed.accounts.is_empty() {
        return Err(format!(
            "{TOKENS_FILE_KEY} {shown} holds no token (one `email -> sk-ant-oat01-…` per line; `claude setup-token` makes one)"
        ));
    }
    Ok(parsed)
}

/// Why a tokens file's mode won't do, if it won't: anyone but its owner may read or write it.
#[cfg(unix)]
fn mode_problem(meta: &std::fs::Metadata) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = meta.permissions().mode() & 0o777;
    (mode & 0o077 != 0).then(|| format!("is mode {mode:04o}: other users of this box can get at its tokens"))
}

#[cfg(not(unix))]
fn mode_problem(_meta: &std::fs::Metadata) -> Option<String> {
    None
}

// ---------------------------------------------------------------------------------------
// The state
// ---------------------------------------------------------------------------------------

/// How a machine came to be on its account.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum How {
    /// The least-loaded account (or its owner's other machines').
    Spread,
    /// `claude assign <pods> <account|none>`, `claude off`.
    Pinned,
    /// Its owner's pin (`claude assign --owner NAME …`).
    Owner,
}

impl How {
    pub fn word(self) -> &'static str {
        match self {
            How::Spread => "spread",
            How::Pinned => "pinned",
            How::Owner => "owner's",
        }
    }
}

/// What arena last put on a machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pushed {
    /// The account whose token went (`None`: the token was taken off).
    pub account: Option<String>,
    /// [`fingerprint`] of the token that went.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fp: Option<String>,
    pub at: u64,
}

/// One machine instance's account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    pub provider: String,
    pub id: String,
    /// Its name as last seen (for whoever reads the file; it's found by provider + id).
    pub name: String,
    /// The account's email; `None`: no Claude Code token on it.
    pub account: Option<String>,
    pub how: How,
    /// When it was put on the account.
    pub at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushed: Option<Pushed>,
}

/// `claude.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    #[serde(default)]
    pub machines: Vec<Assignment>,
    /// Owner pins: a person's machines, now and later, go on this account (`None`: no token).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub owners: BTreeMap<String, Option<String>>,
}

impl Default for State {
    fn default() -> Self {
        State { version: STATE_VERSION, machines: Vec::new(), owners: BTreeMap::new() }
    }
}

impl State {
    /// The machine instance's assignment.
    pub fn get(&self, provider: &str, id: &str) -> Option<&Assignment> {
        self.machines.iter().find(|m| m.provider == provider && m.id == id)
    }

    /// A pod's assignment.
    pub fn of(&self, pod: &Pod) -> Option<&Assignment> {
        self.get(&pod.provider, &pod.id)
    }

    fn get_mut(&mut self, provider: &str, id: &str) -> Option<&mut Assignment> {
        self.machines.iter_mut().find(|m| m.provider == provider && m.id == id)
    }
}

/// A live machine as the planners see it: its instance, its name and its spend owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Machine {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub owner: Option<String>,
}

impl Machine {
    pub fn new(pod: &Pod, owners: &Owners) -> Self {
        Machine {
            provider: pod.provider.clone(),
            id: pod.id.clone(),
            name: pod.name.clone(),
            owner: owners.get(&(pod.provider.clone(), pod.id.clone())).cloned(),
        }
    }

    fn key(&self) -> (String, String) {
        (self.provider.clone(), self.id.clone())
    }
}

/// Machine instance (provider, id) → its spend owner.
pub type Owners = HashMap<(String, String), String>;

/// The owners spend knows (`pods up --owner`, `spend assign`).
pub fn owners_of(spend: &crate::spend::SpendState) -> Owners {
    spend.machines.iter().filter_map(|m| Some(((m.provider.clone(), m.id.clone()), m.owner.clone()?))).collect()
}

/// [`owners_of`] this fleet's spend ledger, as it stands (none without one).
pub fn owners_for(cfg: &Config) -> Owners {
    crate::snapshot::state_file_path_for(cfg, crate::spend::SPEND_FILE)
        .map(|p| owners_of(&crate::spend::load(&p).0))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------------------
// Assigning
// ---------------------------------------------------------------------------------------

/// What [`assign`] does to its targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Want {
    /// Keep what they have; one with none — or on an account no longer in the pool — gets one by
    /// spread (or its owner's pin). What setup, `up`, `copy-keys` and `claude push` do.
    Ensure,
    /// Spread them again (their owners' pins still apply): `claude assign --spread`.
    Spread,
    /// Pin them to this account (an email in the pool), or to none.
    Pin(Option<String>),
}

/// What a machine was on before a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Was {
    /// No assignment yet.
    Nothing,
    /// On no account (no token).
    Off,
    On(String),
}

/// Why a machine is on its new account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    /// It had none.
    New,
    /// Its account (this one) is no longer in the pool.
    Left(String),
    /// Spread again, as asked.
    Respread,
    /// Pinned, as asked.
    Pinned,
    /// Its owner (this one)'s pin.
    Owner(String),
    /// Beside its owner (this one)'s other machines.
    WithOwner(String),
}

/// One machine's change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub was: Was,
    /// The new account (`None`: no token).
    pub to: Option<String>,
    pub how: How,
    pub why: Why,
}

impl Change {
    /// `(new)`, `(spread)`, `(alice's pin)`, `(old@x left the pool)` — after the move.
    pub fn because(&self) -> String {
        match &self.why {
            Why::New => "new, by spread".into(),
            Why::Left(old) => format!("{old} is no longer in the pool"),
            Why::Respread => "spread".into(),
            Why::Pinned => "pinned".into(),
            Why::Owner(o) => format!("{o}'s pin"),
            Why::WithOwner(o) => format!("beside {o}'s other machines"),
        }
    }
}

/// Per account (email), the people on it: a spend owner, or a machine with none.
type Load = HashMap<String, BTreeSet<String>>;

/// Who a machine instance counts as for the load: its owner, else itself.
fn person(provider: &str, id: &str, owners: &Owners) -> String {
    match owners.get(&(provider.to_string(), id.to_string())) {
        Some(o) => format!("owner {o}"),
        None => format!("machine {provider}/{id}"),
    }
}

/// The load of every pool account from the assignments, leaving out `skip` (machines about to be
/// placed again).
fn load_of(state: &State, pool: &Pool, owners: &Owners, skip: &HashSet<(String, String)>) -> Load {
    let mut load: Load = pool.accounts.iter().map(|a| (a.email.clone(), BTreeSet::new())).collect();
    for m in &state.machines {
        if skip.contains(&(m.provider.clone(), m.id.clone())) {
            continue;
        }
        if let Some(set) = m.account.as_ref().and_then(|e| load.get_mut(e)) {
            set.insert(person(&m.provider, &m.id, owners));
        }
    }
    load
}

/// Where `t` goes by spread: its owner's pin (an account in the pool, or none); else the account
/// its owner's other machines are on (the lowest numbered, if several), so a person counts once;
/// else the account with the fewest people, the lowest number first. `None`: the pool is empty.
fn pick(state: &State, pool: &Pool, owners: &Owners, skip: &HashSet<(String, String)>, t: &Machine, load: &Load) -> Option<(Option<String>, How, Option<Why>)> {
    if let Some(o) = &t.owner {
        match state.owners.get(o) {
            Some(None) => return Some((None, How::Owner, Some(Why::Owner(o.clone())))),
            Some(Some(e)) if pool.get(e).is_some() => return Some((Some(e.clone()), How::Owner, Some(Why::Owner(o.clone())))),
            _ => {}
        }
        let beside = state
            .machines
            .iter()
            .filter(|m| {
                let key = (m.provider.clone(), m.id.clone());
                key != t.key() && !skip.contains(&key) && owners.get(&key) == Some(o)
            })
            .filter_map(|m| m.account.as_deref().and_then(|e| pool.get(e)))
            .min_by_key(|a| a.num);
        if let Some(a) = beside {
            return Some((Some(a.email.clone()), How::Spread, Some(Why::WithOwner(o.clone()))));
        }
    }
    let least = pool.accounts.iter().min_by_key(|a| (load.get(&a.email).map_or(0, BTreeSet::len), a.num))?;
    Some((Some(least.email.clone()), How::Spread, None))
}

/// Put `t` on `to` (by `how`) at `now`: `Some(was)` when that's a change, `None` when it was
/// already so (its name is brought up to date either way).
fn set(state: &mut State, t: &Machine, to: Option<String>, how: How, now: u64) -> Option<Was> {
    match state.get_mut(&t.provider, &t.id) {
        Some(a) => {
            a.name = t.name.clone();
            if a.account == to && a.how == how {
                return None;
            }
            let was = match a.account.take() {
                Some(e) => Was::On(e),
                None => Was::Off,
            };
            a.account = to;
            a.how = how;
            a.at = now;
            Some(was)
        }
        None => {
            state.machines.push(Assignment {
                provider: t.provider.clone(),
                id: t.id.clone(),
                name: t.name.clone(),
                account: to,
                how,
                at: now,
                pushed: None,
            });
            Some(Was::Nothing)
        }
    }
}

/// Assign `targets` as `want` says, against the pool and every machine's load: the new state and
/// what changed, in `targets`' order (a target already so isn't listed). Spread targets are placed
/// one by one in that order, each seeing the ones before it. With an empty pool nothing is spread
/// (a pin to none still is). Pure.
pub fn assign(mut state: State, pool: &Pool, owners: &Owners, targets: &[Machine], want: &Want, now: u64) -> (State, Vec<Change>) {
    state.version = STATE_VERSION;
    let mut changes = Vec::new();
    let mut place: Vec<(usize, Why)> = Vec::new();
    for (i, t) in targets.iter().enumerate() {
        match want {
            Want::Pin(to) => {
                if let Some(was) = set(&mut state, t, to.clone(), How::Pinned, now) {
                    changes.push((i, change(t, was, to.clone(), How::Pinned, Why::Pinned)));
                }
            }
            Want::Spread => place.push((i, Why::Respread)),
            Want::Ensure => match state.get(&t.provider, &t.id).map(|a| a.account.clone()) {
                None => place.push((i, Why::New)),
                Some(Some(e)) if pool.get(&e).is_none() => place.push((i, Why::Left(e))),
                Some(_) => {
                    if let Some(a) = state.get_mut(&t.provider, &t.id) {
                        a.name = t.name.clone();
                    }
                }
            },
        }
    }
    let mut skip: HashSet<(String, String)> = place.iter().map(|(i, _)| targets[*i].key()).collect();
    let mut load = load_of(&state, pool, owners, &skip);
    for (i, why) in place {
        let t = &targets[i];
        skip.remove(&t.key());
        let Some((to, how, picked_why)) = pick(&state, pool, owners, &skip, t, &load) else { continue };
        if let Some(e) = &to {
            load.entry(e.clone()).or_default().insert(person(&t.provider, &t.id, owners));
        }
        // An account that left the pool is why it moved; otherwise, why it went where it did.
        let why = match (why, picked_why) {
            (w @ Why::Left(_), _) => w,
            (_, Some(w)) => w,
            (w, None) => w,
        };
        if let Some(was) = set(&mut state, t, to.clone(), how, now) {
            changes.push((i, change(t, was, to, how, why)));
        }
    }
    changes.sort_by_key(|(i, _)| *i);
    (state, changes.into_iter().map(|(_, c)| c).collect())
}

fn change(t: &Machine, was: Was, to: Option<String>, how: How, why: Why) -> Change {
    Change { provider: t.provider.clone(), id: t.id.clone(), name: t.name.clone(), was, to, how, why }
}

/// Pin `owner`'s machines — now (`machines`: theirs that are live) and later — to an account
/// (`Some(Some(email))`), to none (`Some(None)`), or take the pin off (`None`: their machines are
/// spread again). Pure.
pub fn pin_owner(
    mut state: State,
    pool: &Pool,
    owners: &Owners,
    owner: &str,
    pin: Option<Option<String>>,
    machines: &[Machine],
    now: u64,
) -> (State, Vec<Change>) {
    match pin {
        Some(p) => state.owners.insert(owner.to_string(), p),
        None => state.owners.remove(owner),
    };
    assign(state, pool, owners, machines, &Want::Spread, now)
}

/// `owner`'s machines for an owner pin: the live ones (`live`, the listing's) and any other
/// assignment of theirs this state knows — one on a provider that didn't list this time keeps
/// no old account behind the pin (review r2); it's pushed when it's next listed. Pure.
pub fn owner_machines(state: &State, owners: &Owners, owner: &str, live: &[Machine]) -> Vec<Machine> {
    let mut out: Vec<Machine> = live.iter().filter(|m| m.owner.as_deref() == Some(owner)).cloned().collect();
    for a in &state.machines {
        let key = (a.provider.clone(), a.id.clone());
        if owners.get(&key).map(String::as_str) == Some(owner) && !out.iter().any(|m| m.key() == key) {
            out.push(Machine { provider: a.provider.clone(), id: a.id.clone(), name: a.name.clone(), owner: Some(owner.to_string()) });
        }
    }
    out
}

/// Forget the machines a listing begun at `listed_at` shows are gone: a provider that listed
/// (completely) without one had it terminated. A provider that failed, or wasn't asked, leaves its
/// machines as they were; so does any machine assigned or pushed since the listing began (it may
/// have been made after: review r2 — an old listing must never undo a newer `up`). Pure.
pub fn prune(mut state: State, listing: &Listing, listed_at: u64) -> State {
    let live: HashSet<(String, String)> = listing.pods().iter().map(|p| (p.provider.clone(), p.id.clone())).collect();
    state.machines.retain(|m| {
        let since = m.at >= listed_at || m.pushed.as_ref().is_some_and(|p| p.at >= listed_at);
        since || listing.status(&m.provider) != ListingStatus::Ok || live.contains(&(m.provider.clone(), m.id.clone()))
    });
    state
}

/// Record a push: the machine now has `account`'s token (`fp` its [`fingerprint`]), or none. A
/// machine with no assignment is left so (a `--cc-token` push to it): its next push spreads it.
/// Pure.
pub fn record_push(mut state: State, provider: &str, id: &str, account: Option<&str>, fp: Option<&str>, now: u64) -> State {
    if let Some(a) = state.get_mut(provider, id) {
        a.pushed = Some(Pushed { account: account.map(String::from), fp: fp.map(String::from), at: now });
    }
    state
}

/// Whether a machine's box may not have its account's current token: never pushed; pushed another
/// account (or a token, when it's off); or its account's token has changed since (rotated).
pub fn out_of_date(a: &Assignment, pool: &Pool) -> bool {
    match (&a.account, &a.pushed) {
        (_, None) => true,
        (None, Some(p)) => p.account.is_some(),
        (Some(e), Some(p)) => {
            p.account.as_ref() != Some(e) || pool.get(e).is_none_or(|acc| p.fp.as_deref() != Some(acc.fingerprint().as_str()))
        }
    }
}

// ---------------------------------------------------------------------------------------
// The accounts view
// ---------------------------------------------------------------------------------------

/// One account's line in `claude accounts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccountRow {
    /// Its number in the pool; `None`: an account no longer in the pool that machines are still on.
    pub num: Option<u32>,
    pub email: String,
    /// The live machines on it, by name.
    pub machines: Vec<String>,
    /// Their spend owners, each once.
    pub people: Vec<String>,
    /// Of `machines`, the ones whose box may not have this account's current token yet
    /// ([`out_of_date`]): `claude push` puts it on.
    pub pending: Vec<String>,
}

/// `claude accounts`: the pool, and where every live machine is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct View {
    /// Where the pool came from ([`Pool::describe`]).
    pub pool: String,
    pub accounts: Vec<AccountRow>,
    /// Live machines on no account (`claude off`), with whether their box may still have a token.
    pub off: Vec<String>,
    pub off_pending: Vec<String>,
    /// Live machines with no assignment yet: their next push (or setup) spreads them.
    pub unassigned: Vec<String>,
    /// Owner pins.
    pub owner_pins: BTreeMap<String, Option<String>>,
}

/// The view of `live` machines (the listing's) against the pool. Pure.
pub fn view(state: &State, pool: &Pool, live: &[Machine]) -> View {
    let mut rows: Vec<AccountRow> = pool
        .accounts
        .iter()
        .map(|a| AccountRow { num: Some(a.num), email: a.email.clone(), machines: vec![], people: vec![], pending: vec![] })
        .collect();
    let (mut off, mut off_pending, mut unassigned) = (Vec::new(), Vec::new(), Vec::new());
    let mut sorted: Vec<&Machine> = live.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    for m in sorted {
        let Some(a) = state.get(&m.provider, &m.id) else {
            unassigned.push(m.name.clone());
            continue;
        };
        let Some(email) = &a.account else {
            off.push(m.name.clone());
            if out_of_date(a, pool) {
                off_pending.push(m.name.clone());
            }
            continue;
        };
        let row = match rows.iter().position(|r| &r.email == email) {
            Some(i) => &mut rows[i],
            None => {
                rows.push(AccountRow { num: None, email: email.clone(), machines: vec![], people: vec![], pending: vec![] });
                rows.last_mut().expect("just pushed")
            }
        };
        row.machines.push(m.name.clone());
        if let Some(o) = &m.owner {
            if !row.people.contains(o) {
                row.people.push(o.clone());
            }
        }
        if out_of_date(a, pool) {
            row.pending.push(m.name.clone());
        }
    }
    View { pool: pool.describe(), accounts: rows, off, off_pending, unassigned, owner_pins: state.owners.clone() }
}

/// `claude accounts`' text. Pure.
pub fn render(v: &View) -> String {
    use crate::table::{self, Align};
    let mut out = format!("Claude Code accounts: {}\n\n", v.pool);
    if !v.accounts.is_empty() {
        let rows: Vec<Vec<String>> = v
            .accounts
            .iter()
            .map(|r| {
                let mut on: Vec<String> = r
                    .machines
                    .iter()
                    .map(|m| if r.pending.contains(m) { format!("{m}*") } else { m.clone() })
                    .collect();
                if r.num.is_none() {
                    on.push("← not in the pool: `arena claude push` moves them".into());
                }
                vec![
                    r.num.map_or("-".into(), |n| n.to_string()),
                    r.email.clone(),
                    r.machines.len().to_string(),
                    if r.people.is_empty() { "-".into() } else { r.people.len().to_string() },
                    if on.is_empty() { "-".into() } else { on.join(", ") },
                ]
            })
            .collect();
        out.push_str(&table::render(&["#", "ACCOUNT", "MACHINES", "PEOPLE", "ON IT"], &[Align::Right, Align::Left, Align::Right, Align::Right], &rows));
    }
    let people: Vec<String> = v
        .accounts
        .iter()
        .filter(|r| !r.people.is_empty())
        .map(|r| format!("{} {}", r.num.map_or("-".into(), |n| format!("#{n}")), r.people.join(", ")))
        .collect();
    if !people.is_empty() {
        out.push_str(&format!("\nPeople (spend owners): {}\n", people.join("; ")));
    }
    if !v.owner_pins.is_empty() {
        let pins: Vec<String> = v.owner_pins.iter().map(|(o, a)| format!("{o} → {}", a.as_deref().unwrap_or("none"))).collect();
        out.push_str(&format!("Owner pins: {}\n", pins.join(", ")));
    }
    if !v.off.is_empty() {
        let shown: Vec<String> = v.off.iter().map(|m| if v.off_pending.contains(m) { format!("{m}*") } else { m.clone() }).collect();
        out.push_str(&format!("No token (claude off): {}\n", shown.join(", ")));
    }
    if !v.unassigned.is_empty() {
        out.push_str(&format!("Not on an account yet: {} — `arena claude push` spreads them\n", v.unassigned.join(", ")));
    }
    if v.accounts.iter().any(|r| !r.pending.is_empty()) || !v.off_pending.is_empty() {
        out.push_str("* its box may not have this yet (moved, rotated, or never pushed): `arena claude push` puts it on\n");
    }
    out
}

// ---------------------------------------------------------------------------------------
// The state file
// ---------------------------------------------------------------------------------------

/// This fleet's state file (`<state dir>/<prefix>/claude.json`).
pub fn state_path(cfg: &Config) -> Result<PathBuf, String> {
    crate::snapshot::state_file_path_for(cfg, STATE_FILE)
}

/// The state at `path`: none yet → empty. One that can't be read or parsed is an error — its pins
/// are someone's decisions, never silently dropped.
pub fn load(path: &Path) -> Result<State, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(State::default()),
        Err(e) => return Err(format!("can't read {}: {e}", path.display())),
    };
    let s: State = serde_json::from_str(&text).map_err(|e| format!("{} isn't readable ({e}) — fix or move it aside", path.display()))?;
    if s.version != STATE_VERSION {
        return Err(format!("{} is version {} (this arena reads {STATE_VERSION})", path.display(), s.version));
    }
    Ok(s)
}

/// The file's text (pretty, one trailing newline).
pub fn to_json(state: &State) -> String {
    let mut s = serde_json::to_string_pretty(state).unwrap_or_default();
    s.push('\n');
    s
}

/// Change the state file at `path` with `f`: under the state directory's lock (the one spend,
/// balance and the health cache take — two runs never drop each other's change), loaded, changed,
/// and saved atomically and owner-only when it changed. `f` gives the new state and what to hand
/// back.
pub fn record<T>(path: &Path, f: impl FnOnce(State) -> (State, T)) -> Result<(State, T), String> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    crate::snapshot::create_private_dir(dir).map_err(|e| format!("can't make {}: {e}", dir.display()))?;
    let lock = crate::snapshot::lock_dir_within(dir, RECORD_LOCK_WAIT).map_err(|e| e.to_string())?;
    if lock.is_none() {
        return Err(format!("no lock on {} (it can't be opened, or its filesystem has no flock) — nothing recorded", dir.display()));
    }
    let before = load(path)?;
    let (next, out) = f(before.clone());
    if next != before {
        crate::snapshot::write_atomic(path, to_json(&next).as_bytes(), 0o600).map_err(|e| format!("can't write {}: {e}", path.display()))?;
    }
    drop(lock);
    Ok((next, out))
}

/// Record a complete listing begun at `listed_at`, quietly: the machines it shows are gone are
/// forgotten ([`prune`]) — what `pods list`, the dashboard, teardown, `arena balance` and the
/// snapshot cron line do after they listed, so a terminated machine stops counting as load within
/// minutes, whoever terminated it. Nothing is written when nothing changed; a failure changes
/// nothing else (spend's listings record the same way).
pub fn record_listing(path: Option<&Path>, listing: &Listing, listed_at: u64) {
    let Some(path) = path else { return };
    // Only a fleet that has assignments: no state dir made, no lock taken, for one that doesn't.
    if !path.exists() {
        return;
    }
    let _ = record(path, |s| (prune(s, listing, listed_at), ()));
}

/// Forget a machine arena just terminated (`provider`: when known; else the one record with that
/// id) — quietly, as spend records the terminate.
pub fn record_terminated(path: Option<&Path>, provider: Option<&str>, id: &str) {
    let Some(path) = path.filter(|p| p.exists()) else { return };
    let _ = record(path, |mut s| {
        let hits: Vec<usize> =
            s.machines.iter().enumerate().filter(|(_, m)| m.id == id && provider.is_none_or(|p| m.provider == p)).map(|(i, _)| i).collect();
        if let [i] = hits.as_slice() {
            s.machines.remove(*i);
        }
        (s, ())
    });
}

// ---------------------------------------------------------------------------------------
// The push
// ---------------------------------------------------------------------------------------

/// How long a push waits for another push to the same machine to end (one takes at most
/// [`PUSH_TIMEOUT`], and its record a moment).
pub const MACHINE_LOCK_WAIT: Duration = Duration::from_secs(90);

/// The command a machine runs to have exactly the token on its stdin in its shells (`token`), or
/// none. In `~/.bashrc` and `~/.zshrc` — through a symlink to the file it points at — every
/// `export CLAUDE_CODE_OAUTH_TOKEN=…` line is taken out and, with a token, one put at the end.
/// Each file is rewritten whole beside itself (a copy keeping its mode) and renamed over it, so a
/// full disk or a dropped connection leaves the old file, never a cut one; a file already so isn't
/// touched (a re-push changes nothing), and one that doesn't exist isn't made just to say "none".
/// The token is read with `read` (a builtin) and reaches `awk` through its environment: never an
/// argument, so never in `ps` on the pod; and tracing is off first (`set +x`: a startup file that
/// turns it on would print it — review r2). The rewrite holds a lock on the pod too
/// ([`POD_LOCK`]), so a push whose connection timed out while it ran there can't land after the
/// next one. With a token, Claude Code's onboarding is marked done in `~/.claude.json` (it ignores
/// the token until it is), as copy-keys did; a push that can't do that fails. Pure.
pub fn push_command(token: bool, generation: u64) -> String {
    let take = if token {
        "IFS= read -r ARENA_CC || true; [ -n \"$ARENA_CC\" ] || { echo 'no token arrived on stdin' >&2; exit 3; }"
    } else {
        "ARENA_CC="
    };
    format!(
        "{{ set +x; }} 2>/dev/null; set -e; umask 077; tmp=; trap 'rm -f -- \"$tmp\"' EXIT; {take}; export ARENA_CC; gen={generation}; {POD_LOCK} \
         for rc in \"$HOME/.bashrc\" \"$HOME/.zshrc\"; do \
         if [ ! -e \"$rc\" ]; then [ -n \"$ARENA_CC\" ] || continue; : > \"$rc\"; fi; \
         f=$(readlink -f -- \"$rc\" 2>/dev/null || printf '%s' \"$rc\"); \
         tmp=$(mktemp \"$f.arena-XXXXXX\"); cp -p -- \"$f\" \"$tmp\"; \
         awk 'BEGIN {{ t = ENVIRON[\"ARENA_CC\"] }} /^[[:space:]]*export[[:space:]]+{TOKEN_KEY}=/ {{ next }} {{ print }} \
         END {{ if (t != \"\") print \"export {TOKEN_KEY}=\\\"\" t \"\\\"\" }}' \"$f\" > \"$tmp\"; \
         if cmp -s \"$tmp\" \"$f\"; then rm -f -- \"$tmp\"; else mv -f -- \"$tmp\" \"$f\"; fi; tmp=; done; \
         if [ -n \"$ARENA_CC\" ]; then {ONBOARDED}; fi; \
         if [ -n \"$ARENA_CC\" ]; then echo 'claude: token set'; else echo 'claude: token removed'; fi"
    )
}

/// The pod's side of a push's lock (`~/.arena-claude.lock`, `flock` from util-linux), held until
/// the command ends: a push that timed out here — its ssh killed, its command still running on the
/// pod — finishes before the next one starts, never after it. Without `flock`, no lock. Under it,
/// the push's generation ([`next_generation`]) is checked against the last one that ran here (the
/// file holds that number and nothing else): an older push that only reaches this point after a
/// newer one ran — stuck in a startup file past its timeout — changes nothing (review r3). A lock
/// path that's a symlink or not a plain file is refused before it's opened and again before it's
/// written: `>` would truncate whatever a link there points at (review r4).
const POD_LOCK: &str = "lk=\"$HOME/.arena-claude.lock\"; \
     if [ -L \"$lk\" ] || { [ -e \"$lk\" ] && [ ! -f \"$lk\" ]; }; then echo '~/.arena-claude.lock is not a plain file: not writing through it' >&2; exit 7; fi; \
     if command -v flock >/dev/null 2>&1; then exec 9>>\"$lk\"; \
     flock -w 45 9 || { echo 'another push is still running on this machine' >&2; exit 4; }; fi; \
     last=$(cat \"$lk\" 2>/dev/null || true); case \"$last\" in ''|*[!0-9]*) last=0;; esac; \
     if [ \"$gen\" -le \"$last\" ]; then echo 'a newer push has already run on this machine' >&2; exit 6; fi; \
     if [ -L \"$lk\" ]; then echo '~/.arena-claude.lock is not a plain file: not writing through it' >&2; exit 7; fi; \
     printf '%s\\n' \"$gen\" > \"$lk\";";

/// The clock's part of a push's generation: microseconds since the epoch, and never less than one
/// more than the last this process gave out. [`generation_for`] adds what survives the process.
fn next_generation() -> u64 {
    static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_micros() as u64);
    let mut prev = LAST.load(std::sync::atomic::Ordering::SeqCst);
    loop {
        let next = now.max(prev + 1);
        match LAST.compare_exchange(prev, next, std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst) {
            Ok(_) => return next,
            Err(p) => prev = p,
        }
    }
}

/// A push's generation for one machine ([`POD_LOCK`]), taken under its lock: one more than the
/// last given out for it — kept beside the lock (`claude-locks/<provider>-<id>.gen`), so it
/// outlives this process — and never less than [`next_generation`]. Each push is newer than every
/// one before it, from any command, even after the control box's clock went back: a clock alone
/// would make every push for as long as it had been ahead "older" than the last, and the pod
/// would refuse them (review r4). A counter that can't be read or kept fails the push.
fn generation_for(path: &Path, provider: &str, id: &str) -> Result<u64, String> {
    let file = lock_path(path, provider, id).with_extension("gen");
    let last = match std::fs::read_to_string(&file) {
        Ok(s) => s.trim().parse::<u64>().unwrap_or(0),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(format!("can't read {}: {e}", file.display())),
    };
    let next = next_generation().max(last.saturating_add(1));
    crate::snapshot::write_atomic(&file, format!("{next}\n").as_bytes(), 0o600).map_err(|e| format!("can't write {}: {e}", file.display()))?;
    Ok(next)
}

/// Mark Claude Code's onboarding done in `~/.claude.json` (it ignores `CLAUDE_CODE_OAUTH_TOKEN`
/// until it is): with python3, merged into the file and only when it isn't so already, written
/// beside it and renamed over it (its mode kept); with no python3, the file written (beside, then
/// renamed) when there's none, and a failure when there's one it can't merge into.
const ONBOARDED: &str = "if command -v python3 >/dev/null 2>&1; then python3 -c '
import json, os
p = os.path.realpath(os.path.expanduser(\"~/.claude.json\"))
d = json.load(open(p)) if os.path.isfile(p) and os.path.getsize(p) else {}
if d.get(\"hasCompletedOnboarding\") is not True:
    d[\"hasCompletedOnboarding\"] = True
    t = p + \".arena-tmp\"
    with open(t, \"w\") as f:
        json.dump(d, f)
    if os.path.exists(p):
        os.chmod(t, os.stat(p).st_mode & 0o7777)
    os.replace(t, p)
'; elif [ ! -s \"$HOME/.claude.json\" ]; then tmp=$(mktemp \"$HOME/.claude.json.arena-XXXXXX\"); \
     printf '{\"hasCompletedOnboarding\": true}\\n' > \"$tmp\"; mv -f -- \"$tmp\" \"$HOME/.claude.json\"; tmp=; \
     elif ! grep -q '\"hasCompletedOnboarding\": *true' \"$HOME/.claude.json\"; then \
     echo 'no python3 to mark the onboarding done in ~/.claude.json: Claude Code ignores the token until it is' >&2; exit 5; fi";

/// Push one machine: [`push_command`] with `token` on its stdin — or, with `None`, the command
/// alone, which takes the token off. Nothing with a token is written anywhere on the way: not a
/// file here, not one on the pod, not an argument.
pub async fn push(remote: &dyn Remote, t: &SshTarget, token: Option<&str>, generation: u64, timeout: Duration) -> Result<(), String> {
    let cmd = push_command(token.is_some(), generation);
    let done = match token {
        Some(tok) => remote.exec_input(t, &cmd, format!("{tok}\n").as_bytes(), Some(timeout)).await,
        None => remote.exec(t, &cmd, Some(timeout)).await,
    };
    match done {
        Ok(o) if o.success => Ok(()),
        // What the pod said, any token in it masked (a startup file's trace, a tool echoing it).
        Ok(o) => Err(mask_tokens(&format!("{}: {}", o.exit_label(), o.stderr.trim()), token)),
        Err(e) => Err(mask_tokens(&describe_error(&e), token)),
    }
}

/// `text` with `token` and anything that looks like a Claude token (`sk-ant-` and the token
/// characters after it) masked. Pure.
pub fn mask_tokens(text: &str, token: Option<&str>) -> String {
    let mut s = match token.filter(|t| !t.is_empty()) {
        Some(t) => text.replace(t, "<token>"),
        None => text.to_string(),
    };
    while let Some(at) = s.find(TOKEN_PREFIX) {
        let end = s[at..].find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_')).map_or(s.len(), |n| at + n);
        s.replace_range(at..end, "<token>");
    }
    s
}

/// A file name part from a provider name or an id: what isn't a letter, a digit, `-`, `_` or `.`
/// becomes `_`.
fn file_part(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect()
}

/// A machine's push lock (`claude-locks/<provider>-<id>.lock` beside the state file): an empty
/// file, kept — removing one a push may hold or wait on would let two pushes run at once
/// (review r2).
fn lock_path(path: &Path, provider: &str, id: &str) -> PathBuf {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    dir.join("claude-locks").join(format!("{}-{}.lock", file_part(provider), file_part(id)))
}

/// Take a machine's push lock, waiting up to `wait` for another push to it to end: held across
/// one push — what it should get read afresh, the push, its record — so two pushes to one machine
/// (a `claude off` meeting a setup's push) happen one after the other, and the record says what
/// the last of them put there. `Ok(None)`: no lock to be had here (no flock): the push goes on.
async fn lock_machine(path: &Path, provider: &str, id: &str, wait: Duration) -> Result<Option<std::fs::File>, String> {
    let lock = lock_path(path, provider, id);
    if let Some(dir) = lock.parent() {
        crate::snapshot::create_private_dir(dir).map_err(|e| format!("can't make {}: {e}", dir.display()))?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let f = opts.open(&lock).map_err(|e| format!("can't open {}: {e}", lock.display()))?;
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(Some(f)),
            Err(std::fs::TryLockError::WouldBlock) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(format!("another push to it was still going after {}s", wait.as_secs()));
            }
            Err(std::fs::TryLockError::Error(_)) => return Ok(None),
        }
    }
}

/// One machine's outcome in [`stage`] and the commands' pushes.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub name: String,
    pub provider: String,
    pub id: String,
    /// What it got: an account's label (`#2 bob@x`), `token taken off`, or — for a push that
    /// didn't get as far as knowing — `not pushed`.
    pub label: String,
    /// Its change of account, if this run made one.
    pub change: Option<Change>,
    pub result: Result<(), String>,
}

impl Outcome {
    /// `#2 bob@x (new, by spread)`, `token taken off (pinned)`.
    pub fn what(&self) -> String {
        let why = self.change.as_ref().map(|c| format!(" ({})", c.because())).unwrap_or_default();
        format!("{}{why}", self.label)
    }

    /// `✓ apple: #2 bob@x (new, by spread)` / `✗ apple: #2 bob@x — timed out after 60s`.
    pub fn line(&self) -> String {
        match &self.result {
            Ok(()) => format!("✓ {}: {}", self.name, self.what()),
            Err(e) => format!("✗ {}: {} — {e}", self.name, self.what()),
        }
    }
}

/// Drive `futs` side by side on this task, their outputs in `futs`' order — for futures that borrow
/// (`&dyn Remote`), which a `JoinSet` can't spawn.
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

/// One machine's push under its lock ([`lock_machine`]): what it should have read afresh — its
/// assignment from the state, then its account's token from the tokens file (a pool from config
/// as it is); none when it's off, whatever the pool; with a [`Source::Flag`] pool, that one token —
/// pushed, and recorded ([`finish`]). `None`: nothing to push (no assignment); an account no longer
/// in the pool is a failed outcome that says so.
async fn push_one(remote: &dyn Remote, path: &Path, pool: &Pool, pod: &Pod, t: &SshTarget, now: u64) -> Option<(String, Result<(), String>)> {
    let failed = |e: String| Some(("not pushed".to_string(), Err(e)));
    let _lock = match lock_machine(path, &pod.provider, &pod.id, MACHINE_LOCK_WAIT).await {
        Ok(l) => l,
        Err(e) => return failed(e),
    };
    // What it should have now: its assignment first — a machine that's off needs no token, so its
    // removal never waits on the tokens file (review r3) — then that account's token as the file has
    // it now: a file rotated since this run read it (a command that waited at its confirm) must
    // never put the old token back (review r2).
    let wanted: Option<String> = if pool.source == Source::Flag {
        None
    } else {
        let state = match load(path) {
            Ok(s) => s,
            Err(e) => return failed(e),
        };
        match &state.of(pod)?.account {
            None => {
                let label = "token taken off".to_string();
                return Some((label, finish(remote, path, pod, t, None, now).await));
            }
            Some(e) => Some(e.clone()),
        }
    };
    let fresh = match &pool.source {
        Source::File(f) => match read_tokens_file(f) {
            Ok(p) => Pool::of(p.accounts, pool.source.clone()),
            Err(e) => return failed(e),
        },
        _ => pool.clone(),
    };
    let account = match &wanted {
        None => fresh.accounts.first()?,
        Some(e) => match fresh.get(e) {
            Some(a) => a,
            None => return Some((e.clone(), Err("its account is no longer in the pool: `arena claude push` spreads it".into()))),
        },
    };
    Some((account.label(), finish(remote, path, pod, t, Some(account), now).await))
}

/// The push itself and its record, under the machine's lock: the machine marked as not known to
/// have what it had (`pushed` cleared) before the push — its box may be anything after a failure,
/// so the next default `claude push` picks it (review r3, r4) — and what went recorded on success.
async fn finish(remote: &dyn Remote, path: &Path, pod: &Pod, t: &SshTarget, account: Option<&Account>, now: u64) -> Result<(), String> {
    // First, before anything changes on the pod: the machine marked as not known to have what it
    // had. A push that then fails half-way, with the state file unwritable by then too, can't
    // leave the old record saying it's current — the next default push picks it (review r4).
    record(path, |mut s| {
        if let Some(a) = s.get_mut(&pod.provider, &pod.id) {
            a.pushed = None;
        }
        (s, ())
    })
    .map_err(|e| format!("not pushed: can't mark it as changing first: {e}"))?;
    let generation = generation_for(path, &pod.provider, &pod.id).map_err(|e| format!("not pushed: {e}"))?;
    push(remote, t, account.map(Account::token), generation, PUSH_TIMEOUT).await?;
    let (email, fp) = (account.map(|a| a.email.clone()), account.map(Account::fingerprint));
    record(path, |s| (record_push(s, &pod.provider, &pod.id, email.as_deref(), fp.as_deref(), now), ()))
        .map(|_| ())
        .map_err(|e| format!("pushed, but not recorded: {e}"))
}

/// Push each of `pods` what the state says it should have when its turn comes ([`push_one`]), side
/// by side, each within [`PUSH_TIMEOUT`]. `changes` are the assignments this run made, said beside
/// each. A pod with nothing to push isn't in the outcomes.
pub async fn push_all(remote: &dyn Remote, path: &Path, pool: &Pool, pods: &[(Pod, SshTarget)], changes: &[Change], now: u64) -> Vec<Outcome> {
    let pushes: Vec<_> = pods.iter().map(|(pod, t)| push_one(remote, path, pool, pod, t, now)).collect();
    let results = join_all(pushes).await;
    pods.iter()
        .zip(results)
        .filter_map(|((pod, _), r)| {
            let (label, result) = r?;
            Some(Outcome {
                name: pod.name.clone(),
                provider: pod.provider.clone(),
                id: pod.id.clone(),
                label,
                change: changes.iter().find(|c| c.provider == pod.provider && c.id == pod.id).cloned(),
                result,
            })
        })
        .collect()
}

/// The Claude Code stage of setup, `up` and `copy-keys`, for `pods` (each with its SSH target): the
/// ones with no account (or one no longer in the pool) get one by spread, then every pod gets its
/// account's token (or none, if it's off) and what went is recorded. With `token` (`--cc-token`)
/// that one token goes to every pod instead, and no assignment changes. With no pool — unset, or
/// one that can't be read (then still the `Err`) — only the pods that are off and may still have a
/// token get it taken off. `Ok(None)`: nothing to do. `Err`: the pool or the state can't be read.
pub async fn stage(remote: &dyn Remote, cfg: &Config, pods: &[(Pod, SshTarget)], token: Option<&str>, now: u64) -> Result<Option<Vec<Outcome>>, String> {
    if pods.is_empty() {
        return Ok(None);
    }
    let pool = match token {
        Some(t) => Pool::flag(t),
        None => load_pool(cfg),
    };
    let pool = match pool {
        Ok(p) if !p.is_empty() => p,
        // No pool (unset, or one that can't be read): the machines that are off still get their
        // token taken off when it's pending (review r3) — that needs no pool; nothing else is
        // pushed, and a pool that can't be read is still the error.
        other => {
            let removals = pending_removals(remote, cfg, pods, now).await?;
            return match other {
                Err(e) => Err(e),
                Ok(_) => Ok(removals),
            };
        }
    };
    let path = state_path(cfg)?;
    let changes = if pool.source == Source::Flag {
        Vec::new()
    } else {
        let owners = owners_for(cfg);
        let machines: Vec<Machine> = pods.iter().map(|(p, _)| Machine::new(p, &owners)).collect();
        record(&path, |s| assign(s, &pool, &owners, &machines, &Want::Ensure, now))?.1
    };
    Ok(Some(push_all(remote, &path, &pool, pods, &changes, now).await))
}

/// [`stage`] with no pool: of `pods`, the ones that are off with a token perhaps still on them get
/// it taken off. `Ok(None)`: none is (no state file is made for a fleet that has none).
async fn pending_removals(remote: &dyn Remote, cfg: &Config, pods: &[(Pod, SshTarget)], now: u64) -> Result<Option<Vec<Outcome>>, String> {
    let path = state_path(cfg)?;
    if !path.exists() {
        return Ok(None);
    }
    let state = load(&path)?;
    let off: Vec<(Pod, SshTarget)> = pods
        .iter()
        .filter(|(p, _)| state.of(p).is_some_and(|a| a.account.is_none() && out_of_date(a, &Pool::empty())))
        .cloned()
        .collect();
    if off.is_empty() {
        return Ok(None);
    }
    Ok(Some(push_all(remote, &path, &Pool::empty(), &off, &[], now).await))
}

#[cfg(test)]
#[path = "claude_tests.rs"]
mod tests;
