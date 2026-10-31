//! `pods copy-home <from> <to>`: move a person to another machine that keeps its own name
//! (the human, 2026-10-30: "migrating from arena9-rarity -> arena9-scoot") — their home copied
//! straight from one machine to the other, nothing renamed.
//!
//! The same copy as `replace`/`migrate` ([`copy_pod_files`]: direct machine to machine, the
//! replication filters — `.git` kept, caches and venvs left — a replica that never destroys
//! what it replaces on the receiver, the delivery verified on the right machine), behind three
//! checks: no one on `<from>` (what they'd write after the copy wouldn't be on `<to>`), no one on
//! `<to>`, and none of `<to>`'s own files in the way — a dry run of the very copy says which it
//! would overwrite or delete there, and those are refused unless `--overwrite` (they're moved
//! aside to `~/arena-sync-kept/<stamp>/`, never deleted, like `pods restore` keeps what it
//! replaces).

use std::sync::Arc;

use anyhow::{Context, Result};
use arena_core::provider::Provider;
use arena_core::remote::Remote;
use arena_core::selector::Naming;
use arena_core::{Config, Pod};

use crate::confirm;
use crate::pods::adopt;
use crate::pods::home_copy::{copy_pod_files, would_move_aside, CopyOpts};
use crate::pods::lock::print_would_refuse;
use crate::pods::replace::{clean_marker, copy_scope_line};
use crate::pods::selection::resolve_target_any;

/// `copy-home`'s switches.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CopyHomeOpts {
    /// Copy over `<to>`'s own files (moved aside, never deleted).
    pub overwrite: bool,
    /// Copy with someone on either machine.
    pub force: bool,
    pub dry_run: bool,
    pub yes: bool,
}

/// How many of the files in the way a refusal names.
const NAMED: usize = 8;

/// `pods copy-home <from> <to>`: resolve both (on one provider), then [`copy_home_on`].
pub(crate) async fn handle_copy_home(remote: Arc<dyn Remote>, cfg: &Config, from: &str, to: &str, opts: CopyHomeOpts) -> Result<()> {
    let (owner, from_id, from_label) = resolve_target_any(cfg, from).await?;
    let pods = owner.list_pods().await.context("listing pods")?;
    let src = pods.iter().find(|p| p.id == from_id).cloned().with_context(|| format!("{from_label} vanished while planning"))?;
    let naming = Naming::from_config(cfg);
    let dest = match arena_core::selector::resolve_one(&naming, &pods, to) {
        Ok(i) => pods[i].clone(),
        Err(e) => anyhow::bail!(
            "{to}: {e} — copy-home copies between two machines of one provider ({} is on {}): the copy runs from one \
             straight to the other",
            src.name,
            owner.name()
        ),
    };
    copy_home_on(remote, cfg, owner.as_ref(), &src, &dest, opts).await
}

/// Why `src` → `dest` can't be a home copy, needing no probe (pure): the same machine, a
/// stopped one (its endpoint is stale), or a migration's own `-new`/`-old` (that's `migrate`'s).
pub(crate) fn pair_refusal(src: &Pod, dest: &Pod) -> Option<String> {
    if src.id == dest.id {
        return Some(format!("{} and {} are the same machine", src.name, dest.name));
    }
    for p in [src, dest] {
        if arena_core::status::is_stopped(&p.status) {
            return Some(format!("{} is stopped ({}) — `arena pods start {}` first; nothing was copied", p.name, p.status, p.name));
        }
        if p.name.ends_with("-new") || p.name.ends_with("-old") {
            return Some(format!(
                "{} is a migration's own pod — `arena pods migrate copy|cutover` moves a machine onto its `-new`",
                p.name
            ));
        }
    }
    None
}

/// The refusal for `dest`'s own files in the way (`files`: what the copy would move aside),
/// naming the first few; `None` when there are none or `--overwrite` was given. Pure.
pub(crate) fn files_refusal(src: &str, dest: &str, files: &[String], overwrite: bool) -> Option<String> {
    if files.is_empty() || overwrite {
        return None;
    }
    let shown: Vec<&str> = files.iter().take(NAMED).map(String::as_str).collect();
    let more = files.len().saturating_sub(NAMED);
    let more = if more > 0 { format!(" and {more} more") } else { String::new() };
    Some(format!(
        "{dest} has files of its own the copy would overwrite or delete ({} file{}: {}{more}) — someone's work, perhaps. \
         Back it up first (`arena pods pull {dest}`), then re-run with --overwrite: they're moved aside to ~/{}/<time>/ \
         on {dest}, never deleted. (Copying {src}'s home onto a fresh machine needs no flag.)",
        files.len(),
        if files.len() == 1 { "" } else { "s" },
        shown.join(", "),
        arena_core::replica::KEPT_DIR
    ))
}

/// The refusals for someone on `src` or `dest` (none with `force`), `src`'s first. Read-only.
async fn busy_refusal(remote: &dyn Remote, cfg: &Config, src: &Pod, dest: &Pod, force: bool) -> Vec<String> {
    let (from, to) = (src.name.as_str(), dest.name.as_str());
    let mut busy = Vec::new();
    if force {
        return busy;
    }
    for (pod, alternative) in [
        (src, format!("What they write after the copy wouldn't be on {to}: re-run once they're off.")),
        (dest, format!("The copy writes into {to}'s home under them: re-run once they're off.")),
    ] {
        let presence = arena_core::idle::presence(&adopt::probe(remote, cfg, pod).await);
        if let Some(why) = adopt::presence_words(&presence) {
            busy.push(format!("refusing to copy {from}'s home onto {to}: {} — {why}. {alternative} Or pass --force.", pod.name));
        }
    }
    busy
}

/// [`handle_copy_home`] once both machines are known (`owner` lists and holds both) — the seam
/// tests drive with a fake owner.
pub(crate) async fn copy_home_on(remote: Arc<dyn Remote>, cfg: &Config, owner: &dyn Provider, src: &Pod, dest: &Pod, opts: CopyHomeOpts) -> Result<()> {
    let CopyHomeOpts { overwrite, force, dry_run, yes } = opts;
    let (from, to) = (src.name.as_str(), dest.name.as_str());
    if let Some(r) = pair_refusal(src, dest) {
        anyhow::bail!("refusing to copy {from}'s home onto {to}: {r}");
    }
    let floor = crate::disk::floor_refusal(cfg, "copy-home");
    if let (Some(r), false) = (&floor, dry_run) {
        anyhow::bail!("{r}");
    }
    // No one on either: what they'd write on `from` after the copy wouldn't reach `to`, and
    // someone on `to` is working in the home it overwrites.
    let busy = busy_refusal(remote.as_ref(), cfg, src, dest, force).await;
    if let (Some(r), false) = (busy.first(), dry_run) {
        anyhow::bail!("{r}");
    }
    // What of `to`'s own the copy would move aside: the copy's own dry run says.
    let in_way = match would_move_aside(cfg, owner, remote.as_ref(), &src.id, &dest.id).await {
        Ok(files) => files,
        Err(why) if overwrite => {
            eprintln!("note: couldn't tell what of {to}'s own the copy replaces ({why}) — --overwrite: whatever it is is moved aside");
            Vec::new()
        }
        Err(why) => anyhow::bail!(
            "refusing to copy {from}'s home onto {to}: couldn't tell whether {to} has files of its own in the way ({why}). \
             --overwrite copies anyway (what it replaces is moved aside to ~/{}/ on {to}, never deleted)",
            arena_core::replica::KEPT_DIR
        ),
    };
    let in_the_way = files_refusal(from, to, &in_way, overwrite);
    if let (Some(r), false) = (&in_the_way, dry_run) {
        anyhow::bail!("{r}");
    }

    let plan = format!(
        "copy {from}'s home onto {to} ({}, ids {} → {}): {}; deletions mirrored, and what it replaces of {to}'s own ({} \
         file{}) moved aside to ~/{}/ there. Nothing is renamed: {from} keeps its name and files.",
        owner.name(),
        src.id,
        dest.id,
        copy_scope_line(),
        in_way.len(),
        if in_way.len() == 1 { "" } else { "s" },
        arena_core::replica::KEPT_DIR
    );
    if dry_run {
        for r in busy.iter().chain(floor.iter()).chain(in_the_way.iter()) {
            print_would_refuse(r);
        }
        println!("[dry-run] would {plan}\n[dry-run] nothing changed.");
        return Ok(());
    }
    if !confirm(yes, &format!("Will {plan}"))? {
        println!("aborted.");
        return Ok(());
    }
    // Asked again after the dry run and the prompt (review r2: someone may have come on) — and
    // the dry run again (review r3: a file of `to`'s own may have appeared while it waited).
    if let Some(r) = busy_refusal(remote.as_ref(), cfg, src, dest, force).await.into_iter().next() {
        anyhow::bail!("{r} (nothing was copied)");
    }
    if !overwrite {
        match would_move_aside(cfg, owner, remote.as_ref(), &src.id, &dest.id).await {
            Ok(files) => {
                if let Some(r) = files_refusal(from, to, &files, false) {
                    anyhow::bail!("{r} (they appeared while the prompt waited; nothing was copied)");
                }
            }
            Err(why) => anyhow::bail!(
                "refusing to copy {from}'s home onto {to}: couldn't look again whether {to} has files of its own in the way \
                 ({why}) — nothing was copied; --overwrite copies anyway (moved aside, never deleted)"
            ),
        }
    }
    println!("copying {from} → {to}…");
    copy_pod_files(cfg, owner, remote.as_ref(), &src.id, &dest.id, &CopyOpts::new(false))
        .await
        .with_context(|| format!("copying {from}'s home onto {to} — nothing was renamed; re-run to continue (rsync picks up where it stopped)"))?;
    clean_marker(owner, remote.as_ref(), &dest.id, cfg).await;
    println!("✓ {to} has {from}'s home (verified on {to} itself). {from} is untouched.");
    let ledger = crate::spend::SpendCtx::from_config(cfg).state();
    let owner_of = arena_core::spend::owner_of(&ledger, &src.provider, &src.id).map(String::from);
    println!("  Next: point the person at {to} (`ssh {to}` via the proxy / ssh-config: its own name, port and keys).");
    if let Some(o) = owner_of {
        // `spend assign` refuses to move a machine's history from one person to another without
        // --force: say so up front when `to` has run up spend under someone else.
        let names = [dest.name.clone()];
        let theirs = arena_core::spend::assign_moves(&ledger, &names, &dest.id, arena_core::snapshot::unix_now()).ok().flatten();
        match theirs.filter(|m| m.takes_from(Some(&o))) {
            Some(m) => println!(
                "  Their spend follows if you say so: `arena spend assign {to} {o} --force` — {to}'s own history ({}, {}'s) moves to {o} with it.",
                m.words(),
                m.from.as_deref().unwrap_or(arena_core::spend::NO_OWNER)
            ),
            None => println!("  Their spend follows if you say so: `arena spend assign {to} {o}`."),
        }
    }
    println!("  Once they're settled: `arena pods terminate {from}` (back it up first: `arena pods backup {from}`).");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(id: &str, name: &str, status: &str) -> Pod {
        Pod { id: id.into(), name: name.into(), provider: "hetzner".into(), status: status.into(), ..Default::default() }
    }

    #[test]
    fn a_pair_that_cant_be_copied_is_refused_up_front() {
        let a = pod("1", "devtest-rarity", "RUNNING");
        assert!(pair_refusal(&a, &a).unwrap().contains("same machine"));
        assert!(pair_refusal(&a, &pod("2", "devtest-scoot", "OFF")).unwrap().contains("arena pods start devtest-scoot"));
        assert!(pair_refusal(&a, &pod("2", "devtest-scoot-new", "RUNNING")).unwrap().contains("migration's own pod"));
        assert_eq!(pair_refusal(&a, &pod("2", "devtest-scoot", "RUNNING")), None);
    }

    #[test]
    fn files_in_the_way_are_named_and_need_overwrite() {
        assert_eq!(files_refusal("a", "b", &[], false), None);
        let files: Vec<String> = (1..=10).map(|i| format!("work/f{i}.py")).collect();
        assert_eq!(files_refusal("a", "b", &files, true), None);
        let r = files_refusal("devtest-rarity", "devtest-scoot", &files, false).unwrap();
        assert!(r.contains("(10 files: work/f1.py,") && r.contains("work/f8.py and 2 more"), "{r}");
        assert!(r.contains("arena pods pull devtest-scoot") && r.contains("--overwrite") && r.contains("never deleted"), "{r}");
    }
}
