//! `pods lock`/`unlock`, and how a dry run says the real run would be refused (`print_would_refuse`).

#[cfg(test)]
use std::time::Duration;

use anyhow::Result;
use arena_core::capability::Op;
use arena_core::provider::Provider;

use crate::confirm;
use crate::pods::selection::Selected;

/// Appended to a replace/cutover refused over a lock: the replacement isn't locked by itself.
/// (Worded for all three: `replace --keep-old` and `migrate cutover` leave the terminate to
/// a later step, but they still retire the locked pod.)
pub(crate) const LOCKED_REPLACEMENT_NOTE: &str = " (it retires this pod — renamed to <name>-old, then terminated — for a new one; \
     lock the replacement with `arena pods lock` once it's in)";

/// A dry run's note that the real run would be refused or fail (a locked pod: the same
/// sentence the real run fails with). One helper so every command's dry run says it the same
/// way — and, under test, keeps the line, so a test can see it was printed
/// ([`take_would_refuse`]).
pub(crate) fn print_would_refuse(r: &str) {
    let line = format!("(without --dry-run: {r})");
    #[cfg(test)]
    WOULD_REFUSE.with(|n| n.borrow_mut().push(line.clone()));
    println!("{line}");
}

#[cfg(test)]
thread_local! {
    static WOULD_REFUSE: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Under test: the [`print_would_refuse`] lines printed on this thread so far (taken). A
/// `#[tokio::test]` runs the command on its own thread, so these are that test's.
#[cfg(test)]
pub(crate) fn take_would_refuse() -> Vec<String> {
    WOULD_REFUSE.with(|n| std::mem::take(&mut *n.borrow_mut()))
}

/// Under test: take `lock` (a per-pod backup lock file), retrying for up to 2 s — whether a lock
/// this process just released is free. A [`arena_core::pull::PodLock`] unlocks when dropped, so
/// one of ours is free at once; one released by a close alone (a run that was killed) can read
/// as held for a moment: a child another test thread forks meanwhile (a git, an rsync stub)
/// holds a copy of every open file — the lock's too — until it execs (`O_CLOEXEC` closes it only
/// then), and a flock belongs to the open file, not to the process. A lock still held after 2 s
/// is a real one. Says on stderr when it had to wait (`--nocapture` shows it).
#[cfg(test)]
pub(crate) fn lock_settles_free(lock: std::fs::File) -> bool {
    lock_settles(lock).is_some()
}

/// Under test: [`lock_settles_free`]'s lock, taken and held (until the guard is dropped).
#[cfg(test)]
pub(crate) fn lock_settles(mut lock: std::fs::File) -> Option<arena_core::pull::PodLock> {
    let start = std::time::Instant::now();
    loop {
        match arena_core::pull::try_lock(lock).unwrap() {
            Ok(held) => {
                if !start.elapsed().is_zero() {
                    eprintln!("(lock_settles_free: free after {:?})", start.elapsed());
                }
                return Some(held);
            }
            Err(f) => lock = f,
        }
        if start.elapsed() > Duration::from_secs(2) {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `pods lock` / `pods unlock` (`lock` says which). The selection is sorted first
/// ([`arena_core::lock::plan`]): pods already in that state are said and left alone; pods
/// whose backend can't lock (RUNPOD_API=v1, Vast, Hetzner) are reported one by one and make
/// the exit non-zero — the operator asked for them to be (un)protected and they aren't; the
/// rest are set one at a time after one confirm, a failure reported and carried on from.
pub(crate) async fn handle_lock(provider: &dyn Provider, sel: &Selected, lock: bool, dry_run: bool, yes: bool) -> Result<()> {
    use arena_core::lock;
    let (verb, done) = if lock { ("lock", "locked") } else { ("unlock", "unlocked") };
    let plan = lock::plan(&sel.pods, lock, |p| provider.supports(Op::Lock, Some(p)).check());
    for p in &plan.already {
        println!("= {} already {done}", p.name);
    }
    for (p, why) in &plan.unsupported {
        eprintln!("✗ {} can't be {done}: {why}", p.name);
    }
    let unsupported: Vec<&str> = plan.unsupported.iter().map(|(p, _)| p.name.as_str()).collect();
    // The command fails for these: they were asked to be (un)protected and can't be. Other
    // providers' pods in a broad selection can be left out with `--on runpod`.
    let others_only = plan.unsupported.iter().all(|(p, _)| p.provider != "runpod");
    let cant = (!unsupported.is_empty()).then(|| {
        format!(
            "{} pod(s) can't be {done}: {}{}",
            unsupported.len(),
            unsupported.join(", "),
            if others_only { " (leave other providers' pods out with --on runpod)" } else { "" }
        )
    });
    let names: Vec<&str> = plan.act.iter().map(|p| p.name.as_str()).collect();
    let what = format!("{verb} {} pod(s): {}\n  ({})", names.len(), names.join(", "), lock::consequence(lock));
    if dry_run {
        if !names.is_empty() {
            println!("[dry-run] would {what}");
        }
        if let Some(c) = &cant {
            println!("(without --dry-run this fails: {c})");
        }
        println!("\nDry-run only — nothing {done}.");
        return Ok(());
    }
    if names.is_empty() {
        if let Some(c) = cant {
            anyhow::bail!("{c}");
        }
        println!("nothing to {verb}.");
        return Ok(());
    }
    if !confirm(yes, &format!("Will {what}"))? {
        println!("aborted.");
        return Ok(());
    }
    let policy = arena_core::retry::RetryPolicy::default();
    let mut failed: Vec<&str> = Vec::new();
    for p in &plan.act {
        match arena_core::retry::retrying(&policy, || provider.set_locked(&p.id, lock)).await {
            Ok(()) => println!("[{done}] {}", p.name),
            Err(e) => {
                eprintln!("[FAILED] {}: {e}", p.name);
                failed.push(p.name.as_str());
            }
        }
    }
    println!("\n{done} {}/{}", names.len() - failed.len(), names.len());
    let mut problems = Vec::new();
    if !failed.is_empty() {
        problems.push(format!("{} pod(s) failed to {verb}: {}", failed.len(), failed.join(", ")));
    }
    problems.extend(cant);
    if !problems.is_empty() {
        anyhow::bail!("{}", problems.join("; "));
    }
    Ok(())
}
