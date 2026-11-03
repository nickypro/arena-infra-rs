//! `arena clods install` and clods' part of setup, `up` and replace (`arena_core::clods`): what
//! is printed, the `--clods-only` run and `config check`'s row. Configured means `CLODS_DIR` names
//! a folder with a `tokens` file; otherwise setup says nothing about clods at all.

use std::path::{Path, PathBuf};

use anyhow::Result;
use arena_core::clods::{self, Clods, Staged, Wrote};
use arena_core::out;
use arena_core::remote::Remote;
use arena_core::ssh::SshTarget;
use arena_core::{Config, Pod};
use clap::Subcommand;

use crate::pods::selection::Selected;

#[derive(Subcommand)]
pub(crate) enum ClodsCmd {
    /// Write the clods scripts this build carries into a clods folder
    ///
    /// DIR: default CLODS_DIR from the config, else ~/.clods. Writes setup.sh, push.sh,
    /// clods.sh, README.md, fleet.example and .gitignore where they differ, and never touches
    /// tokens, fleet or profiles/. How a box's own clods gets updates. (The pods get the scripts
    /// from setup: CLODS_DIR set, every account to each pod.)
    #[command(verbatim_doc_comment)]
    Install {
        /// The clods folder (default: CLODS_DIR, else ~/.clods)
        dir: Option<PathBuf>,
        /// Preview only: say what would be written, write nothing
        #[arg(long, visible_aliases = ["dryrun", "dry"])]
        dry_run: bool,
    },
}

/// `path` with `$HOME` shown as `~`.
fn shown(path: &Path) -> String {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) if path == home => "~".into(),
        Some(home) => match path.strip_prefix(&home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        },
        None => path.display().to_string(),
    }
}

/// `arena clods`. Runs before the config is loaded (as `update`): a box with no arena config can
/// still install clods; `cfg` is the config when there is one, for `CLODS_DIR`.
pub(crate) fn handle_clods(cmd: ClodsCmd, cfg: Option<&Config>) -> Result<()> {
    match cmd {
        ClodsCmd::Install { dir, dry_run } => {
            let dir = dir.or_else(|| cfg.and_then(clods::configured_dir)).unwrap_or_else(|| clods::expand_home("~/.clods"));
            handle_install(&dir, dry_run)
        }
    }
}

/// [`clods::install`] into `dir`, and a line: what was written (or would be), or that it's up to
/// date; where there's no tokens file yet, what to do next.
pub(crate) fn handle_install(dir: &Path, dry_run: bool) -> Result<()> {
    let done = clods::install(dir, dry_run).map_err(|e| anyhow::anyhow!("writing into {}: {e}", dir.display()))?;
    let at = shown(dir);
    let names = |w: Wrote| done.iter().filter(|(_, x)| *x == w).map(|(n, _)| *n).collect::<Vec<_>>();
    let (new, updated) = (names(Wrote::New), names(Wrote::Updated));
    if new.is_empty() && updated.is_empty() {
        out!(Out, "{}", out::ok(format!("{at} is up to date")));
        return Ok(());
    }
    let mut what = Vec::new();
    if !updated.is_empty() {
        what.push(format!("{} updated", updated.join(", ")));
    }
    if !new.is_empty() {
        what.push(format!("{} new", new.join(", ")));
    }
    let what = what.join("; ");
    if dry_run {
        out!(Out, "Would write into {at}: {what} (tokens, fleet and profiles/ untouched)");
        return Ok(());
    }
    out!(Out, "{}", out::ok(format!("{at}: {what} (tokens, fleet and profiles/ untouched)")));
    if !dir.join(clods::TOKENS).exists() {
        out!(Out, "Next: your accounts in {at}/tokens (one `email -> sk-ant-oat01-…` per line, chmod 600), then {at}/setup.sh");
    }
    Ok(())
}

/// What setup says about clods before it starts (`-v`): the accounts every pod gets, or why none
/// go. `None`: not configured.
pub(crate) fn plan_line(cfg: &Config) -> Option<String> {
    let c = Clods::from_config(cfg)?;
    Some(match c.read() {
        Ok(t) => format!(
            "clods: {} from {} — every pod gets them all, a default by spread unless it has one",
            clods::accounts(t.accounts.len() as u32),
            shown(&c.tokens_path())
        ),
        Err(e) => format!("clods: {} {e} — nothing pushed", out::cross()),
    })
}

/// clods' part of setup and replace, for `pods` (the ones set up, each with its target): a line
/// per pod, and a warning with its fix for a pod that didn't get it — never a failed setup. Nothing
/// at all when clods isn't configured. How many pods didn't get it.
pub(crate) async fn stage(remote: &dyn Remote, cfg: &Config, pods: &[(Pod, SshTarget)]) -> usize {
    match clods::stage(remote, cfg, pods).await {
        None => 0,
        Some(Staged::Unusable(why)) => {
            out!(Err, "{}", out::warn(format!("claude: clods not pushed — {why}")));
            pods.len()
        }
        Some(Staged::Pushed(outcomes)) => {
            let mut failed = 0;
            for o in &outcomes {
                if o.result.is_ok() {
                    out!(Brief, "{}", o.brief());
                    out!(Detail, "{}", o.detail());
                } else {
                    out!(Err, "{}", o.brief());
                    failed += 1;
                }
            }
            failed
        }
    }
}

/// `pods setup --clods-only`: just the clods push, to running pods — nothing else of setup. It's
/// all the run does, so a pod that didn't get it fails the command (after the rest are pushed).
pub(crate) async fn handle_clods_only(remote: &dyn Remote, cfg: &Config, apply: bool, sel: &Selected) -> Result<()> {
    let Some(c) = Clods::from_config(cfg) else {
        anyhow::bail!(
            "clods isn't configured: set {} to the folder holding your clods `tokens` (e.g. ~/.clods), then run this again",
            clods::DIR_KEY
        );
    };
    let tokens = c.read().map_err(|e| anyhow::anyhow!("clods: {e} — nothing pushed"))?;
    let targets = sel.ssh_targets(cfg)?;
    if targets.is_empty() {
        out!(Out, "(no pods with an SSH endpoint)");
        return Ok(());
    }
    let names: Vec<&str> = targets.iter().map(|(p, _)| p.name.as_str()).collect();
    let n = clods::accounts(tokens.accounts.len() as u32);
    if !apply {
        out!(Out, "Would push clods ({n}, defaults spread) to {}", crate::pods::setup::all_names(&names));
        out!(Detail, "{}", plan_line(cfg).unwrap_or_default());
        return Ok(());
    }
    let pods: Vec<(Pod, SshTarget)> = targets.into_iter().map(|(p, t)| (p.clone(), t)).collect();
    let failed = stage(remote, cfg, &pods).await;
    if pods.len() > 1 && failed == 0 {
        out!(Brief, "{}", out::ok(format!("clods on {} of {} pods", pods.len(), pods.len())));
    }
    if failed > 0 {
        anyhow::bail!("{failed} pod(s) didn't get clods");
    }
    Ok(())
}

/// `config check`'s `CLODS_DIR` row: unset, set with its accounts, set with no tokens file (setup
/// pushes nothing), or a tokens file that can't be used (then also in `missing`).
pub(crate) fn config_check_row(cfg: &Config, missing: &mut Vec<String>) -> String {
    let key = clods::DIR_KEY;
    let Some(dir) = clods::configured_dir(cfg) else {
        return format!("  · {key:<28} (missing — optional: setup puts every clods account on each pod)");
    };
    match Clods::from_config(cfg) {
        None => format!("  ⚠ {key:<28} {}: no tokens file there — setup pushes nothing", shown(&dir)),
        Some(c) => match c.read() {
            Ok(t) => format!(
                "  ✓ {key:<28} {}: {} (every pod gets them all; tokens never shown)",
                shown(&dir),
                clods::accounts(t.accounts.len() as u32)
            ),
            Err(e) => {
                missing.push(format!("{key} (its tokens file can't be used)"));
                format!("  ✗ {key:<28} {e}")
            }
        },
    }
}

#[cfg(test)]
#[path = "clods_tests.rs"]
mod tests;
