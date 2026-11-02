//! Notes for the coding agents on a machine: which Python to use there.
//!
//! Why: setup installs Claude Code and Codex on every machine, and they don't know the course env.
//! On project week's first day a Claude asked to run GPT-2 on a GPU pod used the system Python
//! (`/usr/bin/python3`), found transformers missing and pip-installed it there, while
//! `/opt/arena-env` had torch with CUDA and transformers all along. A shell started over ssh, by a
//! script or by an agent's tool may not have the env active (live 2026-11-02: `ssh host 'cmd'` and
//! `zsh -lc` find only the system `python3` on every kind of machine; see the PR for the table).
//!
//! So setup writes a short block of this machine's facts — the env's real path, how to install
//! into it, the course repo — into Claude Code's user memory (`~/.claude/CLAUDE.md`) and Codex's
//! (`~/.codex/AGENTS.md`), between [`BEGIN`] and [`END`]. The block is replaced, never added twice,
//! and the rest of each file is the participant's: kept as it was. Which env is found on the
//! machine ([`env_candidates`]: the first that has a `bin/python`), and whether `uv` is there.
//! The step first links `uv`/`uvx` into `/usr/local/bin` ([`LINK_UV_SH`]): on a Hetzner VM uv was
//! only in `~/.local/bin`, which no shell puts on PATH (`uv` → "command not found", project week).
//!
//! Split like the VS Code warm-up: the block's text and the on-machine script's settings are here
//! (pure, [`block`] renders what lands on a machine); `agent_notes.sh` does the work. Setup runs
//! it as a best-effort step ([`AgentNotes::step`]); `pods setup --notes-only` runs just this step
//! on running machines, touching nothing else.

use std::time::Duration;

use crate::config::Config;
use crate::error::Result;
use crate::setup::{bare_vm, shell_quote, BareVm, ProvisionStep};

/// The line that starts the block (whole, on a line of its own; the stopgap run on the fleet
/// wrote the same markers, so this replaces its block).
pub const BEGIN: &str = "<!-- BEGIN arena-infra python -->";
/// The line that ends it.
pub const END: &str = "<!-- END arena-infra python -->";

/// The files the block goes into: Claude Code's user memory and Codex's global instructions.
pub const FILES: [&str; 2] = ["~/.claude/CLAUDE.md", "~/.codex/AGENTS.md"];

/// The step's label (its warnings say it).
pub const LABEL: &str = "agent notes";

/// The step's budget: a few small files and two symlinks.
pub const TIMEOUT: Duration = Duration::from_secs(60);

/// Links `uv` and `uvx` from `~/.local/bin` (where astral's installer puts them) into
/// `/usr/local/bin`, which every shell has on PATH — login or not, zsh or bash, `ssh host 'cmd'`,
/// `pods run` and the agents' own tools. Only where nothing is there yet: a link already there
/// (one made by hand, or by a run before) is kept, and so is a uv installed there some other way.
/// Run by setup's agents step (image-based pods), the bare-VM script (`hetzner_setup.sh` carries
/// the same text; a test holds them together) and the notes step.
pub const LINK_UV_SH: &str = r#"for arena_b in uv uvx; do if [ -x "$HOME/.local/bin/$arena_b" ] && [ ! -e "/usr/local/bin/$arena_b" ] && [ ! -L "/usr/local/bin/$arena_b" ]; then ln -s "$HOME/.local/bin/$arena_b" "/usr/local/bin/$arena_b" || echo "arena-warning: linking /usr/local/bin/$arena_b to $HOME/.local/bin/$arena_b failed"; fi; done"#;

/// The on-machine script ([`AgentNotes::command`] fills it in).
const SCRIPT: &str = include_str!("agent_notes.sh");

/// How to install into the env when `uv` is on PATH…
const INSTALL_UV: &str = "uv pip install --python {env}/bin/python PACKAGE";
/// …and when it isn't (both envs have pip: the image's conda env, and the venv the bare-VM script
/// builds, which installs pip into it).
const INSTALL_PIP: &str = "{env}/bin/python -m pip install PACKAGE";

/// The block, a line each, `{env}` / `{install}` / `{repo}` filled in per machine. Under about 10
/// lines: it's read at the start of every session.
fn template(gpu: bool) -> [&'static str; 7] {
    [
        BEGIN,
        "## Python on this machine (arena setup rewrites this block; write your own notes outside it)",
        if gpu {
            "- Use the course env, {env}: torch with CUDA, transformers, transformer_lens and the other course packages are in it. Run {env}/bin/python, or `source {env}/bin/activate` first."
        } else {
            "- Use the course env, {env}: torch (a CPU build: this machine has no GPU), transformers, transformer_lens and the other course packages are in it. Run {env}/bin/python, or `source {env}/bin/activate` first."
        },
        "- Shells started over ssh, by scripts or by your tools may not have it active, and their `python3` is then the system one (/usr/bin/python3), without those packages. Check `which python` before running anything.",
        "- Missing a package? Check first: `{env}/bin/python -m pip show PACKAGE`. Install into the env: `{install}`. Never pip install into the system Python, and don't make another venv or conda env for the course.",
        "- The course repo (the exercises, and the participants' work) is {repo}.",
        END,
    ]
}

/// `t` with each `{env}` / `{install}` / `{repo}` replaced by `var(name)` and the text between by
/// `text(piece)` — the one walk both renderings share, so they can't drift apart.
fn render(t: &str, text: impl Fn(&str) -> String, var: impl Fn(&str) -> String) -> String {
    const VARS: [&str; 3] = ["env", "install", "repo"];
    let mut out = String::new();
    let mut rest = t;
    while let Some((at, name)) =
        VARS.iter().filter_map(|v| rest.find(&format!("{{{v}}}")).map(|i| (i, *v))).min_by_key(|(i, _)| *i)
    {
        if at > 0 {
            out.push_str(&text(&rest[..at]));
        }
        out.push_str(&var(name));
        rest = &rest[at + name.len() + 2..];
    }
    if !rest.is_empty() || out.is_empty() {
        out.push_str(&text(rest));
    }
    out
}

/// The block as it lands on a machine whose env is `env` (`uv`: uv is on PATH there), its course
/// repo `repo`, with a GPU or not. Lines end in `\n`. Pure.
pub fn block(env: &str, uv: bool, repo: &str, gpu: bool) -> String {
    let install = render(if uv { INSTALL_UV } else { INSTALL_PIP }, str::to_string, |_| env.to_string());
    let value = |name: &str| match name {
        "env" => env.to_string(),
        "install" => install.clone(),
        _ => repo.to_string(),
    };
    template(gpu).into_iter().map(|l| render(l, str::to_string, value) + "\n").collect()
}

/// `t` as one shell word: the text single-quoted, each placeholder its shell variable ($E the
/// env, $I the install command, $R the repo), double-quoted.
fn shell_word(t: &str) -> String {
    render(t, shell_quote, |name| match name {
        "env" => "\"$E\"".into(),
        "install" => "\"$I\"".into(),
        _ => "\"$R\"".into(),
    })
}

/// Where the course env can be on a machine, most likely first: on an image-based pod (RunPod,
/// Vast) the image's conda env (`/opt/<CONDA_ENV>`, then the usual conda roots) before the repo's
/// venv; on a bare VM (Hetzner, Shadeform) the venv its setup script builds (`<repo>/.venv`)
/// first. The same places the VS Code warm-up looks ([`crate::vscode::python_candidates`]).
/// `~/` is the machine user's home. Pure.
pub fn env_candidates(conda_env: &str, repo: &str, bare: bool) -> Vec<String> {
    let mut envs: Vec<String> = crate::vscode::python_candidates(conda_env, repo)
        .into_iter()
        .filter_map(|p| p.strip_suffix("/bin/python").map(String::from))
        .collect();
    if bare {
        // The venv is the candidates' last entry.
        envs.rotate_right(1);
    }
    envs
}

/// Whether `provider`'s machines have a GPU for the block to speak of: all but a CPU VM's
/// (Hetzner). Pure.
fn has_gpu(provider: &str) -> bool {
    bare_vm(provider) != Some(BareVm::Cpu)
}

/// `AGENT_NOTES`: on unless `0`/`false`/`no`/`off`; absent or empty = on. Anything else is an
/// error rather than a guess.
pub fn enabled(raw: Option<&str>) -> Result<bool> {
    match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        None | Some("" | "1" | "true" | "yes" | "on") => Ok(true),
        Some("0" | "false" | "no" | "off") => Ok(false),
        Some(other) => Err(crate::Error::Config(format!("AGENT_NOTES must be 1 or 0 (got `{other}`)"))),
    }
}

/// The notes' settings, from config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentNotes {
    /// `CONDA_ENV` (default `arena-env`): the image's env's name.
    pub conda_env: String,
    /// The course checkout on the machine (`BACKUP_REPO_PATH`, else `/root/<ARENA_REPO_NAME>`).
    pub repo_path: String,
}

impl AgentNotes {
    /// From config: `CONDA_ENV` and the repo path as setup has it
    /// ([`crate::setup::repo_path`]; an error without `BACKUP_REPO_PATH` or `ARENA_REPO_NAME`).
    pub fn from_config(cfg: &Config) -> Result<Self> {
        Ok(Self::new(cfg.get("CONDA_ENV").unwrap_or("arena-env"), &crate::setup::repo_path(cfg)?))
    }

    pub fn new(conda_env: &str, repo_path: &str) -> Self {
        Self { conda_env: conda_env.to_string(), repo_path: repo_path.to_string() }
    }

    /// Where the env can be on `provider`'s machines ([`env_candidates`]).
    pub fn envs(&self, provider: &str) -> Vec<String> {
        env_candidates(&self.conda_env, &self.repo_path, crate::setup::is_bare_vm(provider))
    }

    /// The command for one `Remote::exec` on a `provider` machine: [`LINK_UV_SH`], then the notes
    /// (`agent_notes.sh`, filled in), under `sh` whatever the login shell. Exits 0 with an
    /// `arena-warning:` line when no env is there (a non-arena image); non-zero only when a file
    /// couldn't be written.
    pub fn command(&self, provider: &str) -> String {
        let envs = self.envs(provider);
        let quoted: Vec<String> = envs.iter().map(|e| shell_quote(e)).collect();
        let no_env = format!("{}no course env here (looked in {}) - no agent notes written", crate::volume::WARNING_PREFIX, envs.join(", "));
        let block: Vec<String> = template(has_gpu(provider)).into_iter().map(shell_word).collect();
        let script = SCRIPT
            .replace("@LINK_UV@", LINK_UV_SH)
            .replace("@ENVS@", &quoted.join(" "))
            .replace("@NO_ENV@", &shell_quote(&no_env))
            .replace("@REPO@", &shell_quote(&self.repo_path))
            .replace("@INSTALL_UV@", &shell_word(INSTALL_UV))
            .replace("@INSTALL_PIP@", &shell_word(INSTALL_PIP))
            .replace("@BLOCK@", &block.join(" "))
            .replace("@BEGIN@", &shell_quote(BEGIN))
            .replace("@END@", &shell_quote(END))
            .replace("@WARN@", crate::volume::WARNING_PREFIX);
        format!("sh -c {} arena-agent-notes", shell_quote(&script))
    }

    /// What the dry-run shows in place of [`Self::command`].
    pub fn summary(&self, provider: &str) -> String {
        format!(
            "link uv/uvx into /usr/local/bin (where missing); the Python notes ({}, the block between {BEGIN} and {END}) for \
             the first env there of {}, the repo {}",
            FILES.join(", "),
            self.envs(provider).join(", "),
            self.repo_path,
        )
    }

    /// The step on a `provider` machine: best-effort in a whole setup (a failure is a warning on a
    /// set-up pod, as the VS Code warm-up's), required on its own (`pods setup --notes-only`:
    /// it's all that run does, so a failure fails the pod).
    pub fn step(&self, provider: &str, required: bool) -> ProvisionStep {
        let cmd = self.command(provider);
        if required {
            ProvisionStep::Run { label: LABEL, cmd, timeout: TIMEOUT }
        } else {
            ProvisionStep::Optional { label: LABEL, cmd, summary: self.summary(provider), timeout: TIMEOUT }
        }
    }
}

#[cfg(test)]
mod tests;
