use super::*;
use crate::setup::{provisioning_steps, SetupConfig, SetupTimeouts};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The block as it lands on each kind of machine, line for line: a RunPod/Vast pod (the image's
/// conda env, no uv on it: pip) and a Hetzner VM (the repo's venv, uv linked onto PATH).
#[test]
fn the_block_for_each_kind_of_machine() {
    let gpu = block("/opt/arena-env", false, "/root/ARENA_materials", true);
    assert_eq!(
        gpu,
        "<!-- BEGIN arena-infra python -->\n\
         ## Python on this machine (arena setup rewrites this block; write your own notes outside it)\n\
         - Use the course env, /opt/arena-env: torch with CUDA, transformers, transformer_lens and the other course packages are in it. \
         Run /opt/arena-env/bin/python, or `source /opt/arena-env/bin/activate` first.\n\
         - Shells started over ssh, by scripts or by your tools may not have it active, and their `python3` is then the system one \
         (/usr/bin/python3), without those packages. Check `which python` before running anything.\n\
         - Missing a package? Check first: `/opt/arena-env/bin/python -m pip show PACKAGE`. Install into the env: \
         `/opt/arena-env/bin/python -m pip install PACKAGE`. Never pip install into the system Python, and don't make another venv \
         or conda env for the course.\n\
         - The course repo (the exercises, and the participants' work) is /root/ARENA_materials.\n\
         <!-- END arena-infra python -->\n"
    );
    let cpu = block("/root/ARENA_materials/.venv", true, "/root/ARENA_materials", false);
    assert!(cpu.contains(
        "- Use the course env, /root/ARENA_materials/.venv: torch (a CPU build: this machine has no GPU), transformers, \
         transformer_lens and the other course packages are in it. Run /root/ARENA_materials/.venv/bin/python, or \
         `source /root/ARENA_materials/.venv/bin/activate` first.\n"
    ));
    assert!(cpu.contains("Install into the env: `uv pip install --python /root/ARENA_materials/.venv/bin/python PACKAGE`."));
    for b in [&gpu, &cpu] {
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
}

fn has_shell(shell: &str) -> bool {
    Command::new(shell).arg("-c").arg("true").status().is_ok_and(|s| s.success())
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
/// found where each kind of machine has it, the block written into both files, the stopgap's block
/// replaced in place with the participant's text around it kept byte for byte, a second run
/// changing nothing (not even the file's time), the file's mode kept, no file / an empty file /
/// a file of only the block, two blocks made one, a BEGIN with no END left alone, and no env: a
/// warning and nothing written. uv and uvx are linked into /usr/local/bin where nothing is there,
/// and a link or a file already there is kept.
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

        // A RunPod/Vast pod: the image's env, no uv. No notes files yet (no ~/.claude at all).
        let m = Machine::new(&format!("image-{inner}-{outer}"));
        let env = m.path("opt/arena-env").display().to_string();
        let repo = m.path("root/ARENA_materials").display().to_string();
        m.exe("opt/arena-env/bin/python");
        let pod = swap(AgentNotes::new("arena-env", &repo).command("runpod"));
        let said = ok(&m.run(outer, &pod, false));
        assert!(!said.contains("arena-warning"), "{inner}: {said}");
        let want = block(&env, false, &repo, true);
        assert_eq!((m.read(claude), m.read(codex)), (want.clone(), want.clone()), "{inner}");
        // The participant's text around the stopgap's block; the file is theirs at 0600.
        let before = "# My notes\n\nUse black.\n";
        let after = "\n## More of mine\n- tabs, not spaces\n";
        m.put(claude, &format!("{before}{}{after}", stopgap(&env)));
        std::fs::set_permissions(m.path(claude), std::fs::Permissions::from_mode(0o600)).unwrap();
        ok(&m.run(outer, &pod, false));
        assert_eq!(m.read(claude), format!("{before}{want}{after}"), "{inner}: replaced in place");
        let mode = |rel: &str| std::fs::metadata(m.path(rel)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(claude), 0o600, "{inner}: mode kept");
        // Again: nothing changes, nothing is written.
        let mtime = |rel: &str| std::fs::metadata(m.path(rel)).unwrap().modified().unwrap();
        let (c, x) = (mtime(claude), mtime(codex));
        std::thread::sleep(std::time::Duration::from_millis(20));
        ok(&m.run(outer, &pod, false));
        assert_eq!(m.read(claude), format!("{before}{want}{after}"), "{inner}");
        assert_eq!((mtime(claude), mtime(codex)), (c, x), "{inner}: a second run writes nothing");
        // An empty file, a file of only the block, text without one (and no final newline).
        for (start, end) in [
            (String::new(), want.clone()),
            (want.clone(), want.clone()),
            ("mine".to_string(), format!("mine\n\n{want}")),
            ("mine\n\n".to_string(), format!("mine\n\n{want}")),
            // Two blocks (whatever wrote them): one, where the first was.
            (format!("a\n{}b\n{}c\n", stopgap(&env), stopgap(&env)), format!("a\n{want}b\nc\n")),
        ] {
            m.put(codex, &start);
            ok(&m.run(outer, &pod, false));
            assert_eq!(m.read(codex), end, "{inner}: from {start:?}");
            ok(&m.run(outer, &pod, false));
            assert_eq!(m.read(codex), end, "{inner}: idempotent from {start:?}");
        }
        // A BEGIN line with no END: left as it is, and said.
        let open = format!("mine\n{BEGIN}\nhalf a block\n");
        m.put(codex, &open);
        let said = ok(&m.run(outer, &pod, false));
        assert_eq!(m.read(codex), open, "{inner}");
        assert!(said.contains("arena-warning: ") && said.contains("AGENTS.md has a BEGIN arena-infra python line with no END"), "{said}");

        // A Hetzner VM: the repo's venv (an env at /opt too is not the one), uv in ~/.local/bin.
        let m = Machine::new(&format!("vm-{inner}-{outer}"));
        let repo = m.path("root/ARENA_materials").display().to_string();
        m.exe("root/ARENA_materials/.venv/bin/python");
        m.exe("opt/arena-env/bin/python");
        for b in ["uv", "uvx"] {
            m.exe(&format!("home/.local/bin/{b}"));
            m.exe(&format!("bin/{b}"));
        }
        let vm = swap(AgentNotes::new("arena-env", &repo).command("hetzner"));
        ok(&m.run(outer, &vm, true));
        let want = block(&format!("{repo}/.venv"), true, &repo, false);
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

        // No env anywhere (a non-arena image): a warning, nothing written, not a failure.
        let m = Machine::new(&format!("none-{inner}-{outer}"));
        let said = ok(&m.run(outer, &swap(AgentNotes::new("arena-env", &m.path("r").display().to_string()).command("runpod")), false));
        assert!(said.starts_with("arena-warning: no course env here (looked in ") && said.contains("no agent notes written"), "{said}");
        assert!(!m.path(claude).exists() && !m.path(codex).exists(), "{inner}");
    }
    assert!(ran > 0, "no shell to run the notes under");
}

/// A file that can't be written fails the step (non-zero, the reason last on stderr) — a
/// warning in a whole setup, a ✗ under `--notes-only` — and the other file is still written.
#[test]
fn a_file_that_cannot_be_written_fails_the_step() {
    let m = Machine::new("unwritable");
    let repo = m.path("r").display().to_string();
    m.exe("r/.venv/bin/python");
    // ~/.claude is a file, so ~/.claude/CLAUDE.md can't be made.
    m.put("home/.claude", "not a dir\n");
    let out = m.run("sh", &AgentNotes::new("arena-env", &repo).command("hetzner"), false);
    assert!(!out.status.success(), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.trim_end().ends_with(&format!("couldn't write the agent notes into {}", m.path("home/.claude/CLAUDE.md").display())), "{err}");
    assert_eq!(m.read("home/.codex/AGENTS.md"), block(&format!("{repo}/.venv"), false, &repo, false));
}
