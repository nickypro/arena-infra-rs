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
    // The swap step's tools, logged — no test makes a real swap file or turns one on. `df` says
    // DF_FREE_KB free (100 GiB unless a test sets less); MKSWAP_EXIT fails mkswap.
    let logged = |name: &str, rest: &str| stub(name, &format!("echo \"{name} $*\" >> \"$(dirname \"$0\")/swap.log\"\n{rest}"));
    logged("fallocate", "for f; do :; done; : > \"$f\"");
    logged("mkswap", "exit ${MKSWAP_EXIT:-0}");
    logged("swapon", "exit 0");
    logged("sysctl", "exit 0");
    stub("df", "echo 'Filesystem 1024-blocks Used Available Capacity Mounted on'; echo \"/dev/sda1 1 1 ${DF_FREE_KB:-104857600} 1% /\"");
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
        .env("REPO_URL", "https://example.invalid/never-cloned.git");
    for (k, v) in env {
        cmd.env(k, v);
    }
    // /tmp, stood in for: nothing here touches the box's own.
    // The swap step's files, in the test's own dir: never /swapfile, /etc/fstab or /proc/swaps.
    let base = home.parent().unwrap();
    let swaps = base.join("proc-swaps");
    if !swaps.exists() {
        std::fs::write(&swaps, "Filename\tType\tSize\tUsed\tPriority\n").unwrap();
    }
    cmd.env("ARENA_SWAPFILE", base.join("swapfile"))
        .env("ARENA_FSTAB", base.join("fstab"))
        .env("ARENA_SYSCTL_CONF", base.join("sysctl.conf"))
        .env("ARENA_PROC_SWAPS", &swaps);
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

/// The repo's `install_uv.sh`, stood in for (not executable: the script runs it with `bash`).
/// It notes how it was run — its cwd and `UV_TORCH_BACKEND` — in `install_uv.ran` beside it,
/// then builds the venv with the stub `uv` the way the real one does (`uv venv --clear --python
/// 3.13 --seed`, `uv pip install -r requirements.txt`) — or fails with `$INSTALL_UV_EXIT`.
fn install_uv_sh(repo: &Path) {
    std::fs::write(
        repo.join("install_uv.sh"),
        r#"#!/bin/bash
set -e
D="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
echo "cwd=$(pwd) UV_TORCH_BACKEND=${UV_TORCH_BACKEND:-} args=$*" >> "$D/install_uv.ran"
if [ "${INSTALL_UV_EXIT:-0}" != 0 ]; then echo "install_uv: boom, no solution" >&2; exit "$INSTALL_UV_EXIT"; fi
uv venv --clear --python 3.13 --seed "$D/.venv"
uv pip install --python "$D/.venv/bin/python" -r "$D/requirements.txt"
"#,
    )
    .unwrap();
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
    assert_eq!(total, 7);
}

/// The swap step (VM_SWAP_GB, default 8): a swap file made, turned on, in fstab once, with a
/// low swappiness; a second run keeps the one that's on. Never fatal, so every way it can't
/// make one is a WARN line and no file.
#[test]
fn swap_is_made_once_and_kept_on_a_rerun() {
    let (t, stubs, home, repo) = setup("swap");
    let swapfile = t.0.join("swapfile");
    let r = run(&stubs, &home, &repo, &[]);
    assert!(r.stdout.contains(&format!("swap: 8 GB at {}", swapfile.display())), "{}", r.stdout);
    let log = std::fs::read_to_string(stubs.join("swap.log")).unwrap();
    let f = swapfile.display();
    assert_eq!(log, format!("fallocate -l 8589934592 {f}\nmkswap {f}\nswapon {f}\nsysctl -q -p {}\n", t.0.join("sysctl.conf").display()));
    assert_eq!(std::fs::read_to_string(t.0.join("fstab")).unwrap(), format!("{f} none swap sw 0 0\n"));
    assert_eq!(std::fs::read_to_string(t.0.join("sysctl.conf")).unwrap(), "vm.swappiness = 10\n");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&swapfile).unwrap().permissions().mode() & 0o777, 0o600);

    // Now the kernel lists it (8 GiB, in KiB as /proc/swaps has it): kept, nothing redone.
    std::fs::write(t.0.join("proc-swaps"), format!("Filename\tType\tSize\tUsed\tPriority\n{f}\tfile\t8388608\t0\t-2\n")).unwrap();
    std::fs::remove_file(stubs.join("swap.log")).unwrap();
    let r = run(&stubs, &home, &repo, &[]);
    assert!(r.stdout.contains(&format!("swap: {f} already on (8 GB)")), "{}", r.stdout);
    assert!(!stubs.join("swap.log").exists(), "nothing redone");
    assert_eq!(std::fs::read_to_string(t.0.join("fstab")).unwrap().lines().count(), 1, "fstab once");

    // Asked for more later: the live one stays as it is, and says so.
    let r = run(&stubs, &home, &repo, &[("ARENA_SWAP_GB", "16")]);
    assert!(r.stdout.contains("already on (8 GB, not the 16 GB asked"), "{}", r.stdout);
}

#[test]
fn no_swap_is_made_when_asked_for_none_without_room_or_when_mkswap_fails() {
    for (tag, env, said) in [
        ("swap-off", vec![("ARENA_SWAP_GB", "0")], "swap: none (VM_SWAP_GB=0)"),
        ("swap-bad", vec![("ARENA_SWAP_GB", "8G")], "WARN: swap size '8G' isn't a whole number of GB — no swap"),
        // 8 GiB of swap must leave 10 GiB free: 17 GiB isn't enough.
        ("swap-room", vec![("DF_FREE_KB", "17825792")], "WARN: no swap — 8 GB of it would leave under 10 GB free"),
        ("swap-mkswap", vec![("MKSWAP_EXIT", "1")], "WARN: no swap — mkswap/swapon refused"),
    ] {
        let (t, stubs, home, repo) = setup(tag);
        let r = run(&stubs, &home, &repo, &env);
        assert!(r.stdout.contains(said), "{tag}: {}", r.stdout);
        assert!(!t.0.join("swapfile").exists(), "{tag}: no swap file left");
        assert!(!t.0.join("fstab").exists(), "{tag}: fstab untouched");
        assert!(r.stdout.contains("### 2/7 system packages"), "{tag}: setup goes on: {}", r.stdout);
    }
    // Someone else's swap, as big as asked (a provider's partition): none added.
    let (t, stubs, home, repo) = setup("swap-theirs");
    std::fs::write(t.0.join("proc-swaps"), "Filename\tType\tSize\tUsed\tPriority\n/dev/sda2\tpartition\t8388608\t0\t-2\n").unwrap();
    let r = run(&stubs, &home, &repo, &[]);
    assert!(r.stdout.contains("swap: 8 GB already on (not arena's) — none added"), "{}", r.stdout);
    assert!(!t.0.join("swapfile").exists());
}

/// The shared dotfiles' .zshrc, as nickypro/arena-infra has it (cut down): the GPU image's, so
/// it starts conda and activates its env.
const SHARED_ZSHRC: &str = r#"# export PATH="/opt/conda/bin:$PATH"  # commented out by conda initialize
export MACHINE_NAME="${MACHINE_NAME:="root"}"
[[ ! -f ~/.p10k.zsh ]] || source ~/.p10k.zsh

# >>> conda initialize >>>
__conda_setup="$('/opt/conda/bin/conda' 'shell.zsh' 'hook' 2> /dev/null)"
if [ $? -eq 0 ]; then
    eval "$__conda_setup"
fi
unset __conda_setup
# <<< conda initialize <<<

conda activate arena-env
"#;

/// The human (2026-10-29): "the .zshrc still has conda activate by default, can we remove
/// that?" A VM has no conda, so each login said "command not found: conda". The VM's .zshrc
/// is the shared one without the image's conda, plus the venv. A re-run filters the VM's own
/// (it used to copy the shared file over it, dropping `copy-keys`' tokens and the participant's
/// lines); only oh-my-zsh's untouched template is replaced, and anything else is kept first.
#[test]
fn the_vms_zshrc_has_no_conda_and_a_rerun_keeps_its_tokens() {
    let (_t, stubs, home, repo) = setup("zshrc");
    let dotfiles = home.join(".arena_infra/dotfiles");
    std::fs::create_dir_all(&dotfiles).unwrap();
    std::fs::write(dotfiles.join(".zshrc"), SHARED_ZSHRC).unwrap();
    let zshrc = || std::fs::read_to_string(home.join(".zshrc")).unwrap();
    let kept = || {
        let mut v: Vec<_> = std::fs::read_dir(&home)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with(".zshrc.pre-arena-"))
            .collect();
        v.sort();
        v
    };
    let venv = repo.join(".venv");
    let activate = format!("source \"{}/bin/activate\"", venv.display());
    let new_block = format!("\n# ARENA env: the uv venv.\nif [ -f \"{0}/bin/activate\" ]; then\n    {activate}\nfi\n", venv.display());

    // A fresh VM: oh-my-zsh's template, replaced (nothing worth keeping).
    let template = "# If you come from bash you might have to change your $PATH.\nexport ZSH=\"$HOME/.oh-my-zsh\"\n\
                    ZSH_THEME=\"robbyrussell\"\nplugins=(git)\nsource $ZSH/oh-my-zsh.sh\n";
    std::fs::write(home.join(".zshrc"), template).unwrap();
    let r = run(&stubs, &home, &repo, &[("UV_INSTALL_MAKES_TORCH", "1")]);
    assert!(r.ok, "{}", r.stderr);
    let rc = zshrc();
    assert!(!rc.contains("conda"), "{rc}");
    assert!(!rc.contains("robbyrussell"), "the template is replaced: {rc}");
    assert!(rc.contains("export MACHINE_NAME=") && rc.contains("source ~/.p10k.zsh"), "the rest is kept: {rc}");
    assert!(rc.ends_with(&new_block), "{rc}");
    assert_eq!(kept(), Vec::<String>::new());

    // copy-keys and the participant add lines, a conda of their own among them (its init
    // block; an activate inside their own `if`). A re-run keeps all of it, adds nothing twice.
    let added = "export OPENROUTER_API_KEY=\"sk-or-test\"\nalias gs='git status'\n\
                 # >>> conda initialize >>>\n__conda_setup=\"$('/root/miniconda3/bin/conda' 'shell.zsh' 'hook')\"\n# <<< conda initialize <<<\n\
                 if command -v conda >/dev/null 2>&1; then\n    conda activate arena-env\nfi\n";
    std::fs::write(home.join(".zshrc"), format!("{rc}{added}")).unwrap();
    let again = run(&stubs, &home, &repo, &[]);
    assert!(again.ok, "{}", again.stderr);
    assert_eq!(zshrc(), format!("{rc}{added}"));

    // They took the venv block out: still arena's rc (the shared MACHINE_NAME line), so filtered
    // in place, tokens kept, the block back once.
    let without = rc.replace(&new_block, "");
    std::fs::write(home.join(".zshrc"), format!("{without}{added}")).unwrap();
    let r = run(&stubs, &home, &repo, &[]);
    assert!(r.ok, "{}", r.stderr);
    assert_eq!(zshrc(), format!("{without}{added}{new_block}"));
    assert_eq!(kept(), Vec::<String>::new());

    // Not arena's and not the template: replaced, but kept first, byte for byte.
    std::fs::write(home.join(".zshrc"), "export MY_TOKEN=1\nalias ll='ls -l'\n").unwrap();
    let r = run(&stubs, &home, &repo, &[]);
    assert!(r.ok, "{}", r.stderr);
    assert!(zshrc().contains("export MACHINE_NAME="), "{}", zshrc());
    let k = kept();
    assert_eq!(k.len(), 1, "{k:?}");
    assert_eq!(std::fs::read_to_string(home.join(&k[0])).unwrap(), "export MY_TOKEN=1\nalias ll='ls -l'\n");
    std::fs::remove_file(home.join(&k[0])).unwrap();

    // A VM set up before this: the shared rc with conda, the venv block setup wrote then (it
    // skipped activation whenever a conda was on PATH), their lines; .bashrc with that block
    // and their own conda `if`. A re-run takes the image's conda out and turns the old block
    // into today's; their lines stay, and .bashrc still parses.
    let old_block = format!(
        "\n# ARENA env: no conda on CPU pods — activate the uv venv (no-op on conda GPU pods).\n\
         if ! command -v conda >/dev/null 2>&1 && [ -f \"{0}/bin/activate\" ]; then\n    {activate}\nfi\n",
        venv.display()
    );
    std::fs::write(home.join(".zshrc"), format!("{SHARED_ZSHRC}{old_block}{added}")).unwrap();
    let theirs = "if command -v conda >/dev/null 2>&1; then\n    conda activate arena-env\nfi\n";
    std::fs::write(home.join(".bashrc"), format!("# ubuntu's\n{theirs}{old_block}")).unwrap();
    let old = run(&stubs, &home, &repo, &[]);
    assert!(old.ok, "{}", old.stderr);
    let rc = zshrc();
    assert!(!rc.contains("/opt/conda") && !rc.contains("\nconda activate") && !rc.contains("! command -v conda"), "{rc}");
    assert!(rc.ends_with(&format!("{new_block}{added}")), "{rc}");
    assert_eq!(rc.matches(&activate).count(), 1, "{rc}");
    let bashrc = std::fs::read_to_string(home.join(".bashrc")).unwrap();
    assert_eq!(bashrc, format!("# ubuntu's\n{theirs}{new_block}"));
    let parses = Command::new("bash").arg("-n").arg(home.join(".bashrc")).status().unwrap();
    assert!(parses.success());
    assert_eq!(kept(), Vec::<String>::new());
    // What a shell gets, with a conda on PATH: the venv last (the old block gave it neither).
    arena_core::remote::write_test_script(&stubs.join("conda"), "#!/bin/sh\nexit 0\n".to_string()).unwrap();
    let out = Command::new("bash")
        .args(["-c", "source ~/.bashrc; command -v conda >/dev/null && echo \"$PATH\""])
        .env_clear()
        .env("HOME", &home)
        .env("PATH", format!("{}:/usr/bin:/bin", stubs.display()))
        .output()
        .unwrap();
    let path = String::from_utf8_lossy(&out.stdout);
    assert!(path.starts_with(&format!("{}/bin:", venv.display())), "{path}");
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

/// Item 4 (an evals node, 2026-10-15): the VM got py3.11 and the old CPU requirements, not the
/// course's env. Now the configured repo's own installer builds it — its `install_uv.sh` (uv,
/// Python 3.13), as the image RunPod/Vast pods boot from was built — run from the repo with CPU
/// torch wheels (`UV_TORCH_BACKEND=cpu`) inside the setup's own temp dir; the built-in
/// requirements install doesn't run, and the output names the installer. Idempotent: an env
/// that passes the gate isn't rebuilt.
#[test]
fn the_repos_install_uv_sh_builds_the_env_and_is_named() {
    let (_t, stubs, home, repo) = setup("install-uv");
    install_uv_sh(&repo);
    let r = run(&stubs, &home, &repo, &[("UV_INSTALL_MAKES_TORCH", "1")]);
    assert!(r.ok, "{}", r.stderr);
    let ran = std::fs::read_to_string(repo.join("install_uv.ran")).unwrap();
    assert_eq!(ran, format!("cwd={} UV_TORCH_BACKEND=cpu args=\n", repo.display()));
    assert!(r.uv_log.contains("uv venv --clear --python 3.13 --seed "), "{}", r.uv_log);
    assert!(!r.uv_log.contains("--override overrides.txt") && !repo.join("requirements.cpu.txt").exists(), "{}", r.uv_log);
    let ours = format!("{}/.arena-setup.", home.parent().unwrap().join("tmp").display());
    assert!(r.uv_log.lines().filter_map(|l| l.strip_prefix("TMPDIR ")).all(|d| d.starts_with(&ours)), "{}", r.uv_log);
    assert!(r.stdout.contains(&format!("installer: {}/install_uv.sh (the repo's own), torch backend cpu", repo.display())), "{}", r.stdout);
    assert!(last_line(&r.stdout).contains("installer: install_uv.sh,"), "{}", r.stdout);
    assert_eq!(installers_said(&r.stdout), ["install_uv.sh"], "for the ✓ line (live hz F4): {}", r.stdout);
    assert!(!r.stdout.contains(arena_core::volume::WARNING_PREFIX), "nothing to warn about: {}", r.stdout);

    std::fs::write(stubs.join("uv.log"), "").unwrap();
    let again = run(&stubs, &home, &repo, &[]);
    assert!(again.ok, "{}", again.stderr);
    assert_eq!(std::fs::read_to_string(repo.join("install_uv.ran")).unwrap(), ran, "not run again");
    assert!(!again.uv_log.contains("venv"), "{}", again.uv_log);
    assert!(last_line(&again.stdout).contains("installer: none (env already present),"), "{}", again.stdout);
    assert_eq!(installers_said(&again.stdout), ["none (env already present)"]);
}

/// A failing install_uv.sh fails setup: its own error stays visible, the reason is last.
#[test]
fn a_failing_install_uv_sh_fails_setup_with_the_reason_last() {
    let (_t, stubs, home, repo) = setup("install-uv-fails");
    install_uv_sh(&repo);
    let r = run(&stubs, &home, &repo, &[("INSTALL_UV_EXIT", "3"), ("UV_INSTALL_MAKES_TORCH", "1")]);
    assert!(!r.ok, "{}", r.stderr);
    assert!(r.stderr.contains("install_uv: boom, no solution"), "{}", r.stderr);
    assert_eq!(last_line(&r.stderr), "FAILED: the repo's install_uv.sh failed (exit 3) — see its output above");
    assert!(!r.uv_log.contains("pip install --no-cache-dir"), "no fallback after a failed install_uv.sh: {}", r.uv_log);
}

/// A repo without install_uv.sh (an older checkout) gets the built-in uv install of its
/// requirements.txt, as before — and says so as an `arena-warning:` line, which `pods setup`
/// shows on the pod's line. Without a requirements.txt either, nothing can be installed.
#[test]
fn without_install_uv_sh_the_requirements_install_runs_and_says_so() {
    let (_t, stubs, home, repo) = setup("no-install-uv");
    let r = run(&stubs, &home, &repo, &[("UV_INSTALL_MAKES_TORCH", "1")]);
    assert!(r.ok, "{}", r.stderr);
    let warn = format!(
        "{}no install_uv.sh in {} — installed its requirements.txt with the setup script's own uv install instead",
        arena_core::volume::WARNING_PREFIX,
        repo.display()
    );
    assert!(r.stdout.lines().any(|l| l == warn), "{}", r.stdout);
    assert!(install_line(&r.uv_log).ends_with("-r requirements.cpu.txt"), "{}", r.uv_log);
    assert!(last_line(&r.stdout).contains("installer: requirements.txt (built-in uv install),"), "{}", r.stdout);
    assert_eq!(installers_said(&r.stdout), ["requirements.txt (built-in uv install)"]);

    let (_t, stubs, home, repo) = setup("nothing-to-install");
    std::fs::remove_file(repo.join("requirements.txt")).unwrap();
    let r = run(&stubs, &home, &repo, &[]);
    assert!(!r.ok);
    assert_eq!(last_line(&r.stderr), format!("FAILED: no install_uv.sh or requirements.txt in {} to install the ARENA env from", repo.display()));
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
