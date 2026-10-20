//! Running `claude` or `codex` for `pods ask`: the AI gets the prompt on stdin and **no tools**,
//! in an empty temporary directory, within a time budget.
//!
//! Why no tools: this runs on the fleet's control box, next to the config and the keys, and the
//! bundle holds text participants control (process arguments, file names, commit subjects, job
//! logs) — a prompt injection must have nothing to act with. So claude runs as
//! `scripts/ai-review` runs it (`--tools ''` with `--safe-mode`, `--strict-mcp-config`, no slash
//! commands, no saved session: verified there to list `tools: []`), plus an explicitly empty
//! `--mcp-config`; and codex read-only, ephemeral, without the operator's `config.toml` (its MCP
//! servers, hooks, profiles) and with `mcp_servers` emptied, web search off and every tool
//! feature in [`CODEX_TOOL_FEATURES`] disabled. Everything the model sees is in the prompt.
//!
//! **Fail closed.** Neither is sent anything until the CLI itself has confirmed it knows every
//! switch that takes its tools away — claude's `--help` lists each of [`CLAUDE_NO_TOOL_FLAGS`],
//! codex's `features list` names each of [`CODEX_TOOL_FEATURES`] ([`confirm_tools_off`]). A
//! check that fails, times out or finds one missing refuses the ask: a codex that doesn't know a
//! feature can't be told to turn it off, and one run without those switches could read files on
//! this machine through its shell. Both get an empty working directory and a minimal environment
//! ([`child_env`]: not the operator's provider keys).
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
/// `scripts/ai-review` runs it with, plus the explicit empty MCP config.
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

/// codex features that give it a tool, every one turned off — `scripts/ai-review`'s list, plus
/// `view_image` (it reads an image file anywhere on this machine). Its `features list` must name
/// each before it is asked anything ([`confirm_tools_off`]): a feature it doesn't know can't be
/// disabled (`--disable` of an unknown name is an error), and one it has under another name
/// would stay on.
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

/// codex's argv: `exec` read-only and ephemeral, in `workdir` (an empty directory), its answer
/// written to `out`; without the operator's `config.toml` (`--ignore-user-config`: its MCP
/// servers, hooks, profiles, notify program) or execpolicy rules, and `mcp_servers` emptied
/// besides (a system or managed config layer is still read); web search off, every tool feature
/// in [`CODEX_TOOL_FEATURES`] disabled — always all of them: [`ask`] runs it only once its
/// `features list` has named each — and the prompt on stdin (`-`).
pub fn codex_args(workdir: &Path, out: &Path, model: Option<&str>) -> Vec<String> {
    let mut a: Vec<String> =
        ["exec", "-s", "read-only", "--skip-git-repo-check", "--ephemeral", "--ignore-user-config", "--ignore-rules", "--color", "never", "-C"]
            .map(String::from)
            .to_vec();
    a.push(workdir.display().to_string());
    a.extend(["-o", &out.display().to_string(), "-c", "web_search=\"disabled\"", "-c", "mcp_servers={}"].map(String::from));
    for f in CODEX_TOOL_FEATURES {
        a.extend(["--disable".to_string(), f.to_string()]);
    }
    if let Some(m) = model {
        a.extend(["-m".to_string(), m.to_string()]);
    }
    a.push("-".to_string());
    a
}

/// The feature names in `codex features list`'s output: each line's first word.
pub fn parse_features(stdout: &str) -> Vec<String> {
    stdout.lines().filter_map(|l| l.split_whitespace().next()).map(String::from).collect()
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
/// (as a word: `--tools,` or `--tools <tools...>`), codex's `features list` names each of
/// [`CODEX_TOOL_FEATURES`]. `Err`: the ones it doesn't.
pub fn confirm_tools_off(ai: Ai, described: &str) -> Result<(), Vec<&'static str>> {
    let missing: Vec<&'static str> = match ai {
        Ai::Claude => {
            let words: Vec<&str> = described.split(|c: char| c.is_whitespace() || c == ',').collect();
            CLAUDE_NO_TOOL_FLAGS.iter().copied().filter(|f| !words.contains(f)).collect()
        }
        Ai::Codex => {
            let known = parse_features(described);
            CODEX_TOOL_FEATURES.iter().copied().filter(|f| !known.iter().any(|k| k == f)).collect()
        }
    };
    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing)
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
/// is `empty/` inside it, codex's answer file beside that.
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
        Ok(s)
    }

    fn empty(&self) -> PathBuf {
        self.0.join("empty")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// How long `claude --help` / `codex features list` may take (at most the ask's own budget).
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(20);

/// The last non-empty stderr line, made printable and short — what a failed run says.
fn last_line(stderr: &str) -> String {
    let l = stderr.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("(no output)");
    crate::fleet::clip(&crate::jobs::sanitize(l), 200)
}

/// Text from the AI, safe for the operator's terminal: escape sequences and control characters
/// out, line breaks kept, trailing space trimmed.
pub fn printable(answer: &str) -> String {
    answer.lines().map(|l| crate::jobs::sanitize(l).trim_end().to_string()).collect::<Vec<_>>().join("\n").trim().to_string()
}

/// Run `ai`'s self-description ([`discovery_args`]) and hold it to [`confirm_tools_off`]:
/// `Ok` only when it ran, within `budget`, and named every switch. Otherwise why not, as the
/// refusal `ask` returns — nothing has been sent.
async fn check_tools_off(ai: Ai, bin: &Path, cwd: &Path, vars: &[(OsString, OsString)], budget: Duration) -> Result<(), String> {
    let args = discovery_args(ai);
    let cmd = format!("{} {}", ai.name(), args.join(" "));
    let refuse = |why: String| format!("{}: couldn't confirm its tools are off — not asked ({why})", ai.name());
    let run = LocalRun { program: bin, args: &args, cwd, env: vars, input: b"" };
    match run_local_input(&run, &cmd, Some(budget)).await {
        Ok(out) if out.success => confirm_tools_off(ai, &out.stdout).map_err(|missing| {
            let what = if ai == Ai::Claude { "list" } else { "name" };
            refuse(format!("`{cmd}` doesn't {what} {}", missing.join(", ")))
        }),
        Ok(out) => Err(refuse(format!("`{cmd}` failed ({}): {}", out.exit_label(), last_line(&out.stderr)))),
        Err(crate::Error::Timeout { after, .. }) => Err(refuse(format!("`{cmd}` didn't answer within {}", crate::error::human_duration(&after)))),
        Err(e) => Err(refuse(format!("`{cmd}`: {e}"))),
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
    check_tools_off(ai, &bin, &empty, &vars, timeout.min(DISCOVERY_TIMEOUT)).await?;
    let args = match ai {
        Ai::Claude => claude_args(model),
        Ai::Codex => codex_args(&empty, &answer_file, model),
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

    /// What a stub `codex features list` prints: every feature in [`CODEX_TOOL_FEATURES`] and
    /// a few others, as `name  stage  enabled` rows, less `without`.
    pub fn codex_features(without: &[&str]) -> String {
        let names = CODEX_TOOL_FEATURES.iter().chain(&["web_search_request", "goals", "zzz_new"]);
        names.filter(|f| !without.contains(f)).map(|f| format!("{f:<40} stable             true\n")).collect()
    }

    /// Stub `claude` and `codex` in `<tmp>/bin` that record their argv (one per line), stdin,
    /// working directory and environment under `<tmp>/rec/<name>.*` and answer `ANSWER from
    /// <name>` — codex into its `-o` file, as the real one does. `claude --help` lists the no-tools
    /// flags ([`claude_help`]) and `codex features list` every tool feature ([`codex_features`]),
    /// without recording anything.
    #[cfg(unix)]
    pub fn recording_stubs(tmp: &Path) -> AiEnv {
        let (bin, rec) = (tmp.join("bin"), tmp.join("rec"));
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&rec).unwrap();
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
                "if [ \"$1\" = features ]; then cat <<'EOF'\n{}EOF\nexit 0; fi\n\
                 printf '%s\\n' \"$@\" > {rec}/codex.argv\ncat > {rec}/codex.stdin\npwd > {rec}/codex.cwd\nenv > {rec}/codex.env\n\
                 out=; prev=; for a; do [ \"$prev\" = -o ] && out=$a; prev=$a; done\n\
                 echo 'ANSWER from codex' > \"$out\"\necho 'codex chatter on stdout'",
                codex_features(&[])
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
        want.extend(words(&["-m", "gpt-x", "-"]));
        assert_eq!(codex_args(Path::new("/t/empty"), Path::new("/t/a.md"), Some("gpt-x")), want);
        assert_eq!(codex_args(Path::new("/t/empty"), Path::new("/t/a.md"), None).last().map(String::as_str), Some("-"), "the prompt on stdin");
        assert_eq!(parse_features("shell_tool stable true\n\napps beta false\n"), ["shell_tool", "apps"]);
    }

    /// The check before anything is sent: claude's --help must list every no-tools flag (as a
    /// word — `--tools-extra` or `--mcp-config-file` isn't `--tools`/`--mcp-config`), codex's
    /// features list must name every tool feature; what's missing is named.
    #[test]
    fn tools_off_is_confirmed_by_the_clis_own_description() {
        assert_eq!(confirm_tools_off(Ai::Claude, &fixtures::claude_help(&[])), Ok(()));
        assert_eq!(confirm_tools_off(Ai::Claude, &fixtures::claude_help(&["--strict-mcp-config", "--tools"])), Err(vec!["--tools", "--strict-mcp-config"]));
        let lookalikes = "  --tools-extra <x>\n  --safe-mode\n  --strict-mcp-config\n  --mcp-config-file\n  --disable-slash-commands\n  --no-session-persistence\n";
        assert_eq!(confirm_tools_off(Ai::Claude, lookalikes), Err(vec!["--tools", "--mcp-config"]));
        assert_eq!(confirm_tools_off(Ai::Claude, ""), Err(CLAUDE_NO_TOOL_FLAGS.to_vec()));
        assert_eq!(confirm_tools_off(Ai::Codex, &fixtures::codex_features(&[])), Ok(()));
        assert_eq!(confirm_tools_off(Ai::Codex, &fixtures::codex_features(&["hooks", "view_image"])), Err(vec!["hooks", "view_image"]));
        // A name inside another line's text isn't a feature it lists.
        assert_eq!(confirm_tools_off(Ai::Codex, "web_search  stable  true  (not shell_tool)\n").unwrap_err().len(), CODEX_TOOL_FEATURES.len());
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
        assert_eq!(argv, codex_args(Path::new(&cwd), Path::new(&out), None), "exactly the no-tools argv, the MCP override included");
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
            (Ai::Codex, "true".into(), 10, "doesn't name shell_tool, unified_exec,"),
            (Ai::Claude, "echo 'Error: unknown option' >&2; exit 1".into(), 10, "(`claude --help` failed (exit 1): Error: unknown option)"),
            (Ai::Claude, format!("cat <<'EOF'\n{}EOF", claude_help(&["--strict-mcp-config", "--mcp-config"])), 10, "(`claude --help` doesn't list --strict-mcp-config, --mcp-config)"),
        ];
        for (ai, describe, secs, why) in cases {
            let discovery = discovery_args(ai).join(" ");
            stub(&bin, ai.name(), &format!("if [ \"$*\" = '{discovery}' ]; then\n{describe}\nexit $?\nfi\ncat > {}\necho ANSWER", ran.display()));
            let started = std::time::Instant::now();
            let e = ask(ai, "the PROMPT", None, Duration::from_secs(secs), &env).await.unwrap_err();
            assert!(e.starts_with(&format!("{}: couldn't confirm its tools are off — not asked (", ai.name())), "{e}");
            assert!(e.contains(why), "{why} in {e}");
            assert!(!ran.exists(), "{}: it was run with the prompt after {why}", ai.name());
            assert!(started.elapsed() < Duration::from_secs(8), "{:?}", started.elapsed());
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

    #[test]
    fn printable_strips_escapes_and_keeps_lines() {
        assert_eq!(printable("\n\u{1b}[31mred\u{1b}[0m  \n\u{1b}]0;title\u{7}ok\n\n"), "red\nok");
    }
}
