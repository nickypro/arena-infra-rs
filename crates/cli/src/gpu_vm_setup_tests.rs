//! The bare-VM setup script ([`crate::HETZNER_SETUP`]) in its GPU mode (`ARENA_GPU=1`, what a
//! Shadeform VM gets — `setup::BareVm::Gpu`), run for real under bash with every system command
//! stubbed on `PATH` and `HOME` / the repo in a temp dir — nothing is downloaded or installed.
//! The CPU mode (Hetzner) is `hetzner_setup_tests`, untouched: these stubs are its own plus a
//! GPU the venv's python can be told to see (`$STUBS/cuda-ok`).

use std::path::{Path, PathBuf};
use std::process::Command;

struct Tmp(PathBuf);
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The stubs: as `hetzner_setup_tests`' (`uv` logs its argv to `$STUBS/uv.log`, `uv venv … DIR`
/// makes a venv, `uv pip install` exits `$UV_INSTALL_EXIT` and "installs" torch with
/// `$UV_INSTALL_MAKES_TORCH`; `--help` lists `--torch-backend` unless `$UV_OLD`) — and the venv's
/// python, asked about `cuda`, answers only once `$STUBS/cuda-ok` exists (`$UV_INSTALL_MAKES_CUDA`
/// makes it at install).
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
    cat > "$d/bin/python" <<PY
#!/bin/sh
[ -f "$S/torch-ok" ] || { echo "ModuleNotFoundError: No module named torch" >&2; exit 1; }
case "\$*" in *cuda*) [ -f "$S/cuda-ok" ] || { echo "AssertionError: torch.cuda.is_available() is False (driver/CUDA mismatch?)" >&2; exit 1; };; esac
exit 0
PY
    chmod 755 "$d/bin/python"
    printf 'export PATH="%s/bin:$PATH"\n' "$d" > "$d/bin/activate";;
  pip)
    if [ "$2" = install ] && [ "$3" = --help ]; then [ -n "${UV_OLD:-}" ] || echo "  --torch-backend <BACKEND>"; exit 0; fi
    [ "$2" = install ] && [ "${UV_INSTALL_EXIT:-0}" != 0 ] && { echo "  x No solution found when resolving dependencies" >&2; exit "$UV_INSTALL_EXIT"; }
    [ "$2" = install ] && [ -n "${UV_INSTALL_MAKES_TORCH:-}" ] && touch "$S/torch-ok"
    [ "$2" = install ] && [ -n "${UV_INSTALL_MAKES_CUDA:-}" ] && touch "$S/cuda-ok"
    exit 0;;
esac"#,
    );
}

struct Run {
    ok: bool,
    stderr: String,
    uv_log: String,
}

/// One GPU-mode run of the script against `repo` (with `env` extra).
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
        .env("REPO_URL", "https://example.invalid/never-cloned.git")
        .env("ARENA_GPU", "1")
        .env("ARENA_MACHINE_NAME", "devtest-alpha");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let tmp_root = home.parent().unwrap().join("tmp");
    std::fs::create_dir_all(&tmp_root).unwrap();
    cmd.env("ARENA_SETUP_TMP_ROOT", &tmp_root);
    let out = cmd.output().expect("bash");
    Run {
        ok: out.status.success(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        uv_log: std::fs::read_to_string(stubs.join("uv.log")).unwrap_or_default(),
    }
}

const REQUIREMENTS: &str = "jax[cuda13]==0.11.2\nbitsandbytes==0.45.0\ntorch==2.9.0+cu130\nnvidia-cudnn-cu13==9.1\n\
     --extra-index-url https://download.pytorch.org/whl/cu130\ntransformer_lens\n";

fn setup(tag: &str) -> (Tmp, PathBuf, PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!("arena-gpu-vm-setup-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (stubs, home, repo) = (base.join("stubs"), base.join("home"), base.join("repo"));
    for d in [&stubs, &home, &repo] {
        std::fs::create_dir_all(d).unwrap();
    }
    install_stubs(&stubs);
    std::fs::write(repo.join("requirements.txt"), REQUIREMENTS).unwrap();
    (Tmp(base), stubs, home, repo)
}

fn last_line(s: &str) -> &str {
    s.lines().map(str::trim).filter(|l| !l.is_empty()).last().unwrap_or("")
}

fn install_line(uv_log: &str) -> &str {
    uv_log.lines().find(|l| l.starts_with("uv pip install --no-cache-dir")).unwrap_or_else(|| panic!("no install in:\n{uv_log}"))
}

/// A GPU VM keeps the CUDA requirements whole (no CPU substitutions), lets uv match torch to
/// the driver (`--torch-backend auto`), and passes once torch imports *and* sees the GPU; the
/// machine name goes into `~/.name` (a Shadeform VM's hostname isn't it). Then idempotent.
#[test]
fn a_gpu_install_keeps_cuda_and_needs_the_gpu() {
    let (_t, stubs, home, repo) = setup("ok");
    let r = run(&stubs, &home, &repo, &[("UV_INSTALL_MAKES_TORCH", "1"), ("UV_INSTALL_MAKES_CUDA", "1")]);
    assert!(r.ok, "{}", r.stderr);
    let install = install_line(&r.uv_log);
    assert!(
        install.contains(" --torch-backend auto --index-strategy unsafe-best-match --override overrides.txt -r requirements.gpu.txt"),
        "{install}"
    );
    assert_eq!(std::fs::read_to_string(repo.join("requirements.gpu.txt")).unwrap(), REQUIREMENTS, "CUDA kept, nothing dropped");
    assert!(!repo.join("requirements.cpu.txt").exists());
    assert_eq!(std::fs::read_to_string(home.join(".name")).unwrap(), "export MACHINE_NAME=devtest-alpha\n");
    std::fs::write(stubs.join("uv.log"), "").unwrap();
    let again = run(&stubs, &home, &repo, &[]);
    assert!(again.ok && !again.uv_log.contains("pip install --no-cache-dir"), "{}\n{}", again.stderr, again.uv_log);
}

/// torch imports but can't see the GPU (a CPU wheel, a driver/CUDA mismatch): not set up, the
/// reason last — never a ✓ for an env the course can't use.
#[test]
fn a_gpu_install_whose_torch_cant_see_the_gpu_fails() {
    let (_t, stubs, home, repo) = setup("nocuda");
    let r = run(&stubs, &home, &repo, &[("UV_INSTALL_MAKES_TORCH", "1")]);
    assert!(!r.ok, "{}", r.stderr);
    let last = last_line(&r.stderr);
    assert!(last.starts_with("FAILED: the ARENA env") && last.contains("or use the GPU") && last.contains("torch.cuda.is_available() is False"), "{last}");
}

/// The backend can be named (`ARENA_TORCH_BACKEND=cu130`); an older uv without
/// `--torch-backend` installs from the requirements' own CUDA index — never the CPU one.
#[test]
fn the_gpu_backend_can_be_named_and_an_older_uv_uses_the_requirements_index() {
    let (_t, stubs, home, repo) = setup("cu130");
    let r = run(&stubs, &home, &repo, &[("UV_INSTALL_MAKES_TORCH", "1"), ("UV_INSTALL_MAKES_CUDA", "1"), ("ARENA_TORCH_BACKEND", "cu130")]);
    assert!(r.ok && install_line(&r.uv_log).contains(" --torch-backend cu130 "), "{}\n{}", r.stderr, r.uv_log);
    let (_t, stubs, home, repo) = setup("old-uv");
    let r = run(&stubs, &home, &repo, &[("UV_INSTALL_MAKES_TORCH", "1"), ("UV_INSTALL_MAKES_CUDA", "1"), ("UV_OLD", "1")]);
    let install = install_line(&r.uv_log);
    assert!(r.ok && install.contains(" --index-strategy unsafe-best-match --override overrides.txt -r requirements.gpu.txt"), "{install}");
    assert!(!install.contains("--torch-backend") && !install.contains("whl/cpu"), "{install}");
}

/// A failed install fails the script with its reason last, as in CPU mode.
#[test]
fn a_failed_gpu_install_fails_with_the_reason_last() {
    let (_t, stubs, home, repo) = setup("fails");
    let r = run(&stubs, &home, &repo, &[("UV_INSTALL_EXIT", "1")]);
    assert!(!r.ok && r.stderr.contains("No solution found"), "{}", r.stderr);
    assert!(last_line(&r.stderr).starts_with("FAILED: installing the ARENA packages (requirements.gpu.txt)"), "{}", r.stderr);
}
