//! Running `claude` or `codex` for `pods ask`: the AI gets the prompt on stdin and **no tools**,
//! in an empty temporary directory, within a time budget.
//!
//! Why no tools: this runs on the fleet's control box, next to the config and the keys, and the
//! bundle holds text participants control (the programs and scripts they run, file names, commit
//! subjects, job logs) — a prompt injection must have nothing to act with. So claude runs with `--tools ''`,
//! `--safe-mode`, `--strict-mcp-config` with an explicitly empty `--mcp-config`, no slash
//! commands and no saved session (verified with `scripts/ai-review` to list `tools: []`); and
//! codex read-only, ephemeral, without the operator's `config.toml` (its MCP servers, hooks,
//! profiles) and with `mcp_servers` emptied, web search off, every tool feature in
//! [`CODEX_TOOL_FEATURES`] disabled, and so every one of [`CODEX_OFF_WHEN_LISTED`] (and any web
//! search one) it has. Everything the model sees is in the prompt.
//! `scripts/ai-review` runs both exactly so — the same argv, check and environment; a test holds
//! its lists to these.
//!
//! **Fail closed.** Neither is sent anything until the CLI itself has confirmed it knows every
//! switch that takes its tools away — claude's `--help` lists each of [`CLAUDE_NO_TOOL_FLAGS`],
//! codex's `features list`, read strictly ([`parse_features`]), names each of
//! [`CODEX_TOOL_FEATURES`] ([`confirm_tools_off`]) — and,
//! for codex, that nothing else stays on: its `features list` run again with the very overrides
//! the ask gets may show on only what [`CODEX_SAFE_FEATURES`] allows ([`codex_still_on`]; review
//! ai-review claude #1: a feature it has under a name not listed here, or an override it ignored,
//! would have stayed on). A check that fails, times out or finds one missing or one more on
//! refuses the ask: a codex that doesn't know a feature can't be told to turn it off, and one run
//! without those switches could read files on this machine through its shell. Both get an empty
//! working directory and a minimal environment ([`child_env`]: not the operator's provider keys).
//!
//! The binaries are looked up on `PATH`, then in `~/.local/bin` (where both install), as
//! `ai-review` does; a missing one is an error that names the other option. The pure parts —
//! the argv, the checks, the environment, the config value, the lookup — are separate from the
//! spawns.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::Config;
use crate::remote::{run_local_input, LocalRun};

/// Which AI answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ai {
    Claude,
    Codex,
}

impl Ai {
    /// The CLI's name — also the binary's and `--ai`'s value.
    pub fn name(self) -> &'static str {
        match self {
            Ai::Claude => "claude",
            Ai::Codex => "codex",
        }
    }

    /// Who the prompt goes to.
    pub fn vendor(self) -> &'static str {
        match self {
            Ai::Claude => "Anthropic",
            Ai::Codex => "OpenAI",
        }
    }

    pub fn other(self) -> Ai {
        match self {
            Ai::Claude => Ai::Codex,
            Ai::Codex => Ai::Claude,
        }
    }

    /// `claude` / `codex` (any case, surrounding space ignored).
    pub fn parse(s: &str) -> Option<Ai> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" => Some(Ai::Claude),
            "codex" => Some(Ai::Codex),
            _ => None,
        }
    }
}

/// The config key choosing the AI, or turning `pods ask` off.
pub const ASK_AI: &str = "ASK_AI";

/// What `ASK_AI` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    /// `off`: `pods ask` (and the dashboard's `?`) refuse — nothing leaves the box, whatever
    /// `--ai` says.
    Off,
    /// `claude` (also the default, unset or empty) or `codex`.
    Use(Ai),
}

impl Setting {
    /// `ASK_AI` from `cfg`: unset or empty is claude. Anything but claude/codex/off is an error
    /// that quotes the value only when it is short and word-like — never a pasted key.
    pub fn from_config(cfg: &Config) -> Result<Setting, String> {
        let raw = cfg.get(ASK_AI).unwrap_or("").trim();
        if raw.is_empty() {
            return Ok(Setting::Use(Ai::Claude));
        }
        if raw.eq_ignore_ascii_case("off") {
            return Ok(Setting::Off);
        }
        Ai::parse(raw).map(Setting::Use).ok_or_else(|| {
            let shown = raw.len() <= 12 && raw.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
            if shown {
                format!("{ASK_AI} must be claude, codex or off (got {raw})")
            } else {
                format!("{ASK_AI} must be claude, codex or off")
            }
        })
    }

    /// Whether the config sets it at all (`config check`'s `(default)`).
    pub fn is_default(cfg: &Config) -> bool {
        cfg.get(ASK_AI).is_none_or(|v| v.trim().is_empty())
    }
}

/// Why `pods ask` won't run with `ASK_AI=off`, and how to turn it on.
pub fn off_message() -> String {
    format!(
        "asking an AI is off ({ASK_AI}=off in the config): it would send pods' state to Anthropic or OpenAI — set \
         {ASK_AI}=claude or {ASK_AI}=codex to turn it on (`arena config set {ASK_AI} claude`)"
    )
}

/// The AI to use: `--ai` (`flag`) over `ASK_AI`. `ASK_AI=off` refuses even with `--ai` — off is
/// the config owner's "nothing goes to an AI service from here" — and a malformed `ASK_AI`
/// refuses too (it might be a misspelt `off`).
pub fn choose(cfg: &Config, flag: Option<Ai>) -> Result<Ai, String> {
    match Setting::from_config(cfg)? {
        Setting::Off => Err(off_message()),
        Setting::Use(configured) => Ok(flag.unwrap_or(configured)),
    }
}

/// claude's MCP configuration for a run: no servers ([`claude_args`]'s `--mcp-config`).
pub const CLAUDE_NO_MCP: &str = r#"{"mcpServers":{}}"#;

/// The flags [`claude_args`] takes claude's tools away with — each must be in its `--help`
/// before it is asked anything ([`confirm_tools_off`]).
pub const CLAUDE_NO_TOOL_FLAGS: &[&str] = &["--tools", "--safe-mode", "--strict-mcp-config", "--mcp-config", "--disable-slash-commands", "--no-session-persistence"];

/// claude's argv: print mode, no tools of any kind (`--tools ''`, and `--safe-mode`: no hooks,
/// MCP servers, CLAUDE.md or skills; `--strict-mcp-config` with an empty `--mcp-config`: no MCP
/// servers from anywhere), no slash commands, no saved session, plain text out. The flags
/// `scripts/ai-review` runs it with too.
pub fn claude_args(model: Option<&str>) -> Vec<String> {
    let mut a: Vec<String> = [
        "-p",
        "--safe-mode",
        "--tools",
        "",
        "--strict-mcp-config",
        "--mcp-config",
        CLAUDE_NO_MCP,
        "--disable-slash-commands",
        "--no-session-persistence",
        "--output-format",
        "text",
    ]
    .map(String::from)
    .to_vec();
    if let Some(m) = model {
        a.extend(["--model".to_string(), m.to_string()]);
    }
    a
}

/// codex features that give it a tool, every one turned off (`scripts/ai-review` turns off the
/// same list), `view_image` included: it reads an image file anywhere on this machine. Its
/// `features list` must name each before it is asked anything ([`confirm_tools_off`]): a feature
/// it doesn't know can't be disabled (`--disable` of an unknown name is an error). One it has
/// under another name is caught by what may stay on ([`CODEX_SAFE_FEATURES`]).
pub const CODEX_TOOL_FEATURES: &[&str] = &[
    "shell_tool",
    "unified_exec",
    "code_mode_host",
    "apps",
    "plugins",
    "remote_plugin",
    "browser_use",
    "browser_use_external",
    "in_app_browser",
    "computer_use",
    "multi_agent",
    "image_generation",
    "hooks",
    "view_image",
];

/// codex features also turned off whenever its `features list` names one (none has to be there):
/// each gives the model a tool, keeps what it's told or makes files on this machine — a `sleep`
/// tool, `skill_search` (reads this machine's skills), `skill_mcp_dependency_install` (installs MCP
/// servers), `tool_suggest`, `goals` (a goal the model sets, kept), `in_app_local_automation`,
/// `worktrees` (git worktrees) — or does what no review here has pinned down
/// (`workspace_dependencies`). So is any feature with `web_search` in its name
/// ([`off_when_listed`]): `-c web_search="disabled"` alone is a key a codex that didn't know it
/// would ignore without a word.
pub const CODEX_OFF_WHEN_LISTED: &[&str] =
    &["sleep_tool", "skill_search", "skill_mcp_dependency_install", "tool_suggest", "goals", "in_app_local_automation", "workspace_dependencies", "worktrees"];

/// codex features that may stay on for an ask — reviewed against codex 0.163's `features list`,
/// run with the ask's own overrides: what it still shows on, each known to give the model no
/// tool. Any other feature it shows on, one unknown here included, refuses the ask
/// ([`codex_still_on`]).
pub const CODEX_SAFE_FEATURES: &[&str] = &[
    // Auth, models, the network and the wire; the CLI's own runtime (its app-server daemon, the
    // Daybreak model programme).
    "api_key_cyber_access_programs",
    "api_key_model_discovery",
    "auth_elicitation",
    "enable_request_compression",
    "system_proxy_fallback",
    "unbounded_connection_retries",
    "fast_mode",
    "ultrafast_mode",
    "cli_daybreak",
    "daemon_auto_start",
    // The conversation, and the interactive app's own UI.
    "compaction_image_budget",
    "content_item_kinds",
    "instant_interrupt",
    "mentions_v2",
    "realtime_conversation",
    "in_app_chat",
    "in_app_dictation",
    "in_app_updates",
    "in_app_voice",
    "guardian_approval",
    "guardian_reuse_parent_compaction",
    // Parts of a tool family that is off: the shell (`shell_tool`: unified exec is how it runs —
    // and codex 0.163 shows `unified_exec` on whatever `--disable` says), the browser, code mode,
    // plugins, MCP (no servers).
    "unified_exec",
    "unified_exec_tty",
    "write_stdin_approval",
    "shell_snapshot",
    "browser_annotation_api",
    "browser_use_full_cdp_access",
    "code_mode_interrupt",
    "plugin_sharing",
    "tool_call_mcp_elicitation",
    // Removed: the flag does nothing any more.
    "collaboration_modes",
    "item_ids",
    "resize_all_images",
    "sqlite",
    "steer",
    "terminal_resize_reflow",
    "tool_search_always_defer_mcp_tools",
    "tui_app_server",
];

/// Whether codex is to turn `name` off when its `features list` names it: one of
/// [`CODEX_OFF_WHEN_LISTED`], or any web search feature (`web_search_request`, …).
pub fn off_when_listed(name: &str) -> bool {
    CODEX_OFF_WHEN_LISTED.contains(&name) || name.contains("web_search")
}

/// What codex is run with `--disable` for: every one of [`CODEX_TOOL_FEATURES`], then each feature
/// `listed` ([`parse_features`]) names that [`off_when_listed`] takes, in its order.
pub fn codex_disabled(listed: &[(String, bool)]) -> Vec<String> {
    let mut d: Vec<String> = CODEX_TOOL_FEATURES.iter().map(|f| f.to_string()).collect();
    for (name, _) in listed {
        if off_when_listed(name) && !d.contains(name) {
            d.push(name.clone());
        }
    }
    d
}

/// The features `listed` shows on that [`CODEX_SAFE_FEATURES`] doesn't allow — unknown names
/// included. Empty: nothing on that could give it a tool.
pub fn codex_still_on(listed: &[(String, bool)]) -> Vec<String> {
    listed.iter().filter(|(name, on)| *on && !CODEX_SAFE_FEATURES.contains(&name.as_str())).map(|(name, _)| name.clone()).collect()
}

/// The overrides every codex run gets — its `features list` check's too, so the check sees what
/// the ask gets: web search off, the MCP servers emptied, and `--disable` for each of `disabled`
/// ([`codex_disabled`]).
pub fn codex_overrides(disabled: &[String]) -> Vec<String> {
    let mut a: Vec<String> = ["-c", "web_search=\"disabled\"", "-c", "mcp_servers={}"].map(String::from).to_vec();
    for f in disabled {
        a.extend(["--disable".to_string(), f.clone()]);
    }
    a
}

/// codex's argv: `exec` read-only and ephemeral, in `workdir` (an empty directory), its answer
/// written to `out`; without the operator's `config.toml` (`--ignore-user-config`: its MCP
/// servers, hooks, profiles, notify program) or execpolicy rules, and `mcp_servers` emptied
/// besides (a system or managed config layer is still read); web search off, `disabled`
/// ([`codex_disabled`]: every tool feature, always — [`ask`] runs it only once its `features list`
/// has named each and shown nothing else on) turned off — and the prompt on stdin (`-`).
pub fn codex_args(workdir: &Path, out: &Path, model: Option<&str>, disabled: &[String]) -> Vec<String> {
    let mut a: Vec<String> =
        ["exec", "-s", "read-only", "--skip-git-repo-check", "--ephemeral", "--ignore-user-config", "--ignore-rules", "--color", "never", "-C"]
            .map(String::from)
            .to_vec();
    a.push(workdir.display().to_string());
    a.extend(["-o".to_string(), out.display().to_string()]);
    a.extend(codex_overrides(disabled));
    if let Some(m) = model {
        a.extend(["-m".to_string(), m.to_string()]);
    }
    a.push("-".to_string());
    a
}

/// The stages `codex features list` prints between a feature's name and its state (codex 0.163).
pub const CODEX_STAGES: &[&str] = &["stable", "experimental", "deprecated", "removed", "under development"];

/// `codex features list`'s rows, read strictly (review ai-review codex #3, claude #1): every
/// non-blank line `<name> <stage> true|false` — a name of `[a-z0-9_.-]`, one of [`CODEX_STAGES`],
/// the state exactly `true` or `false` — no name twice, and something listed. Anything else is
/// `Err`, naming the row and what's wrong with it: reading `enabled`, a header or a column more as
/// "off" was how a changed format could pass a new tool as off, and a listing that can't be read
/// whole proves nothing about what is on.
pub fn parse_features(stdout: &str) -> Result<Vec<(String, bool)>, String> {
    let mut rows: Vec<(String, bool)> = Vec::new();
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        let words: Vec<&str> = line.split_whitespace().collect();
        let bad = |why: String| format!("has a row that can't be read: `{}` — {why}", crate::fleet::clip(&crate::jobs::sanitize(&words.join(" ")), 80));
        let [name, stage @ .., state] = words.as_slice() else { return Err(bad("it isn't `<name> <stage> true|false`".into())) };
        if stage.is_empty() {
            return Err(bad("it isn't `<name> <stage> true|false`".into()));
        }
        let on = match *state {
            "true" => true,
            "false" => false,
            other => return Err(bad(format!("its state `{other}` isn't true or false"))),
        };
        let stage = stage.join(" ");
        if !CODEX_STAGES.contains(&stage.as_str()) {
            return Err(bad(format!("`{stage}` isn't a stage it's known to print")));
        }
        if !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b)) {
            return Err(bad(format!("`{name}` isn't a feature's name")));
        }
        if rows.iter().any(|(n, _)| n == name) {
            return Err(format!("names {name} twice"));
        }
        rows.push((name.to_string(), on));
    }
    if rows.is_empty() {
        return Err("listed nothing".into());
    }
    Ok(rows)
}

/// A `codex features list` that reads whole ([`parse_features`]) and names every one of
/// `required` — its rows, else why not. Both of the check's listings are held to it: the first
/// to [`CODEX_TOOL_FEATURES`], the one with the ask's overrides to every feature it disables.
pub fn codex_listing<S: AsRef<str>>(stdout: &str, required: &[S]) -> Result<Vec<(String, bool)>, String> {
    let rows = parse_features(stdout)?;
    let missing: Vec<&str> = required.iter().map(AsRef::as_ref).filter(|f| !rows.iter().any(|(n, _)| n == f)).collect();
    if missing.is_empty() {
        Ok(rows)
    } else {
        Err(format!("doesn't name {}", missing.join(", ")))
    }
}

/// How `ai` describes itself before it is asked anything: `claude --help`, `codex features list`.
pub fn discovery_args(ai: Ai) -> Vec<String> {
    match ai {
        Ai::Claude => vec!["--help".to_string()],
        Ai::Codex => vec!["features".to_string(), "list".to_string()],
    }
}

/// Whether `ai`'s self-description ([`discovery_args`]'s output) shows it knows every switch
/// its argv takes its tools away with: claude's `--help` lists each of [`CLAUDE_NO_TOOL_FLAGS`]
/// (as a word: `--tools,` or `--tools <tools...>`), codex's `features list` reads whole and names
/// each of [`CODEX_TOOL_FEATURES`] ([`codex_listing`]). `Err`: what it doesn't (`doesn't list
/// --tools, --mcp-config`), or why its listing can't be read.
pub fn confirm_tools_off(ai: Ai, described: &str) -> Result<(), String> {
    match ai {
        Ai::Claude => {
            let words: Vec<&str> = described.split(|c: char| c.is_whitespace() || c == ',').collect();
            let missing: Vec<&str> = CLAUDE_NO_TOOL_FLAGS.iter().copied().filter(|f| !words.contains(f)).collect();
            if missing.is_empty() {
                Ok(())
            } else {
                Err(format!("doesn't list {}", missing.join(", ")))
            }
        }
        Ai::Codex => codex_listing(described, CODEX_TOOL_FEATURES).map(|_| ()),
    }
}

/// What every AI process may keep of this one's environment, besides `PATH` and `HOME` (which
/// [`AiEnv`] carries): the locale, the temp dir, the XDG dirs, proxies and CA bundles. Nothing
/// else — not the operator's provider keys, not a variable that could turn a tool back on.
pub const COMMON_VARS: &[&str] = &[
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    "TZ",
    "TMPDIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "XDG_RUNTIME_DIR",
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
];

/// What `ai`'s own process also keeps: where its login lives and its own vendor's API key.
pub fn own_vars(ai: Ai) -> &'static [&'static str] {
    match ai {
        Ai::Claude => &["CLAUDE_CONFIG_DIR", "ANTHROPIC_API_KEY"],
        Ai::Codex => &["CODEX_HOME", "OPENAI_API_KEY", "CODEX_API_KEY"],
    }
}

/// `ai`'s whole environment: `PATH` and `HOME` from `env`, then those of `env.vars` that
/// [`COMMON_VARS`] or [`own_vars`] name.
pub fn child_env(ai: Ai, env: &AiEnv) -> Vec<(OsString, OsString)> {
    let mut v: Vec<(OsString, OsString)> = Vec::new();
    v.extend(env.path.clone().map(|p| (OsString::from("PATH"), p)));
    v.extend(env.home.clone().map(|h| (OsString::from("HOME"), h.into_os_string())));
    let keep = |k: &str| COMMON_VARS.contains(&k) || own_vars(ai).contains(&k);
    v.extend(env.vars.iter().filter(|(k, _)| keep(k)).map(|(k, val)| (OsString::from(k), val.clone())));
    v
}

/// Where the AI's binary is looked for, where its scratch directory goes, and what of this
/// process's environment its process gets ([`child_env`]).
#[derive(Debug, Clone)]
pub struct AiEnv {
    /// `PATH` (also the child's).
    pub path: Option<OsString>,
    /// `HOME`, for `~/.local/bin` (also the child's: its login is under it).
    pub home: Option<PathBuf>,
    /// Where the empty working directory is made.
    pub tmp: PathBuf,
    /// Variables the child may keep, from this process — only those [`child_env`] names pass.
    pub vars: Vec<(String, OsString)>,
}

impl AiEnv {
    /// This process's `PATH`, `HOME`, temp dir, and the variables [`COMMON_VARS`] and either
    /// AI's [`own_vars`] name.
    pub fn from_process() -> AiEnv {
        let names = COMMON_VARS.iter().chain(own_vars(Ai::Claude)).chain(own_vars(Ai::Codex));
        AiEnv {
            path: std::env::var_os("PATH"),
            home: std::env::var_os("HOME").map(PathBuf::from),
            tmp: std::env::temp_dir(),
            vars: names.filter_map(|k| std::env::var_os(k).map(|v| (k.to_string(), v))).collect(),
        }
    }
}

fn executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// `name`'s binary: the first executable `name` on `env.path`, else `~/.local/bin/name`.
pub fn find_bin(name: &str, env: &AiEnv) -> Option<PathBuf> {
    let on_path = env.path.as_ref().into_iter().flat_map(std::env::split_paths).filter(|d| !d.as_os_str().is_empty()).map(|d| d.join(name));
    let local = env.home.as_ref().map(|h| h.join(".local").join("bin").join(name));
    on_path.chain(local).find(|p| executable(p))
}

/// The error for a missing binary: what to install, or the other AI to use.
pub fn missing(ai: Ai) -> String {
    let other = ai.other();
    format!(
        "{} not found on PATH or in ~/.local/bin — install it and log in, or use {} instead (`--ai {}`, or {ASK_AI}={})",
        ai.name(),
        other.name(),
        other.name(),
        other.name()
    )
}

/// A private scratch directory (0700), removed when dropped: the AI's empty working directory
/// is `empty/` inside it, codex's answer file beside that, and the empty `CODEX_HOME` its
/// `features list` check runs with (`codex-home/`).
struct Scratch(PathBuf);

impl Scratch {
    fn new(tmp: &Path) -> std::io::Result<Scratch> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let dir = tmp.join(format!("arena-ask-{}-{nanos}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let mut b = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            b.mode(0o700);
        }
        b.create(&dir)?;
        let s = Scratch(dir);
        b.create(s.empty())?;
        b.create(s.codex_home())?;
        Ok(s)
    }

    fn empty(&self) -> PathBuf {
        self.0.join("empty")
    }

    fn codex_home(&self) -> PathBuf {
        self.0.join("codex-home")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// How long `claude --help` / `codex features list` may take (at most the ask's own budget).
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(20);

/// The last non-empty stderr line, made printable and short — what a failed run says, printed
/// and saved in `answer.md` — and masked as pod text is ([`super::redact::text`]): an auth error
/// can echo the operator's own API key (review ai-review claude #7).
fn last_line(stderr: &str) -> String {
    let l = stderr.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("(no output)");
    super::redact::text(l, 200)
}

/// Text from the AI, safe for the operator's terminal: escape sequences and control characters
/// out, line breaks kept, trailing space trimmed.
pub fn printable(answer: &str) -> String {
    answer.lines().map(|l| crate::jobs::sanitize(l).trim_end().to_string()).collect::<Vec<_>>().join("\n").trim().to_string()
}

/// One run of `ai`'s self-description with `extra` arguments after [`discovery_args`]: its stdout,
/// or why there is none (for the refusal).
async fn describe(ai: Ai, bin: &Path, extra: &[String], cwd: &Path, vars: &[(OsString, OsString)], budget: Duration) -> Result<String, String> {
    let args: Vec<String> = discovery_args(ai).into_iter().chain(extra.iter().cloned()).collect();
    let cmd = format!("{} {}", ai.name(), discovery_args(ai).join(" "));
    let run = LocalRun { program: bin, args: &args, cwd, env: vars, input: b"" };
    match run_local_input(&run, &cmd, Some(budget)).await {
        Ok(out) if out.success => Ok(out.stdout),
        Ok(out) => Err(format!("`{cmd}` failed ({}): {}", out.exit_label(), last_line(&out.stderr))),
        Err(crate::Error::Timeout { after, .. }) => Err(format!("`{cmd}` didn't answer within {}", crate::error::human_duration(&after))),
        Err(e) => Err(format!("`{cmd}`: {e}")),
    }
}

/// Hold `ai` to its own description before it is sent anything, each run within `budget`:
/// claude's `--help` must list every one of [`CLAUDE_NO_TOOL_FLAGS`]; codex's `features list` must
/// read whole and name every one of [`CODEX_TOOL_FEATURES`] ([`confirm_tools_off`]), then — run
/// again with the overrides the ask will get ([`codex_overrides`] of [`codex_disabled`]) — read
/// whole, name every feature it disables ([`codex_listing`]) and show nothing on that
/// [`CODEX_SAFE_FEATURES`] doesn't allow ([`codex_still_on`]). codex's checks run with an empty
/// `CODEX_HOME` of their own (`codex_home`): `features list` has no `--ignore-user-config`, and the
/// operator's `config.toml` that the ask doesn't read must not show a feature off there that the
/// ask then has on. `Ok`: what codex is to run with `--disable` for (nothing for claude).
/// Otherwise why not, as the refusal `ask` returns — nothing has been sent.
async fn check_tools_off(ai: Ai, bin: &Path, cwd: &Path, vars: &[(OsString, OsString)], codex_home: &Path, budget: Duration) -> Result<Vec<String>, String> {
    let refuse = |why: String| format!("{}: couldn't confirm its tools are off — not asked ({why})", ai.name());
    let cmd = format!("{} {}", ai.name(), discovery_args(ai).join(" "));
    match ai {
        Ai::Claude => {
            let help = describe(ai, bin, &[], cwd, vars, budget).await.map_err(refuse)?;
            confirm_tools_off(ai, &help).map_err(|why| refuse(format!("`{cmd}` {why}")))?;
            Ok(Vec::new())
        }
        Ai::Codex => {
            let home = (OsString::from("CODEX_HOME"), codex_home.as_os_str().to_os_string());
            let vars: Vec<(OsString, OsString)> = vars.iter().filter(|(k, _)| k != "CODEX_HOME").cloned().chain([home]).collect();
            let listed = describe(ai, bin, &codex_overrides(&[]), cwd, &vars, budget).await.map_err(refuse)?;
            let rows = codex_listing(&listed, CODEX_TOOL_FEATURES).map_err(|why| refuse(format!("`{cmd}` {why}")))?;
            let disabled = codex_disabled(&rows);
            let after = describe(ai, bin, &codex_overrides(&disabled), cwd, &vars, budget).await.map_err(refuse)?;
            let rows = codex_listing(&after, &disabled).map_err(|why| refuse(format!("`{cmd}` with the ask's overrides {why}")))?;
            match codex_still_on(&rows).as_slice() {
                [] => Ok(disabled),
                on => Err(refuse(format!(
                    "`{cmd}` shows {} on with the ask's overrides — not known to give it no tool (arena_core::ask::ai::CODEX_SAFE_FEATURES)",
                    on.join(", ")
                ))),
            }
        }
    }
}

/// Ask `ai` `prompt` (on stdin) with `model` (or the CLI's own default), within `timeout`; the
/// answer, made printable, or why there is none — a missing binary, a CLI that couldn't confirm
/// its tools are off (it is then never sent the prompt), a timeout (the AI is stopped: SIGTERM,
/// then SIGKILL), a failed run, an empty answer.
pub async fn ask(ai: Ai, prompt: &str, model: Option<&str>, timeout: Duration, env: &AiEnv) -> Result<String, String> {
    let bin = find_bin(ai.name(), env).ok_or_else(|| missing(ai))?;
    let scratch = Scratch::new(&env.tmp).map_err(|e| format!("can't make a scratch directory in {}: {e}", env.tmp.display()))?;
    let empty = scratch.empty();
    let answer_file = scratch.0.join("answer.md");
    let vars = child_env(ai, env);
    let disabled = check_tools_off(ai, &bin, &empty, &vars, &scratch.codex_home(), timeout.min(DISCOVERY_TIMEOUT)).await?;
    let args = match ai {
        Ai::Claude => claude_args(model),
        Ai::Codex => codex_args(&empty, &answer_file, model, &disabled),
    };
    let run = LocalRun { program: &bin, args: &args, cwd: &empty, env: &vars, input: prompt.as_bytes() };
    let out = match run_local_input(&run, ai.name(), Some(timeout)).await {
        Ok(out) => out,
        Err(crate::Error::Timeout { after, .. }) => {
            return Err(format!(
                "{} didn't answer within {} — stopped (a longer --timeout, or ask about fewer pods)",
                ai.name(),
                crate::error::human_duration(&after)
            ))
        }
        Err(e) => return Err(format!("{}: {e}", ai.name())),
    };
    if !out.success {
        return Err(format!("{} failed ({}): {}", ai.name(), out.exit_label(), last_line(&out.stderr)));
    }
    let text = match ai {
        Ai::Claude => out.stdout,
        // codex writes its last message to `-o`; its stdout repeats it with other chatter.
        Ai::Codex => std::fs::read_to_string(&answer_file).ok().filter(|t| !t.trim().is_empty()).unwrap_or(out.stdout),
    };
    let text = printable(&text);
    if text.is_empty() {
        return Err(format!("{} answered nothing ({})", ai.name(), last_line(&out.stderr)));
    }
    Ok(text)
}

/// Stub `claude`/`codex` binaries for tests here and in the CLI.
#[cfg(any(test, feature = "test-util"))]
pub mod fixtures {
    use super::*;

    /// An executable `name` in `dir` running `body` under sh — written by
    /// [`crate::remote::write_test_script`], never held open here, so another test thread's fork
    /// can't make it "Text file busy" when it runs.
    #[cfg(unix)]
    pub fn stub(dir: &Path, name: &str, body: &str) {
        crate::remote::write_test_script(dir.join(name), format!("#!/bin/sh\n{body}\n")).unwrap();
    }

    /// What a stub `claude --help` prints: an option table listing every flag in
    /// [`CLAUDE_NO_TOOL_FLAGS`] the way claude's own does (`--tools <tools...>`, an alias before
    /// a comma), less `without`.
    pub fn claude_help(without: &[&str]) -> String {
        let mut h = String::from("Usage: claude [options] [command] [prompt]\n\nOptions:\n  --allowedTools, --allowed-tools <tools...>  allow\n");
        for f in CLAUDE_NO_TOOL_FLAGS.iter().filter(|f| !without.contains(f)) {
            h.push_str(&format!("  {f} <x>   what it does\n"));
        }
        h
    }

    /// A stub codex's features, in its order: every one of [`CODEX_TOOL_FEATURES`], a web search
    /// one, two of [`CODEX_OFF_WHEN_LISTED`] and two of [`CODEX_SAFE_FEATURES`] — less `without`,
    /// plus `extra`.
    pub fn codex_feature_names(without: &[&str], extra: &[&str]) -> Vec<String> {
        let names = CODEX_TOOL_FEATURES.iter().chain(&["web_search_request", "goals", "sleep_tool", "fast_mode", "steer"]).chain(extra);
        names.filter(|f| !without.contains(f)).map(|f| f.to_string()).collect()
    }

    /// What a stub `codex features list` prints, all on: [`codex_feature_names`] less `without`,
    /// as `name  stage  enabled` rows.
    pub fn codex_features(without: &[&str]) -> String {
        codex_feature_names(without, &[]).iter().map(|f| format!("{f:<40} stable             true\n")).collect()
    }

    /// A stub codex's `features …` answer, as sh: [`codex_feature_names`] (less `without`, plus
    /// `extra`) as `name  stage  enabled` rows, each on unless the call `--disable`s it — less
    /// `ignored`, which stay on whatever it says, as `unified_exec` does in codex 0.163. Each call's
    /// arguments go to `<rec>/codex.features.argv` and its `CODEX_HOME` to `…features.home`, when
    /// `rec` is given.
    pub fn codex_features_sh(without: &[&str], extra: &[&str], ignored: &[&str], rec: Option<&Path>) -> String {
        let names = codex_feature_names(without, extra).join(" ");
        let ignored = ignored.join(" ");
        let record = rec.map_or(String::new(), |r| {
            format!("printf '%s\\n' \"$*\" >> {0}/codex.features.argv\nprintf '%s\\n' \"${{CODEX_HOME:-}}\" >> {0}/codex.features.home\n", r.display())
        });
        format!(
            "{record}off=' '; prev=; for a; do [ \"$prev\" = --disable ] && off=\"$off$a \"; prev=$a; done\n\
             for f in {names}; do\n\
             case \" {ignored} \" in *\" $f \"*) s=true ;; *) case \"$off\" in *\" $f \"*) s=false ;; *) s=true ;; esac ;; esac\n\
             printf '%-40s stable             %s\\n' \"$f\" \"$s\"\n\
             done"
        )
    }

    /// Stub `claude` and `codex` in `<tmp>/bin` that record their argv (one per line), stdin,
    /// working directory and environment under `<tmp>/rec/<name>.*` and answer `ANSWER from
    /// <name>` — codex into its `-o` file, as the real one does. `claude --help` lists the no-tools
    /// flags ([`claude_help`]) and `codex features list` its features as codex does
    /// ([`codex_features_sh`]: each call's arguments and `CODEX_HOME` recorded).
    #[cfg(unix)]
    pub fn recording_stubs(tmp: &Path) -> AiEnv {
        let (bin, rec) = (tmp.join("bin"), tmp.join("rec"));
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&rec).unwrap();
        let features = codex_features_sh(&[], &[], &["unified_exec"], Some(&rec));
        let rec = rec.display();
        stub(
            &bin,
            "claude",
            &format!(
                "if [ \"$1\" = --help ]; then cat <<'EOF'\n{}EOF\nexit 0; fi\n\
                 printf '%s\\n' \"$@\" > {rec}/claude.argv\ncat > {rec}/claude.stdin\npwd > {rec}/claude.cwd\nls -A > {rec}/claude.ls\n\
                 env > {rec}/claude.env\necho 'ANSWER from claude'",
                claude_help(&[])
            ),
        );
        stub(
            &bin,
            "codex",
            &format!(
                "if [ \"$1\" = features ]; then\n{}\nexit 0; fi\n\
                 printf '%s\\n' \"$@\" > {rec}/codex.argv\ncat > {rec}/codex.stdin\npwd > {rec}/codex.cwd\nenv > {rec}/codex.env\n\
                 out=; prev=; for a; do [ \"$prev\" = -o ] && out=$a; prev=$a; done\n\
                 echo 'ANSWER from codex' > \"$out\"\necho 'codex chatter on stdout'",
                features
            ),
        );
        AiEnv { path: Some(on_path(&bin)), home: None, tmp: tmp.to_path_buf(), vars: vec![("LANG".into(), "C.UTF-8".into())] }
    }

    /// `PATH` with `dir` first: the stubs found before any real `claude`/`codex`, the tools
    /// they run (`cat`, `sleep`) still there.
    #[cfg(unix)]
    pub fn on_path(dir: &Path) -> OsString {
        let rest = std::env::var_os("PATH").unwrap_or_default();
        std::env::join_paths(std::iter::once(dir.to_path_buf()).chain(std::env::split_paths(&rest))).unwrap()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use super::fixtures::*;

    /// A temp dir for one test, removed after.
    pub(crate) struct Tmp(pub PathBuf);
    impl Tmp {
        pub(crate) fn new(tag: &str) -> Tmp {
            let d = std::env::temp_dir().join(format!("arena-ask-test-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Tmp(d)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn rec(tmp: &Path, f: &str) -> String {
        std::fs::read_to_string(tmp.join("rec").join(f)).unwrap_or_default()
    }

    #[test]
    fn ask_ai_config_values() {
        let s = |text: &str| Setting::from_config(&Config::parse(text));
        assert_eq!(s(""), Ok(Setting::Use(Ai::Claude)));
        assert_eq!(s("ASK_AI="), Ok(Setting::Use(Ai::Claude)));
        assert_eq!(s("ASK_AI=Codex"), Ok(Setting::Use(Ai::Codex)));
        assert_eq!(s("ASK_AI=\" claude \""), Ok(Setting::Use(Ai::Claude)));
        assert_eq!(s("ASK_AI=OFF"), Ok(Setting::Off));
        assert_eq!(s("ASK_AI=gpt").unwrap_err(), "ASK_AI must be claude, codex or off (got gpt)");
        // A pasted key is never echoed.
        let e = s("ASK_AI=sk-0123456789abcdefghij").unwrap_err();
        assert_eq!(e, "ASK_AI must be claude, codex or off");
        assert!(Setting::is_default(&Config::parse("")) && !Setting::is_default(&Config::parse("ASK_AI=codex")));
        // --ai picks; off refuses whatever --ai says; a bad value refuses too.
        let c = |text: &str, flag| choose(&Config::parse(text), flag);
        assert_eq!(c("", None), Ok(Ai::Claude));
        assert_eq!(c("ASK_AI=codex", None), Ok(Ai::Codex));
        assert_eq!(c("ASK_AI=codex", Some(Ai::Claude)), Ok(Ai::Claude));
        assert!(c("ASK_AI=off", Some(Ai::Claude)).unwrap_err().contains("ASK_AI=off"));
        assert!(c("ASK_AI=of", Some(Ai::Claude)).is_err());
    }

    /// The exact argv: claude with no built-in tools and no MCP server from anywhere (an empty
    /// `--mcp-config` under `--strict-mcp-config`); codex read-only, without the operator's
    /// config.toml and with `mcp_servers` emptied, every tool feature disabled — all of them,
    /// always — and the prompt on stdin.
    #[test]
    fn argv_have_no_tools() {
        let words = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            claude_args(Some("opus")),
            words(&[
                "-p",
                "--safe-mode",
                "--tools",
                "",
                "--strict-mcp-config",
                "--mcp-config",
                r#"{"mcpServers":{}}"#,
                "--disable-slash-commands",
                "--no-session-persistence",
                "--output-format",
                "text",
                "--model",
                "opus"
            ])
        );
        assert!(!claude_args(None).contains(&"--model".to_string()));
        // Every flag claude's --help must list is one the argv uses.
        for f in CLAUDE_NO_TOOL_FLAGS {
            assert!(claude_args(None).iter().any(|a| a == f), "{f}");
        }

        let mut want = words(&[
            "exec",
            "-s",
            "read-only",
            "--skip-git-repo-check",
            "--ephemeral",
            "--ignore-user-config",
            "--ignore-rules",
            "--color",
            "never",
            "-C",
            "/t/empty",
            "-o",
            "/t/a.md",
            "-c",
            "web_search=\"disabled\"",
            "-c",
            "mcp_servers={}",
        ]);
        for f in [
            "shell_tool",
            "unified_exec",
            "code_mode_host",
            "apps",
            "plugins",
            "remote_plugin",
            "browser_use",
            "browser_use_external",
            "in_app_browser",
            "computer_use",
            "multi_agent",
            "image_generation",
            "hooks",
            "view_image",
        ] {
            want.extend(words(&["--disable", f]));
        }
        want.extend(words(&["--disable", "web_search_request", "-m", "gpt-x", "-"]));
        let disabled: Vec<String> = CODEX_TOOL_FEATURES.iter().chain(&["web_search_request"]).map(|f| f.to_string()).collect();
        assert_eq!(codex_args(Path::new("/t/empty"), Path::new("/t/a.md"), Some("gpt-x"), &disabled), want);
        assert_eq!(codex_args(Path::new("/t/empty"), Path::new("/t/a.md"), None, &disabled).last().map(String::as_str), Some("-"), "the prompt on stdin");
        let rows = |v: &[(&str, bool)]| v.iter().map(|(n, on)| (n.to_string(), *on)).collect::<Vec<_>>();
        assert_eq!(parse_features("shell_tool stable true\n\napps experimental false\nin_app_voice under development  true\n"), Ok(rows(&[("shell_tool", true), ("apps", false), ("in_app_voice", true)])));
    }

    /// The check before anything is sent: claude's --help must list every no-tools flag (as a
    /// word — `--tools-extra` or `--mcp-config-file` isn't `--tools`/`--mcp-config`), codex's
    /// features list must name every tool feature; what's missing is named.
    #[test]
    fn tools_off_is_confirmed_by_the_clis_own_description() {
        assert_eq!(confirm_tools_off(Ai::Claude, &fixtures::claude_help(&[])), Ok(()));
        assert_eq!(confirm_tools_off(Ai::Claude, &fixtures::claude_help(&["--strict-mcp-config", "--tools"])), Err("doesn't list --tools, --strict-mcp-config".into()));
        let lookalikes = "  --tools-extra <x>\n  --safe-mode\n  --strict-mcp-config\n  --mcp-config-file\n  --disable-slash-commands\n  --no-session-persistence\n";
        assert_eq!(confirm_tools_off(Ai::Claude, lookalikes), Err("doesn't list --tools, --mcp-config".into()));
        assert_eq!(confirm_tools_off(Ai::Claude, ""), Err(format!("doesn't list {}", CLAUDE_NO_TOOL_FLAGS.join(", "))));
        assert_eq!(confirm_tools_off(Ai::Codex, &fixtures::codex_features(&[])), Ok(()));
        assert_eq!(confirm_tools_off(Ai::Codex, &fixtures::codex_features(&["hooks", "view_image"])), Err("doesn't name hooks, view_image".into()));
        // A name inside another line's text isn't a feature it lists: that line can't be read.
        assert!(confirm_tools_off(Ai::Codex, "web_search  stable  true  (not shell_tool)\n").unwrap_err().starts_with("has a row that can't be read"));
        assert_eq!(discovery_args(Ai::Claude), ["--help"]);
        assert_eq!(discovery_args(Ai::Codex), ["features", "list"]);
    }

    /// The AI's environment: `PATH`, `HOME`, the locale/XDG/proxy variables, and its own login
    /// dir and API key — never the other AI's key, the operator's provider keys or anything else.
    #[test]
    fn the_ai_gets_a_minimal_environment() {
        let env = AiEnv {
            path: Some("/bin".into()),
            home: Some("/home/op".into()),
            tmp: "/tmp".into(),
            vars: [("LANG", "C.UTF-8"), ("HTTPS_PROXY", "http://p:3128"), ("ANTHROPIC_API_KEY", "a"), ("OPENAI_API_KEY", "o"), ("CODEX_HOME", "/c"), ("RUNPOD_API_KEY", "r"), ("CLAUDE_CODE_ENTRYPOINT", "x")]
                .map(|(k, v)| (k.to_string(), OsString::from(v)))
                .to_vec(),
        };
        let names = |ai| child_env(ai, &env).into_iter().map(|(k, _)| k.into_string().unwrap()).collect::<Vec<_>>();
        assert_eq!(names(Ai::Claude), ["PATH", "HOME", "LANG", "HTTPS_PROXY", "ANTHROPIC_API_KEY"]);
        assert_eq!(names(Ai::Codex), ["PATH", "HOME", "LANG", "HTTPS_PROXY", "OPENAI_API_KEY", "CODEX_HOME"]);
        assert_eq!(child_env(Ai::Claude, &env)[1], (OsString::from("HOME"), OsString::from("/home/op")));
        let bare = AiEnv { path: None, home: None, tmp: "/tmp".into(), vars: Vec::new() };
        assert!(child_env(Ai::Codex, &bare).is_empty());
    }

    /// `scripts/ai-review` runs codex and claude as `ask` does — it had the same fail-open codex
    /// check and no MCP override (lane ASK3's note) — so the two can't drift apart: the same tool
    /// features, features turned off when listed, features that may stay on, no-tools flags, MCP
    /// config and environment lists, the same argv and the same codex check (the overrides, an
    /// empty `CODEX_HOME`), and both its check and its review started under `env -i`. (Its runs themselves are tested with stub
    /// CLIs; this holds the text.)
    #[test]
    fn ai_review_runs_claude_and_codex_as_ask_does() {
        let script = include_str!("../../../../scripts/ai-review");
        // A bash array's words, a `'…'` one word (a stage of two).
        let array = |name: &str| -> Vec<String> {
            let start = script.find(&format!("\n{name}=(")).unwrap_or_else(|| panic!("{name} in ai-review")) + name.len() + 3;
            let mut rest = script[start..start + script[start..].find(')').unwrap()].trim_start();
            let mut words = Vec::new();
            while !rest.is_empty() {
                let (w, r) = match rest.strip_prefix('\'') {
                    Some(quoted) => quoted.split_once('\'').unwrap(),
                    None => rest.split_once(char::is_whitespace).unwrap_or((rest, "")),
                };
                words.push(w.to_string());
                rest = r.trim_start();
            }
            words
        };
        assert_eq!(array("CODEX_TOOL_FEATURES"), CODEX_TOOL_FEATURES);
        assert_eq!(array("CODEX_OFF_WHEN_LISTED"), CODEX_OFF_WHEN_LISTED);
        assert_eq!(array("CODEX_SAFE_FEATURES"), CODEX_SAFE_FEATURES);
        assert_eq!(array("CODEX_STAGES"), CODEX_STAGES);
        assert_eq!(array("CLAUDE_NO_TOOL_FLAGS"), CLAUDE_NO_TOOL_FLAGS);
        assert_eq!(array("COMMON_VARS"), COMMON_VARS);
        assert_eq!(array("CLAUDE_VARS"), own_vars(Ai::Claude));
        assert_eq!(array("CODEX_VARS"), own_vars(Ai::Codex));
        assert!(script.contains(&format!("\nCLAUDE_NO_MCP='{CLAUDE_NO_MCP}'\n")));

        // The argv as the script spells it: continuation lines joined, `"$CLAUDE_NO_MCP"` its value.
        let text = script.replace("\\\n", " ").replace("\"$CLAUDE_NO_MCP\"", &format!("'{CLAUDE_NO_MCP}'"));
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let sh = |w: &String| match w.as_str() {
            "" => "''".to_string(),
            w if w.starts_with('$') => format!("\"{w}\""),
            w if w.contains(['"', '{']) => format!("'{w}'"),
            w => w.to_string(),
        };
        let printf = |words: &[String]| format!("printf '%s\\0' {}", words.iter().map(sh).collect::<Vec<_>>().join(" "));
        assert!(text.contains(&printf(&claude_args(None))), "claude's argv");
        let tools: Vec<String> = CODEX_TOOL_FEATURES.iter().map(|f| f.to_string()).collect();
        let codex = codex_args(Path::new("$WORK/empty"), Path::new("$DIR/codex.md"), None, &tools);
        let first_disable = codex.iter().position(|a| a == "--disable").unwrap();
        assert!(text.contains(&printf(&codex[..first_disable])), "codex's argv up to the features");
        assert!(text.contains(r#"mapfile -t disable <"$WORK/codex.disable""#), "the features its check chose");
        assert!(text.contains(r#"for f in "${disable[@]}"; do printf '%s\0' --disable "$f"; done"#), "each of them");
        assert!(text.contains(r#"printf '%s\n' "${CODEX_TOOL_FEATURES[@]}""#), "every tool feature, always");
        assert!(text.contains(r#"case $f in *web_search*) echo "$f" ;; esac"#), "web search, as off_when_listed");
        assert!(text.contains(r#"args=(features list -c 'web_search="disabled"' -c 'mcp_servers={}')"#), "its check gets the overrides");
        // Both listings read strictly, as `parse_features` and `codex_listing` read them.
        assert!(text.contains(r#"why=$(features_ok "$WORK/codex.check" "${CODEX_TOOL_FEATURES[@]}")"#), "the first, every tool feature named");
        assert!(text.contains(r#"why=$(features_ok "$WORK/codex.check" "${disable[@]}")"#), "the second, every disabled one named");
        for rule in [r#"if ($NF != "true" && $NF != "false")"#, r#"if (index(stages, "|" stage "|") == 0)"#, r#"if ($1 in seen)"#, r#"print "listed nothing""#] {
            assert!(text.contains(rule), "{rule}");
        }
        assert!(text.contains(r#"envs+=("CODEX_HOME=$WORK/codex-home")"#), "its check: an empty CODEX_HOME");
        assert_eq!(text.matches(r#"exec timeout -k "#).count(), 2, "the check and the review");
        assert_eq!(text.matches(r#"env -i "${envs[@]}" "$bin" "${args[@]}""#).count(), 2, "both under env -i");
    }

    #[cfg(unix)]
    #[test]
    fn find_bin_looks_on_path_then_in_local_bin() {
        let t = Tmp::new("find");
        let (a, b, home) = (t.0.join("a"), t.0.join("b"), t.0.join("home"));
        for d in [&a, &b, &home.join(".local/bin")] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(a.join("claude"), "not executable").unwrap();
        stub(&b, "claude", "true");
        stub(&home.join(".local/bin"), "codex", "true");
        let env = AiEnv { path: Some(std::env::join_paths([&a, &b]).unwrap()), home: Some(home.clone()), tmp: t.0.clone(), vars: Vec::new() };
        assert_eq!(find_bin("claude", &env), Some(b.join("claude")), "the first *executable* one");
        assert_eq!(find_bin("codex", &env), Some(home.join(".local/bin/codex")));
        assert_eq!(find_bin("nope", &env), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn claude_gets_the_prompt_on_stdin_and_no_tools_in_an_empty_dir() {
        let t = Tmp::new("claude");
        let env = recording_stubs(&t.0);
        let answer = ask(Ai::Claude, "the PROMPT\nline 2\n", Some("sonnet"), Duration::from_secs(20), &env).await.unwrap();
        assert_eq!(answer, "ANSWER from claude");
        let argv: Vec<String> = rec(&t.0, "claude.argv").lines().map(String::from).collect();
        assert_eq!(argv, claude_args(Some("sonnet")), "exactly the no-tools flags");
        assert_eq!(rec(&t.0, "claude.stdin"), "the PROMPT\nline 2\n", "the prompt on stdin, never argv");
        assert!(rec(&t.0, "claude.cwd").trim().ends_with("/empty"), "{}", rec(&t.0, "claude.cwd"));
        assert_eq!(rec(&t.0, "claude.ls"), "", "nothing to read where it runs");
        // Its environment is only what child_env gives: none of this test process's own
        // (cargo's CARGO_*, the build VM's HOME and USER).
        let vars = rec(&t.0, "claude.env");
        assert!(vars.lines().any(|l| l == "LANG=C.UTF-8") && vars.lines().any(|l| l.starts_with("PATH=")), "{vars}");
        assert!(!vars.lines().any(|l| l.starts_with("CARGO") || l.starts_with("HOME=") || l.starts_with("USER=")), "{vars}");
        // The scratch directory is gone afterwards.
        let left: Vec<_> = std::fs::read_dir(&t.0).unwrap().flatten().map(|e| e.file_name()).filter(|n| n.to_string_lossy().starts_with("arena-ask-")).collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn codex_runs_read_only_with_its_tools_off_and_answers_through_its_file() {
        let t = Tmp::new("codex");
        let env = recording_stubs(&t.0);
        let answer = ask(Ai::Codex, "PROMPT", None, Duration::from_secs(20), &env).await.unwrap();
        assert_eq!(answer, "ANSWER from codex", "the -o file, not its stdout chatter");
        let argv: Vec<String> = rec(&t.0, "codex.argv").lines().map(String::from).collect();
        let at = |flag: &str| argv.iter().position(|a| a == flag).map(|i| argv[i + 1].clone()).unwrap();
        let (cwd, out) = (at("-C"), at("-o"));
        assert_eq!(cwd, rec(&t.0, "codex.cwd").trim(), "-C is where it runs: the empty dir");
        let disabled = codex_disabled(&parse_features(&codex_features(&[])).unwrap());
        assert_eq!(argv, codex_args(Path::new(&cwd), Path::new(&out), None, &disabled), "exactly the no-tools argv, the MCP override included");
        assert_eq!(rec(&t.0, "codex.stdin"), "PROMPT");
        assert!(!rec(&t.0, "codex.env").lines().any(|l| l.starts_with("CARGO")), "{}", rec(&t.0, "codex.env"));
    }

    /// Fail closed: a codex whose `features list` fails, hangs or doesn't name every tool
    /// feature, and a claude whose `--help` fails or doesn't list every no-tools flag, is refused
    /// — and never run with the prompt (nothing recorded, nothing read from stdin).
    #[cfg(unix)]
    #[tokio::test]
    async fn a_cli_that_cant_confirm_its_tools_are_off_is_never_asked() {
        let t = Tmp::new("unconfirmed");
        let bin = t.0.join("bin");
        let ran = t.0.join("ran");
        std::fs::create_dir_all(&bin).unwrap();
        let env = AiEnv { path: Some(on_path(&bin)), home: None, tmp: t.0.clone(), vars: Vec::new() };
        let cases: Vec<(Ai, String, u64, &str)> = vec![
            (Ai::Codex, "echo 'error: unrecognized subcommand' >&2; exit 2".into(), 10, "(`codex features list` failed (exit 2): error: unrecognized subcommand)"),
            (Ai::Codex, "exec sleep 30".into(), 1, "(`codex features list` didn't answer within 1s)"),
            (Ai::Codex, format!("cat <<'EOF'\n{}EOF", codex_features(&["hooks", "view_image"])), 10, "(`codex features list` doesn't name hooks, view_image)"),
            (Ai::Codex, "true".into(), 10, "(`codex features list` listed nothing)"),
            (Ai::Claude, "echo 'Error: unknown option' >&2; exit 1".into(), 10, "(`claude --help` failed (exit 1): Error: unknown option)"),
            (Ai::Claude, format!("cat <<'EOF'\n{}EOF", claude_help(&["--strict-mcp-config", "--mcp-config"])), 10, "(`claude --help` doesn't list --strict-mcp-config, --mcp-config)"),
        ];
        for (ai, describe, secs, why) in cases {
            let discovery = discovery_args(ai).join(" ");
            stub(&bin, ai.name(), &format!("case \"$*\" in '{discovery}'*)\n{describe}\nexit $? ;;\nesac\ncat > {}\necho ANSWER", ran.display()));
            let started = std::time::Instant::now();
            let e = ask(ai, "the PROMPT", None, Duration::from_secs(secs), &env).await.unwrap_err();
            assert!(e.starts_with(&format!("{}: couldn't confirm its tools are off — not asked (", ai.name())), "{e}");
            assert!(e.contains(why), "{why} in {e}");
            assert!(!ran.exists(), "{}: it was run with the prompt after {why}", ai.name());
            assert!(started.elapsed() < Duration::from_secs(8), "{:?}", started.elapsed());
        }
    }

    /// Review ai-review codex #3, claude #1: `codex features list` is read whole or not at all —
    /// every row `<name> <stage> true|false`, its state exactly `true` or `false`, a stage codex is
    /// known to print, no name twice, something listed — and must name what's required. Each way a
    /// changed format could read as "off" is a refusal naming it: an empty listing, an `enabled`
    /// state, an extra column (after the state, or between), a header, a missing feature.
    #[test]
    fn codex_features_list_is_read_strictly() {
        assert_eq!(parse_features(&codex_features(&[])).unwrap().len(), CODEX_TOOL_FEATURES.len() + 5);
        let err = |text: &str| parse_features(text).unwrap_err();
        let row = |r: &str, why: &str| format!("has a row that can't be read: `{r}` — {why}");
        assert_eq!(err(""), "listed nothing");
        assert_eq!(err("\n   \n"), "listed nothing");
        assert_eq!(err("shell_tool    stable    enabled\n"), row("shell_tool stable enabled", "its state `enabled` isn't true or false"));
        assert_eq!(err("apps stable true\nshell_tool stable true extra\n"), row("shell_tool stable true extra", "its state `extra` isn't true or false"));
        assert_eq!(err("shell_tool stable 0.163 true\n"), row("shell_tool stable 0.163 true", "`stable 0.163` isn't a stage it's known to print"));
        assert_eq!(err("shell_tool beta true\n"), row("shell_tool beta true", "`beta` isn't a stage it's known to print"));
        assert_eq!(err("NAME STAGE ENABLED\nshell_tool stable true\n"), row("NAME STAGE ENABLED", "its state `ENABLED` isn't true or false"));
        assert_eq!(err("feature stage on\n"), row("feature stage on", "its state `on` isn't true or false"));
        assert_eq!(err("shell_tool true\n"), row("shell_tool true", "it isn't `<name> <stage> true|false`"));
        assert_eq!(err("Shell_Tool stable true\n"), row("Shell_Tool stable true", "`Shell_Tool` isn't a feature's name"));
        assert_eq!(err("shell_tool stable true\nshell_tool stable false\n"), "names shell_tool twice");
        // A missing feature, in the first listing or in the one with the overrides.
        assert_eq!(codex_listing(&codex_features(&["hooks"]), CODEX_TOOL_FEATURES).unwrap_err(), "doesn't name hooks");
        assert_eq!(codex_listing(&codex_features(&["goals"]), &["goals".to_string(), "steer".into()]).unwrap_err(), "doesn't name goals");
        assert_eq!(confirm_tools_off(Ai::Codex, ""), Err("listed nothing".into()));
        assert!(codex_listing(&codex_features(&[]), CODEX_TOOL_FEATURES).is_ok());
    }

    /// The same, end to end: a listing that can't be read whole — the first, or the one run with
    /// the ask's overrides — refuses the ask naming the problem, and codex is never sent the
    /// prompt: an `enabled` state, an extra column, a feature it disables no longer named, an
    /// empty listing.
    #[cfg(unix)]
    #[tokio::test]
    async fn codex_is_refused_when_a_listing_cant_be_read_whole() {
        let t = Tmp::new("strict");
        let (bin, ran) = (t.0.join("bin"), t.0.join("ran"));
        std::fs::create_dir_all(&bin).unwrap();
        let (first, second) = (t.0.join("first.txt"), t.0.join("second.txt"));
        stub(
            &bin,
            "codex",
            &format!(
                "if [ \"$1\" = features ]; then case \"$*\" in *--disable*) cat '{}' ;; *) cat '{}' ;; esac; exit 0; fi\ncat > {}\necho ANSWER",
                second.display(),
                first.display(),
                ran.display()
            ),
        );
        let env = AiEnv { path: Some(on_path(&bin)), home: None, tmp: t.0.clone(), vars: Vec::new() };
        let good = codex_features(&[]);
        let off = good.replace(" true\n", " false\n");
        let without = |f: &str| off.lines().filter(|l| l.split_whitespace().next() != Some(f)).map(|l| format!("{l}\n")).collect::<String>();
        let cases = [
            (good.replace(" true\n", " enabled\n"), off.clone(), "(`codex features list` has a row that can't be read: `shell_tool stable enabled` — its state `enabled` isn't true or false)"),
            (good.clone(), off.replace(" false\n", " false 0.163\n"), "(`codex features list` with the ask's overrides has a row that can't be read: `shell_tool stable false 0.163` — its state `0.163` isn't true or false)"),
            (good.clone(), without("sleep_tool"), "(`codex features list` with the ask's overrides doesn't name sleep_tool)"),
            (good.clone(), String::new(), "(`codex features list` with the ask's overrides listed nothing)"),
        ];
        for (one, two, why) in cases {
            std::fs::write(&first, one).unwrap();
            std::fs::write(&second, two).unwrap();
            let e = ask(Ai::Codex, "the PROMPT", None, Duration::from_secs(20), &env).await.unwrap_err();
            assert!(e.starts_with("codex: couldn't confirm its tools are off — not asked (") && e.ends_with(why), "{why}: {e}");
            assert!(!ran.exists(), "{why}: it was sent the prompt");
        }
        // Both read whole, nothing unreviewed on: asked.
        std::fs::write(&first, &good).unwrap();
        std::fs::write(&second, &off).unwrap();
        assert_eq!(ask(Ai::Codex, "the PROMPT", None, Duration::from_secs(20), &env).await.unwrap(), "ANSWER");
    }

    /// Review ai-review claude #1: what may stay on. The features turned off are every tool
    /// feature, then each web search one and each of [`CODEX_OFF_WHEN_LISTED`] that codex lists;
    /// once they are, anything it still shows on outside [`CODEX_SAFE_FEATURES`] is named — an
    /// unknown feature, or one whose `--disable` it ignored. No tool feature may stay on but
    /// `unified_exec` (the shell's implementation, which codex 0.163 keeps on), never `shell_tool`.
    #[test]
    fn codex_may_keep_on_only_the_reviewed_features() {
        let listed = parse_features(&codex_features(&[])).unwrap();
        let disabled = codex_disabled(&listed);
        assert_eq!(disabled[..CODEX_TOOL_FEATURES.len()], *CODEX_TOOL_FEATURES);
        assert_eq!(disabled[CODEX_TOOL_FEATURES.len()..], ["web_search_request", "goals", "sleep_tool"]);
        assert!(off_when_listed("web_search_cached") && off_when_listed("standalone_web_search") && off_when_listed("worktrees"));
        assert!(!off_when_listed("fast_mode") && !off_when_listed("shell_snapshot"));
        let row = |n: &str, on| (n.to_string(), on);
        assert_eq!(codex_still_on(&[row("fast_mode", true), row("unified_exec", true), row("shell_tool", false), row("zzz_new", false)]), Vec::<String>::new());
        assert_eq!(codex_still_on(&[row("fast_mode", true), row("zzz_new", true), row("shell_tool", true), row("goals", true)]), ["zzz_new", "shell_tool", "goals"]);
        for f in CODEX_TOOL_FEATURES.iter().chain(CODEX_OFF_WHEN_LISTED) {
            assert!(!CODEX_SAFE_FEATURES.contains(f) || *f == "unified_exec", "{f} may stay on");
        }
        assert!(!CODEX_SAFE_FEATURES.iter().any(|f| f.contains("web_search") || *f == "shell_tool"));
    }

    /// The allow-list is codex 0.163's own: its real `features list` (run on this box with the ask's
    /// overrides — an empty `CODEX_HOME`, `-c web_search="disabled" -c mcp_servers={}` and a
    /// `--disable` for each of [`codex_disabled`]; no prompt) shows on nothing else, every allowed
    /// feature is one it has on, and what it was told to turn off is off — but `unified_exec`.
    #[test]
    fn codex_0_163_keeps_on_only_what_is_allowed() {
        let listed = parse_features(include_str!("testdata/codex-0.163-features-list.txt")).unwrap();
        assert_eq!(listed.len(), 175);
        assert_eq!(codex_still_on(&listed), Vec::<String>::new());
        for f in CODEX_SAFE_FEATURES {
            assert!(listed.iter().any(|(n, on)| n == f && *on), "{f} isn't on in 0.163");
        }
        let disabled = codex_disabled(&listed);
        let still: Vec<&str> = listed.iter().filter(|(n, on)| *on && disabled.contains(n)).map(|(n, _)| n.as_str()).collect();
        assert_eq!(still, ["unified_exec"]);
        let web: Vec<&str> = disabled.iter().map(String::as_str).filter(|f| f.contains("web_search")).collect();
        assert_eq!(web, ["standalone_web_search", "web_search_cached", "web_search_request"]);
    }

    /// codex is asked only once its `features list`, with the ask's own overrides, shows nothing on
    /// beyond what may stay on: all known → asked, every web search feature and every one of
    /// [`CODEX_OFF_WHEN_LISTED`] it lists off too; a feature unknown here on, or an override it
    /// ignored (`shell_tool` still on) → refused, naming it, never sent the prompt. Both checks run
    /// with an empty `CODEX_HOME` of their own — the operator's `config.toml`, which the ask
    /// doesn't read, can't hide one — and the ask itself with the operator's (its login).
    #[cfg(unix)]
    #[tokio::test]
    async fn codex_is_asked_only_when_nothing_unreviewed_stays_on() {
        let t = Tmp::new("allow");
        let mut env = recording_stubs(&t.0);
        let operator = t.0.join("operator-codex");
        env.vars.push(("CODEX_HOME".into(), operator.clone().into_os_string()));
        assert_eq!(ask(Ai::Codex, "PROMPT", None, Duration::from_secs(20), &env).await.unwrap(), "ANSWER from codex");
        let argv = rec(&t.0, "codex.argv");
        for f in ["web_search_request", "goals", "sleep_tool", "shell_tool"] {
            assert!(argv.contains(&format!("--disable\n{f}\n")), "{f}: {argv}");
        }
        let checks: Vec<String> = rec(&t.0, "codex.features.argv").lines().map(String::from).collect();
        let disabled = codex_disabled(&parse_features(&codex_features(&[])).unwrap());
        let line = |d: &[String]| ["features", "list"].map(String::from).into_iter().chain(codex_overrides(d)).collect::<Vec<_>>().join(" ");
        assert_eq!(checks, [line(&[]), line(&disabled)], "the names, then the overrides the ask gets");
        let homes: Vec<String> = rec(&t.0, "codex.features.home").lines().map(String::from).collect();
        assert!(homes.len() == 2 && homes.iter().all(|h| h.ends_with("/codex-home") && h.contains("arena-ask-")), "{homes:?}");
        assert!(rec(&t.0, "codex.env").lines().any(|l| l == format!("CODEX_HOME={}", operator.display())), "the ask keeps its login");

        let bin = t.0.join("bin");
        let ran = t.0.join("ran");
        for (extra, ignored, named) in [(&["zzz_new"][..], &["unified_exec"][..], "zzz_new"), (&[][..], &["unified_exec", "shell_tool"][..], "shell_tool")] {
            stub(&bin, "codex", &format!("if [ \"$1\" = features ]; then\n{}\nexit 0; fi\ncat > {}\necho ANSWER", codex_features_sh(&[], extra, ignored, None), ran.display()));
            let e = ask(Ai::Codex, "the PROMPT", None, Duration::from_secs(20), &env).await.unwrap_err();
            assert!(e.starts_with("codex: couldn't confirm its tools are off — not asked (`codex features list` shows "), "{e}");
            assert!(e.contains(&format!("shows {named} on with the ask's overrides — not known to give it no tool")), "{e}");
            assert!(!ran.exists(), "{named}: it was sent the prompt");
        }
    }

    #[tokio::test]
    async fn a_missing_binary_names_the_other_option() {
        let t = Tmp::new("missing");
        let env = AiEnv { path: Some(t.0.clone().into_os_string()), home: None, tmp: t.0.clone(), vars: Vec::new() };
        let e = ask(Ai::Claude, "p", None, Duration::from_secs(5), &env).await.unwrap_err();
        assert!(e.contains("claude not found") && e.contains("--ai codex") && e.contains("ASK_AI=codex"), "{e}");
        let e = ask(Ai::Codex, "p", None, Duration::from_secs(5), &env).await.unwrap_err();
        assert!(e.contains("codex not found") && e.contains("--ai claude"), "{e}");
    }

    /// Over its budget the AI is stopped — the process is gone when `ask` returns — and the
    /// error says what happened; a failed run says how, with its last stderr line.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_timeout_stops_the_ai_and_a_failure_says_why() {
        let t = Tmp::new("timeout");
        let bin = t.0.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let pidfile = t.0.join("pid");
        let help = format!("if [ \"$1\" = --help ]; then cat <<'EOF'\n{}EOF\nexit 0; fi", claude_help(&[]));
        stub(&bin, "claude", &format!("{help}\necho $$ > {}\ncat > /dev/null\nexec sleep 30", pidfile.display()));
        let env = AiEnv { path: Some(on_path(&bin)), home: None, tmp: t.0.clone(), vars: Vec::new() };
        let started = std::time::Instant::now();
        let e = ask(Ai::Claude, "p", None, Duration::from_secs(1), &env).await.unwrap_err();
        assert!(e.contains("didn't answer within 1s") && e.contains("stopped"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
        let pid = std::fs::read_to_string(&pidfile).unwrap();
        assert!(!Path::new(&format!("/proc/{}", pid.trim())).exists(), "the stub is gone (pid {pid})");

        stub(&bin, "claude", &format!("{help}\necho 'partial' ; echo 'Error: not logged in' >&2; exit 3"));
        let e = ask(Ai::Claude, "p", None, Duration::from_secs(10), &env).await.unwrap_err();
        assert_eq!(e, "claude failed (exit 3): Error: not logged in");
        stub(&bin, "claude", &format!("{help}\ncat > /dev/null; printf '  \\n'"));
        let e = ask(Ai::Claude, "p", None, Duration::from_secs(10), &env).await.unwrap_err();
        assert!(e.starts_with("claude answered nothing"), "{e}");
    }

    /// Review ai-review claude #7: a failed run's stderr line — printed, and saved in
    /// `answer.md` — is masked as pod text is: an auth error echoing the operator's key.
    #[test]
    fn a_failed_runs_stderr_line_is_masked() {
        let key = format!("{}{}AA", "sk-ant-api03-", crate::ask::redact::tests::body(crate::ask::redact::tests::URLSAFE, 93, 3));
        let line = last_line(&format!("starting\nError: invalid x-api-key {key} (401)\n\n"));
        assert_eq!(line, "Error: invalid x-api-key REDACTED-ANTHROPIC-KEY (401)");
        assert_eq!(last_line("auth failed: OPENAI_API_KEY=short-one\n"), "auth failed: OPENAI_API_KEY=<redacted>");
        assert_eq!(last_line("Error: not logged in"), "Error: not logged in");
        assert_eq!(last_line(""), "(no output)");
    }

    #[test]
    fn printable_strips_escapes_and_keeps_lines() {
        assert_eq!(printable("\n\u{1b}[31mred\u{1b}[0m  \n\u{1b}]0;title\u{7}ok\n\n"), "red\nok");
    }
}
