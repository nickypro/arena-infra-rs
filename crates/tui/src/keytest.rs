//! The dashboard's `test` (`e` in a pod's menu): beside the torch version it now says whether
//! the pod's AI keys work — `pods test --keys`'s check, one short exec run alongside the torch
//! one (`arena_core::health::key_check_command`, judged by `judge_key_call` against the fleet's
//! keys CSV and `HF_TOKEN`). A pod whose key FAILs gets a ✗ line, so the result's `n/N ok`
//! counts it; a fleet with no keys configured gets the torch line alone, as before.

use std::path::PathBuf;

use arena_core::health::{is_key_check, judge_key_call, key_check_command, HealthPolicy, KeyBook, PodHealth, Status, AI_KEY, KEY_CHECK_TIMEOUT};
use arena_core::remote::{describe_error, Remote};
use arena_core::ssh::SshTarget;
use arena_core::{Config, Pod};

/// The fleet's spend ledger, for the owners a key check holds machines to — none under test
/// unless the test's config sets `ARENA_STATE_DIR` (a test never reads the developer's own).
fn ledger(cfg: &Config) -> Option<PathBuf> {
    if cfg!(test) && cfg.get("ARENA_STATE_DIR").is_none() {
        return None;
    }
    arena_core::snapshot::state_file_path_for(cfg, arena_core::spend::SPEND_FILE).ok()
}

/// `pod`'s AI keys, checked over `remote` within [`KEY_CHECK_TIMEOUT`] — `None` when the fleet
/// holds no keys to compare (nothing configured) or the pod has no endpoint (the torch line
/// says so).
pub(crate) async fn check(remote: &dyn Remote, cfg: &Config, pod: &Pod) -> Option<PodHealth> {
    let target = SshTarget::from_pod(pod, cfg).ok()?;
    let book = KeyBook::load(cfg, ledger(cfg).as_deref());
    if !book.any() {
        return None;
    }
    let cmd = key_check_command(Some(cfg.get("CONDA_ENV").unwrap_or("arena-env")));
    let call = remote.exec(&target, &cmd, Some(KEY_CHECK_TIMEOUT)).await.map_err(|e| describe_error(&e));
    Some(judge_key_call(pod, call, &HealthPolicy { keys: Some(book), ..HealthPolicy::default() }))
}

/// The test's line for one pod: the torch line, then each key verdict (`· AI key ✓ ok, $4.97 of
/// $5.00 left`), or why they weren't checked — its mark turned ✗ when a key FAILs. Pure.
pub(crate) fn with_keys(torch: String, keys: Option<&PodHealth>) -> String {
    let Some(k) = keys else { return torch };
    let judged: Vec<String> = k
        .checks
        .iter()
        .filter(|c| is_key_check(&c.name) && c.status != Status::Skip)
        .map(|c| {
            let what = if c.name == AI_KEY { "AI key" } else { "HF token" };
            let mark = match c.status {
                Status::Pass => "✓",
                Status::Warn => "!",
                _ => "✗",
            };
            format!("{what} {mark} {}", c.detail)
        })
        .collect();
    let reported = k.facts.as_ref().is_some_and(|f| f.started && (f.openrouter.is_some() || f.hf.is_some()));
    let tail = match (reported, judged.is_empty()) {
        (false, _) => format!("AI key not checked ({})", k.notes()),
        (true, true) => return torch,
        (true, false) => judged.join(" · "),
    };
    let failed = k.checks.iter().any(|c| is_key_check(&c.name) && c.status == Status::Fail);
    let line = format!("{torch} · {tail}");
    match line.strip_prefix('✓') {
        Some(rest) if failed => format!("✗{rest}"),
        _ => line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena_core::health::Check;

    fn health(checks: Vec<Check>, reported: bool) -> PodHealth {
        let facts = arena_core::health::parse_deep(if reported {
            "deep_check=2\nopenrouter.key=set\nopenrouter.http=200\ndeep_check_end=1\n"
        } else {
            ""
        });
        PodHealth {
            id: "id1".into(),
            name: "devtest-apple".into(),
            provider: "runpod".into(),
            status: arena_core::health::overall(&checks),
            checks,
            facts: Some(facts),
            host: None,
        }
    }

    /// The torch line keeps its own words; the keys follow it, a FAIL turns its mark, and a
    /// check that couldn't run says so — a fleet without keys (`None`) changes nothing.
    #[test]
    fn the_test_line_carries_the_key_verdicts() {
        let torch = "✓ devtest-apple torch: 2.9.0+cu130".to_string();
        assert_eq!(with_keys(torch.clone(), None), torch);
        let ok = health(
            vec![
                Check { name: "ai_key".into(), status: Status::Pass, detail: "ok, $4.97 of $5.00 left".into() },
                Check { name: "hf_token".into(), status: Status::Skip, detail: "no HF_TOKEN configured for this fleet (none here)".into() },
            ],
            true,
        );
        assert_eq!(with_keys(torch.clone(), Some(&ok)), "✓ devtest-apple torch: 2.9.0+cu130 · AI key ✓ ok, $4.97 of $5.00 left");
        let dead = health(
            vec![
                Check {
                    name: "ai_key".into(),
                    status: Status::Fail,
                    detail: "rejected by OpenRouter (HTTP 401) — `arena keys rotate apple --copy`".into(),
                },
                Check { name: "hf_token".into(), status: Status::Pass, detail: "accepted by Hugging Face".into() },
            ],
            true,
        );
        assert_eq!(
            with_keys(torch.clone(), Some(&dead)),
            "✗ devtest-apple torch: 2.9.0+cu130 · AI key ✗ rejected by OpenRouter (HTTP 401) — `arena keys rotate apple --copy` · \
             HF token ✓ accepted by Hugging Face"
        );
        let unreached = PodHealth::unreachable(&Pod { name: "devtest-apple".into(), ..Default::default() }, "timed out after 40s");
        assert_eq!(with_keys(torch, Some(&unreached)), "✓ devtest-apple torch: 2.9.0+cu130 · AI key not checked (ssh: timed out after 40s)");
    }
}
