//! The bare-VM setup script ([`crate::HETZNER_SETUP`]) run for real under bash, with every
//! system-touching command stubbed on `PATH` (apt-get, docker, curl, git, uv, …) and `HOME` /
//! the repo in a temp dir — nothing is downloaded or installed. The live failure this pins: a
//! VM whose venv couldn't `import torch` (uv found no solution on Python 3.11) reported setup ✓,
//! because the script had no `set -e` and no check at the end.

use std::path::{Path, PathBuf};
use std::process::Command;

struct Tmp(PathBuf);
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The stubs. `uv` logs its argv to `$STUBS/uv.log`; `uv venv … DIR` makes a venv whose
/// `python` "imports" only once `$STUBS/torch-ok` exists; `uv pip install` exits
/// `$UV_INSTALL_EXIT` (default 0) and, when `$UV_INSTALL_MAKES_TORCH` is set, "installs" torch;
/// its `--help` lists `--torch-backend` unless `$UV_OLD` is set (an older uv).
fn install_stubs(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let stub = |name: &str, body: &str| {
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    for quiet in ["apt-get", "systemctl", "chsh", "ssh-keyscan"] {
        stub(quiet, "exit 0");
    }
    stub("docker", "echo 'Docker version 0 (stub)'");
    // Nothing is fetched: every download/clone "fails" (the script treats those as best-effort).
    stub("curl", "exit 22");
    stub("git", "exit 1");
    stub(
        "uv",
        r#"S="$(dirname "$0")"
echo "uv $*" >> "$S/uv.log"
case "$1" in
  --version) echo "uv 0.0.0 (stub)";;
  venv)
    for d; do :; done
    mkdir -p "$d/bin"
    printf '#!/bin/sh\n[ -f "%s/torch-ok" ] && exit 0\necho "ModuleNotFoundError: No module named torch" >&2\nexit 1\n' "$S" > "$d/bin/python"
    chmod 755 "$d/bin/python"
    printf 'export PATH="%s/bin:$PATH"\n' "$d" > "$d/bin/activate";;
  pip)
    if [ "$2" = install ] && [ "$3" = --help ]; then [ -n "${UV_OLD:-}" ] || echo "  --torch-backend <BACKEND>"; exit 0; fi
    [ "$2" = install ] && [ "${UV_INSTALL_EXIT:-0}" != 0 ] && { echo "  x No solution found when resolving dependencies" >&2; exit "$UV_INSTALL_EXIT"; }
    [ "$2" = install ] && [ -n "${UV_INSTALL_MAKES_TORCH:-}" ] && touch "$S/torch-ok"
    exit 0;;
esac"#,
    );
}

struct Run {
    ok: bool,
    stderr: String,
    uv_log: String,
}

/// One run of the script against `repo` (with `env` extra), stubs in `stubs`, `HOME` = `home`.
fn run(stubs: &Path, home: &Path, repo: &Path, env: &[(&str, &str)]) -> Run {
    let script = home.join("hetzner_setup.sh");
    std::fs::write(&script, crate::HETZNER_SETUP).unwrap();
    let path = format!("{}:{}", stubs.display(), std::env::var("PATH").unwrap_or_default());
    let mut cmd = Command::new("bash");
    cmd.arg(&script)
        .env_clear()
        .env("PATH", path)
        .env("HOME", home)
        .env("REPO_DIR", repo)
        .env("REPO_URL", "https://example.invalid/never-cloned.git");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("bash");
    Run {
        ok: out.status.success(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        uv_log: std::fs::read_to_string(stubs.join("uv.log")).unwrap_or_default(),
    }
}

fn setup(tag: &str) -> (Tmp, PathBuf, PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!("arena-hetzner-setup-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (stubs, home, repo) = (base.join("stubs"), base.join("home"), base.join("repo"));
    for d in [&stubs, &home, &repo] {
        std::fs::create_dir_all(d).unwrap();
    }
    install_stubs(&stubs);
    // The upstream lines the old exact-match substitutions missed (jax went cuda12 → cuda13).
    std::fs::write(
        repo.join("requirements.txt"),
        "jax[cuda13]==0.11.2\nbitsandbytes==0.45.0\ntorch==2.9.0+cu130\nnvidia-cudnn-cu13==9.1\n\
         --extra-index-url https://download.pytorch.org/whl/cu130\ntransformer_lens\nbitsandbytes-foo==1\n",
    )
    .unwrap();
    (Tmp(base), stubs, home, repo)
}

fn last_line(s: &str) -> &str {
    s.lines().map(str::trim).filter(|l| !l.is_empty()).last().unwrap_or("")
}

#[test]
fn the_script_parses() {
    let out = Command::new("bash").arg("-n").arg("-c").arg(crate::HETZNER_SETUP).output().expect("bash");
    assert!(out.status.success(), "bash -n: {}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn a_failed_install_fails_the_script_with_the_reason_last() {
    let (_t, stubs, home, repo) = setup("install-fails");
    let r = run(&stubs, &home, &repo, &[("UV_INSTALL_EXIT", "1")]);
    assert!(!r.ok, "a failed install must fail setup:\n{}", r.stderr);
    assert!(r.stderr.contains("No solution found"), "uv's own error stays visible: {}", r.stderr);
    assert!(last_line(&r.stderr).starts_with("FAILED: installing the ARENA packages"), "{}", r.stderr);
}

#[test]
fn an_install_that_leaves_no_importable_torch_fails_the_script() {
    // uv "succeeds", but the venv still can't import torch: the gate, not the exit code, decides.
    let (_t, stubs, home, repo) = setup("gate");
    let r = run(&stubs, &home, &repo, &[]);
    assert!(!r.ok, "no torch must fail setup:\n{}", r.stderr);
    let last = last_line(&r.stderr);
    assert!(last.contains("can't import torch") && last.contains("No module named torch"), "{last}");
}

#[test]
fn a_good_install_passes_with_python_3_13_and_cpu_substitutions_then_skips_on_rerun() {
    let (_t, stubs, home, repo) = setup("ok");
    let r = run(&stubs, &home, &repo, &[("UV_INSTALL_MAKES_TORCH", "1")]);
    assert!(r.ok, "{}", r.stderr);
    assert!(r.uv_log.contains("uv venv --python 3.13 "), "{}", r.uv_log);
    assert!(r.uv_log.contains("--torch-backend cpu") && r.uv_log.contains("--override overrides.txt"), "{}", r.uv_log);
    let cpu = std::fs::read_to_string(repo.join("requirements.cpu.txt")).unwrap();
    assert_eq!(cpu, "jax[cpu]==0.11.2\ntorch==2.9.0\ntransformer_lens\nbitsandbytes-foo==1\n");

    // Idempotent: the env passes the gate, so nothing is reinstalled.
    std::fs::write(stubs.join("uv.log"), "").unwrap();
    let again = run(&stubs, &home, &repo, &[]);
    assert!(again.ok, "{}", again.stderr);
    assert!(!again.uv_log.contains("pip install --no-cache-dir") && !again.uv_log.contains("venv"), "{}", again.uv_log);
}

#[test]
fn the_venv_python_follows_a_newer_requires_python_and_the_override() {
    let (_t, stubs, home, repo) = setup("python");
    std::fs::write(repo.join("pyproject.toml"), "[project]\nname = \"x\"\nrequires-python = \">=3.14\"\n").unwrap();
    let r = run(&stubs, &home, &repo, &[("UV_INSTALL_MAKES_TORCH", "1")]);
    assert!(r.ok && r.uv_log.contains("uv venv --python 3.14 "), "{}\n{}", r.stderr, r.uv_log);

    // An older floor keeps 3.13; ARENA_PYTHON wins over both.
    std::fs::write(repo.join("pyproject.toml"), "requires-python = '>=3.10'\n").unwrap();
    let _ = std::fs::remove_file(stubs.join("torch-ok"));
    std::fs::write(stubs.join("uv.log"), "").unwrap();
    let r = run(&stubs, &home, &repo, &[("UV_INSTALL_MAKES_TORCH", "1")]);
    assert!(r.ok && r.uv_log.contains("uv venv --python 3.13 "), "{}", r.uv_log);
    let _ = std::fs::remove_file(stubs.join("torch-ok"));
    std::fs::write(stubs.join("uv.log"), "").unwrap();
    let r = run(&stubs, &home, &repo, &[("UV_INSTALL_MAKES_TORCH", "1"), ("ARENA_PYTHON", "3.12")]);
    assert!(r.ok && r.uv_log.contains("uv venv --python 3.12 "), "{}", r.uv_log);
}

/// The install line the script ran (not its `--help` probe).
fn install_line(uv_log: &str) -> &str {
    uv_log.lines().find(|l| l.starts_with("uv pip install --no-cache-dir")).unwrap_or_else(|| panic!("no install in:\n{uv_log}"))
}

#[test]
fn an_older_uv_without_torch_backend_installs_from_the_cpu_index() {
    let (_t, stubs, home, repo) = setup("old-uv");
    let r = run(&stubs, &home, &repo, &[("UV_INSTALL_MAKES_TORCH", "1"), ("UV_OLD", "1")]);
    assert!(r.ok, "{}", r.stderr);
    let install = install_line(&r.uv_log);
    assert!(
        install.contains(
            " --extra-index-url https://download.pytorch.org/whl/cpu --index-strategy unsafe-best-match --override overrides.txt \
             -r requirements.cpu.txt"
        ),
        "{install}"
    );
    assert!(!install.contains("--torch-backend"), "{install}");
    // A current uv: the backend flag, and no extra index.
    let (_t, stubs, home, repo) = setup("new-uv");
    let r = run(&stubs, &home, &repo, &[("UV_INSTALL_MAKES_TORCH", "1")]);
    let install = install_line(&r.uv_log);
    assert!(r.ok && install.contains(" --torch-backend cpu --override overrides.txt") && !install.contains("--extra-index-url"), "{install}");
}
