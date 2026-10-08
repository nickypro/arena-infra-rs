//! VS Code Remote-SSH warm-up: pre-install the VS Code server and the course extensions on a
//! pod during setup, so a participant's first connect doesn't download them — on the pod,
//! slowly, and again on every new, replaced or restarted pod (the arena image ships no
//! `~/.vscode-server`).
//!
//! Split like the deep check: [`WARMUP_SCRIPT`] (`vscode_setup.sh`) does the work on the pod
//! — server layout, idempotency, checksums, the settings merge; see its header — and this
//! module holds the config ([`VscodeSetup::from_config`], validated so a typo fails `setup`
//! before it touches any pod) and renders the one-exec command
//! ([`VscodeSetup::remote_command`]). Setup runs it as a *best-effort* step
//! ([`crate::setup::ProvisionStep::Optional`]): if it fails or runs out of time the pod is
//! still set up, with a warning — an editor convenience must never fail a course pod.
//!
//! Where Remote-SSH looks (VS Code ≥ 1.82, exec-server mode — the default):
//! `~/.vscode-server/code-<commit>` (the `code` CLI it starts first, from the update API's
//! `cli-alpine-<arch>` build), which serves `~/.vscode-server/cli/servers/Stable-<commit>/server`
//! (`server-linux-<arch>`) and records it in `cli/servers/lru.json`; older clients (or
//! `remote.SSH.useExecServer: false`) look in `~/.vscode-server/bin/<commit>`, which the script
//! links to the same server. Extensions live in `~/.vscode-server/extensions` whatever the
//! server version, and machine settings in `~/.vscode-server/data/Machine/settings.json`.
//! "Latest" is the update API's `…/api/update/server-linux-<arch>/stable/latest` (commit,
//! version, URL, sha256), resolved on the pod at setup time — so a participant on that
//! release connects instantly, and one on another release still finds the extensions.

use std::time::Duration;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::setup::shell_quote;

/// The on-pod installer. Embedded (like the deep check) so the binary is all an operator
/// needs, and versioned with the command that calls it.
pub const WARMUP_SCRIPT: &str = include_str!("vscode_setup.sh");

/// What a participant needs for the course notebooks: Python (with its debugger, pulled in as
/// a dependency), Pylance, and Jupyter (whose extension pack brings the renderers/keymap).
pub const DEFAULT_EXTENSIONS: &[&str] = &["ms-python.python", "ms-python.vscode-pylance", "ms-toolsai.jupyter"];

/// The warm-up step's budget: its own, not the main setup step's (`SETUP_TIMEOUT_SECS`) —
/// the downloads (~70 MB server + CLI, a few extensions) take a minute on a healthy pod, and
/// past this it isn't worth holding up the pod's `READY` for.
pub const WARMUP_TIMEOUT: Duration = Duration::from_secs(300);

/// Kept back from the step's budget for the ssh connect and the script's own wrap-up, so the
/// script stops itself (and says what it got done) before the caller's timeout kills it.
const SSH_MARGIN: Duration = Duration::from_secs(30);

/// The warm-up's settings, from config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VscodeSetup {
    /// Marketplace ids (`publisher.name`), lowercased, deduplicated, in the configured order.
    pub extensions: Vec<String>,
    /// Interpreters for `python.defaultInterpreterPath`, most likely first; the first that
    /// exists on the pod wins (`~/` = the pod user's home).
    pub pythons: Vec<String>,
}

impl VscodeSetup {
    /// `None` when `VSCODE_PREINSTALL` turns it off; else the extensions (`VSCODE_EXTENSIONS`)
    /// and interpreter candidates (`CONDA_ENV`'s env, then the Hetzner venv under the repo at
    /// `repo_path`). A malformed value is an error — caught before setup touches a pod.
    pub fn from_config(cfg: &Config, repo_path: &str) -> Result<Option<Self>> {
        if !preinstall_enabled(cfg.get("VSCODE_PREINSTALL"))? {
            return Ok(None);
        }
        Ok(Some(Self {
            extensions: parse_extensions(cfg.get("VSCODE_EXTENSIONS"))?,
            pythons: python_candidates(cfg.get("CONDA_ENV").unwrap_or("arena-env"), repo_path),
        }))
    }

    /// The command for one `Remote::exec`: the script travels base64-encoded inside it (no
    /// scp, no temp file, no quoting to get wrong), the settings as env vars and arguments.
    /// The script gets `timeout` minus [`SSH_MARGIN`] to work in.
    pub fn remote_command(&self, timeout: Duration) -> String {
        let budget = timeout.saturating_sub(SSH_MARGIN).max(Duration::from_secs(10)).as_secs();
        let pythons: Vec<String> = self.pythons.iter().map(|p| shell_quote(p)).collect();
        format!(
            "ARENA_VSCODE_BUDGET={budget} ARENA_VSCODE_EXTENSIONS={exts} \
             bash -c \"$(printf %s {script} | base64 -d)\" arena-vscode-warmup {pythons} </dev/null",
            exts = shell_quote(&self.extensions.join(",")),
            script = crate::base64::encode(WARMUP_SCRIPT.as_bytes()),
            pythons = pythons.join(" "),
        )
    }

    /// What the dry-run shows in place of [`Self::remote_command`] (a base64 blob).
    pub fn summary(&self) -> String {
        let extensions =
            if self.extensions.is_empty() { "no extensions".to_string() } else { self.extensions.join(", ") };
        format!(
            "latest stable VS Code server (x64/arm64) into ~/.vscode-server (Remote-SSH layout); extensions: \
             {extensions}; python.defaultInterpreterPath (if unset) → first of: {} — each part skipped when \
             already present",
            self.pythons.join(", ")
        )
    }
}

/// `VSCODE_PREINSTALL`: on unless `0`/`false`/`no`/`off`; absent or empty = on. Anything
/// else is an error rather than a guess.
pub fn preinstall_enabled(raw: Option<&str>) -> Result<bool> {
    match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        None | Some("" | "1" | "true" | "yes" | "on") => Ok(true),
        Some("0" | "false" | "no" | "off") => Ok(false),
        Some(other) => Err(Error::Config(format!("VSCODE_PREINSTALL must be 1 or 0 (got `{other}`)"))),
    }
}

/// `VSCODE_EXTENSIONS`: a comma list of marketplace ids (`publisher.name`), or `none`;
/// absent or empty = [`DEFAULT_EXTENSIONS`]. Ids are lowercased (the marketplace ignores
/// case) and deduplicated. Anything that isn't a plain id — a version pin, a path, a stray
/// character — is refused: it lands in a shell command on every pod.
pub fn parse_extensions(raw: Option<&str>) -> Result<Vec<String>> {
    let raw = raw.map(str::trim).unwrap_or("");
    if raw.is_empty() {
        return Ok(DEFAULT_EXTENSIONS.iter().map(|s| s.to_string()).collect());
    }
    if raw.eq_ignore_ascii_case("none") {
        return Ok(Vec::new());
    }
    let mut out: Vec<String> = Vec::new();
    for id in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if !valid_extension_id(id) {
            return Err(Error::Config(format!(
                "VSCODE_EXTENSIONS: `{id}` is not a marketplace id like ms-python.python (comma-separated, or `none`)"
            )));
        }
        let id = id.to_ascii_lowercase();
        if !out.contains(&id) {
            out.push(id);
        }
    }
    Ok(out)
}

/// The extensions a pod should have, for the deep check's informational `vscode` line:
/// [`parse_extensions`], or none when the pre-install is off.
pub fn expected_extensions(cfg: &Config) -> Result<Vec<String>> {
    if preinstall_enabled(cfg.get("VSCODE_PREINSTALL"))? {
        parse_extensions(cfg.get("VSCODE_EXTENSIONS"))
    } else {
        Ok(Vec::new())
    }
}

/// `publisher.name`: two non-empty parts of ASCII letters, digits, `-` and `_`.
fn valid_extension_id(id: &str) -> bool {
    let part = |p: &str| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    matches!(id.split_once('.'), Some((publisher, name)) if part(publisher) && part(name))
}

/// Where the participants' interpreter can be, most likely first: the conda env `env` as
/// the arena image lays it out (`/opt/<env>`, verified on arena-env 9.1), then the usual
/// conda roots, then the uv venv `hetzner_setup.sh` builds in the repo. An empty or odd
/// `env` (anything but a plain conda env name) contributes nothing.
pub fn python_candidates(env: &str, repo_path: &str) -> Vec<String> {
    let env = env.trim();
    let plain = !env.is_empty() && env.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    let mut out = Vec::new();
    if plain {
        out.push(format!("/opt/{env}/bin/python"));
        for root in ["/opt/conda", "~/miniconda3", "~/miniforge3", "~/anaconda3"] {
            out.push(format!("{root}/envs/{env}/bin/python"));
        }
    }
    out.push(format!("{}/.venv/bin/python", repo_path.trim_end_matches('/')));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preinstall_flag() {
        let table: &[(Option<&str>, Option<bool>)] = &[
            (None, Some(true)),
            (Some(""), Some(true)),
            (Some("1"), Some(true)),
            (Some(" Yes "), Some(true)),
            (Some("true"), Some(true)),
            (Some("0"), Some(false)),
            (Some("off"), Some(false)),
            (Some("FALSE"), Some(false)),
            (Some("2"), None),
            (Some("later"), None),
        ];
        for (raw, want) in table {
            match (preinstall_enabled(*raw), want) {
                (Ok(got), Some(w)) => assert_eq!(got, *w, "{raw:?}"),
                (Err(e), None) => assert!(e.to_string().contains("VSCODE_PREINSTALL"), "{raw:?}: {e}"),
                (got, want) => panic!("{raw:?}: got {got:?}, want {want:?}"),
            }
        }
    }

    #[test]
    fn extensions_default_list_none_and_validation() {
        let defaults: Vec<String> = DEFAULT_EXTENSIONS.iter().map(|s| s.to_string()).collect();
        assert_eq!(parse_extensions(None).unwrap(), defaults);
        assert_eq!(parse_extensions(Some("  ")).unwrap(), defaults);
        assert_eq!(parse_extensions(Some("none")).unwrap(), Vec::<String>::new());
        assert_eq!(
            parse_extensions(Some(" ms-python.python, MS-Toolsai.Jupyter,,ms-python.python , golang.go_x ")).unwrap(),
            ["ms-python.python", "ms-toolsai.jupyter", "golang.go_x"]
        );
        for bad in [
            "python",                  // no publisher
            "a.b.c",                   // two dots
            ".b",                      // empty publisher
            "a.",                      // empty name
            "ms-python.python@2024.1", // version pins aren't supported
            "a.b;rm -rf ~",            // shell
            "a.b c.d",                 // space instead of comma
            "a.b,$(id).x",
            "ms-python.pythön",
        ] {
            let e = parse_extensions(Some(bad)).unwrap_err();
            assert!(matches!(e, Error::Config(_)) && e.to_string().contains("VSCODE_EXTENSIONS"), "{bad}: {e}");
        }
    }

    #[test]
    fn config_switches_and_interpreter_candidates() {
        let cfg = |s: &str| Config::parse(s);
        let on = VscodeSetup::from_config(&cfg(""), "/root/ARENA_materials").unwrap().unwrap();
        assert_eq!(on.extensions, DEFAULT_EXTENSIONS);
        assert_eq!(
            on.pythons,
            [
                "/opt/arena-env/bin/python",
                "/opt/conda/envs/arena-env/bin/python",
                "~/miniconda3/envs/arena-env/bin/python",
                "~/miniforge3/envs/arena-env/bin/python",
                "~/anaconda3/envs/arena-env/bin/python",
                "/root/ARENA_materials/.venv/bin/python",
            ]
        );
        assert_eq!(VscodeSetup::from_config(&cfg("VSCODE_PREINSTALL=0"), "/r").unwrap(), None);
        // A bad extension list fails even though nothing else is wrong; switched off, it's moot.
        assert!(VscodeSetup::from_config(&cfg("VSCODE_EXTENSIONS=oops"), "/r").is_err());
        assert_eq!(VscodeSetup::from_config(&cfg("VSCODE_PREINSTALL=no\nVSCODE_EXTENSIONS=oops"), "/r").unwrap(), None);
        // CONDA_ENV: another env name, or none at all (then only the venv).
        let other = VscodeSetup::from_config(&cfg("CONDA_ENV=course"), "/r/").unwrap().unwrap();
        assert_eq!(other.pythons.first().map(String::as_str), Some("/opt/course/bin/python"));
        assert_eq!(other.pythons.last().map(String::as_str), Some("/r/.venv/bin/python"));
        assert_eq!(python_candidates("", "/r"), ["/r/.venv/bin/python"]);
        assert_eq!(python_candidates("a b; x", "/r"), ["/r/.venv/bin/python"]);
        // The deep check's expectations follow the same switches.
        assert_eq!(expected_extensions(&cfg("")).unwrap(), DEFAULT_EXTENSIONS);
        assert_eq!(expected_extensions(&cfg("VSCODE_PREINSTALL=off")).unwrap(), Vec::<String>::new());
        assert_eq!(expected_extensions(&cfg("VSCODE_EXTENSIONS=a.b")).unwrap(), ["a.b"]);
    }

    fn setup() -> VscodeSetup {
        VscodeSetup {
            extensions: vec!["ms-python.python".into(), "ms-toolsai.jupyter".into()],
            pythons: vec!["/opt/arena-env/bin/python".into(), "/root/it's/.venv/bin/python".into()],
        }
    }

    #[test]
    fn command_carries_the_script_budget_and_settings() {
        let cmd = setup().remote_command(WARMUP_TIMEOUT);
        assert!(cmd.starts_with("ARENA_VSCODE_BUDGET=270 ARENA_VSCODE_EXTENSIONS='ms-python.python,ms-toolsai.jupyter' bash -c "), "{cmd}");
        assert!(cmd.contains(&crate::base64::encode(WARMUP_SCRIPT.as_bytes())));
        assert!(cmd.ends_with(r#" arena-vscode-warmup '/opt/arena-env/bin/python' '/root/it'\''s/.venv/bin/python' </dev/null"#), "{cmd}");
        // One argv string on the remote side: far below Linux's 128 KiB per-arg cap.
        assert!(cmd.len() < 64 * 1024, "{} bytes", cmd.len());
        // A budget too small to subtract the margin from still leaves the script some time.
        assert!(setup().remote_command(Duration::from_secs(20)).starts_with("ARENA_VSCODE_BUDGET=10 "));
        let s = setup().summary();
        assert!(s.contains("ms-python.python, ms-toolsai.jupyter") && s.contains("/opt/arena-env/bin/python"), "{s}");
    }

    fn have(tool: &str) -> bool {
        std::process::Command::new("sh")
            .args(["-c", &format!("command -v {tool}")])
            .output()
            .is_ok_and(|o| o.status.success())
    }

    #[test]
    fn script_is_valid_bash() {
        let out = std::process::Command::new("bash")
            .arg("-n")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/src/vscode_setup.sh"))
            .output()
            .expect("bash");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }

    #[test]
    fn settings_merge_python_compiles() {
        if !have("python3") {
            eprintln!("python3 not installed — skipping");
            return;
        }
        let start = WARMUP_SCRIPT.find("<<'PYEOF'\n").expect("python heredoc") + "<<'PYEOF'\n".len();
        let len = WARMUP_SCRIPT[start..].find("\nPYEOF\n").expect("heredoc end");
        let dir = std::env::temp_dir().join(format!("arena-vscode-pycompile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("merge_settings.py");
        std::fs::write(&file, &WARMUP_SCRIPT[start..start + len]).unwrap();
        let out = std::process::Command::new("python3").args(["-I", "-m", "py_compile"]).arg(&file).output().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }

    /// The delivered command, run for real against a fake pod: `$HOME` in a temp dir, stub
    /// `curl` (answers the update API from the recorded fixtures, "downloads" locally built
    /// tarballs) and `uname` first on PATH, a fake server whose `code-server` records its
    /// arguments and "installs" extensions — no network, no VS Code.
    #[cfg(target_os = "linux")]
    mod on_a_fake_pod {
        use super::super::*;
        use super::have;
        use std::os::unix::fs::PermissionsExt;
        use std::path::{Path, PathBuf};
        use std::process::{Command, Output};

        const SERVER_FIXTURE: &str = include_str!("fixtures/vscode_update_server-linux-x64_2026-10-08.json");
        const CLI_FIXTURE: &str = include_str!("fixtures/vscode_update_cli-alpine-x64_2026-10-08.json");
        /// The fixtures' commit, url and sha256 — the parts each test release replaces.
        const COMMIT: &str = "2a59476c9bfcb90b3ddc372c36762471b7dfad1c";
        const SERVER_URL: &str = "https://vscode.download.prss.microsoft.com/dbazure/download/stable/2a59476c9bfcb90b3ddc372c36762471b7dfad1c/vscode-server-linux-x64.tar.gz";
        const SERVER_SHA: &str = "5711ed2690e550d52ca8531bcd349e273ed7b4e0b22180767a9338fd572de2ba";
        const CLI_URL: &str = "https://vscode.download.prss.microsoft.com/dbazure/download/stable/2a59476c9bfcb90b3ddc372c36762471b7dfad1c/vscode_cli_alpine_x64_cli.tar.gz";
        const CLI_SHA: &str = "523b76cbd077413f1dbb71ec5de48d076d82e0bd564adeea3d5568fdbe95c18e";

        const CURL: &str = r#"#!/bin/sh
out="" url=""
while [ $# -gt 0 ]; do
  case $1 in
    -o) out=$2; shift ;;
    https://*) url=$1 ;;
  esac
  shift
done
echo "$url" >>"$STUB/curl.log"
case $url in
  */api/update/*/stable/latest)
    p=${url#*/api/update/}; p=${p%/stable/latest}
    [ -f "$STUB/api/$p.json" ] || { echo "curl: (22) The requested URL returned error: 404" >&2; exit 22; }
    cat "$STUB/api/$p.json" ;;
  https://example.invalid/*)
    f="$STUB/files/${url##*/}"
    [ -f "$f" ] || { echo "curl: (22) The requested URL returned error: 404" >&2; exit 22; }
    cp "$f" "$out" ;;
  *) echo "curl: (6) unexpected url $url" >&2; exit 6 ;;
esac
"#;
        const UNAME: &str = "#!/bin/sh\ncase \"$1\" in -m) echo \"$STUB_ARCH\" ;; *) echo Linux ;; esac\n";
        /// The fake server's CLI: records its argv; "installs" each extension as `<id>-1.0.0`
        /// (unless STUB_EXT_FAIL is set).
        const CODE_SERVER: &str = r#"#!/bin/sh
echo "$*" >>"$STUB/code-server.log"
ext=""
while [ $# -gt 0 ]; do
  case $1 in
    --extensions-dir) ext=$2; shift ;;
    --install-extension) [ -n "${STUB_EXT_FAIL:-}" ] || mkdir -p "$ext/$2-1.0.0"; shift ;;
  esac
  shift
done
[ -z "${STUB_EXT_FAIL:-}" ] || { echo "Failed Installing Extensions: marketplace unreachable" >&2; exit 1; }
"#;

        struct Pod {
            root: PathBuf,
        }

        impl Drop for Pod {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.root);
            }
        }

        fn write_exec(path: &Path, body: &str) {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        fn sh(dir: &Path, cmd: &str) -> String {
            let out = Command::new("sh").arg("-c").arg(cmd).current_dir(dir).output().unwrap();
            assert!(out.status.success(), "{cmd}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }

        impl Pod {
            /// `None` when a tool the script needs isn't installed here.
            fn new(tag: &str) -> Option<Self> {
                for tool in ["bash", "python3", "sha256sum", "tar", "find", "timeout", "base64"] {
                    if !have(tool) {
                        eprintln!("{tool} not installed — skipping");
                        return None;
                    }
                }
                let root = std::env::temp_dir().join(format!("arena-vscode-{tag}-{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&root);
                std::fs::create_dir_all(root.join("home")).unwrap();
                let pod = Pod { root };
                write_exec(&pod.root.join("stubs/curl"), CURL);
                write_exec(&pod.root.join("stubs/uname"), UNAME);
                // The arena env's python, where the image keeps it (a link to the real one).
                let python3 = sh(&pod.root, "command -v python3");
                std::fs::create_dir_all(pod.root.join("opt/arena-env/bin")).unwrap();
                std::os::unix::fs::symlink(python3, pod.root.join("opt/arena-env/bin/python")).unwrap();
                Some(pod)
            }

            fn vs(&self) -> PathBuf {
                self.root.join("home/.vscode-server")
            }

            fn python(&self) -> String {
                self.root.join("opt/arena-env/bin/python").to_string_lossy().into_owned()
            }

            fn setup(&self) -> VscodeSetup {
                VscodeSetup {
                    extensions: DEFAULT_EXTENSIONS.iter().map(|s| s.to_string()).collect(),
                    pythons: vec!["~/miniconda3/envs/arena-env/bin/python".into(), self.python()],
                }
            }

            /// Publish a release for `arch` (`x64`/`arm64`): a server tarball (top dir
            /// `vscode-server-linux-<arch>`, as the real one) and a CLI tarball (one `code`),
            /// plus the update API's answers — the recorded fixtures with only url and sha256
            /// replaced, then passed through `edit` (to break them).
            fn publish(&self, arch: &str, edit: impl Fn(String) -> String) {
                let build = self.root.join(format!("build-{arch}"));
                write_exec(&build.join(format!("vscode-server-linux-{arch}/bin/code-server")), CODE_SERVER);
                write_exec(&build.join(format!("vscode-server-linux-{arch}/node")), "#!/bin/sh\n");
                write_exec(&build.join("code"), "#!/bin/sh\necho fake code cli\n");
                std::fs::create_dir_all(self.root.join("files")).unwrap();
                std::fs::create_dir_all(self.root.join("api")).unwrap();
                let files = self.root.join("files");
                let srv = files.join(format!("server-{arch}.tgz"));
                let cli = files.join(format!("cli-{arch}.tgz"));
                sh(&build, &format!("tar -czf '{}' vscode-server-linux-{arch}", srv.display()));
                sh(&build, &format!("tar -czf '{}' code", cli.display()));
                let sha = |p: &Path| sh(&self.root, &format!("sha256sum '{}' | cut -d ' ' -f 1", p.display()));
                let server_json = SERVER_FIXTURE
                    .replace(SERVER_URL, &format!("https://example.invalid/server-{arch}.tgz"))
                    .replace(SERVER_SHA, &sha(&srv));
                let cli_json = CLI_FIXTURE
                    .replace(CLI_URL, &format!("https://example.invalid/cli-{arch}.tgz"))
                    .replace(CLI_SHA, &sha(&cli));
                std::fs::write(self.root.join(format!("api/server-linux-{arch}.json")), edit(server_json)).unwrap();
                std::fs::write(self.root.join(format!("api/cli-alpine-{arch}.json")), edit(cli_json)).unwrap();
            }

            fn run_with(&self, uname_m: &str, setup: &VscodeSetup, env: &[(&str, &str)]) -> Output {
                let path = format!("{}:{}", self.root.join("stubs").display(), std::env::var("PATH").unwrap_or_default());
                let mut c = Command::new("sh");
                c.arg("-c")
                    .arg(setup.remote_command(WARMUP_TIMEOUT))
                    .env("PATH", path)
                    .env("HOME", self.root.join("home"))
                    .env("STUB", &self.root)
                    .env("STUB_ARCH", uname_m);
                for (k, v) in env {
                    c.env(k, v);
                }
                c.output().unwrap()
            }

            fn run(&self, uname_m: &str) -> Output {
                self.run_with(uname_m, &self.setup(), &[])
            }

            fn log(&self, name: &str) -> Vec<String> {
                std::fs::read_to_string(self.root.join(name)).unwrap_or_default().lines().map(String::from).collect()
            }

            fn settings(&self) -> String {
                std::fs::read_to_string(self.vs().join("data/Machine/settings.json")).unwrap_or_default()
            }

            fn server(&self) -> PathBuf {
                self.vs().join(format!("cli/servers/Stable-{COMMIT}/server"))
            }

            /// Nothing left over in the temp area (the `.arena-warmup.*` dirs are removed).
            fn assert_clean(&self) {
                let leftovers: Vec<String> = std::fs::read_dir(self.vs())
                    .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
                    .unwrap_or_default();
                assert!(!leftovers.iter().any(|n| n.starts_with(".arena-warmup")), "{leftovers:?}");
            }
        }

        fn ok(out: &Output) -> String {
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            assert!(out.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
            stdout
        }

        /// Exit 3 with the reason as the last stderr line — what the setup warning shows.
        fn warned(out: &Output) -> String {
            assert_eq!(out.status.code(), Some(3), "{}", String::from_utf8_lossy(&out.stdout));
            let stderr = String::from_utf8_lossy(&out.stderr);
            let last = stderr.lines().filter(|l| !l.trim().is_empty()).last().unwrap_or("").to_string();
            assert!(last.starts_with("vscode warm-up incomplete: "), "{stderr}");
            last
        }

        #[test]
        fn fresh_x64_pod_gets_server_cli_extensions_and_settings_then_reruns_skip_everything() {
            let Some(pod) = Pod::new("fresh") else { return };
            pod.publish("x64", |j| j);
            let stdout = ok(&pod.run("x86_64"));
            assert!(stdout.contains("installed server VS Code 1.141.0 (2a59476, x64)"), "{stdout}");

            // The Remote-SSH layout: server, CLI, lru.json, the legacy link.
            assert!(pod.server().join("bin/code-server").is_file());
            assert!(pod.server().join("node").is_file());
            let cli = pod.vs().join(format!("code-{COMMIT}"));
            assert_eq!(std::fs::metadata(&cli).unwrap().permissions().mode() & 0o777, 0o755);
            assert_eq!(
                std::fs::read_to_string(pod.vs().join("cli/servers/lru.json")).unwrap(),
                format!("[\"Stable-{COMMIT}\"]")
            );
            let legacy = pod.vs().join(format!("bin/{COMMIT}"));
            assert_eq!(std::fs::canonicalize(&legacy).unwrap(), std::fs::canonicalize(pod.server()).unwrap());

            // The update API was asked for this CPU's builds, and exactly those were fetched.
            assert_eq!(
                pod.log("curl.log"),
                [
                    "https://update.code.visualstudio.com/api/update/server-linux-x64/stable/latest",
                    "https://example.invalid/server-x64.tgz",
                    "https://update.code.visualstudio.com/api/update/cli-alpine-x64/stable/latest",
                    "https://example.invalid/cli-x64.tgz",
                ]
            );
            // Extensions: one install call, into the shared dir, by the new server.
            let installs = pod.log("code-server.log");
            assert_eq!(
                installs,
                [format!(
                    "--accept-server-license-terms --extensions-dir {}/extensions --install-extension ms-python.python \
                     --install-extension ms-python.vscode-pylance --install-extension ms-toolsai.jupyter",
                    pod.vs().display()
                )]
            );
            for id in DEFAULT_EXTENSIONS {
                assert!(pod.vs().join(format!("extensions/{id}-1.0.0")).is_dir(), "{id}");
            }
            // Machine settings: the first candidate that exists (the ~/ one doesn't).
            let settings: serde_json::Value = serde_json::from_str(&pod.settings()).unwrap();
            assert_eq!(settings, serde_json::json!({ "python.defaultInterpreterPath": pod.python() }));
            pod.assert_clean();

            // Again: one API question, no download, no install, settings untouched.
            let before = pod.settings();
            let stdout = ok(&pod.run("x86_64"));
            assert!(stdout.contains("already present"), "{stdout}");
            assert_eq!(pod.log("curl.log").len(), 5, "{:?}", pod.log("curl.log"));
            assert_eq!(pod.log("curl.log")[4], "https://update.code.visualstudio.com/api/update/server-linux-x64/stable/latest");
            assert_eq!(pod.log("code-server.log").len(), 1, "no second install");
            assert_eq!(pod.settings(), before);
            pod.assert_clean();
        }

        #[test]
        fn arm64_pods_get_the_arm64_builds_and_odd_cpus_get_none() {
            let Some(pod) = Pod::new("arm") else { return };
            pod.publish("arm64", |j| j);
            ok(&pod.run("aarch64"));
            let log = pod.log("curl.log");
            assert!(log.iter().any(|u| u.ends_with("/server-linux-arm64/stable/latest")), "{log:?}");
            assert!(log.iter().any(|u| u.ends_with("/cli-alpine-arm64/stable/latest")), "{log:?}");
            assert!(log.iter().all(|u| !u.contains("x64")), "{log:?}");
            assert!(pod.server().join("bin/code-server").is_file());

            let Some(pod) = Pod::new("armv7") else { return };
            pod.publish("x64", |j| j);
            let why = warned(&pod.run("armv7l"));
            assert!(why.contains("unsupported CPU armv7l"), "{why}");
            assert!(pod.log("curl.log").is_empty(), "nothing asked or fetched");
            assert!(!pod.vs().join("cli").exists());
            // The local part still happened.
            assert!(pod.settings().contains("python.defaultInterpreterPath"));
            pod.assert_clean();
        }

        #[test]
        fn unexpected_update_api_answers_install_nothing() {
            let cases: &[(&str, fn(String) -> String)] = &[
                ("html error page", |_| "<html><body>Service Unavailable</body></html>".into()),
                ("no sha256", |j| j.replace("\"sha256hash\"", "\"sha1hash\"")),
                ("short commit", |j| j.replace(&format!("\"version\":\"{COMMIT}\""), "\"version\":\"2a59476\"")),
                ("plain http", |j| j.replace("https://example.invalid", "http://example.invalid")),
                ("empty", |_| String::new()),
            ];
            for (label, edit) in cases {
                let Some(pod) = Pod::new("shape") else { return };
                pod.publish("x64", edit);
                let why = warned(&pod.run("x86_64"));
                assert!(why.contains("unexpected shape"), "{label}: {why}");
                assert_eq!(pod.log("curl.log").len(), 1, "{label}: asked once, fetched nothing");
                assert!(!pod.vs().join("cli").exists() && !pod.vs().join("extensions").exists(), "{label}");
                assert!(why.contains("no server for the latest release"), "{label}: {why}");
                pod.assert_clean();
            }
        }

        #[test]
        fn json_escaped_slashes_still_parse() {
            let Some(pod) = Pod::new("escaped") else { return };
            pod.publish("x64", |j| j.replace('/', "\\/"));
            ok(&pod.run("x86_64"));
            assert!(pod.server().join("bin/code-server").is_file());
        }

        #[test]
        fn a_bad_download_is_never_installed() {
            let Some(pod) = Pod::new("checksum") else { return };
            pod.publish("x64", |j| j);
            // Swap the server tarball for another file: its sha256 no longer matches.
            std::fs::write(pod.root.join("files/server-x64.tgz"), "not the server").unwrap();
            let why = warned(&pod.run("x86_64"));
            assert!(why.contains("server: checksum mismatch"), "{why}");
            assert!(!pod.vs().join(format!("cli/servers/Stable-{COMMIT}")).exists());
            assert!(!pod.vs().join(format!("bin/{COMMIT}")).exists());
            pod.assert_clean();

            // And a failed download (404).
            std::fs::remove_file(pod.root.join("files/server-x64.tgz")).unwrap();
            let why = warned(&pod.run("x86_64"));
            assert!(why.contains("server: download failed: curl: (22)"), "{why}");
            assert!(!pod.server().exists());
            pod.assert_clean();
        }

        #[test]
        fn settings_are_merged_never_clobbered() {
            let Some(pod) = Pod::new("settings") else { return };
            pod.publish("x64", |j| j);
            let file = pod.vs().join("data/Machine/settings.json");
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            // Other keys are kept, ours is added.
            std::fs::write(&file, "{\"editor.fontSize\": 14, \"files.exclude\": {\"**/.git\": true}}").unwrap();
            ok(&pod.run("x86_64"));
            let v: serde_json::Value = serde_json::from_str(&pod.settings()).unwrap();
            assert_eq!(
                v,
                serde_json::json!({
                    "editor.fontSize": 14,
                    "files.exclude": { "**/.git": true },
                    "python.defaultInterpreterPath": pod.python(),
                })
            );
            // A choice already made (by a participant, or an earlier run) stands.
            std::fs::write(&file, "{\"python.defaultInterpreterPath\": \"/custom/python\"}").unwrap();
            ok(&pod.run("x86_64"));
            assert_eq!(pod.settings(), "{\"python.defaultInterpreterPath\": \"/custom/python\"}");
            // A file it can't read faithfully (JSON with comments) is left alone — a warning.
            let jsonc = "// mine\n{\"editor.fontSize\": 12}\n";
            std::fs::write(&file, jsonc).unwrap();
            let why = warned(&pod.run("x86_64"));
            assert!(why.contains("not plain JSON"), "{why}");
            assert_eq!(pod.settings(), jsonc);

            // `~/` candidates are the pod user's home; with no candidate at all, it's a warning
            // and the rest still happens.
            std::fs::remove_file(&file).unwrap();
            let home_py = pod.root.join("home/miniconda3/envs/arena-env/bin/python");
            std::fs::create_dir_all(home_py.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(pod.python(), &home_py).unwrap();
            ok(&pod.run("x86_64"));
            assert!(pod.settings().contains(&*home_py.to_string_lossy()), "{}", pod.settings());
            std::fs::remove_file(&file).unwrap();
            let none = VscodeSetup { extensions: vec![], pythons: vec!["/nope/bin/python".into()] };
            let why = warned(&pod.run_with("x86_64", &none, &[]));
            assert!(why.contains("no arena python found"), "{why}");
            assert!(!file.exists());
        }

        #[test]
        fn lru_list_is_extended_and_odd_files_left_alone() {
            let Some(pod) = Pod::new("lru") else { return };
            pod.publish("x64", |j| j);
            let lru = pod.vs().join("cli/servers/lru.json");
            std::fs::create_dir_all(lru.parent().unwrap()).unwrap();
            std::fs::write(&lru, "[\"Stable-0123456789012345678901234567890123456789\"]\n").unwrap();
            ok(&pod.run("x86_64"));
            assert_eq!(
                std::fs::read_to_string(&lru).unwrap(),
                format!("[\"Stable-{COMMIT}\",\"Stable-0123456789012345678901234567890123456789\"]")
            );

            let Some(pod) = Pod::new("lru-odd") else { return };
            pod.publish("x64", |j| j);
            let lru = pod.vs().join("cli/servers/lru.json");
            std::fs::create_dir_all(lru.parent().unwrap()).unwrap();
            std::fs::write(&lru, "{\"servers\": []}").unwrap();
            let why = warned(&pod.run("x86_64"));
            assert!(why.contains("lru.json is not a JSON list - left alone"), "{why}");
            assert_eq!(std::fs::read_to_string(&lru).unwrap(), "{\"servers\": []}");
            assert!(pod.server().join("bin/code-server").is_file(), "the server is installed regardless");
        }

        #[test]
        fn extensions_install_only_whats_missing_and_failures_are_named() {
            let Some(pod) = Pod::new("ext") else { return };
            pod.publish("x64", |j| j);
            // Pylance is there already; `ms-toolsai.jupyter-keymap` is not `ms-toolsai.jupyter`.
            std::fs::create_dir_all(pod.vs().join("extensions/ms-python.vscode-pylance-2026.1.0")).unwrap();
            std::fs::create_dir_all(pod.vs().join("extensions/ms-toolsai.jupyter-keymap-1.1.2")).unwrap();
            let why = warned(&pod.run_with("x86_64", &pod.setup(), &[("STUB_EXT_FAIL", "1")]));
            assert!(
                why.contains("extensions not installed: ms-python.python ms-toolsai.jupyter (exit 1: Failed Installing Extensions: marketplace unreachable)"),
                "{why}"
            );
            ok(&pod.run("x86_64"));
            let installs = pod.log("code-server.log");
            assert_eq!(installs.len(), 2);
            for line in &installs {
                assert!(!line.contains("vscode-pylance"), "{line}");
                assert!(line.contains("--install-extension ms-python.python --install-extension ms-toolsai.jupyter"), "{line}");
            }
        }

        #[test]
        fn an_incomplete_server_dir_is_left_to_whoever_is_installing_it() {
            let Some(pod) = Pod::new("busy") else { return };
            pod.publish("x64", |j| j);
            std::fs::create_dir_all(pod.server().join("bin")).unwrap();
            let why = warned(&pod.run("x86_64"));
            assert!(why.contains("exists but is incomplete - left alone"), "{why}");
            assert!(!pod.server().join("bin/code-server").exists());
            assert!(!pod.log("curl.log").iter().any(|u| u.contains("server-x64.tgz")), "not downloaded");
        }
    }
}
