//! Golden tests of `pods setup`'s output (docs/OUTPUT.md), at default and at `-v`, through the
//! command (`handle_pods`, its prompt answered `y`) against a fake fleet and a scripted
//! `FakeRemote` on a paused clock — so each time in them is exact. What they show is what a log
//! or a pipe gets: a terminal draws the in-place line instead of the `[NAME] setup…` ones. `-v`
//! is what the command printed before.

use std::sync::Arc;
use std::time::Duration;

use arena_core::remote::{FakeRemote, FakeReply};
use arena_core::Provider;

use super::setup_tests::{fleet, pods, pubkey_dir, setup_cfg, KeyApi};
use crate::output_test::rendered;
use crate::pods::handle_pods;

/// `pods <args>` on a fresh fake (`remote()`) at default and at `-v`, the prompt answered `y`:
/// what each showed. The config's state dir is a temp dir of its own (no Claude Code pool, no
/// archive, no reap policy: nothing of `copy-keys`', `claude`'s or `reap`'s to say).
async fn both(tag: &str, args: &[&str], provider: &dyn Provider, extra: &str, remote: impl Fn() -> FakeRemote) -> (String, String) {
    let mut out = Vec::new();
    for verbose in [false, true] {
        let state = std::env::temp_dir().join(format!("arena-setup-golden-{tag}-{verbose}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state);
        let cfg = setup_cfg(&format!("MACHINE_NAME_LIST=(apple bloom cloud)\nARENA_STATE_DIR={}\n{extra}", state.display()));
        let run = handle_pods(pods(args), provider, Arc::new(remote()), &cfg, false);
        out.push(rendered(verbose, &["y"], run).await);
        let _ = std::fs::remove_dir_all(&state);
    }
    let loud = out.pop().unwrap();
    (out.pop().unwrap(), loud)
}

/// Two pods, one slower: a line as each starts (a log's) and ends, and the count.
#[tokio::test(start_paused = true)]
async fn setup_that_goes_well() {
    let remote = || {
        let fake = FakeRemote::new();
        fake.script("10.0.0.1:22002", [FakeReply::ok(), FakeReply::ok().after(Duration::from_secs(130))]);
        fake
    };
    let (quiet, loud) = both("well", &["setup", "apple", "bloom"], &fleet(), "", remote).await;
    assert_eq!(quiet, QUIET_WELL.strip_prefix('\n').unwrap(), "default:\n{quiet}");
    assert_eq!(loud, LOUD_WELL.strip_prefix('\n').unwrap(), "-v:\n{loud}");
}

/// One of three fails: its line says why and what to run; the error names it.
#[tokio::test(start_paused = true)]
async fn setup_with_a_failure() {
    let remote = || {
        let fake = FakeRemote::new();
        fake.script("10.0.0.1:22002", [FakeReply::ok(), FakeReply::exit(128, "fatal: could not read from remote repository.")]);
        fake
    };
    let (quiet, loud) = both("fail", &["setup", "apple", "bloom", "cloud"], &fleet(), "", remote).await;
    assert_eq!(quiet, QUIET_FAIL.strip_prefix('\n').unwrap(), "default:\n{quiet}");
    assert_eq!(loud, LOUD_FAIL.strip_prefix('\n').unwrap(), "-v:\n{loud}");
}

/// A pod refusing our key, re-attached through its provider: no FAILED line for it (the
/// dashboard would keep it against the ✓), the repair in one line, then its ✓.
#[tokio::test(start_paused = true)]
async fn setup_after_a_key_repair() {
    let denied = "root@10.0.0.1: Permission denied (publickey).\nscp: Connection closed\r\n";
    let remote = || {
        let fake = FakeRemote::new();
        fake.script("10.0.0.1:22002", [FakeReply::exit(255, denied)]);
        fake
    };
    let dir = pubkey_dir("golden");
    let key = format!("SHARED_SSH_KEY_PATH={}/devtest_key", dir.display());
    let (quiet, loud) = both("repair", &["setup", "apple", "bloom"], &KeyApi::default(), &key, remote).await;
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(quiet, QUIET_REPAIR.strip_prefix('\n').unwrap(), "default:\n{quiet}");
    assert_eq!(loud, LOUD_REPAIR.strip_prefix('\n').unwrap(), "-v:\n{loud}");
}

/// The dry run: a line of steps by default; each step's exact command with `-v` (as before: its
/// shape here, not the scripts, which are setup's own to change).
#[tokio::test(start_paused = true)]
async fn setup_dry_run() {
    let (quiet, loud) = both("dry", &["setup", "apple", "bloom", "--dry-run"], &fleet(), "", FakeRemote::new).await;
    assert_eq!(quiet, QUIET_DRY.strip_prefix('\n').unwrap(), "default:\n{quiet}");
    let head = "Broadcast tokens: Hugging Face ✗ (✓ exported on each pod; ✗ skipped)\n\
                Claude Code: no accounts (CLAUDE_TOKENS_FILE / CLAUDE_CODE_OAUTH_TOKEN unset) — skipped\n\
                Dry-run — would provision 2 pod(s):\n\n# devtest-apple\n  ## copy deploy key (timeout 60s)\n  scp -P 22001 ";
    assert!(loud.starts_with(head), "-v:\n{loud}");
    assert!(loud.contains("\n  ## repo + keys config (timeout 300s)\n  ssh -p 22001 "), "-v:\n{loud}");
    assert!(loud.contains("\n\n# devtest-bloom\n  ## copy deploy key (timeout 60s)\n  scp -P 22002 "), "-v:\n{loud}");
    assert!(loud.ends_with("\n\nAPI keys: none in ./keys — would skip\nPreview only — run without --dry-run to execute over SSH.\n"), "-v:\n{loud}");
}

/// `--notes-only`, run and dry run.
#[tokio::test(start_paused = true)]
async fn notes_only() {
    let notes = "";
    let (quiet, loud) = both("notes", &["setup", "--notes-only", "apple", "bloom"], &fleet(), notes, FakeRemote::new).await;
    assert_eq!(quiet, QUIET_NOTES.strip_prefix('\n').unwrap(), "default:\n{quiet}");
    assert_eq!(loud, LOUD_NOTES.strip_prefix('\n').unwrap(), "-v:\n{loud}");
    let (quiet, loud) = both("notes-dry", &["setup", "--notes-only", "apple", "--dry-run"], &fleet(), notes, FakeRemote::new).await;
    assert_eq!(quiet, QUIET_NOTES_DRY.strip_prefix('\n').unwrap(), "default:\n{quiet}");
    assert_eq!(loud, LOUD_NOTES_DRY.strip_prefix('\n').unwrap(), "-v:\n{loud}");
}

/// The in-place line's words, and the default line of a pod's end.
#[test]
fn the_progress_and_end_lines() {
    use super::{ended_line, failed_line, progress_text, SETUP};
    use arena_core::setup::ProvisionOutcome;
    let cap = arena_core::out::capture(false);
    let s = Duration::from_secs;
    assert_eq!(progress_text(SETUP, s(70), 0, &["devtest-apple".into()]), "setup 1m10s · devtest-apple");
    assert_eq!(progress_text(SETUP, s(70), 1, &["devtest-bloom".into(), "devtest-cloud".into()]), "setup 1m10s · 1 of 3 done · devtest-bloom, devtest-cloud");
    assert_eq!(ended_line(SETUP, "devtest-apple", &ProvisionOutcome::done(), s(182)), "[devtest-apple] setup ✓ (3m02s)");
    let timed_out = ProvisionOutcome::TimedOut { step: "repo + keys config", after: s(300) };
    assert_eq!(
        ended_line(SETUP, "devtest-apple", &timed_out, s(300)),
        "[devtest-apple] FAILED setup: timed out at repo + keys config after 300s (retry: arena pods setup devtest-apple)"
    );
    assert_eq!(failed_line(super::NOTES, "a", "x"), "[a] FAILED notes: x (retry: arena pods setup --notes-only a)");
    drop(cap);
}

const QUIET_WELL: &str = r#"
Set up devtest-apple, devtest-bloom over SSH? [y/N] y
[devtest-apple] setup…
[devtest-bloom] setup…
[devtest-apple] setup ✓ (0s)
[devtest-bloom] setup ✓ (2m10s)
✓ 2 of 2 set up
"#;

const LOUD_WELL: &str = r#"
Will provision 2 pod(s) over SSH (deploy key, ~/.name, repo): devtest-apple, devtest-bloom
Proceed? [y/N] y
Broadcast tokens: Hugging Face ✗ (✓ exported on each pod; ✗ skipped)
Claude Code: no accounts (CLAUDE_TOKENS_FILE / CLAUDE_CODE_OAUTH_TOKEN unset) — skipped
Provisioning 2 pod(s) over SSH (budgets: copy 60s, config 300s, hetzner script 1800s)…
[1/2] ✓ devtest-apple
[2/2] ✓ devtest-bloom

Done: 2 provisioned, 0 failed.
API keys: none generated in ./keys/ — skipping (run `arena keys gen --all` or drop in <provider>_api_keys.csv, then re-run setup / `pods copy-keys`).
"#;

const QUIET_FAIL: &str = r#"
Set up devtest-apple, devtest-bloom, devtest-cloud over SSH? [y/N] y
[devtest-apple] setup…
[devtest-bloom] setup…
[devtest-cloud] setup…
[devtest-apple] setup ✓ (0s)
[devtest-bloom] FAILED setup: failed at repo + keys config, exit 128: fatal: could not read from remote repository. (retry: arena pods setup devtest-bloom)
[devtest-cloud] setup ✓ (0s)
Error: 1 pod(s) failed to set up: devtest-bloom
"#;

const LOUD_FAIL: &str = r#"
Will provision 3 pod(s) over SSH (deploy key, ~/.name, repo): devtest-apple, devtest-bloom, devtest-cloud
Proceed? [y/N] y
Broadcast tokens: Hugging Face ✗ (✓ exported on each pod; ✗ skipped)
Claude Code: no accounts (CLAUDE_TOKENS_FILE / CLAUDE_CODE_OAUTH_TOKEN unset) — skipped
Provisioning 3 pod(s) over SSH (budgets: copy 60s, config 300s, hetzner script 1800s)…
[1/3] ✓ devtest-apple
[2/3] ✗ devtest-bloom (failed at repo + keys config, exit 128): fatal: could not read from remote repository.
[3/3] ✓ devtest-cloud

Done: 2 provisioned, 1 failed.
API keys: none generated in ./keys/ — skipping (run `arena keys gen --all` or drop in <provider>_api_keys.csv, then re-run setup / `pods copy-keys`).
Error: 1 pod(s) failed to set up: devtest-bloom
"#;

const QUIET_REPAIR: &str = r#"
Set up devtest-apple, devtest-bloom over SSH? [y/N] y
[devtest-apple] setup…
[devtest-bloom] setup…
[devtest-apple] setup ✓ (0s)
[devtest-bloom] setup: it refused our SSH key — re-attached it, trying again
[devtest-bloom] setup ✓ (20s)
✓ 2 of 2 set up
"#;

const LOUD_REPAIR: &str = r#"
Will provision 2 pod(s) over SSH (deploy key, ~/.name, repo): devtest-apple, devtest-bloom
Proceed? [y/N] y
Broadcast tokens: Hugging Face ✗ (✓ exported on each pod; ✗ skipped)
Claude Code: no accounts (CLAUDE_TOKENS_FILE / CLAUDE_CODE_OAUTH_TOKEN unset) — skipped
Provisioning 2 pod(s) over SSH (budgets: copy 60s, config 300s, hetzner script 1800s)…
[1/2] ✓ devtest-apple
[2/2] ✗ devtest-bloom (failed at copy deploy key, exit 255): root@10.0.0.1: Permission denied (publickey).
scp: Connection closed
  devtest-bloom refused our SSH key — re-attached 1 key(s) through the runpod API
Retrying setup on 1 pod(s) whose keys were re-attached (up to 3×, 20s apart)…
[1/1] ✓ devtest-bloom

Done: 2 provisioned, 0 failed.
API keys: none generated in ./keys/ — skipping (run `arena keys gen --all` or drop in <provider>_api_keys.csv, then re-run setup / `pods copy-keys`).
"#;

const QUIET_DRY: &str = r#"
Would set up devtest-apple, devtest-bloom: copy deploy key → repo + keys config
"#;

const QUIET_NOTES: &str = r#"
Write the agents' notes on devtest-apple, devtest-bloom, nothing else? [y/N] y
[devtest-apple] notes…
[devtest-bloom] notes…
[devtest-apple] notes ✓ (0s)
[devtest-bloom] notes ✓ (0s)
✓ 2 of 2 up to date
"#;

const LOUD_NOTES: &str = r#"
Will write the agents' notes (~/.claude/CLAUDE.md, ~/.codex/AGENTS.md) and link uv into /usr/local/bin on 2 pod(s), nothing else: devtest-apple, devtest-bloom
Proceed? [y/N] y
Writing the agents' notes on 2 pod(s) over SSH (budget 60s each)…
[1/2] ✓ devtest-apple
[2/2] ✓ devtest-bloom

Done: 2 up to date, 0 failed.
"#;

const QUIET_NOTES_DRY: &str = r#"
Would write the agents' notes (~/.claude/CLAUDE.md, ~/.codex/AGENTS.md) and link uv on devtest-apple
"#;

const LOUD_NOTES_DRY: &str = r#"
Dry-run — would write the agents' notes on 1 pod(s):

# devtest-apple
  link uv/uvx into /usr/local/bin (where missing); the Python notes (~/.claude/CLAUDE.md, ~/.codex/AGENTS.md, the block between <!-- BEGIN arena-infra python --> and <!-- END arena-infra python -->) for the first env there of /opt/arena-env, /opt/conda/envs/arena-env, ~/miniconda3/envs/arena-env, ~/miniforge3/envs/arena-env, ~/anaconda3/envs/arena-env, /root/r/.venv, the repo /root/r

Preview only — run without --dry-run to execute over SSH.
"#;
