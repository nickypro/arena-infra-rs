//! `arena pods …`: the dispatch over `PodCmd`, the per-call SSH budgets and the run-on-every-pod
//! helpers. Each command family is a submodule (`up`, `jobs`, `ask`, `reap`, `resolve` predate this).

use std::sync::Arc;
use std::time::Duration;

use arena_core::remote::{describe_error, Remote};
use arena_core::ssh::SshTarget;

pub(crate) mod backup;
pub(crate) mod branch;
pub(crate) mod list;
pub(crate) mod lock;
pub(crate) mod pull;
pub(crate) mod selection;

// Per-call SSH budgets (PLAN 1.B). Every pod-SSH call is bounded so one wedged pod can
// never hang a fleet command: it reports `✗ <name>: timed out after Ns`, counts as a
// failure, and the others carry on. Quick probes share `arena_core::remote::PROBE_TIMEOUT`.

/// `pods test`: a cold `import torch` (CUDA libs off a cold disk) takes 10–30s, so 90s
/// means the pod is wedged — and a read-only health check must not hang on it.
pub(crate) const TEST_TIMEOUT: Duration = Duration::from_secs(90);

/// `pods run`'s default per-pod budget (`--timeout` overrides). Arbitrary commands can be
/// long (a download, a test suite), so it's generous — but never unbounded: a fleet run
/// must end even when one pod wedges. 30 min.
pub(crate) const RUN_TIMEOUT_SECS: u64 = 30 * 60;

/// `pods backup`: git add + commit + push of a participant's tree — seconds normally, but
/// a first push of notebooks/outputs can take minutes. 5 min.
pub(crate) const BACKUP_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// `pods set-branch` / `init-branches`: a fetch + checkout (+ fast-forward or push) against
/// GitHub — seconds normally, so 2 min means stuck (a hung fetch, a held lock).
pub(crate) const BRANCH_TIMEOUT: Duration = Duration::from_secs(2 * 60);

// set-branch's switch and init-branches bound themselves on the pod (`timeout`, then a KILL) inside
// that budget, so their pod side has ended before arena reports a timeout and releases the pod's
// lock (review ai-review claude #13).
const _: () = assert!(arena_core::backup::SWITCH_BOUND_SECS + arena_core::backup::SWITCH_KILL_AFTER_SECS < BRANCH_TIMEOUT.as_secs());

/// `pods cp`'s scp budget floor: connection setup, a small file, a slow start — 10 min.
/// What's copied adds to it ([`cp_timeout`]). Its `mkdir -p` and post-copy size check are
/// quick probes.
pub(crate) const CP_BASE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The slowest operator uplink `pods cp` budgets for: 1 MB/s (≈8 Mbit/s, a tethered or
/// hotel connection). Every pod's scp starts at once from the operator's machine, so the
/// copies share it — the budget is for detecting a wedged copy, not a slow one.
const CP_MIN_BYTES_PER_SEC: u64 = 1_000_000;

/// `pods cp`'s per-pod scp budget, proportional to what's copied: [`CP_BASE_TIMEOUT`] plus
/// the time to send `bytes` to all `pods` at [`CP_MIN_BYTES_PER_SEC`], capped at a day
/// (the `--timeout` maximum). A fixed budget killed big fleet-wide copies part-way: 500 MB
/// to 30 pods over a 20 MB/s uplink needs ~750s, so every pod would fail at 600s.
pub(crate) fn cp_timeout(bytes: u64, pods: usize) -> Duration {
    let sending = bytes.saturating_mul(pods as u64) / CP_MIN_BYTES_PER_SEC;
    Duration::from_secs(
        CP_BASE_TIMEOUT.as_secs().saturating_add(sending).min(arena_core::setup::MAX_STEP_TIMEOUT_SECS),
    )
}

/// What `pods cp` sends for `path`: a file's size, or a tree's total (as `scp -r` sends
/// it). Best-effort, for [`cp_timeout`] only: unreadable entries count 0 and symlinked
/// dirs inside the tree aren't followed (no loops) — the base budget absorbs the slack.
pub(crate) fn local_size(path: &std::path::Path) -> u64 {
    fn tree(dir: &std::path::Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
        entries
            .flatten()
            .map(|e| match e.file_type() {
                Ok(t) if t.is_dir() => tree(&e.path()),
                // A file, or a symlink to one (scp copies the target).
                Ok(_) => std::fs::metadata(e.path()).ok().filter(|m| m.is_file()).map_or(0, |m| m.len()),
                Err(_) => 0,
            })
            .sum()
    }
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => tree(path),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

/// replace/migrate's direct pod-to-pod rsync of a home dir (caches/models excluded): a
/// few GB, minutes pod-to-pod. Two hours means the transfer is wedged, and the pipeline
/// must give the operator their terminal back instead of waiting forever. Each leg of the
/// via-local fallback ([`CopyLeg`]) gets the same budget, for all of its rsyncs together.
pub(crate) const POD_COPY_TIMEOUT: Duration = Duration::from_secs(2 * 3600);

/// How one pod's SSH call ended, ready for its report line: the command's output (which
/// may still be a non-zero exit), or why there is none — `timed out after Ns`, a spawn
/// error, a crashed task.
pub(crate) type PodCall = std::result::Result<arena_core::ssh::SshOutput, String>;

/// Run one job per pod concurrently and hand each pod's result to `on_done(done, total,
/// key, result)` as it finishes. Finishing order is the point: healthy pods report at
/// once, and a pod whose job is stuck reports when its budget runs out — never holding the
/// others, or the command, hostage (jobs bound their own SSH calls). A job that panicked is
/// still reported, by its key, as `Err` — never silently dropped from the tally.
///
/// `key` is usually the pod's name (all a report line needs). A caller that matches results
/// back to pods uses something unique instead — two pods can share a name (a double
/// create, a leftover), and a name-keyed map would hand one pod the other's result.
pub(crate) async fn each_pod<K, T, F>(
    jobs: Vec<(K, F)>,
    mut on_done: impl FnMut(usize, usize, &K, std::result::Result<T, String>),
) where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let total = jobs.len();
    let mut set = tokio::task::JoinSet::new();
    let mut keys = std::collections::HashMap::new();
    for (key, job) in jobs {
        keys.insert(set.spawn(job).id(), key);
    }
    let mut done = 0;
    while let Some(joined) = set.join_next_with_id().await {
        done += 1;
        let (id, result) = match joined {
            Ok((id, out)) => (id, Ok(out)),
            Err(e) => (e.id(), Err(format!("task crashed: {e}"))),
        };
        // The set only yields tasks spawned above, each recorded with its key.
        let key = keys.remove(&id).expect("every spawned task has a key");
        on_done(done, total, &key, result);
    }
}

/// [`each_pod`] for the common case: one command per pod over `remote`, each bounded by
/// `timeout`, reported as a [`PodCall`].
pub(crate) async fn exec_each_pod<K>(
    remote: &Arc<dyn Remote>,
    jobs: Vec<(K, SshTarget, String)>,
    timeout: Duration,
    mut on_done: impl FnMut(usize, usize, &K, PodCall),
) {
    let jobs = jobs
        .into_iter()
        .map(|(name, target, cmd)| {
            let remote = remote.clone();
            (name, async move { remote.exec(&target, &cmd, Some(timeout)).await.map_err(|e| describe_error(&e)) })
        })
        .collect();
    each_pod(jobs, |done, total, name, result| on_done(done, total, name, result.and_then(|call| call))).await;
}

/// The `[done/total] ✗ <name> …` line for a pod whose call failed: the remote command's
/// non-zero exit (with its stderr), or why there was no answer (`timed out after Ns`, …).
pub(crate) fn failure_line(done: usize, total: usize, name: &str, call: &PodCall) -> String {
    match call {
        Ok(out) => format!("[{done}/{total}] ✗ {name} ({}): {}", out.exit_label(), out.stderr.trim()),
        Err(why) => format!("[{done}/{total}] ✗ {name}: {why}"),
    }
}
