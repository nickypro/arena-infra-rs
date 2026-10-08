//! Opt-in **live** smoke test (PLAN "Testing strategy" 5): the *built* `arena` binary against a
//! sandbox account, end to end — `pods up --check` one cheap community pod, `pods test --deep`
//! it, publish `snapshot --public`, `pods rename` it there and back, terminate it, and confirm
//! with `teardown --check` that nothing it made is left. Unit tests, fakes and fixtures can't
//! catch an API that changed under us; this does, in a few minutes for a few cents. Run it
//! before each cohort — how: README "Testing".
//!
//! It spends real money, so it is `#[ignore]`d and **refuses to start** unless every guard
//! holds. Each guard is a pure function with an ordinary (offline, always-run) test below:
//!
//! - `ARENA_LIVE_SMOKE=1` and `ARENA_LIVE_CONFIG=<path>` are set, and that path — symlinks
//!   resolved — is not the production copy (`/home/dev/prod-ro/…`) or anything under `/root`;
//! - the config's `MACHINE_NAME_PREFIX` starts with `devtest` (or is named in
//!   `ARENA_LIVE_PREFIX_ALLOW`, comma-separated) and is never an `arenaN` cohort prefix,
//!   whatever the allow list says;
//! - a configured proxy is a *local* file outside `/etc`, and is only ever written — the
//!   binary runs with `SSH_PROXY_RELOAD_CMD=` (empty = write-only), so nginx is never touched;
//! - every configured provider lists (one that can't is not "empty"), and lists **only**
//!   `{prefix}-…` pods: anything else means these keys reach another account (production's
//!   staff boxes, a cohort) — stop before creating anything there.
//!
//! The binary runs with a **cleared environment** (a short allowlist, see [`child_env`]):
//! `config.env` values can be overridden from the environment, so a `RUNPOD_API_KEY` or
//! `MACHINE_NAME_PREFIX` exported in the operator's shell would otherwise silently replace
//! what the guards just checked. Its working directory is the config's directory, as with
//! the sandbox's `bin/arena-dev` (relative paths like `keys/` resolve there), and its health
//! cache (`ARENA_STATE_DIR`) is a fresh directory under the cargo target dir.
//!
//! Whatever happens once the create starts — a failed assertion, a panic, a hung command
//! killed at its deadline — the [`Cleanup`] guard terminates every pod holding a name this
//! run used (both were free when it started) and re-lists until they are gone. Only Ctrl+C
//! (or a SIGKILL) gets past it: after an interrupted run, run `arena teardown --check`.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use arena_core::naming::{is_absolute, qualify};
use arena_core::proxy::ProxyConfig;
use arena_core::snapshot::{PublicHealthStatus, PublicMachine, PublicSnapshot, PublicStatus};
use arena_core::{Config, Pod};
use serde_json::Value;

/// The production config copy: never a smoke target, however it is reached.
const PROD_CONFIG: &str = "/home/dev/prod-ro/config.env";
/// Directories no smoke config may live in (production's config and keys).
const FORBIDDEN_DIRS: [&str; 2] = ["/home/dev/prod-ro", "/root"];
/// The prefix a sandbox fleet carries (`devtest-alpha`, …).
const SANDBOX_PREFIX: &str = "devtest";
/// GPUs to try, cheapest first (`--order cheapest`); `ARENA_LIVE_GPU` overrides.
const DEFAULT_GPU_LIST: &str = "A4000,3070";
/// The per-pod $/h cap. With `--gpus 1` and a single pod, a run that goes the whole way to
/// [`UP_LIMIT`] still costs well under the sandbox's "ask first above ~$2" line.
const MAX_PRICE: &str = "0.30";
/// The only environment the binary inherits (see [`child_env`]). No provider keys, no
/// config overrides, no `SSH_AUTH_SOCK` (ssh uses the configured key file, never whatever
/// the operator's agent holds — production keys included). `RUNPOD_API` (`v1`/`v2`, not a
/// secret) passes so the v2 backend can be smoked: `RUNPOD_API=v2 cargo test …`.
const PASSED_ENV: [&str; 8] = ["PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "TZ", "RUNPOD_API"];

/// Deadlines per command. Generous: they exist so a hang fails the run (and the cleanup
/// runs) instead of holding a billing pod forever. `up` may wait 5 min for capacity, then
/// per placement ≤10 min for the endpoint + setup + the deep check, with one replacement.
const LIST_LIMIT: Duration = Duration::from_secs(5 * 60);
const UP_LIMIT: Duration = Duration::from_secs(60 * 60);
const DEEP_LIMIT: Duration = Duration::from_secs(8 * 60);
const MUTATE_LIMIT: Duration = Duration::from_secs(5 * 60);
/// How long a rename may take to show in the listing, and a terminated pod to leave it.
const SETTLE_LIMIT: Duration = Duration::from_secs(5 * 60);
const SETTLE_EVERY: Duration = Duration::from_secs(15);
/// How long to wait for a finished command's stdout to close (a grandchild could hold it).
const READ_GRACE: Duration = Duration::from_secs(30);
const READ_GRACE_KILLED: Duration = Duration::from_secs(2);
/// The cleanup guard's re-list rounds.
const SWEEP_ROUNDS: usize = 8;
const SWEEP_PAUSE: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------------------
// The guards (pure)
// ---------------------------------------------------------------------------------------

/// What the operator asked for, from the environment.
#[derive(Debug, Clone, PartialEq)]
struct Settings {
    config: PathBuf,
    prefix_allow: Vec<String>,
    gpu: String,
}

/// Read the opt-in variables. Nothing here has a default that could make the test run by
/// itself: `ARENA_LIVE_SMOKE` must be exactly `1` and the config must be named.
fn settings(var: impl Fn(&str) -> Option<String>) -> Result<Settings, String> {
    let set = |k: &str| var(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    if set("ARENA_LIVE_SMOKE").as_deref() != Some("1") {
        return Err("ARENA_LIVE_SMOKE is not `1` — this test creates a real (billed) pod; opt in explicitly".into());
    }
    let config = set("ARENA_LIVE_CONFIG")
        .ok_or("ARENA_LIVE_CONFIG is not set — name the sandbox config.env (never the production one)")?;
    let prefix_allow = set("ARENA_LIVE_PREFIX_ALLOW")
        .map(|v| v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect())
        .unwrap_or_default();
    let gpu = set("ARENA_LIVE_GPU").unwrap_or_else(|| DEFAULT_GPU_LIST.to_string());
    Ok(Settings { config: PathBuf::from(config), prefix_allow, gpu })
}

/// Refuse the production config and anything under a forbidden directory. `canonical` has
/// its symlinks resolved, and `forbidden` holds both the literal paths and their resolved
/// forms (see [`forbidden_paths`]), so neither a symlink to the prod copy nor a symlinked
/// prod directory slips through. `Path::starts_with` compares whole components (`/rootx`
/// is not under `/root`).
fn check_config_path(canonical: &Path, forbidden: &[PathBuf]) -> Result<(), String> {
    match forbidden.iter().find(|f| canonical == f.as_path() || canonical.starts_with(f)) {
        Some(f) => Err(format!(
            "{} is (or is under) {} — production config/keys are never a smoke target",
            canonical.display(),
            f.display()
        )),
        None => Ok(()),
    }
}

/// Whether `prefix` names a cohort (`arena`, `arena8`, `arena9`, …): production's naming.
fn is_cohort_prefix(prefix: &str) -> bool {
    prefix.strip_prefix("arena").is_some_and(|rest| rest.chars().all(|c| c.is_ascii_digit()))
}

/// The fleet prefix the run may create under: a sandbox one (`devtest…`) or one the
/// operator allowed by name — but never a cohort's, even if allowed (a typo'd allow list
/// must not aim a create at production's names).
fn check_prefix(prefix: Option<&str>, allow: &[String]) -> Result<String, String> {
    let prefix = prefix.map(str::trim).filter(|p| !p.is_empty()).ok_or("the config has no MACHINE_NAME_PREFIX")?;
    if is_cohort_prefix(prefix) {
        return Err(format!("MACHINE_NAME_PREFIX `{prefix}` is a cohort prefix — refusing even if allowed"));
    }
    if prefix.starts_with(SANDBOX_PREFIX) || allow.iter().any(|a| a == prefix) {
        Ok(prefix.to_string())
    } else {
        Err(format!(
            "MACHINE_NAME_PREFIX `{prefix}` doesn't start with `{SANDBOX_PREFIX}` and isn't in ARENA_LIVE_PREFIX_ALLOW"
        ))
    }
}

/// The environment the binary gets: only [`PASSED_ENV`] (where set), plus the two values
/// the run forces — write-only proxy and its own health cache. Both are keys `Config`
/// accepts from the environment even when the file lacks them, so the child sees exactly
/// [`child_config`].
fn child_env(var: impl Fn(&str) -> Option<OsString>, state_dir: &Path) -> Vec<(String, OsString)> {
    let mut env: Vec<(String, OsString)> =
        PASSED_ENV.iter().filter_map(|k| var(k).map(|v| (k.to_string(), v))).collect();
    env.push(("SSH_PROXY_RELOAD_CMD".into(), OsString::new()));
    env.push(("ARENA_STATE_DIR".into(), state_dir.as_os_str().to_owned()));
    env
}

/// The config as the binary will see it: the file, plus the values [`child_env`] sets
/// (`Config::load` lets those environment keys override or introduce a value).
fn child_config(file_text: &str, state_dir: &Path, runpod_api: Option<&str>) -> Config {
    let mut cfg = Config::parse(file_text);
    cfg.values.insert("SSH_PROXY_RELOAD_CMD".into(), String::new());
    cfg.values.insert("ARENA_STATE_DIR".into(), state_dir.display().to_string());
    if let Some(api) = runpod_api {
        cfg.values.insert("RUNPOD_API".into(), api.to_string());
    }
    cfg
}

/// Lifecycle commands sync the proxy, so a configured one must be safe to write: local
/// (a remote proxy is the shared production host, reached over SSH), outside `/etc`
/// (production's nginx config lives there), and write-only (forced by [`child_env`];
/// checked anyway). `Ok(None)`: no proxy configured, nothing is written.
fn check_proxy(cfg: &Config, home: Option<&str>) -> Result<Option<String>, String> {
    let Ok(px) = ProxyConfig::from_config(cfg) else { return Ok(None) };
    if !px.local {
        return Err(format!(
            "the proxy is remote ({}, PROXY_LOCAL=false): the smoke test only writes a local proxy file",
            px.proxy_host
        ));
    }
    if !px.write_only() {
        return Err("SSH_PROXY_RELOAD_CMD isn't empty for the child — it would reload nginx".into());
    }
    let path = match (px.nginx_path.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => format!("{}/{rest}", home.trim_end_matches('/')),
        _ => px.nginx_path.clone(),
    };
    if Path::new(&path).starts_with("/etc") {
        return Err(format!("the proxy config is {path} — under /etc, that's a system nginx's; use a sandbox file"));
    }
    Ok(Some(path))
}

/// A pod as `teardown --check --json` lists it (any state but TERMINATED).
#[derive(Debug, Clone, PartialEq)]
struct Listed {
    id: String,
    name: String,
    provider: String,
    status: String,
}

/// What the run reads from `teardown --check --json`.
#[derive(Debug, Clone, PartialEq)]
struct Teardown {
    /// Every pod on every configured provider.
    pods: Vec<Listed>,
    /// The names forwarded in the local proxy file; `None` when the check couldn't read it.
    forwards: Option<Vec<String>>,
}

/// Parse the teardown report, failing closed: a provider whose pods couldn't be listed is
/// an error (unknown is not empty — that is the report's own rule), and so is any shape
/// this doesn't recognise. Its exit status is ignored: it is non-zero whenever anything
/// remains, which on a sandbox account is normal.
fn parse_teardown(json: &str) -> Result<Teardown, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("teardown --check --json isn't JSON ({e})"))?;
    let items = v.get("items").and_then(Value::as_array).ok_or("teardown JSON has no `items` array")?;
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(String::from);
    let mut pods = Vec::new();
    let mut saw_pods = false;
    let mut forwards = None;
    for item in items {
        let area = text(item, "area").ok_or("teardown item without an `area`")?;
        let verdict = text(item, "verdict").ok_or("teardown item without a `verdict`")?;
        let scope = text(item, "scope").unwrap_or_default();
        let entries = item.get("entries").and_then(Value::as_array).ok_or("teardown item without `entries`")?;
        match area.as_str() {
            "pods" => {
                saw_pods = true;
                match verdict.as_str() {
                    "clear" | "remaining" | "skipped" => {}
                    "unknown" => {
                        let why = text(item, "summary").unwrap_or_default();
                        return Err(format!("{scope}: {why} — can't tell what is on that account"));
                    }
                    other => return Err(format!("teardown pods verdict `{other}` isn't one this test knows")),
                }
                for e in entries.iter().filter(|e| e.get("kind").and_then(Value::as_str) == Some("pod")) {
                    let (Some(id), Some(name)) = (text(e, "id"), text(e, "name")) else {
                        return Err("teardown pod entry without id/name".into());
                    };
                    pods.push(Listed {
                        id,
                        name,
                        provider: text(e, "provider").unwrap_or_default(),
                        status: text(e, "status").unwrap_or_default(),
                    });
                }
            }
            "proxy" => {
                forwards = match verdict.as_str() {
                    "unknown" => None,
                    _ => Some(
                        entries
                            .iter()
                            .filter(|e| e.get("kind").and_then(Value::as_str) == Some("forward"))
                            .filter_map(|e| text(e, "name"))
                            .collect(),
                    ),
                };
            }
            _ => {}
        }
    }
    if !saw_pods {
        return Err("teardown JSON lists no pods item at all".into());
    }
    Ok(Teardown { pods, forwards })
}

/// The pods that aren't this fleet's (`{prefix}-…`): any one means the wrong account.
fn foreign<'a>(names: impl IntoIterator<Item = &'a str>, prefix: &str) -> Vec<&'a str> {
    let ours = format!("{prefix}-");
    let mut out: Vec<&str> = names.into_iter().filter(|n| !n.starts_with(&ours)).collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// A machine name: the `MACHINE_NAME_LIST` entry and its full pod name.
#[derive(Debug, Clone, PartialEq)]
struct Name {
    short: String,
    full: String,
}

/// `n` list names no pod holds, from the *end* of the list (the start is where people put
/// the machines they're using by hand). Absolute (`@`) entries are staff boxes, never used.
fn pick_free_names(list: &[String], prefix: &str, taken: &BTreeSet<String>, n: usize) -> Result<Vec<Name>, String> {
    let mut out: Vec<Name> = Vec::new();
    for entry in list.iter().rev().filter(|e| !is_absolute(e)) {
        let full = qualify(prefix, entry);
        if !taken.contains(&full) && !out.iter().any(|x| x.full == full) {
            out.push(Name { short: entry.clone(), full });
        }
        if out.len() == n {
            return Ok(out);
        }
    }
    Err(format!("needs {n} free MACHINE_NAME_LIST names (one to create, one to rename to); found {}", out.len()))
}

/// Every IPv4 literal in `s`: four dot-separated 0-255 numbers inside a run of digits and
/// dots (a version `580.65.06` or a time `00.000` isn't one).
fn ipv4_literals(s: &str) -> Vec<String> {
    let octet = |p: &str| {
        !p.is_empty() && p.len() <= 3 && p.bytes().all(|b| b.is_ascii_digit()) && p.parse::<u16>().is_ok_and(|n| n <= 255)
    };
    let mut found = Vec::new();
    for run in s.split(|c: char| !(c.is_ascii_digit() || c == '.')) {
        let parts: Vec<&str> = run.split('.').collect();
        for w in parts.windows(4) {
            if w.iter().all(|p| octet(p)) {
                found.push(w.join("."));
            }
        }
    }
    found
}

/// The deep-check verdict of pod `id` from `pods test --deep --json`, with every check that
/// didn't pass (`name: status — detail`) for the failure message.
fn deep_verdict(json: &str, id: &str) -> Result<(String, Vec<String>), String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("pods test --deep --json isn't JSON ({e}): {json:.200}"))?;
    let rows = v.as_array().ok_or("pods test --deep --json isn't an array")?;
    let mine: Vec<&Value> = rows.iter().filter(|r| r.get("id").and_then(Value::as_str) == Some(id)).collect();
    let [row] = mine.as_slice() else {
        return Err(format!("expected one row for pod {id}, got {}", mine.len()));
    };
    let status = row.get("status").and_then(Value::as_str).ok_or("deep-check row without a status")?.to_string();
    let issues = row
        .get("checks")
        .and_then(Value::as_array)
        .ok_or("deep-check row without checks")?
        .iter()
        .filter(|c| c.get("status").and_then(Value::as_str) != Some("pass"))
        .map(|c| {
            let s = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("?");
            format!("{}: {} — {}", s("name"), s("status"), s("detail"))
        })
        .collect();
    Ok((status, issues))
}

/// The public machine named `short` (exactly one).
fn public_entry<'a>(snap: &'a PublicSnapshot, short: &str) -> Result<&'a PublicMachine, String> {
    let found: Vec<&PublicMachine> = snap.machines.iter().filter(|m| m.name == short).collect();
    match found.as_slice() {
        [m] => Ok(*m),
        _ => Err(format!("expected `{short}` once in the public snapshot, found it {} time(s)", found.len())),
    }
}

/// What the public JSON must never carry, checked against the real pod: IPv4 literals, the
/// pod's SSH host, its provider id, and the fleet prefix (the page shows list names only).
fn public_leaks(text: &str, prefix: &str, pod: &Pod) -> Vec<String> {
    let mut leaks: Vec<String> = ipv4_literals(text).into_iter().map(|ip| format!("IPv4 literal {ip}")).collect();
    if let Some(host) = pod.ssh_ip.as_deref().map(str::trim).filter(|h| !h.is_empty()) {
        if text.contains(host) {
            leaks.push(format!("the pod's SSH host {host}"));
        }
    }
    if !pod.id.is_empty() && text.contains(&pod.id) {
        leaks.push(format!("the pod id {}", pod.id));
    }
    if text.contains(&format!("{prefix}-")) {
        leaks.push(format!("the fleet prefix `{prefix}-`"));
    }
    leaks
}

// ---------------------------------------------------------------------------------------
// Driving the binary (I/O)
// ---------------------------------------------------------------------------------------

/// How a command ended: its exit code, or why there is none (timed out and killed, killed
/// by a signal, couldn't start).
struct Ran {
    status: Result<i32, String>,
    stdout: String,
}

impl Ran {
    fn ok(&self) -> bool {
        self.status == Ok(0)
    }

    fn describe(&self) -> String {
        match &self.status {
            Ok(code) => format!("exit {code}"),
            Err(why) => why.clone(),
        }
    }
}

/// The built binary, aimed at the checked config.
struct Arena {
    bin: PathBuf,
    config: PathBuf,
    cwd: PathBuf,
    state_dir: PathBuf,
}

impl Arena {
    /// Run `arena --config <cfg> <args>` to completion or `limit` (then it's killed). Stdin
    /// is closed, so a confirmation can only pass with `-y` — no prompt can ever hang the
    /// run. Stderr (progress) always goes to the terminal; stdout is captured for the JSON
    /// commands, else shown too.
    fn run(&self, args: &[&str], limit: Duration, capture: bool) -> Ran {
        eprintln!("[smoke] $ arena {}", args.join(" "));
        let mut cmd = Command::new(&self.bin);
        cmd.env_clear()
            .envs(child_env(|k| std::env::var_os(k), &self.state_dir))
            .arg("--config")
            .arg(&self.config)
            .args(args)
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .stdout(if capture { Stdio::piped() } else { Stdio::inherit() })
            .stderr(Stdio::inherit());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Ran { status: Err(format!("couldn't start {}: {e}", self.bin.display())), stdout: String::new() }
            }
        };
        // Drain stdout on its own thread: a child blocked on a full pipe would otherwise
        // look exactly like a hang. Its result comes back over a channel, so a grandchild
        // still holding the pipe after the child is gone can't hang the run either.
        let (tx, rx) = std::sync::mpsc::channel();
        if let Some(mut out) = child.stdout.take() {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = out.read_to_end(&mut buf);
                let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
            });
        }
        let started = Instant::now();
        let mut killed = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(st)) => break st.code().ok_or_else(|| "killed by a signal".to_string()),
                Ok(None) if started.elapsed() >= limit => {
                    let _ = child.kill();
                    let _ = child.wait();
                    killed = true;
                    break Err(format!("timed out after {}s — killed", limit.as_secs()));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(e) => break Err(format!("waiting for it failed: {e}")),
            }
        };
        let drain = if killed { READ_GRACE_KILLED } else { READ_GRACE };
        let stdout = rx.recv_timeout(drain).unwrap_or_default();
        Ran { status, stdout }
    }

    /// Every pod on every configured provider (`teardown --check --json`, read-only), or why
    /// that couldn't be established.
    fn teardown(&self) -> Result<Teardown, String> {
        let ran = self.run(&["teardown", "--check", "--json"], LIST_LIMIT, true);
        if ran.stdout.trim().is_empty() {
            return Err(format!("teardown --check printed nothing ({})", ran.describe()));
        }
        parse_teardown(&ran.stdout)
    }

    /// `pods list --json --no-probe` (read-only): the provider's own view, endpoints
    /// included, TERMINATED pods too where the provider still lists them.
    fn pods(&self) -> Result<Vec<Pod>, String> {
        let ran = self.run(&["pods", "list", "--json", "--no-probe"], LIST_LIMIT, true);
        if !ran.ok() {
            return Err(format!("pods list failed ({})", ran.describe()));
        }
        serde_json::from_str(&ran.stdout).map_err(|e| format!("pods list --json didn't parse ({e})"))
    }

    /// The one live pod named `full`.
    fn only_pod(&self, full: &str) -> Result<Pod, String> {
        let pods: Vec<Pod> = self.pods()?.into_iter().filter(|p| p.name == full && !terminated(&p.status)).collect();
        match <[Pod; 1]>::try_from(pods) {
            Ok([pod]) => Ok(pod),
            Err(pods) => Err(format!("expected one pod named {full}, found {}", pods.len())),
        }
    }

    /// Pod `id`'s current name.
    fn name_of(&self, id: &str) -> Result<String, String> {
        self.pods()?.into_iter().find(|p| p.id == id).map(|p| p.name).ok_or(format!("pod {id} isn't listed"))
    }
}

fn terminated(status: &str) -> bool {
    status.trim().eq_ignore_ascii_case("TERMINATED")
}

/// Retry `attempt` every `every` until it succeeds or `limit` would pass.
fn poll_until<T>(limit: Duration, every: Duration, mut attempt: impl FnMut() -> Result<T, String>) -> Result<T, String> {
    let started = Instant::now();
    loop {
        match attempt() {
            Ok(v) => return Ok(v),
            Err(e) if started.elapsed() + every > limit => return Err(e),
            Err(e) => {
                eprintln!("[smoke]   not yet: {e}");
                std::thread::sleep(every);
            }
        }
    }
}

/// The guard that makes sure nothing this run created outlives it — on success, on a
/// failed assertion and on a panic alike (it runs from `Drop`). It terminates every pod
/// holding one of the run's names (free when it started, so any holder is this run's) or
/// one of the ids it saw, then re-lists. While unwinding it wants two clean listings
/// [`SWEEP_PAUSE`] apart: a create killed mid-flight can surface a little later. It never
/// panics itself (a panic in `Drop` while unwinding aborts the process); what it can't
/// confirm it says loudly, with the commands to finish by hand.
struct Cleanup<'a> {
    arena: &'a Arena,
    names: Vec<String>,
    ids: RefCell<BTreeSet<String>>,
}

impl<'a> Cleanup<'a> {
    fn new(arena: &'a Arena, names: Vec<String>) -> Self {
        Self { arena, names, ids: RefCell::new(BTreeSet::new()) }
    }

    fn note(&self, id: &str) {
        self.ids.borrow_mut().insert(id.to_string());
    }

    fn ours<'p>(&self, pods: &'p [Listed]) -> Vec<&'p Listed> {
        let ids = self.ids.borrow();
        pods.iter().filter(|p| self.names.contains(&p.name) || ids.contains(&p.id)).collect()
    }
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let unwinding = std::thread::panicking();
        let mut clean_in_a_row = 0;
        for round in 1..=SWEEP_ROUNDS {
            match self.arena.teardown() {
                Err(e) => eprintln!("[smoke cleanup] round {round}: couldn't list every provider ({e})"),
                Ok(t) => {
                    let ours = self.ours(&t.pods);
                    if ours.is_empty() {
                        clean_in_a_row += 1;
                        if !unwinding || clean_in_a_row >= 2 {
                            eprintln!("[smoke cleanup] nothing of this run is left");
                            return;
                        }
                    } else {
                        clean_in_a_row = 0;
                        for p in ours {
                            self.note(&p.id);
                            eprintln!("[smoke cleanup] terminating {} ({}, {} {})", p.name, p.id, p.provider, p.status);
                            let ran = self.arena.run(&["pods", "terminate", &p.id, "-y"], MUTATE_LIMIT, false);
                            if !ran.ok() {
                                eprintln!("[smoke cleanup]   terminate {}: {}", p.id, ran.describe());
                            }
                        }
                    }
                }
            }
            std::thread::sleep(SWEEP_PAUSE);
        }
        let ids: Vec<String> = self.ids.borrow().iter().cloned().collect();
        eprintln!(
            "[smoke cleanup] !!! could NOT confirm this run's pods are gone (names {}, ids {}). Check NOW: \
             arena --config {} teardown --check   and   arena --config {} pods terminate <id>",
            self.names.join(", "),
            if ids.is_empty() { "none seen".into() } else { ids.join(", ") },
            self.arena.config.display(),
            self.arena.config.display(),
        );
    }
}

/// The checked plan for one run.
struct Plan {
    arena: Arena,
    prefix: String,
    gpu: String,
    create: Name,
    rename_to: Name,
}

/// The literal forbidden paths plus their resolved forms (where they resolve: resolving
/// only stats, it reads nothing).
fn forbidden_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in std::iter::once(PROD_CONFIG).chain(FORBIDDEN_DIRS) {
        out.push(PathBuf::from(p));
        if let Ok(c) = std::fs::canonicalize(p) {
            out.push(c);
        }
    }
    out
}

/// Every guard, in order; the first that fails refuses the run. Only read-only commands
/// run here.
fn preflight() -> Result<Plan, String> {
    let s = settings(|k| std::env::var(k).ok())?;
    let config = std::fs::canonicalize(&s.config)
        .map_err(|e| format!("ARENA_LIVE_CONFIG {}: {e}", s.config.display()))?;
    check_config_path(&config, &forbidden_paths())?;
    let text = std::fs::read_to_string(&config).map_err(|e| format!("reading {}: {e}", config.display()))?;
    let file = Config::parse(&text);
    let prefix = check_prefix(file.get("MACHINE_NAME_PREFIX"), &s.prefix_allow)?;

    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let state_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("live-smoke-{stamp}"));
    let child = child_config(&text, &state_dir, std::env::var("RUNPOD_API").ok().as_deref());
    let proxy = check_proxy(&child, std::env::var("HOME").ok().as_deref())?;

    let cwd = config.parent().ok_or("the config has no parent directory")?.to_path_buf();
    let arena = Arena { bin: PathBuf::from(env!("CARGO_BIN_EXE_arena")), config, cwd, state_dir };
    eprintln!(
        "[smoke] config {} · prefix {prefix} · proxy {} · health cache {}",
        arena.config.display(),
        proxy.as_deref().unwrap_or("none"),
        arena.state_dir.display()
    );

    // The wrong-account guard, over both views: teardown's (every provider must answer) and
    // the provider's own list (which also shows TERMINATED pods still holding a name).
    let fleet = arena.teardown().map_err(|e| format!("can't confirm whose account this is: {e}"))?;
    let listed = arena.pods().map_err(|e| format!("can't confirm whose account this is: {e}"))?;
    let all_names = fleet.pods.iter().map(|p| p.name.as_str()).chain(listed.iter().map(|p| p.name.as_str()));
    let strangers = foreign(all_names, &prefix);
    if !strangers.is_empty() {
        return Err(format!(
            "the account holds pods not named {prefix}-…: {} — wrong account (production?); nothing created",
            strangers.join(", ")
        ));
    }
    let taken: BTreeSet<String> =
        fleet.pods.iter().map(|p| p.name.clone()).chain(listed.into_iter().map(|p| p.name)).collect();
    let mut names = pick_free_names(&file.machine_names, &prefix, &taken, 2)?.into_iter();
    let (create, rename_to) = (names.next().expect("two names"), names.next().expect("two names"));
    std::fs::create_dir_all(&arena.state_dir).map_err(|e| format!("creating {}: {e}", arena.state_dir.display()))?;
    Ok(Plan { arena, prefix, gpu: s.gpu, create, rename_to })
}

#[test]
#[ignore = "LIVE: creates, checks and terminates one cheap RunPod pod — see README \"Testing\""]
fn live_smoke() {
    let plan = preflight().unwrap_or_else(|why| panic!("live smoke refused: {why}"));
    let Plan { arena, prefix, gpu, create, rename_to } = plan;
    eprintln!("[smoke] creating {} (renamed to {} and back), --gpu {gpu}, ≤ ${MAX_PRICE}/h", create.full, rename_to.full);
    // From here on every exit path — assertion, panic, deadline — goes through the guard.
    let cleanup = Cleanup::new(&arena, vec![create.full.clone(), rename_to.full.clone()]);

    // 1. Spin up one pod: placement over the cheap list, setup, deep check (a FAIL host is
    //    replaced by `up` itself), keys, proxy.
    let up = arena.run(
        &[
            "--provider", "runpod", "pods", "up", &create.short, "--gpu", &gpu, "--gpus", "1", "--cloud", "community",
            "--max-price", MAX_PRICE, "--retry-mins", "5", "--check", "-y",
        ],
        UP_LIMIT,
        false,
    );
    assert!(up.ok(), "pods up didn't bring {} up READY: {}", create.full, up.describe());
    let pod = arena.only_pod(&create.full).unwrap_or_else(|e| panic!("after up: {e}"));
    cleanup.note(&pod.id);
    assert_eq!(pod.status.to_ascii_uppercase(), "RUNNING", "{} after up", create.full);

    // 2. The deep check again, as the operator runs it: parseable JSON, not FAIL.
    let deep = arena.run(&["pods", "test", "--deep", &create.full, "--json"], DEEP_LIMIT, true);
    let (verdict, issues) = deep_verdict(&deep.stdout, &pod.id).unwrap_or_else(|e| panic!("test --deep: {e}"));
    assert_ne!(verdict, "fail", "test --deep FAILed {}: {issues:#?}", create.full);
    assert!(deep.ok(), "test --deep exited {} with verdict {verdict}", deep.describe());
    if !issues.is_empty() {
        eprintln!("[smoke] deep check {verdict}: {issues:?}");
    }

    // 3. The public snapshot: the machine is there (up, with the cached verdict), and none of
    //    what makes it reachable or billable is.
    let snap = arena.run(&["snapshot", "--public"], LIST_LIMIT, true);
    assert!(snap.ok(), "snapshot --public: {}", snap.describe());
    let public: PublicSnapshot =
        serde_json::from_str(&snap.stdout).unwrap_or_else(|e| panic!("snapshot --public isn't a PublicSnapshot ({e})"));
    let machine = public_entry(&public, &create.short).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(machine.status, PublicStatus::Up, "{} in the public snapshot", create.short);
    assert!(
        matches!(machine.health.status, PublicHealthStatus::Pass | PublicHealthStatus::Warn),
        "{}'s health should come from the deep check's cache: {:?}",
        create.short,
        machine.health
    );
    let leaks = public_leaks(&snap.stdout, &prefix, &pod);
    assert!(leaks.is_empty(), "the public snapshot leaks: {leaks:?}");

    // 4. Rename there and back (metadata only: same pod id, ~/.name + proxy follow).
    for (from, to) in [(&create, &rename_to), (&rename_to, &create)] {
        let ran = arena.run(&["pods", "rename", &from.full, &to.short, "-y"], MUTATE_LIMIT, false);
        assert!(ran.ok(), "rename {} → {}: {}", from.full, to.full, ran.describe());
        poll_until(SETTLE_LIMIT, SETTLE_EVERY, || match arena.name_of(&pod.id)? {
            n if n == to.full => Ok(()),
            n => Err(format!("pod {} is still named {n}", pod.id)),
        })
        .unwrap_or_else(|e| panic!("after rename {} → {}: {e}", from.full, to.full));
    }

    // 5. Terminate it, then the end-of-program audit must show nothing of it.
    let ran = arena.run(&["pods", "terminate", &pod.id, "-y"], MUTATE_LIMIT, false);
    assert!(ran.ok(), "terminate {}: {}", pod.id, ran.describe());
    let after = poll_until(SETTLE_LIMIT, SETTLE_EVERY, || {
        let t = arena.teardown()?;
        let left: Vec<String> =
            cleanup.ours(&t.pods).iter().map(|p| format!("{} ({}, {})", p.name, p.id, p.status)).collect();
        if left.is_empty() { Ok(t) } else { Err(format!("teardown still lists {}", left.join(", "))) }
    })
    .unwrap_or_else(|e| panic!("after terminate: {e}"));
    // Its proxy forward goes on the first sync after the provider stops listing the pod —
    // the terminate's own sync can be too early; one `proxy apply` (write-only) settles it.
    let ours_forwarded = |t: &Teardown| -> Vec<String> {
        t.forwards.iter().flatten().filter(|n| cleanup.names.contains(n)).cloned().collect()
    };
    if !ours_forwarded(&after).is_empty() {
        let ran = arena.run(&["proxy", "apply", "-y"], MUTATE_LIMIT, false);
        assert!(ran.ok(), "proxy apply: {}", ran.describe());
        let t = arena.teardown().unwrap_or_else(|e| panic!("teardown after proxy apply: {e}"));
        assert!(ours_forwarded(&t).is_empty(), "the proxy still forwards {:?} after the pod is gone", ours_forwarded(&t));
    }
    if after.forwards.is_none() {
        eprintln!("[smoke] note: teardown couldn't read the proxy file — forwards not checked");
    }

    drop(cleanup); // one more listing; nothing of this run is left
    let _ = std::fs::remove_dir_all(&arena.state_dir);
    eprintln!("[smoke] PASS");
}

// ---------------------------------------------------------------------------------------
// The guards' own tests (offline; these run in every `cargo test`)
// ---------------------------------------------------------------------------------------

fn vars<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
}

#[test]
fn smoke_needs_both_opt_in_variables() {
    let cfg = ("ARENA_LIVE_CONFIG", "/home/dev/sandbox/config.env");
    for (env, refusal) in [
        (vec![cfg], "ARENA_LIVE_SMOKE"),
        (vec![("ARENA_LIVE_SMOKE", "true"), cfg], "ARENA_LIVE_SMOKE"),
        (vec![("ARENA_LIVE_SMOKE", "0"), cfg], "ARENA_LIVE_SMOKE"),
        (vec![("ARENA_LIVE_SMOKE", "1")], "ARENA_LIVE_CONFIG"),
        (vec![("ARENA_LIVE_SMOKE", "1"), ("ARENA_LIVE_CONFIG", "  ")], "ARENA_LIVE_CONFIG"),
    ] {
        let err = settings(vars(&env)).unwrap_err();
        assert!(err.contains(refusal), "{env:?}: {err}");
    }
    let s = settings(vars(&[("ARENA_LIVE_SMOKE", " 1 "), cfg])).unwrap();
    assert_eq!(s, Settings { config: cfg.1.into(), prefix_allow: vec![], gpu: DEFAULT_GPU_LIST.into() });
    let s = settings(vars(&[
        ("ARENA_LIVE_SMOKE", "1"),
        cfg,
        ("ARENA_LIVE_PREFIX_ALLOW", "nick, ,lab"),
        ("ARENA_LIVE_GPU", "3070"),
    ]))
    .unwrap();
    assert_eq!((s.prefix_allow, s.gpu), (vec!["nick".to_string(), "lab".to_string()], "3070".to_string()));
}

#[test]
fn smoke_refuses_production_config_paths() {
    let forbidden: Vec<PathBuf> =
        [PROD_CONFIG, "/home/dev/prod-ro", "/root", "/srv/prod-ro-real"].iter().map(PathBuf::from).collect();
    for bad in [
        "/home/dev/prod-ro/config.env",
        "/home/dev/prod-ro/other.env",
        "/root/arena-infra/config.env",
        "/srv/prod-ro-real/config.env", // where a symlinked prod dir resolves to
    ] {
        assert!(check_config_path(Path::new(bad), &forbidden).is_err(), "{bad}");
    }
    for ok in ["/home/dev/sandbox/config.env", "/rootless/config.env", "/home/dev/prod-ro-copy.env"] {
        assert_eq!(check_config_path(Path::new(ok), &forbidden), Ok(()), "{ok}");
    }
}

#[test]
fn smoke_only_runs_under_a_sandbox_prefix() {
    let allow = vec!["nicktest".to_string(), "arena9".to_string()];
    for (prefix, ok) in [
        (Some("devtest"), true),
        (Some("devtest2"), true),
        (Some("nicktest"), true), // allowed by name
        (Some("nicktest2"), false), // allow is exact
        (Some("arena9"), false),  // a cohort prefix, even though allowed
        (Some("arena"), false),
        (Some("arena10"), false),
        (Some("labtest"), false),
        (Some(""), false),
        (None, false),
    ] {
        assert_eq!(check_prefix(prefix, &allow).is_ok(), ok, "{prefix:?}");
    }
    assert!(is_cohort_prefix("arena8") && !is_cohort_prefix("arena-dev") && !is_cohort_prefix("devtest"));
}

#[test]
fn child_env_passes_only_the_allowlist_and_forces_write_only() {
    let host = |k: &str| -> Option<OsString> {
        match k {
            "PATH" => Some("/usr/bin:/bin".into()),
            "HOME" => Some("/home/dev".into()),
            "RUNPOD_API" => Some("v2".into()),
            // Present in the operator's shell, must not reach the binary:
            "RUNPOD_API_KEY" | "MACHINE_NAME_PREFIX" | "SSH_AUTH_SOCK" | "SSH_PROXY_RELOAD_CMD" => Some("x".into()),
            _ => None,
        }
    };
    let env = child_env(host, Path::new("/tmp/state"));
    let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["PATH", "HOME", "RUNPOD_API", "SSH_PROXY_RELOAD_CMD", "ARENA_STATE_DIR"]);
    let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    assert_eq!(get("SSH_PROXY_RELOAD_CMD"), Some(OsString::new()), "write-only, whatever the shell exported");
    assert_eq!(get("ARENA_STATE_DIR"), Some(OsString::from("/tmp/state")));
    for k in PASSED_ENV {
        assert!(!k.contains("KEY") && !k.contains("TOKEN") && !k.contains("SOCK"), "{k} could carry a secret");
    }
}

#[test]
fn smoke_proxy_must_be_a_local_write_only_file_outside_etc() {
    let state = Path::new("/tmp/state");
    let px = |extra: &str| child_config(&format!("MACHINE_NAME_PREFIX=devtest\n{extra}"), state, None);
    // No proxy: nothing to write.
    assert_eq!(check_proxy(&px(""), Some("/home/dev")), Ok(None));
    // The sandbox's: a local file, reload forced off even though the file asks for one.
    let sandbox =
        px("SSH_PROXY_HOST=localhost\nSSH_PROXY_NGINX_CONFIG_PATH=proxy/proxy.conf\nSSH_PROXY_RELOAD_CMD=\"nginx -s reload\"\n");
    assert_eq!(check_proxy(&sandbox, Some("/home/dev")), Ok(Some("proxy/proxy.conf".into())));
    assert_eq!(check_proxy(&px("SSH_PROXY_HOST=localhost\n"), Some("/home/dev/")), Ok(Some("/home/dev/proxy.conf".into())));
    // Production's nginx file, or a remote proxy host: refused.
    let prod = px("SSH_PROXY_HOST=localhost\nSSH_PROXY_NGINX_CONFIG_PATH=/etc/nginx/streams-enabled/proxy.conf\n");
    assert!(check_proxy(&prod, Some("/home/dev")).unwrap_err().contains("/etc"));
    let remote = px("SSH_PROXY_HOST=cute.sus.cat\nPROXY_LOCAL=false\n");
    assert!(check_proxy(&remote, Some("/home/dev")).unwrap_err().contains("remote"));
    // A child config without the forced empty reload would reload nginx: refused.
    let mut reloading = sandbox.clone();
    reloading.values.insert("SSH_PROXY_RELOAD_CMD".into(), "nginx -s reload".into());
    assert!(check_proxy(&reloading, None).is_err());
    // What the child sees is the file plus the forced values.
    let c = child_config("ARENA_STATE_DIR=/elsewhere\nRUNPOD_API=v1\n", state, Some("v2"));
    assert_eq!((c.get("ARENA_STATE_DIR"), c.get("RUNPOD_API")), (Some("/tmp/state"), Some("v2")));
}

/// A `teardown --check --json` report shaped as `arena_core::teardown::Report` serializes.
fn report(items: Value) -> String {
    serde_json::json!({"cohort": "devtest", "clear": false, "remaining": 1, "unknown": 0, "items": items}).to_string()
}

fn pods_item(scope: &str, verdict: &str, pods: &[(&str, &str)]) -> Value {
    let entries: Vec<Value> = pods
        .iter()
        .map(|(id, name)| {
            serde_json::json!({"kind": "pod", "id": id, "name": name, "provider": scope, "status": "RUNNING",
                "billing": true, "cost_per_hr": 0.13, "cohort": true, "staff": false})
        })
        .collect();
    serde_json::json!({"area": "pods", "scope": scope, "verdict": verdict, "summary": "…", "entries": entries, "notes": [], "fix": []})
}

#[test]
fn teardown_listing_fails_closed() {
    let proxy = serde_json::json!({"area": "proxy", "scope": "proxy/proxy.conf", "verdict": "remaining", "summary": "1",
        "entries": [{"kind": "forward", "name": "devtest-echo", "public_port": 9504, "target": "1.2.3.4:10022", "provider": "runpod"}],
        "notes": [], "fix": []});
    let ok = report(serde_json::json!([
        pods_item("runpod", "remaining", &[("r1", "devtest-echo"), ("r2", "james-gpu")]),
        pods_item("vast", "clear", &[]),
        pods_item("hetzner", "skipped", &[]),
        proxy,
    ]));
    let t = parse_teardown(&ok).unwrap();
    assert_eq!(t.pods.iter().map(|p| (p.id.as_str(), p.name.as_str())).collect::<Vec<_>>(), [("r1", "devtest-echo"), ("r2", "james-gpu")]);
    assert_eq!(t.forwards, Some(vec!["devtest-echo".to_string()]));
    assert_eq!(foreign(t.pods.iter().map(|p| p.name.as_str()), "devtest"), ["james-gpu"]);

    // A provider that couldn't list is not "no pods": refused, naming it.
    let unknown = report(serde_json::json!([pods_item("runpod", "remaining", &[]), pods_item("vast", "unknown", &[])]));
    assert!(parse_teardown(&unknown).unwrap_err().starts_with("vast"));
    // Shapes this test doesn't know are errors, never an empty fleet.
    for bad in [
        "".to_string(),
        "[]".to_string(),
        r#"{"items": 3}"#.to_string(),
        report(serde_json::json!([])),
        report(serde_json::json!([{"area": "pods", "scope": "runpod", "verdict": "clear"}])),
        report(serde_json::json!([pods_item("runpod", "maybe", &[])])),
        report(serde_json::json!([{"area": "pods", "scope": "runpod", "verdict": "remaining", "entries": [{"kind": "pod", "name": "x"}]}])),
    ] {
        assert!(parse_teardown(&bad).is_err(), "{bad}");
    }
    // An unreadable proxy file: forwards unknown (not "none").
    let unread = report(serde_json::json!([pods_item("runpod", "clear", &[]),
        {"area": "proxy", "scope": "", "verdict": "unknown", "summary": "remote", "entries": [], "notes": [], "fix": []}]));
    assert_eq!(parse_teardown(&unread).unwrap().forwards, None);
}

#[test]
fn free_names_come_from_the_end_of_the_list_skipping_staff_and_taken() {
    let list: Vec<String> = ["alpha", "bravo", "charlie", "@james-gpu", "delta", "echo"].iter().map(|s| s.to_string()).collect();
    let taken: BTreeSet<String> = ["devtest-echo".to_string(), "james-gpu".to_string()].into();
    let got = pick_free_names(&list, "devtest", &taken, 2).unwrap();
    assert_eq!(
        got,
        [
            Name { short: "delta".into(), full: "devtest-delta".into() },
            Name { short: "charlie".into(), full: "devtest-charlie".into() }
        ]
    );
    let all: BTreeSet<String> = list.iter().map(|e| qualify("devtest", e)).collect();
    assert!(pick_free_names(&list, "devtest", &all, 2).unwrap_err().contains("found 0"));
    let dup: Vec<String> = vec!["alpha".into(), "alpha".into()];
    assert!(pick_free_names(&dup, "devtest", &BTreeSet::new(), 2).is_err(), "a duplicated entry is one name");
}

#[test]
fn ipv4_literals_are_found_and_lookalikes_are_not() {
    for (text, want) in [
        (r#"{"target":"194.68.245.12:22014"}"#, vec!["194.68.245.12"]),
        ("ssh root@10.0.0.1 -p 22", vec!["10.0.0.1"]),
        ("a 1.2.3.4.5 b", vec!["1.2.3.4", "2.3.4.5"]),
        (r#"{"updated_at":"2026-10-08T12:34:56Z","gpu":"1×RTX 3070","age_secs":42}"#, vec![]),
        ("driver 580.65.06, time 00.000, 256.1.1.1, 1.2.3", vec![]),
    ] {
        assert_eq!(ipv4_literals(text), want, "{text}");
    }
}

#[test]
fn deep_verdict_reads_one_pods_row() {
    let json = r#"[
      {"id": "r1", "name": "devtest-echo", "provider": "runpod", "status": "warn",
       "checks": [{"name": "gpu0", "status": "pass", "detail": "ok"},
                  {"name": "maintenance", "status": "warn", "detail": "10-09 02:00→06:00 UTC"}], "facts": null},
      {"id": "r2", "name": "devtest-delta", "provider": "runpod", "status": "fail", "checks": [], "facts": null}
    ]"#;
    let (status, issues) = deep_verdict(json, "r1").unwrap();
    assert_eq!(status, "warn");
    assert_eq!(issues, ["maintenance: warn — 10-09 02:00→06:00 UTC"]);
    assert!(deep_verdict(json, "r9").unwrap_err().contains("got 0"));
    assert!(deep_verdict("[]", "r1").is_err());
    assert!(deep_verdict("(no pods with an SSH endpoint)", "r1").is_err());
    let twice = r#"[{"id":"r1","status":"pass","checks":[]},{"id":"r1","status":"pass","checks":[]}]"#;
    assert!(deep_verdict(twice, "r1").unwrap_err().contains("got 2"));
}

#[test]
fn public_snapshot_checks_find_the_machine_and_every_leak() {
    let text = r#"{"updated_at":"2026-10-08T12:00:00Z","complete":true,"machines":[
      {"name":"echo","gpu":"1×RTX 3070","status":"up",
       "health":{"status":"warn","checked_at":"2026-10-08T11:58:00Z","age_secs":120,"reason":"maintenance scheduled"},
       "maintenance":{"start":"2026-10-09T02:00:00Z","end":"2026-10-09T06:00:00Z"}}]}"#;
    let snap: PublicSnapshot = serde_json::from_str(text).unwrap();
    let m = public_entry(&snap, "echo").unwrap();
    assert_eq!((m.status, m.health.status), (PublicStatus::Up, PublicHealthStatus::Warn));
    assert!(public_entry(&snap, "delta").is_err());

    let pod = Pod {
        id: "r088kp345z5m8c".into(),
        name: "devtest-echo".into(),
        ssh_ip: Some("194.68.245.12".into()),
        ssh_port: Some(22014),
        ..Default::default()
    };
    assert!(public_leaks(text, "devtest", &pod).is_empty());
    let leaky = text.replace("1×RTX 3070", "1×RTX 3070 @194.68.245.12 r088kp345z5m8c devtest-echo");
    let leaks = public_leaks(&leaky, "devtest", &pod);
    assert_eq!(leaks.len(), 4, "{leaks:?}");
    // A hostname endpoint (Vast's ssh proxy) is caught by name, not by the IPv4 scan.
    let vast = Pod { ssh_ip: Some("ssh4.vast.ai".into()), ..pod };
    assert_eq!(public_leaks(&text.replace("echo", "echo ssh4.vast.ai"), "devtest", &vast), ["the pod's SSH host ssh4.vast.ai"]);
}

#[test]
fn poll_until_stops_at_success_or_the_limit() {
    let mut n = 0;
    let third = poll_until(Duration::from_secs(1), Duration::ZERO, || {
        n += 1;
        if n < 3 { Err("no".into()) } else { Ok(n) }
    });
    assert_eq!(third, Ok(3));
    let r: Result<(), String> = poll_until(Duration::ZERO, Duration::from_millis(1), || Err("never".into()));
    assert_eq!(r, Err("never".into()));
}

/// The process harness itself, against a stub script instead of the real binary (no network,
/// no `arena`): the environment is the allowlist only, the config goes first, the working
/// directory is the config's, stdin is closed, stdout is captured, and a command that
/// outlives its deadline is killed and reported.
#[cfg(unix)]
#[test]
fn the_harness_clears_the_environment_and_kills_at_the_deadline() {
    use std::os::unix::fs::PermissionsExt;
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("live-smoke-harness-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let stub = dir.join("arena-stub.sh");
    let script = [
        "#!/bin/sh",
        "case \"$4\" in sleep) exec sleep 30;; esac",
        "env",
        "echo \"ARGS $*\"",
        "echo \"CWD $(pwd -P)\"",
        "if read -r line; then echo STDIN-OPEN; else echo STDIN-CLOSED; fi",
    ];
    std::fs::write(&stub, script.join("\n") + "\n").unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    let arena = Arena { bin: stub, config: dir.join("config.env"), cwd: dir.clone(), state_dir: dir.join("state") };

    let ran = arena.run(&["pods", "list"], Duration::from_secs(20), true);
    assert!(ran.ok(), "{}", ran.describe());
    let env: Vec<&str> = ran.stdout.lines().filter_map(|l| l.split_once('=').map(|(k, _)| k)).collect();
    let forced_or_shell = ["SSH_PROXY_RELOAD_CMD", "ARENA_STATE_DIR", "PWD", "OLDPWD", "SHLVL", "_"];
    let allowed = |k: &str| PASSED_ENV.contains(&k) || forced_or_shell.contains(&k);
    assert!(env.iter().all(|k| allowed(k)), "unexpected variables reached the child: {env:?}");
    assert!(ran.stdout.lines().any(|l| l == "SSH_PROXY_RELOAD_CMD="), "write-only proxy: {}", ran.stdout);
    let cwd = std::fs::canonicalize(&dir).unwrap();
    assert!(ran.stdout.contains(&format!("ARGS --config {} pods list", dir.join("config.env").display())), "{}", ran.stdout);
    assert!(ran.stdout.contains(&format!("CWD {}", cwd.display())), "{}", ran.stdout);
    assert!(ran.stdout.contains("STDIN-CLOSED"), "a prompt must never wait on input: {}", ran.stdout);

    let started = Instant::now();
    let ran = arena.run(&["pods", "sleep"], Duration::from_millis(300), true);
    assert!(ran.status.as_ref().is_err_and(|e| e.contains("timed out")), "{}", ran.describe());
    assert!(started.elapsed() < Duration::from_secs(10), "the deadline must hold: {:?}", started.elapsed());
    let _ = std::fs::remove_dir_all(&dir);
}
