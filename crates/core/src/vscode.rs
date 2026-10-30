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

/// Which machines get the memory-saving machine settings (Pylance's indexing off, diagnostics for
/// open files only, the env/caches/data out of its analysis, the file watcher and search): a
/// cx33 (8 GB) ran out of memory with Pylance and the course's kernels (ops feedback #4). Decided
/// per provider by [`crate::setup::provisioning_steps`]; the RAM is read on the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LowMemory {
    /// Every machine: a CPU VM (Hetzner), whose kernels keep their models in the same RAM as
    /// VS Code — so even a bigger one fills up — and whose image (`arena image build`) carries the
    /// settings to every VM made from it.
    Always,
    /// Only a machine with less RAM than this many GiB ([`LOW_MEMORY_BELOW_GIB`]; a container's
    /// own memory limit counts): a GPU pod's models live on the GPU, and one with plenty of RAM
    /// keeps Pylance's indexing (auto-imports, workspace symbols).
    BelowGib(u32),
}

/// A GPU machine under this much RAM (GiB) gets the memory-saving settings: the 16 GB class
/// and below (a "16 GB" machine reads ~15.6 GiB).
pub const LOW_MEMORY_BELOW_GIB: u32 = 20;

impl LowMemory {
    /// The script's `ARENA_VSCODE_LOW_MEMORY`.
    fn env_value(self) -> String {
        match self {
            LowMemory::Always => "always".into(),
            LowMemory::BelowGib(gib) => gib.to_string(),
        }
    }

    fn describe(self) -> String {
        match self {
            LowMemory::Always => "always".into(),
            LowMemory::BelowGib(gib) => format!("if under {gib} GiB of RAM"),
        }
    }
}

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
    /// The script gets `timeout` minus [`SSH_MARGIN`] to work in; `low` says whether the machine
    /// gets the memory-saving settings.
    pub fn remote_command(&self, timeout: Duration, low: LowMemory) -> String {
        let budget = timeout.saturating_sub(SSH_MARGIN).max(Duration::from_secs(10)).as_secs();
        let pythons: Vec<String> = self.pythons.iter().map(|p| shell_quote(p)).collect();
        format!(
            "ARENA_VSCODE_BUDGET={budget} ARENA_VSCODE_EXTENSIONS={exts} ARENA_VSCODE_LOW_MEMORY={low} \
             bash -c \"$(printf %s {script} | base64 -d)\" arena-vscode-warmup {pythons} </dev/null",
            exts = shell_quote(&self.extensions.join(",")),
            low = low.env_value(),
            script = crate::base64::encode(WARMUP_SCRIPT.as_bytes()),
            pythons = pythons.join(" "),
        )
    }

    /// What the dry-run shows in place of [`Self::remote_command`] (a base64 blob).
    pub fn summary(&self, low: LowMemory) -> String {
        let extensions =
            if self.extensions.is_empty() { "no extensions".to_string() } else { self.extensions.join(", ") };
        format!(
            "latest stable VS Code server (x64/arm64) into ~/.vscode-server (Remote-SSH layout); extensions: \
             {extensions}; python.defaultInterpreterPath (if unset) → first of: {}; memory-saving settings \
             (Pylance indexing off, excludes; where unset) {} — each part skipped when already present",
            self.pythons.join(", "),
            low.describe(),
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
        let cmd = setup().remote_command(WARMUP_TIMEOUT, LowMemory::BelowGib(20));
        assert!(
            cmd.starts_with(
                "ARENA_VSCODE_BUDGET=270 ARENA_VSCODE_EXTENSIONS='ms-python.python,ms-toolsai.jupyter' \
                 ARENA_VSCODE_LOW_MEMORY=20 bash -c "
            ),
            "{cmd}"
        );
        assert!(cmd.contains(&crate::base64::encode(WARMUP_SCRIPT.as_bytes())));
        assert!(cmd.ends_with(r#" arena-vscode-warmup '/opt/arena-env/bin/python' '/root/it'\''s/.venv/bin/python' </dev/null"#), "{cmd}");
        // One argv string on the remote side: far below Linux's 128 KiB per-arg cap.
        assert!(cmd.len() < 64 * 1024, "{} bytes", cmd.len());
        // A budget too small to subtract the margin from still leaves the script some time.
        assert!(setup().remote_command(Duration::from_secs(20), LowMemory::Always).starts_with("ARENA_VSCODE_BUDGET=10 "));
        assert!(setup().remote_command(WARMUP_TIMEOUT, LowMemory::Always).contains(" ARENA_VSCODE_LOW_MEMORY=always bash -c "));
        let s = setup().summary(LowMemory::BelowGib(20));
        assert!(s.contains("ms-python.python, ms-toolsai.jupyter") && s.contains("/opt/arena-env/bin/python"), "{s}");
        assert!(s.contains("memory-saving settings (Pylance indexing off, excludes; where unset) if under 20 GiB of RAM"), "{s}");
        assert!(setup().summary(LowMemory::Always).contains("(Pylance indexing off, excludes; where unset) always —"));
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
            crate::remote::write_test_script(path, body).unwrap();
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

            /// The machine's RAM as the script reads it: `MemTotal` (kB) in a stand-in
            /// /proc/meminfo, and a stand-in /sys/fs/cgroup with these files (name, contents),
            /// the process in the namespace's top cgroup (`0::/`), cgroup v2 mounted there.
            fn ram(&self, mem_total_kb: u64, cgroup: &[(&str, &str)]) {
                self.ram_in(mem_total_kb, "0::/\n", &[("/", "", "cgroup2", "rw")], cgroup);
            }

            /// [`Self::ram`] with `proc_cgroup` as /proc/self/cgroup and these cgroup mounts in
            /// mountinfo: (the cgroup at the mount's top, the mount point under the stand-in
            /// /sys/fs/cgroup, fstype, super options).
            fn ram_in(&self, mem_total_kb: u64, proc_cgroup: &str, mounts: &[(&str, &str, &str, &str)], cgroup: &[(&str, &str)]) {
                std::fs::write(self.root.join("proc-cgroup"), proc_cgroup).unwrap();
                let cg = self.root.join("cgroup");
                let mut mountinfo = String::from("22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw\n");
                for (i, (top, at, fstype, opts)) in mounts.iter().enumerate() {
                    let point = if at.is_empty() { cg.clone() } else { cg.join(at) };
                    mountinfo.push_str(&format!("{} 22 0:{i} {top} {} rw,nosuid shared:9 - {fstype} {fstype} {opts}\n", 30 + i, point.display()));
                }
                std::fs::write(self.root.join("mountinfo"), mountinfo).unwrap();
                std::fs::write(self.root.join("meminfo"), format!("MemTotal:       {mem_total_kb} kB\nMemFree: 1 kB\n")).unwrap();
                let _ = std::fs::remove_dir_all(self.root.join("cgroup"));
                for (name, body) in cgroup {
                    let f = self.root.join("cgroup").join(name);
                    std::fs::create_dir_all(f.parent().unwrap()).unwrap();
                    std::fs::write(f, body).unwrap();
                }
            }

            /// One run, without the memory-saving settings (`BelowGib(0)`: no machine).
            fn run_with(&self, uname_m: &str, setup: &VscodeSetup, env: &[(&str, &str)]) -> Output {
                self.run_low(uname_m, setup, LowMemory::BelowGib(0), env)
            }

            fn run_low(&self, uname_m: &str, setup: &VscodeSetup, low: LowMemory, env: &[(&str, &str)]) -> Output {
                let path = format!("{}:{}", self.root.join("stubs").display(), std::env::var("PATH").unwrap_or_default());
                if !self.root.join("meminfo").exists() {
                    self.ram(64 << 20, &[]);
                }
                let mut c = Command::new("sh");
                c.arg("-c")
                    .arg(setup.remote_command(WARMUP_TIMEOUT, low))
                    .env("PATH", path)
                    .env("HOME", self.root.join("home"))
                    .env("STUB", &self.root)
                    .env("STUB_ARCH", uname_m)
                    .env("ARENA_MEMINFO", self.root.join("meminfo"))
                    .env("ARENA_CGROUP", self.root.join("cgroup"))
                    .env("ARENA_PROC_CGROUP", self.root.join("proc-cgroup"))
                    .env("ARENA_MOUNTINFO", self.root.join("mountinfo"));
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
            let last = stderr.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("").to_string();
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
            type Case = (&'static str, fn(String) -> String);
            let cases: &[Case] = &[
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
            // JSONC, as VS Code writes and reads it (comments, a trailing comma): merged into the
            // text, every byte that was there kept.
            let jsonc = "// mine\n{\n    \"editor.fontSize\": 12, // big\n}\n";
            std::fs::write(&file, jsonc).unwrap();
            ok(&pod.run("x86_64"));
            assert_eq!(
                pod.settings(),
                format!("// mine\n{{\n    \"editor.fontSize\": 12, // big\n    \"python.defaultInterpreterPath\": \"{}\"\n}}\n", pod.python())
            );
            // A file it can't read the way VS Code does is left alone, byte for byte — a warning.
            for bad in ["{\"editor.fontSize\": 12,, }", "{'editor.fontSize': 12}", "[1]", "{\"a\": 1} {}", "{\"a\": /* open"] {
                std::fs::write(&file, bad).unwrap();
                let why = warned(&pod.run("x86_64"));
                assert!(why.contains("isn't JSON(C) VS Code would read the same way") && why.contains("left alone"), "{bad}: {why}");
                assert_eq!(pod.settings(), bad);
            }
            // One it can't read at all (a directory in its place): the same.
            std::fs::remove_file(&file).unwrap();
            std::fs::create_dir(&file).unwrap();
            let why = warned(&pod.run("x86_64"));
            assert!(why.contains("settings.json can't be read (Is a directory) - left alone"), "{why}");
            std::fs::remove_dir(&file).unwrap();
            std::fs::write(&file, "{}").unwrap();

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

        /// The memory-saving settings, as a fresh machine gets them.
        fn low_memory_settings() -> serde_json::Value {
            serde_json::json!({
                "python.analysis.indexing": false,
                "python.analysis.diagnosticMode": "openFilesOnly",
                "python.analysis.exclude": [
                    "**/.venv", "**/.git", "**/.cache", "**/__pycache__", "**/data", "**/infrastructure", "**/node_modules", "**/.*"
                ],
                "files.watcherExclude": {
                    "**/.venv/**": true, "**/.cache/**": true, "**/__pycache__/**": true, "**/data/**": true, "**/infrastructure/**": true
                },
                "search.exclude": {
                    "**/.venv": true, "**/.cache": true, "**/__pycache__": true, "**/data": true, "**/infrastructure": true
                },
                "files.exclude": { "**/__pycache__": true },
            })
        }

        /// This test's JSONC as JSON: `//` comments and trailing commas out (none of its strings
        /// holds a `//`).
        fn jsonc_value(text: &str) -> serde_json::Value {
            let no_comments: String = text.lines().map(|l| l.split("//").next().unwrap()).collect::<Vec<_>>().join("\n");
            let mut json = String::new();
            for (i, c) in no_comments.char_indices() {
                if c == ',' && no_comments[i + 1..].trim_start().starts_with(['}', ']']) {
                    continue;
                }
                json.push(c);
            }
            serde_json::from_str(&json).unwrap_or_else(|e| panic!("{e}: {json}"))
        }

        /// `small` in `big`, in order: only insertions were made.
        fn only_added_to(small: &str, big: &str) -> bool {
            let mut rest = big.chars();
            small.chars().all(|c| rest.any(|b| b == c))
        }

        /// Ops feedback #4: a cx33 (8 GB) ran out of memory with Pylance and the kernels. Every
        /// CPU VM gets the memory-saving settings; any other machine only under 20 GiB of RAM —
        /// a container's own limit counting, as it sees its host's MemTotal.
        #[test]
        fn memory_saving_settings_go_to_small_machines_only() {
            let Some(pod) = Pod::new("lowmem") else { return };
            pod.publish("x64", |j| j);
            let file = pod.vs().join("data/Machine/settings.json");
            let mut with_py = low_memory_settings();
            with_py["python.defaultInterpreterPath"] = pod.python().into();
            let only_py = serde_json::json!({ "python.defaultInterpreterPath": pod.python() });
            let read = || serde_json::from_str::<serde_json::Value>(&pod.settings()).unwrap();

            // A CPU VM, whatever its RAM.
            pod.ram(64 << 20, &[]);
            let out = ok(&pod.run_low("x86_64", &pod.setup(), LowMemory::Always, &[]));
            assert!(out.contains("memory-saving settings: on (every machine of this kind)"), "{out}");
            assert_eq!(read(), with_py);

            const GIB: u64 = 1 << 20; // in kB
            let below = LowMemory::BelowGib(LOW_MEMORY_BELOW_GIB);
            /// (what, MemTotal in kB, the cgroup files, whether it gets them)
            type Case<'a> = (&'a str, u64, &'a [(&'a str, &'a str)], bool);
            let cases: &[Case] = &[
                ("an 8 GB machine", 8_000_000, &[], true),
                ("a 16 GB machine (15.5 GiB)", 16_270_000, &[], true),
                ("a 64 GiB machine", 64 * GIB, &[], false),
                ("exactly the limit", 20 * GIB, &[], false),
                ("a 16 GiB container on a 512 GiB host (cgroup v2)", 512 * GIB, &[("memory.max", "17179869184\n")], true),
                ("an unlimited container (cgroup v2)", 512 * GIB, &[("memory.max", "max\n")], false),
                ("an 8 GiB container (cgroup v1)", 512 * GIB, &[("memory/memory.limit_in_bytes", "8589934592\n")], true),
                ("an unlimited container (cgroup v1)", 64 * GIB, &[("memory/memory.limit_in_bytes", "9223372036854771712\n")], false),
                ("a limit above the host's RAM", 8 * GIB, &[("memory.max", "68719476736\n")], true),
            ];
            for (label, total, cgroup, on) in cases {
                let _ = std::fs::remove_file(&file);
                pod.ram(*total, cgroup);
                let out = ok(&pod.run_low("x86_64", &pod.setup(), below, &[]));
                let said = if *on { "memory-saving settings: on (" } else { "memory-saving settings: not needed (" };
                assert!(out.contains(said), "{label}: {out}");
                assert_eq!(read(), if *on { with_py.clone() } else { only_py.clone() }, "{label}: {out}");
            }
            // Where the limit is: found through mountinfo (review r1, r2). A container sharing the
            // host's cgroup namespace sees its own at its path; a limit on any cgroup above it
            // holds too; v1 mounts its memory controller alone, combined with another, or with a
            // container's own cgroup bind-mounted at the top.
            const V2: &[(&str, &str, &str, &str)] = &[("/", "", "cgroup2", "rw,nsdelegate")];
            const V1: &[(&str, &str, &str, &str)] = &[("/", "memory", "cgroup", "rw,memory"), ("/", "pids", "cgroup", "rw,pids")];
            /// (what, /proc/self/cgroup, mounts, files, whether it gets them)
            type Nested<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str, &'a str, &'a str)], &'a [(&'a str, &'a str)], bool);
            let nested: &[Nested] = &[
                ("v2, its own", "0::/docker/abc\n", V2, &[("docker/abc/memory.max", "8589934592\n"), ("docker/memory.max", "max\n")], true),
                ("v2, a parent's", "0::/kubepods/pod1/ctr\n", V2, &[("kubepods/pod1/ctr/memory.max", "max\n"), ("kubepods/pod1/memory.max", "8589934592\n")], true),
                ("v2 mounted elsewhere (hybrid)", "0::/docker/abc\n", &[("/", "unified", "cgroup2", "rw")], &[("unified/docker/abc/memory.max", "8589934592\n")], true),
                ("v1, its own", "12:pids:/docker/abc\n9:memory:/docker/abc\n0::/\n", V1, &[("memory/docker/abc/memory.limit_in_bytes", "8589934592\n")], true),
                (
                    "v1, combined with cpu",
                    "4:cpu,memory:/x\n",
                    &[("/", "cpu,memory", "cgroup", "rw,cpu,memory")],
                    &[("cpu,memory/x/memory.limit_in_bytes", "8589934592\n")],
                    true,
                ),
                (
                    "v1, its own bind-mounted at the top",
                    "9:memory:/docker/abc\n",
                    &[("/docker/abc", "memory", "cgroup", "rw,memory")],
                    &[("memory/memory.limit_in_bytes", "8589934592\n")],
                    true,
                ),
                ("v1, another controller's mount", "9:memory:/x\n", &[("/", "cpuset", "cgroup", "rw,cpuset")], &[("cpuset/x/memory.limit_in_bytes", "8589934592\n")], false),
                ("a VM's login session", "0::/user.slice/user-0.slice/session-3.scope\n", V2, &[("user.slice/memory.max", "max\n")], false),
                ("a path with no files", "0::/gone/away\n", V2, &[], false),
            ];
            for (label, proc, mounts, cgroup, on) in nested {
                let _ = std::fs::remove_file(&file);
                pod.ram_in(512 * GIB, proc, mounts, cgroup);
                let out = ok(&pod.run_low("x86_64", &pod.setup(), below, &[]));
                assert_eq!(read(), if *on { with_py.clone() } else { only_py.clone() }, "{label}: {out}");
            }
            pod.ram(8_000_000, &[]);
            let out = ok(&pod.run_low("x86_64", &pod.setup(), below, &[]));
            assert!(out.contains("memory-saving settings: on (7.6 GiB of RAM, under 20)"), "{out}");

            // RAM it can't read: none, said — the interpreter still lands.
            let _ = std::fs::remove_file(&file);
            std::fs::write(pod.root.join("meminfo"), "garbled\n").unwrap();
            let why = warned(&pod.run_low("x86_64", &pod.setup(), below, &[]));
            assert!(why.contains("couldn't read this machine's RAM - no memory-saving settings"), "{why}");
            assert_eq!(read(), only_py);

            // No arena python: the system's python3 merges the memory-saving part (and the
            // missing interpreter is still a warning).
            let _ = std::fs::remove_file(&file);
            let none = VscodeSetup { extensions: vec![], pythons: vec!["/nope/bin/python".into()] };
            let why = warned(&pod.run_low("x86_64", &none, LowMemory::Always, &[]));
            assert!(why.contains("no arena python found"), "{why}");
            assert_eq!(read(), low_memory_settings());
        }

        /// What's there stays: the participant's (or VS Code's) keys and values, comments and
        /// trailing commas; an exclude map gets only the patterns it doesn't name (theirs set to
        /// false stays false); a re-run changes nothing; a big machine's run removes nothing.
        /// Through a symlink, its mode kept.
        #[test]
        fn memory_saving_settings_merge_into_what_is_there() {
            let Some(pod) = Pod::new("lowmem-merge") else { return };
            pod.publish("x64", |j| j);
            let file = pod.vs().join("data/Machine/settings.json");
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            let theirs = "{\n    // the participant's\n    \"python.analysis.indexing\": true,\n    \"files.exclude\": {\n        \
                          \"**/data\": false, // I want to see my data\n    },\n    \"workbench.colorTheme\": \"Default Dark+\",\n}\n";
            // settings.json a link into the participant's dotfiles, mode 600.
            let real = pod.root.join("home/dotfiles/vscode-machine.json");
            std::fs::create_dir_all(real.parent().unwrap()).unwrap();
            std::fs::write(&real, theirs).unwrap();
            std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
            std::os::unix::fs::symlink(&real, &file).unwrap();

            let out = ok(&pod.run_low("x86_64", &pod.setup(), LowMemory::Always, &[]));
            assert!(out.contains("kept python.analysis.indexing"), "{out}");
            assert!(std::fs::symlink_metadata(&file).unwrap().file_type().is_symlink(), "the link stays a link");
            assert_eq!(std::fs::metadata(&real).unwrap().permissions().mode() & 0o777, 0o600);
            let after = pod.settings();
            assert!(only_added_to(theirs, &after), "{after}");
            let mut want = low_memory_settings();
            want["python.defaultInterpreterPath"] = pod.python().into();
            want["python.analysis.indexing"] = true.into();
            want["files.exclude"] = serde_json::json!({ "**/data": false, "**/__pycache__": true });
            want["workbench.colorTheme"] = "Default Dark+".into();
            assert_eq!(jsonc_value(&after), want, "{after}");

            // Again: nothing to add, nothing written.
            let out = ok(&pod.run_low("x86_64", &pod.setup(), LowMemory::Always, &[]));
            assert!(!out.contains("added"), "{out}");
            assert_eq!(pod.settings(), after);
            // A run that doesn't want them (a big machine) takes nothing away.
            pod.ram(64 << 20, &[]);
            ok(&pod.run_low("x86_64", &pod.setup(), LowMemory::BelowGib(LOW_MEMORY_BELOW_GIB), &[]));
            assert_eq!(pod.settings(), after);
            pod.assert_clean();
            let strays: Vec<_> = std::fs::read_dir(real.parent().unwrap()).unwrap().flatten().map(|e| e.file_name()).collect();
            assert_eq!(strays.len(), 1, "no temp file left beside it: {strays:?}");
        }

        /// Review r1: VS Code ends a `//` comment at a CR too. A setting after a CR-only line
        /// break is the participant's, and stays theirs (it was missed, and ours appended after it,
        /// which VS Code would have taken).
        #[test]
        fn a_comment_ends_at_a_carriage_return_as_in_vs_code() {
            let Some(pod) = Pod::new("cr-comment") else { return };
            pod.publish("x64", |j| j);
            let file = pod.vs().join("data/Machine/settings.json");
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            let theirs = "{// mine\r\"python.analysis.indexing\": true,\n\"editor.fontSize\": 12}";
            std::fs::write(&file, theirs).unwrap();
            ok(&pod.run_low("x86_64", &pod.setup(), LowMemory::Always, &[]));
            let after = pod.settings();
            assert!(only_added_to(theirs, &after), "{after}");
            assert_eq!(after.matches("python.analysis.indexing").count(), 1, "{after}");
            let as_json = after.replace("// mine\r", "");
            let v: serde_json::Value = serde_json::from_str(&as_json).unwrap_or_else(|e| panic!("{e}: {as_json}"));
            assert_eq!(v["python.analysis.indexing"], true, "{after}");
            assert_eq!(v["python.analysis.diagnosticMode"], "openFilesOnly", "{after}");
        }

        /// Review r1, r2: VS Code saving a participant's change while setup merges. What it saved
        /// is never lost: the merge is swapped into place in one step, whatever comes out that
        /// isn't what was read goes straight back, and what can't be put back (it saved again
        /// in between) is kept beside it; a file that appeared meanwhile is never replaced; where
        /// no such swap can be done nothing is written. Each says so. (The hooks run a command at
        /// that moment, as VS Code's save.)
        #[test]
        fn a_save_while_merging_is_never_lost() {
            let Some(pod) = Pod::new("race") else { return };
            pod.publish("x64", |j| j);
            let dir = pod.vs().join("data/Machine");
            let file = dir.join("settings.json");
            std::fs::create_dir_all(&dir).unwrap();
            let first = "{\n    \"workbench.colorTheme\": \"Solarized Light\"\n}\n";
            let second = "{\n    \"workbench.colorTheme\": \"Monokai\"\n}\n";
            std::fs::write(pod.root.join("first.json"), first).unwrap();
            std::fs::write(pod.root.join("second.json"), second).unwrap();
            let save = |which: &str| format!("cat '{}' > \"$1\"", pod.root.join(which).display());
            let (save1, save2) = (save("first.json"), save("second.json"));
            let files = || -> Vec<String> {
                let mut v: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
                v.sort();
                v
            };
            let changed = "changed while arena merged its settings (re-run setup)";
            let fresh = || std::fs::write(&file, "{\"editor.fontSize\": 12}").unwrap();

            // A save just before the swap: it goes back.
            fresh();
            let why = warned(&pod.run_low("x86_64", &pod.setup(), LowMemory::Always, &[("ARENA_SETTINGS_RACE", &save1)]));
            assert!(why.contains(&format!("{changed} - left alone")), "{why}");
            assert_eq!(pod.settings(), first);
            assert_eq!(files(), ["settings.json"], "no temp file left");

            // A second save right after the swap: the first goes back, the second is kept beside it.
            fresh();
            let why = warned(&pod.run_low(
                "x86_64",
                &pod.setup(),
                LowMemory::Always,
                &[("ARENA_SETTINGS_RACE", &save1), ("ARENA_SETTINGS_RACE_AGAIN", &save2)],
            ));
            assert_eq!(pod.settings(), first);
            let kept: Vec<String> = files().into_iter().filter(|f| f.starts_with("settings.json.arena-kept-")).collect();
            assert_eq!(kept.len(), 1, "{:?}", files());
            assert_eq!(std::fs::read_to_string(dir.join(&kept[0])).unwrap(), second);
            assert!(why.contains(&format!("{changed}; a save arena couldn't put back is in {}", dir.join(&kept[0]).display())), "{why}");
            std::fs::remove_file(dir.join(&kept[0])).unwrap();

            // What came out of the swap can't be read (a directory in its place): back it goes.
            fresh();
            let why = warned(&pod.run_low("x86_64", &pod.setup(), LowMemory::Always, &[("ARENA_SETTINGS_RACE", "rm \"$1\" && mkdir \"$1\"")]));
            assert!(why.contains(changed), "{why}");
            assert!(file.is_dir());
            std::fs::remove_dir(&file).unwrap();
            assert_eq!(files(), Vec::<String>::new());

            // A file that appeared meanwhile is never replaced.
            let why = warned(&pod.run_low("x86_64", &pod.setup(), LowMemory::Always, &[("ARENA_SETTINGS_RACE", &save1)]));
            assert!(why.contains(changed), "{why}");
            assert_eq!(pod.settings(), first);

            // Where the swap can't be done: nothing written.
            fresh();
            let why = warned(&pod.run_low("x86_64", &pod.setup(), LowMemory::Always, &[("ARENA_SETTINGS_NO_SWAP", "1")]));
            assert!(why.contains("can't be swapped for the merged one in one step here (renameat2)"), "{why}");
            assert_eq!(pod.settings(), "{\"editor.fontSize\": 12}");
            assert_eq!(files(), ["settings.json"]);

            // The next run merges into what's there.
            ok(&pod.run_low("x86_64", &pod.setup(), LowMemory::Always, &[]));
            assert!(pod.settings().starts_with("{\"editor.fontSize\": 12,"), "{}", pod.settings());
            assert!(pod.settings().contains("\"python.analysis.indexing\": false"));
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
