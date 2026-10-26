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

/// The stubs. `uv` logs its argv to `$STUBS/uv.log` (and the `TMPDIR` it got); `uv venv … DIR`
/// makes a venv whose `python` "imports" only once `$STUBS/torch-ok` exists; `uv pip install`
/// leaves a 0-byte, mode-666 `uv-setuptools-<hash>.lock` in its temp dir as the real one does
/// (and, with `$UV_STRAY` set, one at that path too: a uv that ignores `TMPDIR`), exits
/// `$UV_INSTALL_EXIT` (default 0) and, when `$UV_INSTALL_MAKES_TORCH` is set, "installs" torch;
/// its `--help` lists `--torch-backend` unless `$UV_OLD` is set (an older uv). `git` logs its argv
/// to `$STUBS/git.log`; every clone "fails" (nothing is fetched), `remote get-url origin`
/// answers `$GIT_ORIGIN` (none when unset) and `remote set-url` works; `remote get-url upstream`
/// answers `$GIT_UPSTREAM` (none when unset) and `remote add upstream` works unless
/// `$GIT_ADD_FAILS` is set.
fn install_stubs(dir: &Path) {
    let stub = |name: &str, body: &str| {
        let p = dir.join(name);
        arena_core::remote::write_test_script(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    };
    for quiet in ["apt-get", "systemctl", "chsh", "ssh-keyscan"] {
        stub(quiet, "exit 0");
    }
    stub("docker", "echo 'Docker version 0 (stub)'");
    // Nothing is fetched: every download/clone "fails" (the script treats those as best-effort).
    stub("curl", "exit 22");
    stub(
        "git",
        r#"S="$(dirname "$0")"
echo "git $*" >> "$S/git.log"
case "$*" in
  *"remote get-url origin") [ -n "${GIT_ORIGIN:-}" ] && { echo "$GIT_ORIGIN"; exit 0; }; exit 2;;
  *"remote set-url origin "*) exit 0;;
  *"remote get-url upstream") [ -n "${GIT_UPSTREAM:-}" ] && { echo "$GIT_UPSTREAM"; exit 0; }; exit 2;;
  *"remote add upstream "*) [ -n "${GIT_ADD_FAILS:-}" ] && exit 3; exit 0;;
esac
exit 1"#,
    );
    stub(
        "uv",
        r#"S="$(dirname "$0")"
echo "uv $*" >> "$S/uv.log"
echo "TMPDIR ${TMPDIR:-}" >> "$S/uv.log"
case "$1 $2" in
  "pip install")
    [ "$3" = --help ] || {
      D=${TMPDIR:-${ARENA_SETUP_TMP_ROOT:-/tmp}}
      : > "$D/uv-setuptools-9f3c2a1b.lock" && chmod 666 "$D/uv-setuptools-9f3c2a1b.lock"
      [ -z "${UV_STRAY:-}" ] || : > "$UV_STRAY"
    };;
esac
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
    stdout: String,
    stderr: String,
    uv_log: String,
    git_log: String,
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
        .env("VENV", repo.join(".venv"))
        .env("ARENA_BIN_DIR", home.join("bin"))
        .env("ARENA_MOTD_DIR", home.join("motd"))
        .env("REPO_URL", "https://example.invalid/never-cloned.git");
    for (k, v) in env {
        cmd.env(k, v);
    }
    // /tmp, stood in for: nothing here touches the box's own.
    let tmp_root = home.parent().unwrap().join("tmp");
    std::fs::create_dir_all(&tmp_root).unwrap();
    cmd.env("ARENA_SETUP_TMP_ROOT", &tmp_root);
    let out = cmd.output().expect("bash");
    Run {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        uv_log: std::fs::read_to_string(stubs.join("uv.log")).unwrap_or_default(),
        git_log: std::fs::read_to_string(stubs.join("git.log")).unwrap_or_default(),
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
    s.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("")
}

/// What the script told `arena pods setup` built the env: its `arena-installer:` lines (the
/// prefix core reads them by, [`arena_core::setup::INSTALLER_PREFIX`]), prefix stripped.
fn installers_said(stdout: &str) -> Vec<&str> {
    stdout.lines().filter_map(|l| l.strip_prefix(arena_core::setup::INSTALLER_PREFIX)).collect()
}

/// Live (hz F5): the step headers went 1/5 … 4/5, then 5/6, 6/6. Each now counts up by one to
/// the one total.
#[test]
fn the_step_headers_count_up_to_one_total() {
    let steps: Vec<(usize, usize)> = crate::HETZNER_SETUP
        .lines()
        .filter_map(|l| l.strip_prefix("echo \"### "))
        .filter_map(|l| l.split_once(' ')?.0.split_once('/'))
        .map(|(n, of)| (n.parse().unwrap(), of.parse().unwrap()))
        .collect();
    let total = steps.len();
    assert_eq!(steps, (1..=total).map(|n| (n, total)).collect::<Vec<_>>());
    assert_eq!(total, 6);
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
    assert!(r.uv_log.contains("uv venv --python 3.13 --seed "), "{}", r.uv_log);
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
    assert!(r.ok && r.uv_log.contains("uv venv --python 3.14 --seed "), "{}\n{}", r.stderr, r.uv_log);

    // An older floor keeps 3.13; ARENA_PYTHON wins over both.
    std::fs::write(repo.join("pyproject.toml"), "requires-python = '>=3.10'\n").unwrap();
    let _ = std::fs::remove_file(stubs.join("torch-ok"));
    std::fs::write(stubs.join("uv.log"), "").unwrap();
    let r = run(&stubs, &home, &repo, &[("UV_INSTALL_MAKES_TORCH", "1")]);
    assert!(r.ok && r.uv_log.contains("uv venv --python 3.13 --seed "), "{}", r.uv_log);
    let _ = std::fs::remove_file(stubs.join("torch-ok"));
    std::fs::write(stubs.join("uv.log"), "").unwrap();
    let r = run(&stubs, &home, &repo, &[("UV_INSTALL_MAKES_TORCH", "1"), ("ARENA_PYTHON", "3.12")]);
    assert!(r.ok && r.uv_log.contains("uv venv --python 3.12 --seed "), "{}", r.uv_log);
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

/// Live W7 B-H0: a fresh VM never read as idle — the setup's own `uv` had left five 0-byte
/// `/tmp/uv-setuptools-<hash>.lock` files, "data outside the home" to the auto-reaper's probe for
/// the VM's whole life. The script gives every installer a temp dir of its own (`TMPDIR` under
/// `/tmp/.arena-setup.*`) and removes it when it ends — after a failure too; a `uv-*` file a uv
/// wrote to /tmp itself during the run goes with it, while one from before the run (not this
/// run's) stays. `/tmp` is a stand-in here.
#[test]
fn w7_b_h0_setup_leaves_no_uv_files_behind() {
    for (tag, env) in [("clean-ok", [("UV_INSTALL_MAKES_TORCH", "1")]), ("clean-fail", [("UV_INSTALL_EXIT", "1")])] {
        let (_t, stubs, home, repo) = setup(tag);
        let tmp = home.parent().unwrap().join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let older = tmp.join("uv-setuptools-0ld0ld.lock");
        std::fs::write(&older, "").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let stray = tmp.join("uv-setuptools-57a7.lock");
        let stray_s = stray.display().to_string();
        let env: Vec<(&str, &str)> = env.iter().copied().chain([("UV_STRAY", stray_s.as_str())]).collect();
        let r = run(&stubs, &home, &repo, &env);
        assert_eq!(r.ok, tag == "clean-ok", "{tag}: {}", r.stderr);
        let tmpdirs: Vec<&str> = r.uv_log.lines().filter_map(|l| l.strip_prefix("TMPDIR ")).collect();
        let ours = format!("{}/.arena-setup.", tmp.display());
        assert!(!tmpdirs.is_empty() && tmpdirs.iter().all(|d| d.starts_with(&ours)), "{tag}: uv ran without the setup's own TMPDIR:\n{}", r.uv_log);
        for d in &tmpdirs {
            assert!(!Path::new(d).exists(), "{tag}: the setup's temp dir {d} (uv's locks in it) outlived the script");
        }
        let left: Vec<String> = std::fs::read_dir(&tmp).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(left, ["uv-setuptools-0ld0ld.lock"], "{tag}: only what was there before the run stays");
    }
}

/// Shared requirements are installed in the canonical venv on both platforms.
#[test]
fn shared_install_uses_the_canonical_venv_and_baked_assets() {
    let (_t, stubs, home, repo) = setup("shared-paths");
    let canonical = home.join("opt/arena-env");
    std::fs::create_dir_all(repo.join(".venv")).unwrap();
    std::fs::write(repo.join(".venv/kept"), "old environment").unwrap();
    let r = run(&stubs, &home, &repo, &[("UV_INSTALL_MAKES_TORCH", "1"), ("VENV", canonical.to_str().unwrap())]);
    assert!(r.ok, "{}", r.stderr);
    assert_eq!(std::fs::read_link(repo.join(".venv")).unwrap(), canonical);
    let saved = std::fs::read_dir(home.join(".cache/arena-setup/old-venvs")).unwrap().next().unwrap().unwrap().path();
    assert_eq!(std::fs::read_to_string(saved.join("venv/kept")).unwrap(), "old environment");
    assert!(r.stdout.contains("old repo-local venv kept"));
    assert_eq!(std::fs::read_link(home.join("ARENA_3.0")).unwrap(), repo);
    let rc = std::fs::read_to_string(home.join(".zshrc")).unwrap();
    assert!(rc.contains(canonical.to_str().unwrap()) && !rc.contains("conda activate"));
    assert!(home.join(".no_auto_tmux").exists());
    let settings = std::fs::read_to_string(home.join(".vscode/settings.json")).unwrap();
    assert!(settings.contains(canonical.to_str().unwrap()));
    assert!(r.git_log.contains("geometry-of-truth") && r.git_log.contains("arena-llm-context"));
    assert!(!r.git_log.contains("clone --depth 1 https://github.com/nickypro/arena-infra"));
    assert_eq!(installers_said(&r.stdout), ["requirements.txt (shared uv install)"]);
    let again = run(&stubs, &home, &repo, &[("VENV", canonical.to_str().unwrap())]);
    assert!(again.ok, "{}", again.stderr);
    assert_eq!(installers_said(&again.stdout), ["none (env already present)"]);
    assert_eq!(std::fs::read_to_string(home.join(".bashrc")).unwrap().matches("ARENA_ENV_ACTIVATE").count(), 1);
}

#[test]
fn shared_install_requires_the_repos_requirements() {
    let (_t, stubs, home, repo) = setup("no-requirements");
    std::fs::remove_file(repo.join("requirements.txt")).unwrap();
    let r = run(&stubs, &home, &repo, &[]);
    assert!(!r.ok);
    assert!(last_line(&r.stderr).starts_with("FAILED: no requirements.txt"), "{}", r.stderr);
}

/// Test real source edits in a temporary apt directory, with apt itself stubbed.
#[test]
fn hetzner_tls_failure_retries_only_that_mirror_and_keeps_signature_checks() {
    let (_t, stubs, home, repo) = setup("mirror-tls");
    let apt = home.join("apt");
    std::fs::create_dir_all(apt.join("sources.list.d")).unwrap();
    let src = apt.join("sources.list.d/ubuntu.sources");
    std::fs::write(&src, "URIs: https://mirror.hetzner.com/ubuntu/packages\nSigned-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg\nOther: https://example.invalid/ubuntu\n").unwrap();
    arena_core::remote::write_test_script(stubs.join("apt-get"), r#"#!/bin/sh
case "$*" in *update*)
  echo update >> "$HOME/apt-calls"
  if grep -q https://mirror "$ARENA_APT_DIR/sources.list.d/ubuntu.sources"; then
    echo 'Err: https://mirror.hetzner.com/ubuntu Certificate verification failed: expired certificate'
    exit 100
  fi;;
esac
exit 0
"#).unwrap();
    let r = run(&stubs, &home, &repo, &[("UV_INSTALL_MAKES_TORCH","1"), ("ARENA_APT_DIR",apt.to_str().unwrap())]);
    assert!(r.ok, "{}", r.stderr);
    assert!(r.stdout.contains("arena-warning: Hetzner apt mirror TLS failed"));
    let sources = std::fs::read_to_string(src).unwrap();
    assert!(sources.contains("http://mirror.hetzner.com/") && sources.contains("Signed-By:") && sources.contains("https://example.invalid/"));
    assert_eq!(std::fs::read_to_string(home.join("apt-calls")).unwrap().lines().count(), 2);
}

#[test]
fn failed_or_ignored_apt_indexes_stop_before_installing_packages() {
    for (tag, log, exit) in [("network", "Err: unrelated network failure", 100), ("ignored", "W: Some index files failed to download. They have been ignored", 0)] {
        let (_t, stubs, home, repo) = setup(tag);
        arena_core::remote::write_test_script(stubs.join("apt-get"), format!(r#"#!/bin/sh
case "$*" in *update*) echo '{log}'; exit {exit};; esac
echo installed > "$HOME/apt-installed"
"#)).unwrap();
        let r = run(&stubs, &home, &repo, &[]);
        assert!(!r.ok, "{tag}: {}", r.stdout);
        assert!(last_line(&r.stderr).starts_with("FAILED: apt-get update failed"), "{}", r.stderr);
        assert!(!home.join("apt-installed").exists());
        assert!(!r.stdout.contains("retrying that mirror"));
    }
}

/// The configured repo (`REPO_URL`, from ARENA_REPO_OWNER/ARENA_REPO_NAME): cloned when it
/// isn't there; with none given, refused — never a clone of another repo (the public
/// ARENA-education one used to be the default); a checkout already there on another remote is
/// re-pointed at it, as RunPod/Vast setup does, and one already on it is left alone.
#[test]
fn the_configured_repo_is_cloned_or_followed_and_never_guessed() {
    let (_t, stubs, home, repo) = setup("repo-url");
    let url = "git@github.com:styme3279/ARENA_materials.git";
    let absent = repo.parent().unwrap().join("ARENA_materials");
    let r = run(&stubs, &home, &absent, &[("REPO_URL", url)]);
    assert!(!r.ok, "the stub clone fails");
    assert!(r.git_log.lines().any(|l| l == format!("git clone {url} {}", absent.display())), "{}", r.git_log);
    assert_eq!(last_line(&r.stderr), format!("FAILED: couldn't clone {url} into {}", absent.display()));

    std::fs::write(stubs.join("git.log"), "").unwrap();
    let r = run(&stubs, &home, &absent, &[("REPO_URL", "")]);
    assert!(!r.ok && !r.git_log.contains(" clone ") && !r.git_log.contains("ARENA-education"), "{}", r.git_log);
    assert!(last_line(&r.stderr).starts_with(&format!("FAILED: no repo at {} and no REPO_URL to clone", absent.display())), "{}", r.stderr);

    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let old = "https://github.com/ARENA-education/ARENA_materials.git";
    std::fs::write(stubs.join("git.log"), "").unwrap();
    let r = run(&stubs, &home, &repo, &[("REPO_URL", url), ("GIT_ORIGIN", old), ("UV_INSTALL_MAKES_TORCH", "1")]);
    assert!(r.ok, "{}", r.stderr);
    assert!(r.git_log.lines().any(|l| l == format!("git -C {} remote set-url origin {url}", repo.display())), "{}", r.git_log);
    assert!(r.stdout.lines().any(|l| l == format!("origin: {url} (was {old})")), "{}", r.stdout);
    std::fs::write(stubs.join("git.log"), "").unwrap();
    let r = run(&stubs, &home, &repo, &[("REPO_URL", url), ("GIT_ORIGIN", url)]);
    assert!(r.ok && !r.git_log.contains("set-url") && !r.stdout.contains("origin: "), "{}\n{}", r.git_log, r.stdout);
}

/// The human (2026-10-19): with origin the cohort's repo, the public course repo as `upstream`
/// too. Added (and said) when the checkout has none; one already there — a participant's own, an
/// earlier setup's — left alone; nothing without `UPSTREAM_URL` (`ARENA_UPSTREAM_REPO=none`); and a
/// failed add is a warning, never a failed setup.
#[test]
fn the_public_course_repo_becomes_upstream_when_the_checkout_has_none() {
    let (_t, stubs, home, repo) = setup("course-repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let url = "git@github.com:o/r.git";
    let up = "https://github.com/ARENA-education/ARENA_materials.git";
    let base = [("REPO_URL", url), ("GIT_ORIGIN", url), ("UV_INSTALL_MAKES_TORCH", "1")];
    let with = |extra: &[(&str, &str)]| {
        std::fs::write(stubs.join("git.log"), "").unwrap();
        run(&stubs, &home, &repo, &[&base[..], extra].concat())
    };
    let r = with(&[("UPSTREAM_URL", up)]);
    assert!(r.ok, "{}", r.stderr);
    assert!(r.git_log.lines().any(|l| l == format!("git -C {} remote add upstream {up}", repo.display())), "{}", r.git_log);
    assert!(r.stdout.lines().any(|l| l == format!("upstream: {up}")), "{}", r.stdout);
    let default_remote = format!("git -C {} config checkout.defaultRemote origin", repo.display());
    assert!(r.git_log.lines().any(|l| l == default_remote), "{}", r.git_log);
    let r = with(&[("UPSTREAM_URL", up), ("GIT_UPSTREAM", "https://github.com/someone/fork.git")]);
    assert!(r.ok && !r.git_log.contains("remote add") && !r.stdout.contains("upstream: "), "{}\n{}", r.git_log, r.stdout);
    let r = with(&[]);
    assert!(r.ok, "{}", r.stderr);
    assert!(!r.git_log.contains("remote get-url upstream") && !r.git_log.contains("remote add upstream"), "{}", r.git_log);
    let r = with(&[("UPSTREAM_URL", up), ("GIT_ADD_FAILS", "1")]);
    assert!(r.ok, "{}", r.stderr);
    // On the pod's line: setup shows its `arena-warning:` lines (review fb2 R4).
    assert!(r.stdout.lines().any(|l| l == format!("arena-warning: couldn't add the upstream remote {up}")), "{}", r.stdout);
}
