//! `pods cp`: copy a local file or tree to every selected pod (scp), and check where it landed.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use arena_core::remote::{describe_error, Remote, PROBE_TIMEOUT};
use arena_core::ssh::SshTarget;
use arena_core::Config;

use crate::pods::selection::Selected;
use crate::pods::{cp_timeout, each_pod, local_size};
use crate::{confirm, shell_quote};

/// Resolve where a copied file should land on the pod. With an explicit `dest`, use it
/// verbatim. Otherwise, if the local path runs through the repo dir (`<repo_name>/…`),
/// mirror that path under the pod's repo parent (so `…/ARENA_3.0/a/b.py` →
/// `/root/ARENA_3.0/a/b.py`); failing that, fall back to the file's basename (lands in
/// the login/home dir). Pure, so it's unit-tested.
fn resolve_remote_dest(
    local: &std::path::Path,
    dest: Option<&str>,
    repo_name: &str,
    repo_parent: &str,
) -> String {
    if let Some(d) = dest {
        return d.to_string();
    }
    let s = local.to_string_lossy().replace('\\', "/");
    let needle = format!("{repo_name}/");
    if let Some(idx) = s.find(&needle) {
        return format!("{}/{}", repo_parent.trim_end_matches('/'), &s[idx..]);
    }
    local.file_name().and_then(|n| n.to_str()).unwrap_or("copied_file").to_string()
}

/// `pods copy`: scp a local file to every selected pod, creating the remote parent
/// dir first. Destination per [`resolve_remote_dest`]. Pods run concurrently, each step
/// bounded (see [`copy_to_pod`]); the scp gets `timeout` (`--timeout`), else a budget
/// proportional to what's sent ([`cp_timeout`]).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_copy(
    // How we reach pods: `SshRemote` for real, `FakeRemote` in tests.
    remote_ssh: Arc<dyn Remote>,
    cfg: &Config,
    file: &std::path::Path,
    dest: Option<&str>,
    recursive: bool,
    // The pods to copy to, resolved by the caller.
    sel: &Selected,
    timeout: Option<Duration>,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    if recursive {
        if !file.exists() {
            anyhow::bail!("not found: {}", file.display());
        }
    } else if !file.is_file() {
        anyhow::bail!("not a file: {} (pass -r to copy a directory)", file.display());
    }
    let local = file.to_string_lossy().into_owned();

    // Resolve the remote destination (same for every pod).
    let repo_name = cfg.get("ARENA_REPO_NAME").unwrap_or("ARENA_materials");
    let repo_path = cfg.get("BACKUP_REPO_PATH").map(String::from).unwrap_or_else(|| format!("/root/{repo_name}"));
    let repo_parent = std::path::Path::new(&repo_path)
        .parent()
        .and_then(|p| p.to_str())
        .unwrap_or("/root");
    let remote = resolve_remote_dest(file, dest, repo_name, repo_parent);
    // The parent dir to ensure exists: the dir itself if `remote` ends with `/`, else its dirname.
    let remote_parent = if remote.ends_with('/') {
        remote.trim_end_matches('/').to_string()
    } else {
        match remote.rsplit_once('/') {
            Some((dir, _)) if !dir.is_empty() => dir.to_string(),
            _ => ".".to_string(),
        }
    };

    let targets = sel.named_targets(cfg)?;
    if targets.is_empty() {
        println!("(no pods with an SSH endpoint to copy to)");
        return Ok(());
    }

    let rflag = if recursive { "-r " } else { "" };
    let budget = timeout.unwrap_or_else(|| cp_timeout(local_size(file), targets.len()));
    // Clear input→output preview before the confirm. Label the *type* (file vs folder)
    // explicitly — that's the thing people get wrong — and, for folders, spell out the
    // scp -r nesting rule (an existing dest dir, or a trailing slash, lands the folder
    // INSIDE → <dest>/<name>) that quietly produces a `name/name` mess.
    let is_dir = file.is_dir();
    let kind = if is_dir { "folder" } else { "file" };
    let base = file.file_name().and_then(|n| n.to_str()).unwrap_or("");
    println!("Copy preview (same on every pod):");
    println!("  input  [{kind} — check the type!]: {local}");
    println!("  output [{kind}]:                   {remote}");
    println!(
        "  budget: {}s per pod{}",
        budget.as_secs(),
        if timeout.is_some() { " (--timeout)" } else { " (scales with size × pods; --timeout to change)" }
    );
    // Surface a flag/type mismatch: -r on a file is harmless but usually a mistake.
    if recursive && !is_dir {
        println!("  note: -r was passed but '{local}' is a FILE on disk, not a folder — copying it as a single file.");
    }
    if is_dir {
        let nested = format!("{}/{base}", remote.trim_end_matches('/'));
        println!(
            "  ⚠ -r nesting: scp drops the folder AT that path, but if {dest} already exists \
             on the pod (or {dest} has a trailing slash) it lands INSIDE → {nested}\n  \
             To overwrite a folder's contents, copy to its PARENT (or `rm -rf` the remote dir first).",
            dest = remote.trim_end_matches('/'),
        );
    }
    if dry_run {
        println!("\nDry-run — would scp to {} pod(s):\n", targets.len());
        for (name, t) in &targets {
            println!("# {name}");
            let args: Vec<String> = t.scp_args(&local, &remote).iter().map(|a| arena_core::ssh::shell_word(a)).collect();
            println!("scp {rflag}{}\n", args.join(" "));
        }
        println!("Preview only — run without --dry-run to copy.");
        return Ok(());
    }
    let names = targets.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ");
    if !confirm(yes, &format!("Will copy {local} to {} on {} pod(s): {names}", remote, targets.len()))? {
        println!("aborted.");
        return Ok(());
    }

    // For a single file we verify it actually landed afterward (scp can exit 0 without
    // writing what you intended — e.g. the dest already exists as a directory, so the
    // file lands *inside* it). Skipped for -r (the dest is a tree, not one file).
    let plan = Arc::new(CopyPlan {
        expect_size: if recursive { None } else { std::fs::metadata(file).ok().map(|m| m.len()) },
        basename: file.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string(),
        local,
        remote,
        remote_parent,
        recursive,
        timeout: budget,
    });
    let jobs = targets
        .into_iter()
        .map(|(name, t)| {
            let (remote_ssh, plan) = (remote_ssh.clone(), plan.clone());
            (name, async move { copy_to_pod(remote_ssh.as_ref(), &t, &plan).await })
        })
        .collect();
    let (mut ok, mut failed) = (0, 0);
    each_pod(jobs, |done, total, name, result| match result.and_then(|copied| copied) {
        Ok(()) => {
            println!("[{done}/{total}] ✓ {name}");
            ok += 1;
        }
        Err(e) => {
            println!("[{done}/{total}] ✗ {name}: {e}");
            failed += 1;
        }
    })
    .await;
    println!("\nDone: {ok} copied, {failed} failed.");
    if failed > 0 {
        anyhow::bail!("{failed} pod(s) failed to receive the file");
    }
    Ok(())
}

/// What `pods cp` copies where, and how it checks the copy landed — the same on every pod.
pub(crate) struct CopyPlan {
    pub local: String,
    pub remote: String,
    /// The remote dir to `mkdir -p` first.
    pub remote_parent: String,
    /// `-r`: copy a tree (`scp -r`); no size check.
    pub recursive: bool,
    /// A single file's local size, verified on the pod after the copy (`None` with `-r`).
    pub expect_size: Option<u64>,
    pub basename: String,
    /// The scp's budget (see [`cp_timeout`]).
    pub timeout: Duration,
}

/// One pod's `pods cp`: `mkdir -p` the parent → scp (`-r` for a tree) → for a single file,
/// verify it landed at the expected size. Stops at the first failure: no copy into a
/// parent that couldn't be made, no check of a copy that failed. The mkdir and the check
/// are quick probes (`PROBE_TIMEOUT`); the scp gets the plan's `timeout`.
pub(crate) async fn copy_to_pod(remote: &dyn Remote, t: &SshTarget, plan: &CopyPlan) -> std::result::Result<(), String> {
    let mkdir = format!("mkdir -p {}", shell_quote(&plan.remote_parent));
    match remote.exec(t, &mkdir, Some(PROBE_TIMEOUT)).await {
        Ok(o) if o.success => {}
        Ok(o) => return Err(format!("mkdir failed: {}", o.stderr.trim())),
        Err(e) => return Err(describe_error(&e)),
    }
    let copied = if plan.recursive {
        remote.copy_recursive(t, &plan.local, &plan.remote, Some(plan.timeout)).await
    } else {
        remote.copy(t, &plan.local, &plan.remote, Some(plan.timeout)).await
    };
    match copied {
        Ok(o) if o.success => {}
        Ok(o) => return Err(o.stderr.trim().to_string()),
        Err(e) => return Err(describe_error(&e)),
    }
    // Verify the single-file copy actually landed at the expected size. A `remote` ending
    // in `/` means "into this dir" (intended → check <remote><basename>); otherwise
    // `remote` should BE the file, and finding a directory there is a silent misplacement
    // (scp dropped the file inside it) we flag rather than pass.
    let Some(expected) = plan.expect_size else { return Ok(()) };
    let remote_path = &plan.remote;
    let check = if remote_path.ends_with('/') {
        let final_path = format!("{remote_path}{}", plan.basename);
        format!(
            "f={f}; [ -f \"$f\" ] && echo \"OK $(wc -c < \"$f\" | tr -d ' ')\" || echo MISSING",
            f = shell_quote(&final_path),
        )
    } else {
        format!(
            "f={r}; if [ -d \"$f\" ]; then echo MISPLACED; elif [ -f \"$f\" ]; then echo \"OK $(wc -c < \"$f\" | tr -d ' ')\"; else echo MISSING; fi",
            r = shell_quote(remote_path),
        )
    };
    match remote.exec(t, &check, Some(PROBE_TIMEOUT)).await {
        Ok(o) if o.success => {
            let line = o.stdout.trim();
            if let Some(n) = line.strip_prefix("OK ") {
                match n.trim().parse::<u64>() {
                    Ok(sz) if sz == expected => Ok(()),
                    Ok(sz) => Err(format!("size mismatch after copy: {sz}B on pod vs {expected}B local (partial / clobbered)")),
                    Err(_) => Ok(()), // couldn't parse size; don't false-fail
                }
            } else if line == "MISPLACED" {
                Err(format!("{remote_path} is a directory on the pod — the file landed *inside* it; pass an explicit file DEST or remove that dir"))
            } else {
                Err(format!("nothing at {remote_path} after scp (silent non-write)"))
            }
        }
        // If the verify probe itself can't run (or times out), don't override a successful scp.
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn copy_dest_mirrors_repo_path_or_falls_back() {
        use super::resolve_remote_dest;
        use std::path::Path;
        // explicit dest wins
        assert_eq!(
            resolve_remote_dest(Path::new("./x/y.py"), Some("/tmp/z.py"), "ARENA_3.0", "/root"),
            "/tmp/z.py"
        );
        // path through the repo dir mirrors under the repo parent
        assert_eq!(
            resolve_remote_dest(Path::new("./ARENA_3.0/ch1/ex/tests.py"), None, "ARENA_3.0", "/root"),
            "/root/ARENA_3.0/ch1/ex/tests.py"
        );
        // not under the repo -> basename (lands in home)
        assert_eq!(
            resolve_remote_dest(Path::new("/home/dev/notes.txt"), None, "ARENA_3.0", "/root"),
            "notes.txt"
        );
    }
}
