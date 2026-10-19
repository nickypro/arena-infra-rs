//! Running `claude` or `codex` for `pods ask`: the AI gets the prompt on stdin and **no tools**,
//! in an empty temporary directory, within a time budget.
//!
//! Why no tools: this runs on the fleet's control box, next to the config and the keys, and the
//! bundle holds text participants control (process arguments, file names, commit subjects, job
//! logs) — a prompt injection must have nothing to act with. So claude runs exactly as
//! `scripts/ai-review` runs it (`--tools ''` with `--safe-mode`, `--strict-mcp-config`, no slash
//! commands, no saved session: verified there to list `tools: []`), and codex read-only,
//! ephemeral, with web search and every tool feature its `features list` knows of turned off
//! (codex's own sandbox can't run commands on this machine anyway). Everything the model sees is
//! in the prompt.
//!
//! The binaries are looked up on `PATH`, then in `~/.local/bin` (where both install), as
//! `ai-review` does; a missing one is an error that names the other option. The pure parts —
//! the argv, the config value, the lookup — are separate from the one spawn.

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
                format!("{ASK_AI}={raw} isn't claude, codex or off")
            } else {
                format!("{ASK_AI} isn't claude, codex or off")
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

/// claude's argv: print mode, no tools of any kind (`--tools ''`, and `--safe-mode`: no hooks,
/// MCP servers, CLAUDE.md or skills; `--strict-mcp-config`: no MCP servers from anywhere), no
/// slash commands, no saved session, plain text out. The flags `scripts/ai-review` runs it with.
pub fn claude_args(model: Option<&str>) -> Vec<String> {
    let mut a: Vec<String> = [
        "-p",
        "--safe-mode",
        "--tools",
        "",
        "--strict-mcp-config",
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

/// codex features that give it a tool, each turned off when its `features list` names it (an
/// unknown name is an error to codex) — `scripts/ai-review`'s list.
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
];

/// codex's argv: `exec` read-only and ephemeral, in `workdir` (an empty directory), its answer
/// written to `out`, web search off, every tool feature in [`CODEX_TOOL_FEATURES`] that `known`
/// (its `features list`) has turned off, the prompt on stdin (`-`).
pub fn codex_args(known: &[String], workdir: &Path, out: &Path, model: Option<&str>) -> Vec<String> {
    let mut a: Vec<String> = ["exec", "-s", "read-only", "--skip-git-repo-check", "--ephemeral", "--color", "never", "-C"]
        .map(String::from)
        .to_vec();
    a.push(workdir.display().to_string());
    a.extend(["-o".to_string(), out.display().to_string(), "-c".to_string(), "web_search=\"disabled\"".to_string()]);
    for f in CODEX_TOOL_FEATURES {
        if known.iter().any(|k| k == f) {
            a.extend(["--disable".to_string(), f.to_string()]);
        }
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

/// Where the AI's binary is looked for, and where its scratch directory goes.
#[derive(Debug, Clone)]
pub struct AiEnv {
    /// `PATH` (also the child's).
    pub path: Option<OsString>,
    /// `HOME`, for `~/.local/bin`.
    pub home: Option<PathBuf>,
    /// Where the empty working directory is made.
    pub tmp: PathBuf,
}

impl AiEnv {
    /// This process's `PATH`, `HOME` and temp dir.
    pub fn from_process() -> AiEnv {
        AiEnv { path: std::env::var_os("PATH"), home: std::env::var_os("HOME").map(PathBuf::from), tmp: std::env::temp_dir() }
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

/// How long `codex features list` may take.
const FEATURES_TIMEOUT: Duration = Duration::from_secs(20);

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

/// Ask `ai` `prompt` (on stdin) with `model` (or the CLI's own default), within `timeout`; the
/// answer, made printable, or why there is none — a missing binary, a timeout (the AI is
/// stopped: SIGTERM, then SIGKILL), a failed run, an empty answer.
pub async fn ask(ai: Ai, prompt: &str, model: Option<&str>, timeout: Duration, env: &AiEnv) -> Result<String, String> {
    let bin = find_bin(ai.name(), env).ok_or_else(|| missing(ai))?;
    let scratch = Scratch::new(&env.tmp).map_err(|e| format!("can't make a scratch directory in {}: {e}", env.tmp.display()))?;
    let empty = scratch.empty();
    let answer_file = scratch.0.join("answer.md");
    let args = match ai {
        Ai::Claude => claude_args(model),
        Ai::Codex => {
            let list = ["features".to_string(), "list".to_string()];
            let run = LocalRun { program: &bin, args: &list, cwd: &empty, path: env.path.as_deref(), input: b"" };
            let known = match run_local_input(&run, "codex features list", Some(FEATURES_TIMEOUT)).await {
                Ok(out) if out.success => parse_features(&out.stdout),
                _ => Vec::new(),
            };
            codex_args(&known, &empty, &answer_file, model)
        }
    };
    let run = LocalRun { program: &bin, args: &args, cwd: &empty, path: env.path.as_deref(), input: prompt.as_bytes() };
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

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

    /// An executable `name` in `dir` running `body` under sh.
    #[cfg(unix)]
    pub(crate) fn stub(dir: &Path, name: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let f = dir.join(name);
        std::fs::write(&f, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Stub `claude` and `codex` in `<tmp>/bin` that record their argv (one per line), stdin
    /// and working directory under `<tmp>/rec/<name>.*` and answer `ANSWER from <name>` — codex
    /// into its `-o` file, as the real one does; `codex features list` lists a few features.
    #[cfg(unix)]
    pub(crate) fn recording_stubs(tmp: &Path) -> AiEnv {
        let (bin, rec) = (tmp.join("bin"), tmp.join("rec"));
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&rec).unwrap();
        let rec = rec.display();
        stub(
            &bin,
            "claude",
            &format!("printf '%s\\n' \"$@\" > {rec}/claude.argv\ncat > {rec}/claude.stdin\npwd > {rec}/claude.cwd\nls -A > {rec}/claude.ls\necho 'ANSWER from claude'"),
        );
        stub(
            &bin,
            "codex",
            &format!(
                "if [ \"$1\" = features ]; then printf 'shell_tool  stable  true\\nweb_search  stable  true\\napps  beta  false\\nhooks  beta  false\\n'; exit 0; fi\n\
                 printf '%s\\n' \"$@\" > {rec}/codex.argv\ncat > {rec}/codex.stdin\npwd > {rec}/codex.cwd\n\
                 out=; prev=; for a; do [ \"$prev\" = -o ] && out=$a; prev=$a; done\n\
                 echo 'ANSWER from codex' > \"$out\"\necho 'codex chatter on stdout'"
            ),
        );
        AiEnv { path: Some(on_path(&bin)), home: None, tmp: tmp.to_path_buf() }
    }

    /// `PATH` with `dir` first: the stubs found before any real `claude`/`codex`, the tools
    /// they run (`cat`, `sleep`) still there.
    #[cfg(unix)]
    pub(crate) fn on_path(dir: &Path) -> OsString {
        let rest = std::env::var_os("PATH").unwrap_or_default();
        std::env::join_paths(std::iter::once(dir.to_path_buf()).chain(std::env::split_paths(&rest))).unwrap()
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
        assert_eq!(s("ASK_AI=gpt").unwrap_err(), "ASK_AI=gpt isn't claude, codex or off");
        // A pasted key is never echoed.
        let e = s("ASK_AI=sk-0123456789abcdefghij").unwrap_err();
        assert_eq!(e, "ASK_AI isn't claude, codex or off");
        assert!(Setting::is_default(&Config::parse("")) && !Setting::is_default(&Config::parse("ASK_AI=codex")));
        // --ai picks; off refuses whatever --ai says; a bad value refuses too.
        let c = |text: &str, flag| choose(&Config::parse(text), flag);
        assert_eq!(c("", None), Ok(Ai::Claude));
        assert_eq!(c("ASK_AI=codex", None), Ok(Ai::Codex));
        assert_eq!(c("ASK_AI=codex", Some(Ai::Claude)), Ok(Ai::Claude));
        assert!(c("ASK_AI=off", Some(Ai::Claude)).unwrap_err().contains("ASK_AI=off"));
        assert!(c("ASK_AI=of", Some(Ai::Claude)).is_err());
    }

    #[test]
    fn argv_have_no_tools() {
        let a = claude_args(Some("opus"));
        let i = a.iter().position(|x| x == "--tools").unwrap();
        assert_eq!(a[i + 1], "", "--tools '' = no built-in tools");
        for f in ["-p", "--safe-mode", "--strict-mcp-config", "--disable-slash-commands", "--no-session-persistence"] {
            assert!(a.iter().any(|x| x == f), "{f}");
        }
        assert!(a.ends_with(&["--model".to_string(), "opus".to_string()]));
        assert!(!claude_args(None).contains(&"--model".to_string()));

        let known: Vec<String> = ["shell_tool", "apps", "web_search", "zzz"].map(String::from).to_vec();
        let c = codex_args(&known, Path::new("/t/empty"), Path::new("/t/a.md"), None);
        let joined = c.join(" ");
        assert!(joined.starts_with("exec -s read-only --skip-git-repo-check --ephemeral"), "{joined}");
        assert!(joined.contains("-C /t/empty -o /t/a.md -c web_search=\"disabled\""), "{joined}");
        assert!(joined.contains("--disable shell_tool --disable apps"), "{joined}");
        assert!(!joined.contains("--disable web_search") && !joined.contains("zzz"), "only tool features it knows");
        assert_eq!(c.last().map(String::as_str), Some("-"), "the prompt on stdin");
        assert_eq!(parse_features("shell_tool stable true\n\napps beta false\n"), ["shell_tool", "apps"]);
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
        let env = AiEnv { path: Some(std::env::join_paths([&a, &b]).unwrap()), home: Some(home.clone()), tmp: t.0.clone() };
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
        let joined = argv.join(" ");
        assert!(joined.starts_with("exec -s read-only --skip-git-repo-check"), "{joined}");
        assert!(joined.contains("--disable shell_tool --disable apps --disable hooks"), "{joined}");
        assert_eq!(argv.last().map(String::as_str), Some("-"));
        assert_eq!(rec(&t.0, "codex.stdin"), "PROMPT");
    }

    #[tokio::test]
    async fn a_missing_binary_names_the_other_option() {
        let t = Tmp::new("missing");
        let env = AiEnv { path: Some(t.0.clone().into_os_string()), home: None, tmp: t.0.clone() };
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
        stub(&bin, "claude", &format!("echo $$ > {}\ncat > /dev/null\nexec sleep 30", pidfile.display()));
        let env = AiEnv { path: Some(on_path(&bin)), home: None, tmp: t.0.clone() };
        let started = std::time::Instant::now();
        let e = ask(Ai::Claude, "p", None, Duration::from_secs(1), &env).await.unwrap_err();
        assert!(e.contains("didn't answer within 1s") && e.contains("stopped"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
        let pid = std::fs::read_to_string(&pidfile).unwrap();
        assert!(!Path::new(&format!("/proc/{}", pid.trim())).exists(), "the stub is gone (pid {pid})");

        stub(&bin, "claude", "echo 'partial' ; echo 'Error: not logged in' >&2; exit 3");
        let e = ask(Ai::Claude, "p", None, Duration::from_secs(10), &env).await.unwrap_err();
        assert_eq!(e, "claude failed (exit 3): Error: not logged in");
        stub(&bin, "claude", "cat > /dev/null; printf '  \\n'");
        let e = ask(Ai::Claude, "p", None, Duration::from_secs(10), &env).await.unwrap_err();
        assert!(e.starts_with("claude answered nothing"), "{e}");
    }

    #[test]
    fn printable_strips_escapes_and_keeps_lines() {
        assert_eq!(printable("\n\u{1b}[31mred\u{1b}[0m  \n\u{1b}]0;title\u{7}ok\n\n"), "red\nok");
    }
}
