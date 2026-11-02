use super::*;
use crate::setup::{provisioning_steps, SetupConfig, SetupTimeouts};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The block for these facts.
fn facts(env: &str, venv: bool, uv: bool, repo: &str, gpu: bool) -> String {
    block(Facts { env, venv, uv, repo, gpu })
}

/// The block as it lands on each kind of machine, line for line: a RunPod/Vast pod (the image's
/// env, a uv venv, with uv on PATH) and a Hetzner VM (the repo's venv; uv linked onto PATH) — and
/// a plain conda env with neither an activate script nor uv.
#[test]
fn the_block_for_each_kind_of_machine() {
    let gpu = facts("/opt/arena-env", true, true, "/root/ARENA_materials", true);
    assert_eq!(
        gpu,
        "<!-- BEGIN arena-infra python -->\n\
         ## Python on this machine (arena setup rewrites this block; write your own notes outside it)\n\
         - Use the course env, /opt/arena-env: torch with CUDA, transformers, transformer_lens and the other course packages are in it. \
         Run /opt/arena-env/bin/python, or `source /opt/arena-env/bin/activate` first.\n\
         - Shells started over ssh, by scripts or by your tools may not have it active: there `python3` and `pip` can be the \
         system's (/usr/bin/python3 may even have torch, but not the course packages). Check `which python` before running anything.\n\
         - Missing a package? Check first: `/opt/arena-env/bin/python -m pip show PACKAGE`. Install into the env: \
         `uv pip install --python /opt/arena-env/bin/python PACKAGE`. Never pip install into the system Python, and don't make another \
         venv or conda env for the course.\n\
         - The course repo (the exercises, and the participants' work) is /root/ARENA_materials.\n\
         <!-- END arena-infra python -->\n"
    );
    let cpu = facts("/root/ARENA_materials/.venv", true, true, "/root/ARENA_materials", false);
    assert!(cpu.contains(
        "- Use the course env, /root/ARENA_materials/.venv: torch (a CPU build: this machine has no GPU), transformers, \
         transformer_lens and the other course packages are in it. Run /root/ARENA_materials/.venv/bin/python, or \
         `source /root/ARENA_materials/.venv/bin/activate` first.\n"
    ));
    assert!(cpu.contains("Install into the env: `uv pip install --python /root/ARENA_materials/.venv/bin/python PACKAGE`."));
    let conda = facts("/opt/conda/envs/arena-env", false, false, "/root/ARENA_materials", true);
    assert!(conda.contains("Run /opt/conda/envs/arena-env/bin/python, or `export PATH=\"/opt/conda/envs/arena-env/bin:$PATH\"` first."), "{conda}");
    assert!(conda.contains("Install into the env: `/opt/conda/envs/arena-env/bin/python -m pip install PACKAGE`."), "{conda}");
    for b in [&gpu, &cpu, &conda] {
        let lines: Vec<&str> = b.lines().collect();
        assert!(lines.len() <= 10, "{b}");
        assert_eq!((lines[0], lines[lines.len() - 1]), (BEGIN, END));
        assert!(!b.contains('{') && !b.contains('}'), "every placeholder filled: {b}");
    }
}

/// Where the env is looked for: the image's conda env first on an image-based pod, the venv the
/// bare-VM script builds first on a VM — the places the VS Code warm-up looks.
#[test]
fn the_env_is_looked_for_where_each_kind_of_machine_has_it() {
    let image = env_candidates("arena-env", "/root/ARENA_materials/", false);
    assert_eq!(
        image,
        [
            "/opt/arena-env",
            "/opt/conda/envs/arena-env",
            "~/miniconda3/envs/arena-env",
            "~/miniforge3/envs/arena-env",
            "~/anaconda3/envs/arena-env",
            "/root/ARENA_materials/.venv",
        ]
    );
    let vm = env_candidates("arena-env", "/root/ARENA_materials", true);
    assert_eq!(vm[0], "/root/ARENA_materials/.venv");
    assert_eq!(vm[1..], image[..5]);
    // No usable conda env name: the venv alone.
    assert_eq!(env_candidates("", "/r", false), ["/r/.venv"]);
    let notes = AgentNotes::new("arena-env", "/root/ARENA_materials");
    for (provider, first) in [("runpod", "/opt/arena-env"), ("vast", "/opt/arena-env"), ("hetzner", "/root/ARENA_materials/.venv"), ("shadeform", "/root/ARENA_materials/.venv")] {
        assert_eq!(notes.envs(provider)[0], first, "{provider}");
    }
    assert!(has_gpu("runpod") && has_gpu("vast") && has_gpu("shadeform") && !has_gpu("hetzner"));
}

#[test]
fn agent_notes_switch_and_config() {
    for (raw, want) in [(None, Some(true)), (Some(""), Some(true)), (Some(" Yes"), Some(true)), (Some("0"), Some(false)), (Some("off"), Some(false)), (Some("later"), None)] {
        match (enabled(raw), want) {
            (Ok(got), Some(w)) => assert_eq!(got, w, "{raw:?}"),
            (Err(e), None) => assert!(e.to_string().contains("AGENT_NOTES"), "{raw:?}: {e}"),
            (got, want) => panic!("{raw:?}: got {got:?}, want {want:?}"),
        }
    }
    let cfg = |extra: &str| Config::parse(&format!("ARENA_REPO_OWNER=o\nARENA_REPO_NAME=r\nGIT_SSH_KEY_LOCAL=/k\n{extra}"));
    assert_eq!(SetupConfig::from_config(&cfg("")).unwrap().agent_notes, Some(AgentNotes::new("arena-env", "/root/r")));
    assert_eq!(
        SetupConfig::from_config(&cfg("CONDA_ENV=course\nBACKUP_REPO_PATH=/workspace/r")).unwrap().agent_notes,
        Some(AgentNotes::new("course", "/workspace/r"))
    );
    assert_eq!(SetupConfig::from_config(&cfg("AGENT_NOTES=0")).unwrap().agent_notes, None);
    assert!(SetupConfig::from_config(&cfg("AGENT_NOTES=maybe")).unwrap_err().to_string().contains("AGENT_NOTES"));
    // `--notes-only` needs no deploy key, only where the repo is.
    assert_eq!(AgentNotes::from_config(&Config::parse("ARENA_REPO_NAME=r\n")).unwrap(), AgentNotes::new("arena-env", "/root/r"));
    assert!(AgentNotes::from_config(&Config::parse("")).unwrap_err().to_string().contains("ARENA_REPO_NAME"));
}

/// In a whole setup the notes are a best-effort step once the env and the agents are there —
/// after the image config step or the bare-VM script, before the VS Code warm-up — on every
/// provider; `--notes-only` runs the same command as a required step.
#[test]
fn the_notes_step_comes_after_the_env_and_before_the_warm_up() {
    let cfg = Config::parse("ARENA_REPO_OWNER=o\nARENA_REPO_NAME=ARENA_materials\nGIT_SSH_KEY_LOCAL=/k\n");
    let scfg = SetupConfig::from_config(&cfg).unwrap();
    assert!(scfg.vscode.is_some() && scfg.agent_notes.is_some());
    let t = SetupTimeouts::default();
    for (provider, main) in [("runpod", "repo + keys config"), ("vast", "repo + keys config"), ("hetzner", "hetzner setup script"), ("shadeform", "GPU VM setup script")] {
        let steps = provisioning_steps(provider, &scfg, "devtest-apple", false, "/tmp/h.sh", &t);
        let labels: Vec<&str> = steps.iter().map(ProvisionStep::label).collect();
        let at = labels.iter().position(|l| *l == LABEL).unwrap_or_else(|| panic!("{provider}: {labels:?}"));
        assert_eq!((labels[at - 1], labels[at + 1], labels.len()), (main, "vscode warm-up", at + 2), "{provider}: {labels:?}");
        let ProvisionStep::Optional { cmd, summary, timeout, .. } = &steps[at] else { panic!("{provider}: {:?}", steps[at]) };
        let notes = scfg.agent_notes.as_ref().unwrap();
        assert_eq!((cmd, summary, *timeout), (&notes.command(provider), &notes.summary(provider), TIMEOUT));
        assert!(summary.contains("~/.claude/CLAUDE.md, ~/.codex/AGENTS.md") && summary.contains(&notes.envs(provider).join(", ")), "{summary}");
        assert_eq!(notes.step(provider, true), ProvisionStep::Run { label: LABEL, cmd: cmd.clone(), timeout: TIMEOUT });
    }
    // Off: no step, and nothing else changes.
    let off = SetupConfig { agent_notes: None, ..scfg.clone() };
    let with: Vec<ProvisionStep> = provisioning_steps("runpod", &scfg, "a", false, "", &t).into_iter().filter(|s| s.label() != LABEL).collect();
    assert_eq!(provisioning_steps("runpod", &off, "a", false, "", &t), with);
}

/// uv and uvx get onto every shell's PATH the same way on every machine: the bare-VM script, the
/// image-based pods' agents step and the notes step carry the same line.
#[test]
fn every_setup_links_uv_the_same_way() {
    assert!(crate::setup::HETZNER_SETUP.contains(LINK_UV_SH), "hetzner_setup.sh has drifted from LINK_UV_SH");
    let scfg = SetupConfig::from_config(&Config::parse("ARENA_REPO_OWNER=o\nARENA_REPO_NAME=r\nGIT_SSH_KEY_LOCAL=/k\n")).unwrap();
    let config = scfg.remote_command("devtest-apple", false);
    assert!(config.contains(&format!("/usr/local/bin/$b; done\n{LINK_UV_SH}\n) || true")), "{config}");
    // In the script: right after uv is installed, before anything that needs it.
    let s = crate::setup::HETZNER_SETUP;
    assert!(s.find("uv --version ||").unwrap() < s.find(LINK_UV_SH).unwrap() && s.find(LINK_UV_SH).unwrap() < s.find("### 5/7").unwrap());
    for provider in ["runpod", "hetzner"] {
        assert!(AgentNotes::new("arena-env", "/r").command(provider).contains(LINK_UV_SH), "{provider}");
    }
}

/// A scratch machine: a home, a /usr/local/bin and an env, all under a temp dir.
struct Machine {
    dir: PathBuf,
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Machine {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("arena-notes-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["home", "bin", "usrlocalbin"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        Self { dir }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    fn exe(&self, rel: &str) {
        let p = self.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A venv at `rel`: its python and its activate script.
    fn venv(&self, rel: &str) {
        self.exe(&format!("{rel}/bin/python"));
        self.put(&format!("{rel}/bin/activate"), "# activate\n");
    }

    fn put(&self, rel: &str, text: &str) {
        let p = self.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    }

    /// `cmd` (a notes command, its paths moved into this machine) run by `shell` with this home,
    /// and `uv` on PATH or not.
    fn run(&self, shell: &str, cmd: &str, uv: bool) -> Output {
        let d = self.dir.display().to_string();
        let cmd = cmd.replace("/usr/local/bin", &format!("{d}/usrlocalbin")).replace("'/opt/", &format!("'{d}/opt/"));
        // `uv` absent: asked for under a name no machine has (the test box may well have a uv).
        let cmd = if uv { cmd } else { cmd.replace("command -v uv ", "command -v arena-test-no-uv ") };
        let path = format!("{d}/bin:{}", std::env::var("PATH").unwrap_or_default());
        Command::new(shell).arg("-c").arg(&cmd).env("HOME", self.path("home")).env("PATH", path).output().unwrap()
    }

    /// The files kept beside `dir`'s notes (`<file>.arena-XXXXXX`), by name, with their text.
    fn kept(&self, dir: &str) -> Vec<String> {
        let mut kept: Vec<(String, String)> = std::fs::read_dir(self.path(dir))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".arena-"))
            .map(|n| (n.clone(), self.read(&format!("{dir}/{n}"))))
            .collect();
        kept.sort_by_key(|(_, text)| text.clone());
        kept.into_iter().map(|(_, text)| text).collect()
    }

    /// A stand-in for `name` on PATH: counts its calls, runs `<name>.hook<N>` (if the test wrote
    /// one) before the Nth and `<name>.after<N>` after it — a save, or a kill, at that very moment
    /// (`$ARENA_STUB_PARENT`: the script that called it) — and between them `body` ("$@" its
    /// arguments).
    fn hooked(&self, name: &str, body: &str) {
        let d = self.dir.display();
        let stub = self.path(&format!("bin/{name}"));
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\nc={d}/{name}.calls; n=$(($(cat \"$c\" 2>/dev/null || echo 0) + 1)); echo $n > \"$c\"\n\
                 export ARENA_STUB_PARENT=$PPID\n[ -f {d}/{name}.hook$n ] && sh {d}/{name}.hook$n\n{body}\nrc=$?\n\
                 [ -f {d}/{name}.after$n ] && sh {d}/{name}.after$n\nexit $rc\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn has_shell(shell: &str) -> bool {
    Command::new(shell).arg("-c").arg("true").status().is_ok_and(|s| s.success())
}

/// `name`'s path on this box, if it has one.
fn which(name: &str) -> Option<String> {
    let out = Command::new("sh").arg("-c").arg(format!("command -v {name}")).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|p| p.starts_with('/'))
}

fn ok(out: &Output) -> String {
    assert!(out.status.success(), "exit {:?}: {}{}", out.status.code(), String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The stopgap the fleet may already have (the brief's block, `$E` filled in).
fn stopgap(env: &str) -> String {
    format!(
        "{BEGIN}\n## Python on this machine\n- Use the course env, {env} (torch with CUDA, transformers and the course packages are installed). \
         Run {env}/bin/python, or source {env}/bin/activate first.\n- Shells started over ssh or by scripts may not have it activated, \
         so check which python before running anything.\n- Install packages into it: uv pip install --python {env}/bin/python PACKAGE. \
         Never pip install into the system Python (/usr/bin/python3).\n{END}\n"
    )
}

/// The notes command on a scratch machine, under sh, dash and bash (it runs `sh -c` whatever the
/// login shell; the inner shell is swapped to cover each) and from a zsh login shell: the env
/// found where each kind of machine has it (a conda env in the home too), the block written into
/// both files, the stopgap's block replaced where it was with the participant's text around it
/// kept byte for byte (and the file it was kept beside), a second run changing nothing (not even
/// the file's time, nothing more kept), the file's mode kept, no file / an empty file / a file of
/// only the block / a last line without a newline / two blocks made one, a file that's a link
/// (the link kept), a BEGIN with no END left alone, and no env: a warning and nothing written. uv
/// and uvx are linked into /usr/local/bin where nothing is there, and a link or a file already
/// there is kept.
#[test]
fn the_notes_land_between_the_markers_and_keep_the_rest() {
    let claude = "home/.claude/CLAUDE.md";
    let codex = "home/.codex/AGENTS.md";
    let mut ran = 0;
    for (outer, inner) in [("sh", "sh"), ("sh", "dash"), ("sh", "bash"), ("zsh", "sh")] {
        if !has_shell(outer) || !has_shell(inner) {
            continue;
        }
        ran += 1;
        let swap = |cmd: String| if inner == "sh" { cmd } else { cmd.replacen("sh -c ", &format!("{inner} -c "), 1) };

        // A RunPod/Vast pod: the image's env (a venv), no uv. No notes files yet (no ~/.claude).
        let m = Machine::new(&format!("image-{inner}-{outer}"));
        let env = m.path("opt/arena-env").display().to_string();
        let repo = m.path("root/ARENA_materials").display().to_string();
        m.venv("opt/arena-env");
        let pod = swap(AgentNotes::new("arena-env", &repo).command("runpod"));
        let said = ok(&m.run(outer, &pod, false));
        assert!(!said.contains("arena-warning"), "{inner}: {said}");
        let want = facts(&env, true, false, &repo, true);
        assert_eq!((m.read(claude), m.read(codex)), (want.clone(), want.clone()), "{inner}");
        assert!(m.kept("home/.claude").is_empty(), "{inner}: nothing was there to keep");
        // The participant's text around the stopgap's block; the file is theirs at 0600.
        let before = "# My notes\n\nUse black.\n";
        let after = "\n## More of mine\n- tabs, not spaces\n";
        let theirs = format!("{before}{}{after}", stopgap(&env));
        m.put(claude, &theirs);
        std::fs::set_permissions(m.path(claude), std::fs::Permissions::from_mode(0o600)).unwrap();
        ok(&m.run(outer, &pod, false));
        assert_eq!(m.read(claude), format!("{before}{want}{after}"), "{inner}: replaced where it was");
        assert_eq!(m.kept("home/.claude"), [theirs.clone()], "{inner}: the file it replaced is kept beside it");
        let mode = |rel: &str| std::fs::metadata(m.path(rel)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(claude), 0o600, "{inner}: mode kept");
        // Again: nothing changes, nothing is written, nothing more is kept.
        let mtime = |rel: &str| std::fs::metadata(m.path(rel)).unwrap().modified().unwrap();
        let (c, x) = (mtime(claude), mtime(codex));
        std::thread::sleep(std::time::Duration::from_millis(20));
        ok(&m.run(outer, &pod, false));
        assert_eq!(m.read(claude), format!("{before}{want}{after}"), "{inner}");
        assert_eq!((mtime(claude), mtime(codex)), (c, x), "{inner}: a second run writes nothing");
        assert_eq!(m.kept("home/.claude"), [theirs], "{inner}");
        // An empty file, a file of only the block, text without one (and no final newline).
        for (start, end) in [
            (String::new(), want.clone()),
            (want.clone(), want.clone()),
            ("mine".to_string(), format!("mine\n\n{want}")),
            ("mine\n\n".to_string(), format!("mine\n\n{want}")),
            // The participant's last line has no newline: it still hasn't, block before it or not.
            (format!("{}mine", stopgap(&env)), format!("{want}mine")),
            (format!("x\n{}\nmine", stopgap(&env)), format!("x\n{want}\nmine")),
            // Two blocks (whatever wrote them): one, where the first was — and the participant's
            // line between them keeps its newline, though the file ends without one.
            (format!("a\n{}mine\n{}", stopgap(&env), stopgap(&env).trim_end()), format!("a\n{want}mine\n")),
            (format!("a\n{}b\n{}c\n", stopgap(&env), stopgap(&env)), format!("a\n{want}b\nc\n")),
        ] {
            m.put(codex, &start);
            ok(&m.run(outer, &pod, false));
            assert_eq!(m.read(codex), end, "{inner}: from {start:?}");
            let kept = m.kept("home/.codex").len();
            ok(&m.run(outer, &pod, false));
            assert_eq!(m.read(codex), end, "{inner}: idempotent from {start:?}");
            assert_eq!(m.kept("home/.codex").len(), kept, "{inner}: nothing more kept from {start:?}");
        }
        // A file that's a link (to a dotfiles checkout, say): the file it points at gets the
        // block, and the link stays a link.
        m.put("dotfiles/AGENTS.md", "from my dotfiles\n");
        std::fs::remove_file(m.path(codex)).unwrap();
        std::os::unix::fs::symlink(m.path("dotfiles/AGENTS.md"), m.path(codex)).unwrap();
        ok(&m.run(outer, &pod, false));
        assert!(std::fs::symlink_metadata(m.path(codex)).unwrap().file_type().is_symlink(), "{inner}");
        assert_eq!(m.read("dotfiles/AGENTS.md"), format!("from my dotfiles\n\n{want}"), "{inner}");
        assert_eq!(m.kept("dotfiles"), ["from my dotfiles\n"], "{inner}");
        std::fs::remove_file(m.path(codex)).unwrap();
        // A BEGIN line with no END: left as it is, and said.
        let open = format!("mine\n{BEGIN}\nhalf a block\n");
        m.put(codex, &open);
        let kept = m.kept("home/.codex").len();
        let said = ok(&m.run(outer, &pod, false));
        assert_eq!(m.read(codex), open, "{inner}");
        assert!(said.contains("arena-warning: ") && said.contains("AGENTS.md has a BEGIN arena-infra python line with no END"), "{said}");
        assert_eq!(m.kept("home/.codex").len(), kept, "{inner}: no temp file left");

        // A Hetzner VM: the repo's venv (an env at /opt too is not the one), uv in ~/.local/bin.
        let m = Machine::new(&format!("vm-{inner}-{outer}"));
        let repo = m.path("root/ARENA_materials").display().to_string();
        m.venv("root/ARENA_materials/.venv");
        m.venv("opt/arena-env");
        for b in ["uv", "uvx"] {
            m.exe(&format!("home/.local/bin/{b}"));
            m.exe(&format!("bin/{b}"));
        }
        let vm = swap(AgentNotes::new("arena-env", &repo).command("hetzner"));
        ok(&m.run(outer, &vm, true));
        let want = facts(&format!("{repo}/.venv"), true, true, &repo, false);
        assert_eq!((m.read(claude), m.read(codex)), (want.clone(), want), "{inner}");
        for b in ["uv", "uvx"] {
            let link = std::fs::read_link(m.path(&format!("usrlocalbin/{b}"))).unwrap();
            assert_eq!(link, m.path(&format!("home/.local/bin/{b}")), "{inner}: {b}");
        }
        // The stopgap's links (or anything there) are kept: a link to elsewhere, a real file.
        std::fs::remove_file(m.path("usrlocalbin/uv")).unwrap();
        std::os::unix::fs::symlink("/elsewhere/uv", m.path("usrlocalbin/uv")).unwrap();
        std::fs::remove_file(m.path("usrlocalbin/uvx")).unwrap();
        m.put("usrlocalbin/uvx", "a uv of its own\n");
        ok(&m.run(outer, &vm, true));
        assert_eq!(std::fs::read_link(m.path("usrlocalbin/uv")).unwrap(), Path::new("/elsewhere/uv"), "{inner}");
        assert_eq!(m.read("usrlocalbin/uvx"), "a uv of its own\n", "{inner}");

        // A conda env in the home (`~/`): no activate script, so the env's bin first on PATH.
        let m = Machine::new(&format!("conda-{inner}-{outer}"));
        let repo = m.path("r").display().to_string();
        m.exe("home/miniconda3/envs/arena-env/bin/python");
        ok(&m.run(outer, &swap(AgentNotes::new("arena-env", &repo).command("vast")), false));
        let env = m.path("home/miniconda3/envs/arena-env").display().to_string();
        assert_eq!(m.read(claude), facts(&env, false, false, &repo, true), "{inner}");

        // No env anywhere (a non-arena image): a warning, nothing written, not a failure.
        let m = Machine::new(&format!("none-{inner}-{outer}"));
        let said = ok(&m.run(outer, &swap(AgentNotes::new("arena-env", &m.path("r").display().to_string()).command("runpod")), false));
        assert!(said.starts_with("arena-warning: no course env here (looked in ") && said.contains("no agent notes written"), "{said}");
        assert!(!m.path(claude).exists() && !m.path(codex).exists(), "{inner}");
    }
    assert!(ran > 0, "no shell to run the notes under");
}

/// A file that can't be written fails the step (non-zero, the reason last on stderr) — a
/// warning in a whole setup, a ✗ under `--notes-only` — and the other file is still written; one
/// that can't be replaced is left whole.
#[test]
fn a_file_that_cannot_be_written_fails_the_step() {
    let m = Machine::new("unwritable");
    let repo = m.path("r").display().to_string();
    m.venv("r/.venv");
    // ~/.claude is a file, so ~/.claude/CLAUDE.md can't be made.
    m.put("home/.claude", "not a dir\n");
    let out = m.run("sh", &AgentNotes::new("arena-env", &repo).command("hetzner"), false);
    assert!(!out.status.success(), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.trim_end().ends_with(&format!("couldn't write the agent notes into {}", m.path("home/.claude/CLAUDE.md").display())), "{err}");
    assert_eq!(m.read("home/.codex/AGENTS.md"), facts(&format!("{repo}/.venv"), true, false, &repo, false));
    // A file there that can't be replaced (its folder is read-only; not as root, which writes
    // anyway): it fails, and the file is left whole.
    let root = Command::new("id").arg("-u").output().map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0").unwrap_or(true);
    if !root {
        m.put("home/.codex/AGENTS.md", "mine\n");
        std::fs::set_permissions(m.path("home/.codex"), std::fs::Permissions::from_mode(0o555)).unwrap();
        let out = m.run("sh", &AgentNotes::new("arena-env", &repo).command("hetzner"), false);
        std::fs::set_permissions(m.path("home/.codex"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!out.status.success(), "{out:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("couldn't write the agent notes into"), "{out:?}");
        assert_eq!(m.read("home/.codex/AGENTS.md"), "mine\n");
    }
}

/// Nothing a participant saves while the notes are being written is lost — they, Claude Code's own
/// `#` memory or another setup may save at any moment. Stand-ins for `python3` (the swap) and `ln`
/// (the create) save, or stop the step, at the worst moments:
/// - between the read and the swap: it goes back, and round again — the save and the block both
///   end up in the file;
/// - again before the swap back: the first save is in the file with the block, the second kept
///   beside it, and said;
/// - a file made between the check that there's none and the create: that one gets the block;
/// - a writer holding the file open across the swap: its write lands in the file kept beside it;
/// - the step stopped (TERM) right after a swap that took out a save: that save is kept;
/// - no swap in one step here: a file that's there is left as it is, with a warning.
#[test]
fn saves_made_meanwhile_are_never_lost() {
    let (Some(python), Some(ln)) = (which("python3"), which("ln")) else {
        return; // no python3 here: the swap can't be made
    };
    let notes = |m: &Machine| AgentNotes::new("arena-env", &m.path("r").display().to_string()).command("hetzner");
    let want = |m: &Machine| {
        let repo = m.path("r").display().to_string();
        facts(&format!("{repo}/.venv"), true, false, &repo, false)
    };
    let claude = "home/.claude/CLAUDE.md";
    let machine = |tag: &str| {
        let m = Machine::new(tag);
        m.venv("r/.venv");
        m.put(claude, "mine\n");
        m.hooked("python3", &format!("\"{python}\" \"$@\""));
        m
    };
    let save = |m: &Machine, hook: &str, text: &str| m.put(hook, &format!("printf '{text}\\n' >> {}\n", m.path(claude).display()));

    // Saved between the read and the swap.
    let m = machine("race-swap");
    save(&m, "python3.hook1", "saved meanwhile");
    let said = ok(&m.run("sh", &notes(&m), false));
    assert!(!said.contains("arena-warning"), "{said}");
    assert_eq!(m.read(claude), format!("mine\nsaved meanwhile\n\n{}", want(&m)));
    assert_eq!(m.read("python3.calls"), "3\n", "swap, swap back, swap (AGENTS.md is made, not swapped)");
    assert_eq!(m.kept("home/.claude"), [format!("mine\n\n{}", want(&m)), "mine\nsaved meanwhile\n".to_string()]);

    // …and again before the swap back.
    let m = machine("race-twice");
    save(&m, "python3.hook1", "first save");
    save(&m, "python3.hook2", "second save");
    let said = ok(&m.run("sh", &notes(&m), false));
    assert_eq!(m.read(claude), format!("mine\nfirst save\n\n{}", want(&m)));
    let kept = m.kept("home/.claude");
    assert_eq!(kept, [format!("mine\n\n{}second save\n", want(&m)), "mine\nfirst save\n".to_string()]);
    assert!(said.contains("CLAUDE.md changed while the notes were being written - a save made meanwhile is kept as "), "{said}");

    // A file made between the check that there's none and the create.
    let m = Machine::new("race-create");
    m.venv("r/.venv");
    m.hooked("ln", &format!("\"{ln}\" \"$@\""));
    m.put("ln.hook1", &format!("printf 'theirs\\n' > {}\n", m.path(claude).display()));
    ok(&m.run("sh", &notes(&m), false));
    assert_eq!(m.read(claude), format!("theirs\n\n{}", want(&m)));
    assert_eq!(m.read("home/.codex/AGENTS.md"), want(&m));

    // A writer that holds the file open across the swap: what it writes is in the kept file.
    let m = machine("open-writer");
    let mut held = std::fs::OpenOptions::new().append(true).open(m.path(claude)).unwrap();
    ok(&m.run("sh", &notes(&m), false));
    std::io::Write::write_all(&mut held, b"written late\n").unwrap();
    drop(held);
    assert_eq!(m.read(claude), format!("mine\n\n{}", want(&m)));
    assert_eq!(m.kept("home/.claude"), ["mine\nwritten late\n"]);

    // Stopped right after a swap that took a save out: the save is kept, not cleaned up.
    let m = machine("stopped");
    save(&m, "python3.hook1", "saved meanwhile");
    m.put("python3.after1", "kill -TERM $ARENA_STUB_PARENT\n");
    let _ = m.run("sh", &notes(&m), false);
    assert_eq!(m.read(claude), format!("mine\n\n{}", want(&m)));
    assert_eq!(m.kept("home/.claude"), ["mine\nsaved meanwhile\n"]);

    // No swap in one step here.
    let m = machine("no-swap");
    m.hooked("python3", "exit 2");
    let said = ok(&m.run("sh", &notes(&m), false));
    assert_eq!(m.read(claude), "mine\n");
    assert!(said.contains("CLAUDE.md can't be swapped for the new one in one step here"), "{said}");
    assert_eq!(m.read("home/.codex/AGENTS.md"), want(&m));
    assert!(m.kept("home/.claude").is_empty() && m.kept("home/.codex").is_empty());
}
