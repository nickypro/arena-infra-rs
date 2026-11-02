//! Golden tests of the save commands' output (docs/OUTPUT.md) — `pods backup`, `pull`,
//! `set-branch`, `init-branches`, `restore`, `baseline` — at default and at `-v`, from fakes: the
//! pods a `FakeRemote`, rsync a stand-in script. What they show is what a log or a pipe gets (the
//! cron's log among them): by default each failure with its fix and a line of tally, with `-v`
//! every line the command printed before. Pods answer a few milliseconds apart, so they finish in
//! the same order every run.

use super::*;
use crate::output_test::rendered;
use crate::pods::pull::handle_pull;

/// What a pod's backup says, `ms` milliseconds in.
fn after(ms: u64, reply: FakeReply) -> FakeReply {
    reply.after(Duration::from_millis(ms))
}

/// A stand-in rsync that takes its time by tier, so the copies end in the same order every run:
/// each snapshot pass `snap` seconds, the big tier `big`; one whose destination holds `fail` fails
/// after `big` seconds as rsync does when the connection drops (exit 12).
fn paced_rsync(tag: &str, snap: &str, big: &str, fail: &str) -> StubRsync {
    let stub = StubRsync::new(tag);
    arena_core::remote::write_test_script(
        stub.0.join("rsync"),
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\n\
             case \"$last\" in */big/*{fail}*) sleep {big}; \
             echo 'rsync: connection unexpectedly closed (0 bytes received so far) [Receiver]' >&2; \
             echo 'rsync error: error in rsync protocol data stream (code 12)' >&2; exit 12;; \
             */big/*) sleep {big};; *) sleep {snap};; esac\n"
        ),
    )
    .unwrap();
    stub
}

/// The run's text with its temp dir and today's snapshot label (from ARENA_START_DATE) made
/// stable.
fn stable(text: &str, stub: &StubRsync, cfg: &Config) -> String {
    let (w, d) = crate::cron::resolve_week_day(cfg, None, None).unwrap();
    text.replace(&stub.0.display().to_string(), "<tmp>").replace(&format!("w{w}d{d}"), "wNdM")
}

/// `pods backup --no-pull --yes`, the */15 cron's run: apple pushed, bloom refused by GitHub's
/// push protection (copied by rsync instead), cloud unchanged, delta on main. By default: the
/// failure with its fix, the tally, the main reminder, the copy's line; with `-v`, every pod.
#[tokio::test]
async fn backup_cron_run_says_failures_and_a_tally_by_default() {
    let mut out = Vec::new();
    for verbose in [false, true] {
        let stub = paced_rsync(&format!("golden-backup-{verbose}"), "0.05", "0.4", "no-such-pod");
        let cfg = cfg_at(&stub);
        let four = fleet_of(&[("apple", 22001), ("bloom", 22002), ("cloud", 22003), ("delta", 22004)]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [after(10, FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n"))]);
        fake.script(&host(22002), [after(20, push_blocked("autocommit-b")), repo_in_place()]);
        fake.script(&host(22003), [after(30, FakeReply::stdout("BRANCH autocommit-c\nNO_CHANGES autocommit-c\n"))]);
        fake.script(&host(22004), [after(40, FakeReply::stdout("SKIP main\n"))]);
        let (sel, rsync) = (everyone(&four), stub.path());
        let run = handle_full_backup(fake.clone(), &cfg, &sel, true, None, false, true, &rsync);
        out.push(stable(&rendered(verbose, &[], run).await, &stub, &cfg));
    }
    let blocked = "GitHub push protection refuses autocommit-b: OpenRouter API Key at ex.py:3 — a secret the backup doesn't \
                   redact. Its pattern belongs in arena_core::backup::SECRET_PATTERNS; until then, take it out of the unpushed \
                   commits on devtest-bloom by hand (the file on disk may keep it)";
    let fallback = "✗ devtest-bloom: git push failed (GitHub push protection: OpenRouter API Key at ex.py:3) → files copied to \
                    <tmp>/backups/wNdM/devtest-bloom (and <tmp>/backups/big/devtest-bloom)";
    assert_eq!(
        out[0],
        format!(
            "✗ devtest-bloom: {blocked}\n\
             ✗ git backup: 1 of 4 failed · 1 pushed, 1 no changes, 1 on main/master\n\
             ⚠ 1 cohort pod on main: not backed up until `arena pods init-branches devtest-delta`\n\
             ✓ home copy of 1 pod → <tmp>/backups/wNdM\n\
             {fallback}\n\
             Error: 1 pod(s) failed to back up\n"
        )
    );
    assert_eq!(
        out[1],
        format!(
            "Backing up 4 pod(s) over SSH (current branch; main/master skipped; 300s budget each)…\n\
             [1/4] ✓ devtest-apple -> autocommit-a\n\
             [2/4] ✗ devtest-bloom: {blocked}\n\
             [3/4] = devtest-cloud (no changes, on autocommit-c)\n\
             \n\
             Done: 1 pushed, 1 unchanged, 1 skipped (main/master), 1 failed.\n\
             ⚠ 1 cohort pod on main: not backed up until `arena pods init-branches devtest-delta`\n\
             \n\
             Copying the 1 pod(s) git didn't back up into <tmp>/backups instead (rsync, both tiers): devtest-bloom\n\
             Source ~/ (home) · snapshot < 50M -> <tmp>/backups/wNdM/<pod>/ · all files -> <tmp>/backups/big/<pod>/ · incl .git · \
             each pod ≤ 7200s (BACKUP_TIMEOUT_SECS), ≤ 300s without I/O\n\
             \n\
             Pulling 1 pod(s) into <tmp>/backups/ (dated snapshot wNdM + the all-files big/)…\n\
             [1/2] ✓ devtest-bloom [snapshot] (done)\n\
             [2/2] ✓ devtest-bloom [big] (done)\n\
             \n\
             Done: 2 rsync job(s) ok, 0 failed across 1 pod(s).\n\
             \n\
             {fallback}\n\
             Error: 1 pod(s) failed to back up\n"
        )
    );
}

/// `pods backup` that goes well, by default: one line.
#[tokio::test]
async fn backup_that_goes_well_is_one_line() {
    let stub = StubRsync::new("golden-backup-ok");
    let cfg = cfg_at(&stub);
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [after(10, FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n"))]);
    fake.script(&host(22002), [after(20, FakeReply::stdout("BRANCH autocommit-b\nNO_CHANGES autocommit-b\n"))]);
    fake.script(&host(22003), [after(30, FakeReply::stdout("BRANCH autocommit-c\nNO_CHANGES autocommit-c\n"))]);
    let (sel, rsync) = (everyone(&fleet()), stub.path());
    let run = handle_full_backup(fake.clone(), &cfg, &sel, true, None, false, true, &rsync);
    assert_eq!(rendered(false, &[], run).await, "✓ git backup of 3 pods: 1 pushed, 2 no changes\n");
}

/// `pods backup --dry-run`: by default what would run, in a line; with `-v` each pod's ssh line
/// and the whole script, as before.
#[tokio::test]
async fn backup_dry_run_is_a_line_and_the_script_is_for_v() {
    let stub = StubRsync::new("golden-backup-dry");
    let cfg = cfg_at(&stub);
    let one = fleet_of(&[("apple", 22001)]);
    let mut out = Vec::new();
    for verbose in [false, true] {
        let fake = Arc::new(FakeRemote::new());
        let (sel, rsync) = (everyone(&one), stub.path());
        let run = handle_full_backup(fake.clone(), &cfg, &sel, true, None, true, false, &rsync);
        out.push(stable(&rendered(verbose, &[], run).await, &stub, &cfg));
        assert!(fake.calls().is_empty());
    }
    assert_eq!(out[0], "[dry-run] would commit + push the current branch of 1 pod(s): devtest-apple (main/master skipped; -v: the script)\n");
    let target = targets(&one).remove(0).1;
    let line = arena_core::backup::backup_dry_run_line(&target, REPO, Some(KEY), "arena backup devtest-apple", None);
    assert!(
        out[1].starts_with(&format!(
            "Dry-run — would commit + push the current branch on 1 pod(s) (skips main/master; files over 95 MB and nested repos \
             with their own history stay out of git), running the backup script below on each:\n\n# devtest-apple\n{line}\n\n\
             # The backup script, run under sh on each pod — paste this first: it writes ./arena-backup.sh, which the lines above \
             read\n{}\n\nPreview only — run without --dry-run to execute over SSH.\n",
            arena_core::backup::backup_script_heredoc()
        )),
        "{}",
        out[1]
    );
    assert!(out[1].ends_with(
        "(a pod whose git push fails at the push itself, or whose origin isn't the configured repo, is then copied into <tmp>/backups \
         by rsync — even under --no-pull)\n"
    ));
    assert!(out[1].lines().count() > 300, "the script is there with -v");
}

/// `pods pull`: by default a failed copy's line (its telling rsync line and its fix) and the
/// tally; with `-v` each rsync job, the sizes and the totals, as before.
#[tokio::test]
async fn pull_says_its_failures_and_a_tally_by_default() {
    let mut out = Vec::new();
    for verbose in [false, true] {
        let stub = paced_rsync(&format!("golden-pull-{verbose}"), "0.05", "0.4", "devtest-bloom");
        let cfg = cfg_at(&stub);
        let two = fleet_of(&[("apple", 22001), ("bloom", 22002)]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [repo_in_place()]);
        fake.script(&host(22002), [repo_in_place()]);
        let remote: Arc<dyn Remote> = fake.clone();
        let dir = local_backup_dir(&cfg);
        let (sel, rsync) = (everyone(&two), stub.path());
        let run = handle_pull(&remote, &cfg, None, &dir, None, None, false, false, &sel, false, true, &rsync);
        out.push(stable(&rendered(verbose, &[], run).await, &stub, &cfg));
    }
    assert_eq!(
        out[0],
        "✗ devtest-bloom [big]: rsync exit 12: rsync error: error in rsync protocol data stream (code 12) (retry: arena pods pull \
         devtest-bloom)\n\
         ✗ home copy: 1 of 2 failed · 1 copied → <tmp>/backups/wNdM\n\
         Error: 1 rsync job(s) failed: devtest-bloom [big]\n"
    );
    assert_eq!(
        out[1],
        "Source ~/ (home) · snapshot < 50M -> <tmp>/backups/wNdM/<pod>/ · all files -> <tmp>/backups/big/<pod>/ · incl .git · \
         each pod ≤ 7200s (BACKUP_TIMEOUT_SECS), ≤ 300s without I/O\n\
         \n\
         Pulling 2 pod(s) into <tmp>/backups/ (dated snapshot wNdM + the all-files big/)…\n\
         [1/4] ✓ devtest-apple [snapshot] (done)\n\
         [2/4] ✓ devtest-bloom [snapshot] (done)\n\
         [3/4] ✓ devtest-apple [big] (done)\n\
         [4/4] ✗ devtest-bloom [big] (rsync exit 12): rsync: connection unexpectedly closed (0 bytes received so far) [Receiver]\n\
         rsync error: error in rsync protocol data stream (code 12)\n\
         \n\
         Done: 3 rsync job(s) ok, 1 failed across 2 pod(s).\n\
         Error: 1 rsync job(s) failed: devtest-bloom [big]\n"
    );
}

/// `pods pull --dry-run`: by default where the copies would go, in a line; each rsync with `-v`.
#[tokio::test]
async fn pull_dry_run_is_a_line() {
    let stub = StubRsync::new("golden-pull-dry");
    let cfg = cfg_at(&stub);
    let one = fleet_of(&[("apple", 22001)]);
    let fake = Arc::new(FakeRemote::new());
    let remote: Arc<dyn Remote> = fake.clone();
    let dir = local_backup_dir(&cfg);
    let mut out = Vec::new();
    for verbose in [false, true] {
        let (sel, rsync) = (everyone(&one), stub.path());
        let run = handle_pull(&remote, &cfg, None, &dir, None, None, false, false, &sel, true, false, &rsync);
        out.push(stable(&rendered(verbose, &[], run).await, &stub, &cfg));
    }
    assert_eq!(
        out[0],
        "[dry-run] would copy 1 pod(s) into <tmp>/backups/wNdM/<pod> (files < 50M) + <tmp>/backups/big/<pod> (all files): \
         devtest-apple (-v: each rsync)\n"
    );
    assert!(out[1].contains("Dry-run — would rsync 1 pod(s), 2 tier(s) each:\n\n# devtest-apple\n  [snapshot] rsync -avz"), "{}", out[1]);
    assert!(out[1].ends_with("Preview only — run without --dry-run to copy.\n"), "{}", out[1]);
    assert_eq!(out[1].lines().filter(|l| l.starts_with("  [")).count(), 3, "{}", out[1]);
}

/// `pods set-branch main --hard`: apple pushed and reset; bloom refused by GitHub, so copied
/// first, then reset — its line, by default too, says so. By default: the question in a line,
/// bloom's lines and the tally; with `-v` the plan, every pod's backup and switch, as before.
#[tokio::test]
async fn set_branch_says_what_needs_saying_and_a_tally() {
    let mut out = Vec::new();
    for verbose in [false, true] {
        let stub = paced_rsync(&format!("golden-setbranch-{verbose}"), "0.05", "0.4", "no-such-pod");
        let cfg = cfg_at(&stub);
        let two = fleet_of(&[("apple", 22001), ("bloom", 22002)]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [after(10, FakeReply::stdout("BRANCH autocommit-a\nPUSHED autocommit-a\n")), after(10, FakeReply::ok())]);
        fake.script(&host(22002), [after(20, push_blocked("autocommit-b")), repo_in_place(), after(20, FakeReply::ok())]);
        let (sel, rsync) = (everyone(&two), stub.path());
        let run = handle_set_branch(fake.clone(), &cfg, "main", &sel, true, false, false, &rsync);
        out.push(stable(&rendered(verbose, &["y"], run).await, &stub, &cfg));
    }
    let blocked = "GitHub push protection refuses autocommit-b: OpenRouter API Key at ex.py:3 — a secret the backup doesn't \
                   redact. Its pattern belongs in arena_core::backup::SECRET_PATTERNS; until then, take it out of the unpushed \
                   commits on devtest-bloom by hand (the file on disk may keep it)";
    let switched = "✓ devtest-bloom (on main) — ⚠ NOT PUSHED FIRST: branch autocommit-b (GitHub push protection: OpenRouter API \
                    Key at ex.py:3); its files were copied to <tmp>/backups/wNdM/devtest-bloom (and <tmp>/backups/big/devtest-bloom) \
                    first, and its commits stay on the pod in that branch (switching deletes no branch)";
    let error = "Error: 1 pod(s) switched with their branch copied, not pushed (see each one's line): devtest-bloom\n";
    let reminder = "⚠ 2 cohort pods on main: not backed up until `arena pods init-branches`";
    assert_eq!(
        out[0],
        format!(
            "HARD-reset devtest-apple, devtest-bloom to origin/main (each one's branch pushed first; local changes discarded, \
             saved on the pod)? [y/N] y\n\
             ✗ devtest-bloom: {blocked}\n\
             ✓ home copy of 1 pod → <tmp>/backups/wNdM\n\
             [1/1] {switched}\n\
             ✓ switched to main: 2 of 2 pods\n\
             {reminder}\n\
             {error}"
        )
    );
    assert_eq!(
        out[1],
        format!(
            "Will HARD-reset 2 pod(s) to 'origin/main' — DISCARDS local commits/changes (each pod's current branch committed + \
             pushed first): devtest-apple, devtest-bloom\n\
             Proceed? [y/N] y\n\
             Backing up the branch each pod is on before the switch (commit + push; main/master skipped; 300s budget each)…\n\
             [1/2] ✓ devtest-apple -> autocommit-a\n\
             [2/2] ✗ devtest-bloom: {blocked}\n\
             \n\
             Switching 1 pod(s) to 'main' over SSH (120s budget each)…\n\
             [1/1] ✓ devtest-apple (on main)\n\
             \n\
             Copying the 1 pod(s) git didn't back up into <tmp>/backups instead (rsync, both tiers): devtest-bloom\n\
             Source ~/ (home) · snapshot < 50M -> <tmp>/backups/wNdM/<pod>/ · all files -> <tmp>/backups/big/<pod>/ · incl .git · \
             each pod ≤ 7200s (BACKUP_TIMEOUT_SECS), ≤ 300s without I/O\n\
             \n\
             Pulling 1 pod(s) into <tmp>/backups/ (dated snapshot wNdM + the all-files big/)…\n\
             [1/2] ✓ devtest-bloom [snapshot] (done)\n\
             [2/2] ✓ devtest-bloom [big] (done)\n\
             \n\
             Done: 2 rsync job(s) ok, 0 failed across 1 pod(s).\n\
             \n\
             Switching 1 pod(s) to 'main' over SSH (120s budget each)…\n\
             [1/1] {switched}\n\
             \n\
             switched 2/2\n\
             {reminder}\n\
             {error}"
        )
    );
}

/// The dry runs of `set-branch` and `init-branches`: a line each by default (the per-pod
/// commands — scripts of a hundred lines — and the note are `-v`'s, as before).
#[tokio::test]
async fn branch_dry_runs_are_a_line() {
    let stub = StubRsync::new("golden-branch-dry");
    let cfg = cfg_at(&stub);
    let one = fleet_of(&[("apple", 22001)]);
    let fake = Arc::new(FakeRemote::new());
    let mut out = Vec::new();
    for verbose in [false, true] {
        let (sel, rsync) = (everyone(&one), stub.path());
        let set = handle_set_branch(fake.clone(), &cfg, "main", &sel, false, true, false, &rsync);
        out.push(stable(&rendered(verbose, &[], set).await, &stub, &cfg));
        let init = handle_init_branches(fake.clone(), &cfg, Some(1), Some(2), &sel, true, false, &rsync);
        out.push(stable(&rendered(verbose, &[], init).await, &stub, &cfg));
    }
    assert!(fake.calls().is_empty());
    assert_eq!(out[0], "[dry-run] would switch devtest-apple to main (each one's branch pushed first; -v: the commands)\n");
    assert_eq!(
        out[1],
        "[dry-run] would create + push the w1d2 branch on devtest-apple (devtest-apple → autocommit-devtest-w1d2-apple) (each \
         one's branch pushed first; -v: the commands)\n"
    );
    let target = targets(&one).remove(0).1;
    let checkout = checkout_command(REPO, "main", Some(KEY), false);
    let note = stable(&push_first_note(&cfg, false), &stub, &cfg);
    assert_eq!(out[2], format!("{note}\n\n# devtest-apple\n{}\n\nDry-run only — nothing changed (preview).\n", target.display_command(&checkout)));
    let init = init_branch_command(&BackupConfig::from_config(&cfg, 1, 2), "devtest-apple");
    assert_eq!(
        out[3],
        format!(
            "Iteration: w1d2\n\n{note}\n\nDry-run — would init the w1d2 branch on 1 pod(s):\n\n# devtest-apple  ->  branch \
             autocommit-devtest-w1d2-apple\n{}\n\nPreview only — run without --dry-run to execute over SSH.\n",
            target.display_command(&init)
        )
    );
}

/// `pods init-branches`: by default the question and the tally; each pod's backup and new
/// branch with `-v`.
#[tokio::test]
async fn init_branches_is_its_question_and_a_tally() {
    let mut out = Vec::new();
    for verbose in [false, true] {
        let stub = StubRsync::new(&format!("golden-init-{verbose}"));
        let cfg = cfg_at(&stub);
        let two = fleet_of(&[("apple", 22001), ("bloom", 22002)]);
        let fake = Arc::new(FakeRemote::new());
        fake.script(&host(22001), [after(10, FakeReply::stdout("SKIP main\n")), after(10, FakeReply::ok())]);
        fake.script(&host(22002), [after(20, FakeReply::stdout("SKIP main\n")), after(20, FakeReply::ok())]);
        let (sel, rsync) = (everyone(&two), stub.path());
        let run = handle_init_branches(fake.clone(), &cfg, Some(1), Some(2), &sel, false, false, &rsync);
        out.push(stable(&rendered(verbose, &["y"], run).await, &stub, &cfg));
    }
    assert_eq!(
        out[0],
        "Create + push the w1d2 branch on devtest-apple, devtest-bloom (each one's branch pushed first)? [y/N] y\n\
         ✓ on the w1d2 branch: 2 of 2 pods\n"
    );
    assert_eq!(
        out[1],
        "Iteration: w1d2\n\n\
         Will create + push the w1d2 autocommit branch on 2 pod(s): devtest-apple, devtest-bloom\n\
         Proceed? [y/N] y\n\
         Backing up the branch each pod is on before the switch (commit + push; main/master skipped; 300s budget each)…\n\
         \n\
         Switching 2 pod(s) to their w1d2 branch over SSH (120s budget each)…\n\
         [1/2] ✓ devtest-apple -> autocommit-devtest-w1d2-apple\n\
         [2/2] ✓ devtest-bloom -> autocommit-devtest-w1d2-bloom\n\
         \n\
         Done: 2 initialized, 0 failed, 0 not switched (of 2).\n"
    );
}

/// `pods restore --dry-run`: by default the restore in a line; the plan and the exact rsync
/// with `-v`, as before.
#[tokio::test]
async fn restore_dry_run_is_a_line() {
    let stub = StubRsync::new("golden-restore-dry");
    let dir = backups(&stub.0);
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(22001), [repo_in_place(), repo_in_place()]);
    let remote: Arc<dyn Remote> = fake.clone();
    let mut out = Vec::new();
    for verbose in [false, true] {
        let opts = RestoreOpts { dry_run: true, ..restore_opts(&dir) };
        let (f, c, rsync) = (fleet(), cfg(), stub.path());
        let run = handle_restore(&f, &remote, &c, "apple", opts, &rsync, NOW);
        out.push(rendered(verbose, &[], run).await.replace(&stub.0.display().to_string(), "<tmp>"));
    }
    assert_eq!(
        out[0],
        "[dry-run] would restore devtest-apple from w1d2 (2 file(s), 11) into ~/ — nothing deleted; what it replaces is kept on \
         the pod in ~/.arena-restore/20261008T050640Z/ (-v: the plan and rsync)\n"
    );
    assert!(out[1].starts_with("[dry-run] would restore devtest-apple (id=id-devtest-apple, runpod):\n  from:  <tmp>/backups/w1d2/devtest-apple (w1d2, a snapshot: files under the size cap) — 2 file(s), 11\n"), "{}", out[1]);
    assert!(out[1].lines().last().is_some_and(|l| l.starts_with("  ") && l.contains("rsync") && l.contains("--backup-dir=")), "{}", out[1]);
}
