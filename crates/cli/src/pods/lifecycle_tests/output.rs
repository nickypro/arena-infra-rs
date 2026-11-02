//! Golden tests of what the change/move/stop commands print (docs/OUTPUT.md) — `stop`, `start`,
//! `restart`, `terminate`, `lock`/`unlock`, `rename`, `replace`, `migrate copy|cutover|finish|
//! revert` — at default and at `-v`, through the commands against the fake fleet and a scripted
//! `FakeRemote`. What they show is what a log or a pipe gets (a terminal draws the in-place
//! lines instead of the `[NAME] step…` ones). `-v` is what each command printed before.

use super::*;
use crate::output_test::rendered;

/// `pods <args>` at default and at `-v`, each against a fresh fleet from `fleet` (so a real
/// run's changes don't carry over), the prompts answered from `answers`: what each showed.
async fn both(args: &[&str], cfg: &Config, answers: &[&str], fleet: impl Fn() -> Fleet) -> (String, String) {
    let mut out = Vec::new();
    for verbose in [false, true] {
        let f = fleet();
        let run = handle_pods(pods_cmd(args), &f, Arc::new(FakeRemote::new()), cfg, false);
        out.push(rendered(verbose, answers, run).await);
    }
    let loud = out.pop().unwrap();
    (out.pop().unwrap(), loud)
}

/// `golden` without the newline after its opening `r#"`.
fn g(golden: &str) -> &str {
    golden.strip_prefix('\n').unwrap()
}

/// stop: by default each pod and what the stop does to its disk, in a line — before the prompt,
/// in the dry run — then a line per pod and, for several, the count. `-v`: each pod's whole plan
/// and `Proceed?`, as before.
#[tokio::test]
async fn stop_says_each_pods_disk_in_a_line() {
    let dir = tmpdir("out-stop");
    let cfg = cfg(&dir.0);
    let (quiet, loud) = both(&["stop", "alpha", "delta", "--dry-run"], &cfg, &[], fleet).await;
    assert_eq!(quiet, g(STOP_DRY_QUIET), "default:\n{quiet}");
    let alpha = DiskFate::Wiped { volume_gb: Some(0) }.describe(DiskOp::Stop, &RepoSite::configured("/root/r"));
    let delta = DiskFate::Kept.describe(DiskOp::Stop, &RepoSite::configured("/root/r"));
    let plan = format!("  devtest-alpha (id=r-alpha, runpod): {alpha}\n  devtest-delta (id=h-delta, hetzner): {delta}");
    let want = format!("[dry-run] would stop 2 pod(s):\n{plan}\n(would be refused without --wipe-ok: devtest-alpha)\n\nDry-run only — would stop 2 pod(s).\n");
    assert_eq!(loud, want, "-v:\n{loud}");

    let (quiet, loud) = both(&["stop", "alpha", "delta", "--wipe-ok"], &cfg, &["y"], fleet).await;
    assert_eq!(quiet, g(STOP_QUIET), "default:\n{quiet}");
    let want = format!("Will stop 2 running/starting pod(s):\n{plan}\nProceed? [y/N] y\n[stopped] devtest-alpha\n[stopped] devtest-delta\n\nstopped 2/2\n");
    assert_eq!(loud, want, "-v:\n{loud}");
}

const STOP_DRY_QUIET: &str = r#"
[dry-run] would stop devtest-alpha (discards its container disk and the work on it); devtest-delta (disk kept)
(would be refused without --wipe-ok: devtest-alpha)
"#;

const STOP_QUIET: &str = r#"
Stop devtest-alpha (discards its container disk and the work on it); devtest-delta (disk kept)? [y/N] y
[stopped] devtest-alpha
[stopped] devtest-delta
✓ stopped 2 of 2
"#;

/// start: a stopped GPU pod comes back as a fresh image, set up again, and ends in a READY line
/// (the dashboard's runs panel reads it as the pod's end — before, a `start` from there ended
/// "unfinished"); a VM that kept its disk is READY once started.
#[tokio::test(start_paused = true)]
async fn start_ends_each_pod_ready() {
    let dir = tmpdir("out-start");
    let cfg = cfg(&dir.0);
    let keys_dir = dir.0.display().to_string();
    let stopped = || {
        let f = fleet();
        for i in [0, 3] {
            f.pods.lock().unwrap()[i].status = "EXITED".into();
        }
        f
    };
    let mut out = Vec::new();
    for verbose in [false, true] {
        let f = stopped();
        let pods = f.pods.lock().unwrap().clone();
        let sel = named(&[pods[0].clone(), pods[3].clone()]);
        let run = handle_start(&f, Arc::new(FakeRemote::new()), &cfg, &sel, RestartOpts::default(), FAST, &keys_dir);
        out.push(rendered(verbose, &["y"], run).await);
    }
    assert_eq!(out[0], g(START_QUIET), "default:\n{}", out[0]);
    let then = afterwards_line(ContainerReset::Yes, true, false);
    let head = format!(
        "Will start 2 stopped pod(s):\n  devtest-alpha (id=r-alpha, runpod): comes back as a fresh image (its disk went at the stop); only a \
         /workspace volume kept its files\n  devtest-delta (id=h-delta, hetzner): disk kept\n  {then}\nProceed? [y/N] y\n\
         [started] devtest-alpha\n[started] devtest-delta\nwaiting for devtest-alpha's SSH endpoint to settle…\n\n{}\n",
        reprovision_line(1, "start", ContainerReset::Yes)
    );
    assert!(out[1].starts_with(&head), "-v:\n{}", out[1]);
    assert!(out[1].contains("[1/1] ✓ devtest-alpha\n\nDone: 1 provisioned, 0 failed.\n"), "-v:\n{}", out[1]);
    assert!(!out[1].contains("READY"), "-v says what it said before:\n{}", out[1]);

    let (quiet, loud) = both(&["start", "alpha", "--dry-run"], &cfg, &[], stopped).await;
    assert_eq!(quiet, "[dry-run] would start devtest-alpha (a fresh image, set up again)\n");
    let want = format!(
        "[dry-run] would start 1 pod(s):\n  devtest-alpha (id=r-alpha, runpod): comes back as a fresh image (its disk went at the stop); only a \
         /workspace volume kept its files\n  {then}\n"
    );
    assert_eq!(loud, want);
}

const START_QUIET: &str = r#"
Start devtest-alpha (a fresh image, set up again); devtest-delta (disk kept) — billing again? [y/N] y
[started] devtest-alpha
[started] devtest-delta
[devtest-alpha] waiting for SSH…
[devtest-alpha] setup…
[devtest-alpha] setup ✓ (0s)
[devtest-alpha] READY after 0s
[devtest-delta] READY after 0s · disk kept
"#;

/// restart: a line before the prompt (the pod, what goes, that it's set up again) and the
/// restarted pod's name; `-v` the whole plan and its id.
#[tokio::test(start_paused = true)]
async fn restart_says_the_pod_and_its_disk_in_a_line() {
    let dir = tmpdir("out-restart");
    let cfg = cfg(&dir.0);
    let (quiet, loud) = both(&["restart", "delta"], &cfg, &["y"], fleet).await;
    assert_eq!(quiet, "Restart devtest-delta (disk kept: a power-cycle)? [y/N] y\n[restarted] devtest-delta\n", "default:\n{quiet}");
    let kept = DiskFate::Kept.describe(DiskOp::Restart, &RepoSite::configured("/root/r"));
    let then = afterwards_line(ContainerReset::No, false, false);
    let synced = "[proxy] after restart: +0 ~0 -0 =0 (unchanged; 1 pod(s) not forwarded yet (no SSH endpoint))";
    let want = format!("Will restart devtest-delta (id=h-delta, hetzner):\n  {kept}\n  {then}\nProceed? [y/N] y\n[restarted] devtest-delta (id=h-delta, hetzner)\n{synced}\n");
    assert_eq!(loud, want, "-v:\n{loud}");

    let (quiet, loud) = both(&["restart", "alpha", "--dry-run"], &cfg, &[], fleet).await;
    assert_eq!(
        quiet,
        "[dry-run] would restart devtest-alpha (wipes its container disk and the work on it; set up again)\n(would be refused without --wipe-ok)\n",
        "default:\n{quiet}"
    );
    let wiped = DiskFate::Wiped { volume_gb: Some(0) }.describe(DiskOp::Restart, &RepoSite::configured("/root/r"));
    let then = afterwards_line(ContainerReset::Yes, true, false);
    let want = format!("[dry-run] would restart devtest-alpha (id=r-alpha, runpod):\n  {wiped}\n  {then}\n(would be refused without --wipe-ok)\n");
    assert_eq!(loud, want, "-v:\n{loud}");
}

/// terminate: one pod is a one-line `y/N`; several ask for the count, typed, after one line
/// naming every pod and what it costs; `-v` the plan as before (ids, providers, the sync).
#[tokio::test]
async fn terminate_says_every_pod_and_what_it_costs_in_a_line() {
    let dir = tmpdir("out-terminate");
    let cfg = cfg(&dir.0);
    let (quiet, loud) = both(&["terminate", "alpha", "--skip-proxy"], &cfg, &["y"], fleet).await;
    assert_eq!(quiet, g(TERMINATE_ONE_QUIET), "default:\n{quiet}");
    assert_eq!(loud, g(TERMINATE_ONE_LOUD), "-v:\n{loud}");

    let (quiet, loud) = both(&["terminate", "alpha", "bravo", "--skip-proxy"], &cfg, &["2"], fleet).await;
    assert_eq!(quiet, g(TERMINATE_TWO_QUIET), "default:\n{quiet}");
    assert_eq!(loud, g(TERMINATE_TWO_LOUD), "-v:\n{loud}");

    let (quiet, loud) = both(&["terminate", "alpha", "bravo", "--dry-run"], &cfg, &[], fleet).await;
    assert_eq!(quiet, "[dry-run] would terminate devtest-alpha, devtest-bravo (irreversible)\n", "default:\n{quiet}");
    assert_eq!(loud, g(TERMINATE_DRY_LOUD), "-v:\n{loud}");

    // A locked pod's terminate (locked since the listing): its reason and the retry that lifts it.
    let f = fleet();
    f.server_locked.lock().unwrap().push("r-bravo".into());
    let got = rendered(false, &["2"], handle_pods(pods_cmd(&["terminate", "alpha", "bravo", "--skip-proxy"]), &f, Arc::new(FakeRemote::new()), &cfg, false)).await;
    assert_eq!(got, g(TERMINATE_LOCKED_QUIET), "default:\n{got}");
}

const TERMINATE_ONE_QUIET: &str = r#"
Terminate devtest-alpha (irreversible)? [y/N] y
[terminated] devtest-alpha
"#;

const TERMINATE_ONE_LOUD: &str = r#"
Will TERMINATE devtest-alpha (id=r-alpha, runpod) — irreversible.
Proceed? [y/N] y
[terminated] devtest-alpha (id=r-alpha, runpod)
"#;

const TERMINATE_TWO_QUIET: &str = r#"
TERMINATE devtest-alpha, devtest-bravo (irreversible).
Type 2 to proceed (anything else aborts): 2
[terminated] devtest-alpha
[terminated] devtest-bravo
✓ terminated 2 of 2
"#;

const TERMINATE_TWO_LOUD: &str = r#"
Will TERMINATE these 2 pods — irreversible.
  devtest-alpha (id=r-alpha, runpod)
  devtest-bravo (id=r-bravo, runpod)
Type 2 to proceed (anything else aborts): 2
[terminated] devtest-alpha (id=r-alpha, runpod)
[terminated] devtest-bravo (id=r-bravo, runpod)

terminated 2/2
"#;

const TERMINATE_DRY_LOUD: &str = r#"
[dry-run] would terminate devtest-alpha (id=r-alpha, runpod)
[dry-run] would terminate devtest-bravo (id=r-bravo, runpod)

Dry-run only — would terminate 2 pod(s), then sync the proxy (preview).
"#;

const TERMINATE_LOCKED_QUIET: &str = r#"
TERMINATE devtest-alpha, devtest-bravo (irreversible).
Type 2 to proceed (anything else aborts): 2
[terminated] devtest-alpha
[devtest-bravo] FAILED terminate: locked (retry: arena pods terminate devtest-bravo --unlock)
terminated 1 of 2
Error: 1 pod(s) failed to terminate
"#;

/// lock/unlock: the pods and what a lock does, in a line; `-v` the consequence in full.
#[tokio::test]
async fn lock_says_what_it_protects_in_a_line() {
    let dir = tmpdir("out-lock");
    let cfg = cfg(&dir.0);
    let (quiet, loud) = both(&["lock", "alpha", "bravo"], &cfg, &["y"], fleet).await;
    assert_eq!(quiet, g(LOCK_QUIET), "default:\n{quiet}");
    let consequence = arena_core::lock::consequence(true, &["runpod", "runpod"]);
    let want = format!("Will lock 2 pod(s): devtest-alpha, devtest-bravo\n  ({consequence})\nProceed? [y/N] y\n[locked] devtest-alpha\n[locked] devtest-bravo\n\nlocked 2/2\n");
    assert_eq!(loud, want, "-v:\n{loud}");
    let (quiet, loud) = both(&["unlock", "alpha", "--dry-run"], &cfg, &[], locked_fleet).await;
    assert_eq!(quiet, "[dry-run] would unlock devtest-alpha (it can be stopped, restarted and terminated again)\n", "default:\n{quiet}");
    let consequence = arena_core::lock::consequence(false, &["runpod"]);
    assert_eq!(loud, format!("[dry-run] would unlock 1 pod(s): devtest-alpha\n  ({consequence})\n\nDry-run only — nothing unlocked.\n"), "-v:\n{loud}");
}

const LOCK_QUIET: &str = r#"
Lock devtest-alpha, devtest-bravo (stop, restart, reimage and terminate refused until `arena pods unlock`)? [y/N] y
[locked] devtest-alpha
[locked] devtest-bravo
✓ locked 2 of 2
"#;

/// rename: by default the rename and what follows it, in a line, then the rename; the steps,
/// each `~/.name` and key move, and the MACHINE_NAME note with `-v`.
#[tokio::test]
async fn rename_says_what_follows_the_name_in_a_line() {
    let dir = tmpdir("out-rename");
    let cfg = cfg(&dir.0);
    let csv = keys_csv(&dir.0, "devtest-alpha,sk-or-a\n");
    let mut out = Vec::new();
    for (verbose, dry_run) in [(false, true), (true, true), (false, false), (true, false)] {
        let f = fleet();
        let book = no_api(&csv);
        let run = handle_rename(&f, Arc::new(FakeRemote::new()), &cfg, &book, one("alpha", "echo"), false, dry_run, false);
        out.push(rendered(verbose, &["y"], run).await);
        if !dry_run {
            // Back as it was for the next run (the row moved).
            std::fs::write(dir.0.join("devtest_openrouter_keys.csv"), "# OpenRouter API keys — arena iteration: devtest\ndevtest-alpha,sk-or-a\n").unwrap();
        }
    }
    assert_eq!(out[0], g(RENAME_DRY_QUIET), "default dry run:\n{}", out[0]);
    assert!(out[1].starts_with("[dry-run] would rename 1 pod(s):\n  devtest-alpha → devtest-echo (id=r-alpha)\nthen:\n  1. rewrite ~/.name"), "-v:\n{}", out[1]);
    assert!(out[1].ends_with("each renamed pod's MACHINE_NAME env var keeps the old name — env only changes with a reimage (which wipes the disk); ~/.name carries the new one.\n[dry-run] nothing changed.\n"), "-v:\n{}", out[1]);
    assert_eq!(out[2], g(RENAME_QUIET), "default:\n{}", out[2]);
    assert!(out[3].contains("Proceed? [y/N] y\n[renamed] devtest-alpha → devtest-echo\n[~/.name] ✓ devtest-echo\n[keys] "), "-v:\n{}", out[3]);
    assert!(out[3].ends_with("note: proxy ports follow names: devtest-alpha :7000 → devtest-echo :7004 — whoever is on it connects as the new name from now on (`arena ssh-config --proxy` has every name)\nnote: each renamed pod's MACHINE_NAME env var keeps the old name — env only changes with a reimage (which wipes the disk); ~/.name carries the new one.\n"), "-v:\n{}", out[3]);
}

const RENAME_DRY_QUIET: &str = r#"
[dry-run] would rename devtest-alpha → devtest-echo (~/.name, the keys and the proxy follow; its proxy port :7000 → :7004)
"#;

const RENAME_QUIET: &str = r#"
Rename devtest-alpha → devtest-echo (~/.name, the keys and the proxy follow; its proxy port :7000 → :7004)? [y/N] y
[renamed] devtest-alpha → devtest-echo
proxy port moved: devtest-alpha :7000 → devtest-echo :7004 — whoever is on it connects as the new name now
"#;

/// The fakes a replace of alpha onto a new pod (`r-new`, at 10.0.0.2) runs against: the copy
/// answered, the marker read back, the health check passed.
fn replace_fixture() -> (Fleet, Arc<FakeRemote>) {
    let f = fleet();
    let mut new = pod("r-new", "", "runpod", Some(("10.0.0.2", 22001)));
    new.cost_per_hr = Some(0.29);
    *f.to_create.lock().unwrap() = vec![new];
    let (old_host, new_host) = ("10.0.0.1:22001", "10.0.0.2:22001");
    let remote = Arc::new(FakeRemote::new());
    remote.on(old_host, "arena-replica-sent", FakeReply::stdout("arena-replica-sent=no\n"));
    remote.on(old_host, "RUNPOD_POD_ID", FakeReply::stdout("r-alpha"));
    remote.on(new_host, "arena-replica-ready", FakeReply::stdout("arena-replica-ready\n"));
    remote.on(new_host, "MARK=", FakeReply::stdout("ID=r-new\nMARK=arena-replace-r-new\n"));
    remote.on(new_host, "SSH_OK", FakeReply::stdout("NVIDIA RTX A4000\nSSH_OK\n"));
    // Nothing of its own moved aside by the sync.
    remote.on(new_host, "arena-replica-kept", FakeReply::stdout("arena-replica-kept=0\n"));
    (f, remote)
}

/// replace: by default the new pod in a line, one question, a line as each long step starts (a
/// log's; a terminal's is in place), the swap and the end — instead of the 7-step paragraph and
/// each step's chatter, which `-v` keeps.
#[tokio::test(start_paused = true)]
async fn replace_says_the_new_pod_the_steps_and_the_end() {
    let dir = tmpdir("out-replace");
    let cfg = keyed_cfg(&dir.0, "");
    let keys_dir = dir.0.display().to_string();
    let (ov, place, interrupt) = (SpecOverrides::default(), PlaceFlags::default(), Interrupt::manual());
    let timeouts = arena_core::setup::SetupTimeouts::from_config(&cfg, None).unwrap();
    let mut out = Vec::new();
    for (verbose, dry_run) in [(false, true), (true, true), (false, false), (true, false)] {
        let (f, remote) = replace_fixture();
        let opts = ReplaceOpts { keep_old: false, skip_proxy: false, dry_run, yes: false, force: true };
        let job = ReplaceJob { setup_timeouts: timeouts, ov: &ov, place: &place, opts, interrupt: &interrupt, keys_dir: &keys_dir, balance: None, source: crate::pods::adopt::Source::Create, spares: None, volumes: None };
        let run = replace_on(remote, &f, &cfg, &f, "r-alpha".into(), "devtest-alpha".into(), job);
        out.push(rendered(verbose, &["y", "y"], run).await);
    }
    assert_eq!(out[0], g(REPLACE_DRY_QUIET), "default dry run:\n{}", out[0]);
    assert_eq!(out[1], g(REPLACE_DRY_LOUD), "-v dry run:\n{}", out[1]);
    assert_eq!(out[2], g(REPLACE_QUIET), "default:\n{}", out[2]);
    let loud = out[3].replace(&format!("{keys_dir}/"), "<keys>/");
    assert_eq!(loud, g(REPLACE_LOUD), "-v:\n{loud}");
}

const REPLACE_DRY_QUIET: &str = r#"
devtest-alpha → a new 1×RTX A4000 · RunPod COMMUNITY
[dry-run] would replace devtest-alpha (its home copied onto the new one, swapped in; the old one terminated)
"#;

const REPLACE_DRY_LOUD: &str = r#"
Replace devtest-alpha  (id=r-alpha, runpod)
  target spec: 
  note: GPU NVIDIA RTX A4000: config GPU_TYPE — the source's couldn't be read (pass --gpu to choose)
  pipeline:
    1. create   devtest-alpha-new
    2. setup    devtest-alpha-new   (deploy key, ssh config, repo, ~/.name=alpha-new until the swap)
    3. copy     devtest-alpha -> devtest-alpha-new   (copies the home incl. .git, ~/.config, ~/.jupyter, ~/.ipython, ~/.local (bin, share); skips caches and other dot-dirs (.cache, .venv, .vscode-server, .ssh, .claude), venvs, site-packages and ~/.local/lib (pip --user), HF caches, ~/.local/share/uv, the claude/codex that setup installs and Claude Code's own state, shell rc files + history (the API keys follow by name), zsh's completion cache, ~/.name)
    4. verify   devtest-alpha-new   (the copy read back on the right pod, ssh + GPU health check, maintenance window)
    5. swap     rename devtest-alpha -> devtest-alpha-old, then devtest-alpha-new -> devtest-alpha (+ ~/.name, API keys under devtest-alpha)
    6. proxy    apply (repoint nginx; stable port reclaimed by the canonical name)
    7. cleanup  terminate devtest-alpha-old once the proxy routes devtest-alpha to the new pod

[dry-run] nothing changed.
"#;

const REPLACE_QUIET: &str = r#"
devtest-alpha → a new 1×RTX A4000 · RunPod COMMUNITY
Replace devtest-alpha (its home copied onto the new one, swapped in; the old one terminated)? [y/N] y
[created] devtest-alpha-new · 1×RTX A4000 COMMUNITY · $0.29/h
[devtest-alpha-new] waiting for SSH…
[devtest-alpha-new] setup…
[devtest-alpha-new] setup ✓ (0s)
[devtest-alpha-new] copy from devtest-alpha…
[devtest-alpha-new] checking the copy stays (90s)…
[devtest-alpha-new] health check…
devtest-alpha-new is built, copied and checked. Swap it in as devtest-alpha (devtest-alpha → devtest-alpha-old)? [y/N] y
[devtest-alpha-new] final sync from devtest-alpha…
[swapped] devtest-alpha is the new pod; the old one is devtest-alpha-old
✓ devtest-alpha replaced; devtest-alpha-old terminated
"#;

const REPLACE_LOUD: &str = r#"
Replace devtest-alpha  (id=r-alpha, runpod)
  target spec: 
  note: GPU NVIDIA RTX A4000: config GPU_TYPE — the source's couldn't be read (pass --gpu to choose)
  pipeline:
    1. create   devtest-alpha-new
    2. setup    devtest-alpha-new   (deploy key, ssh config, repo, ~/.name=alpha-new until the swap)
    3. copy     devtest-alpha -> devtest-alpha-new   (copies the home incl. .git, ~/.config, ~/.jupyter, ~/.ipython, ~/.local (bin, share); skips caches and other dot-dirs (.cache, .venv, .vscode-server, .ssh, .claude), venvs, site-packages and ~/.local/lib (pip --user), HF caches, ~/.local/share/uv, the claude/codex that setup installs and Claude Code's own state, shell rc files + history (the API keys follow by name), zsh's completion cache, ~/.name)
    4. verify   devtest-alpha-new   (the copy read back on the right pod, ssh + GPU health check, maintenance window)
    5. swap     rename devtest-alpha -> devtest-alpha-old, then devtest-alpha-new -> devtest-alpha (+ ~/.name, API keys under devtest-alpha)
    6. proxy    apply (repoint nginx; stable port reclaimed by the canonical name)
    7. cleanup  terminate devtest-alpha-old once the proxy routes devtest-alpha to the new pod
Will build devtest-alpha-new, copy devtest-alpha's files onto it, verify, then swap it in as devtest-alpha (old parked as devtest-alpha-old, then terminated).
Proceed? [y/N] y
[1/7] creating devtest-alpha-new…
      [created] devtest-alpha-new id=r-new on 1×RTX A4000 COMMUNITY (price unknown)
      created devtest-alpha-new id=r-new
[2/7] waiting for devtest-alpha-new to come up and stabilize…
[3/7] provisioning devtest-alpha-new… (attempt 1/3; its per-host API keys follow at the swap)
Broadcast tokens: Hugging Face ✗ (✓ exported on each pod; ✗ skipped)
Claude Code: no accounts (CLAUDE_TOKENS_FILE / CLAUDE_CODE_OAUTH_TOKEN unset) — skipped
Provisioning 1 pod(s) over SSH (budgets: copy 60s, config 300s, hetzner script 1800s)…
[1/1] ✓ devtest-alpha-new

Done: 1 provisioned, 0 failed.
API keys: none generated in <keys>/ — skipping (run `arena keys gen --all` or drop in <provider>_api_keys.csv, then re-run setup / `pods copy-keys`).
[4/7] copying devtest-alpha → devtest-alpha-new (copies the home incl. .git, ~/.config, ~/.jupyter, ~/.ipython, ~/.local (bin, share); skips caches and other dot-dirs (.cache, .venv, .vscode-server, .ssh, .claude), venvs, site-packages and ~/.local/lib (pip --user), HF caches, ~/.local/share/uv, the claude/codex that setup installs and Claude Code's own state, shell rc files + history (the API keys follow by name), zsh's completion cache, ~/.name)…
      copied (direct pod-to-pod)
[5/7] confirming the copy persists (~90s — pods can reset while still settling)…
      verifying health of devtest-alpha-new…
      ssh ok; gpu: NVIDIA RTX A4000
devtest-alpha-new is built, copied and verified. Swap it in as devtest-alpha now? (renames devtest-alpha -> devtest-alpha-old, then devtest-alpha-new -> devtest-alpha)
Proceed? [y/N] y
      final sync of what changed on devtest-alpha since the copy…
      copied (direct pod-to-pod)
[6/7] swapping…
      devtest-alpha -> devtest-alpha-old
      devtest-alpha-new -> devtest-alpha
      note: devtest-alpha's MACHINE_NAME env var still says devtest-alpha-new — env is fixed at create (only a reimage, which wipes the disk, resets it); ~/.name carries alpha.
[7/7] re-pointing proxy…
[proxy] after replace: +0 ~0 -0 =0 (unchanged; 1 pod(s) not forwarded yet (no SSH endpoint))
terminating parked devtest-alpha-old…
done — devtest-alpha replaced; devtest-alpha-old terminated.
"#;

/// migrate: `copy` (dry run and real), `cutover` (dry run and real, onto a write-only proxy),
/// `finish` (dry run and real) and `revert`'s dry run — each a line or two before its question,
/// a line as each long step starts, and its end; `-v` as before.
#[tokio::test(start_paused = true)]
async fn migrate_says_each_stage_in_a_few_lines() {
    let dir = tmpdir("out-migrate");
    let cfg = keyed_cfg(&dir.0, "");
    let keys_dir = dir.0.display().to_string();
    let (ov, place, interrupt) = (SpecOverrides::default(), PlaceFlags::default(), Interrupt::manual());
    let timeouts = arena_core::setup::SetupTimeouts::from_config(&cfg, None).unwrap();
    let mut out: Vec<(String, String)> = Vec::new();
    // copy: a new pod made, set up and synced onto.
    for dry_run in [true, false] {
        for verbose in [false, true] {
            let (f, remote) = replace_fixture();
            let pods = f.pods.lock().unwrap().clone();
            let job = MigrateCopyJob {
                setup_timeouts: timeouts,
                ov: &ov,
                place: &place,
                dry_run,
                yes: false,
                interrupt: &interrupt,
                keys_dir: &keys_dir,
                balance: None,
                source: crate::pods::adopt::Source::Create,
                force: false,
                spares: None,
                volumes: None,
            };
            let got = rendered(verbose, &["y"], migrate_copy_on(remote, &cfg, &f, "devtest-alpha", &pods, job)).await;
            out.push((format!("copy dry={dry_run} v={verbose}"), got.replace(&format!("{keys_dir}/"), "<keys>/")));
        }
    }
    // cutover: alpha-new is there; the final sync, the swap, the (write-only) proxy.
    let pair = || {
        let (f, remote) = replace_fixture();
        f.pods.lock().unwrap().push(pod("r-new", "devtest-alpha-new", "runpod", Some(("10.0.0.2", 22001))));
        (f, remote)
    };
    for dry_run in [true, false] {
        for verbose in [false, true] {
            let (f, remote) = pair();
            let pods = f.pods.lock().unwrap().clone();
            let opts = CutoverOpts { volumes: None, skip_proxy: false, no_final_sync: false, dry_run, yes: false, force: true };
            let got = rendered(verbose, &["y"], migrate_cutover_on(remote, &f, &cfg, &f, "devtest-alpha", &pods, opts, &keys_dir)).await;
            out.push((format!("cutover dry={dry_run} v={verbose}"), got.replace(&format!("{keys_dir}/"), "<keys>/")));
        }
    }
    // finish: -old goes once the proxy routes alpha to the live pod.
    for dry_run in [true, false] {
        for verbose in [false, true] {
            let f = Fleet::new(vec![
                pod("r-new", "devtest-alpha", "runpod", Some(("10.0.0.2", 22001))),
                pod("r-alpha", "devtest-alpha-old", "runpod", Some(("10.0.0.1", 22001))),
            ]);
            let listed = f.pods.lock().unwrap().clone();
            let opts = FinishOpts { dry_run, ..Default::default() };
            let got = rendered(verbose, &["y"], migrate_finish_on(&FakeRemote::new(), &f, &cfg, &f, "devtest-alpha", &listed, opts)).await;
            out.push((format!("finish dry={dry_run} v={verbose}"), got));
        }
    }
    let want = [
        MIGRATE_COPY_DRY_QUIET,
        MIGRATE_COPY_DRY_LOUD,
        MIGRATE_COPY_QUIET,
        MIGRATE_COPY_LOUD,
        CUTOVER_DRY_QUIET,
        CUTOVER_DRY_LOUD,
        CUTOVER_QUIET,
        CUTOVER_LOUD,
        FINISH_DRY_QUIET,
        FINISH_DRY_LOUD,
        FINISH_QUIET,
        FINISH_LOUD,
    ];
    assert_eq!(out.len(), want.len());
    for ((label, got), want) in out.iter().zip(want) {
        assert_eq!(got, g(want), "{label}:\n{got}");
    }
}

const MIGRATE_COPY_DRY_QUIET: &str = r#"
devtest-alpha-new: a new 1×RTX A4000 · RunPod COMMUNITY
[dry-run] would create devtest-alpha-new, set it up and sync devtest-alpha onto it (no rename, no proxy change)
"#;

const MIGRATE_COPY_DRY_LOUD: &str = r#"
migrate copy devtest-alpha (dry-run):
  - create devtest-alpha-new:
  target spec: 
  note: GPU NVIDIA RTX A4000: config GPU_TYPE — the source's couldn't be read (pass --gpu to choose)
  - wait for it to come up, then set it up (~/.name=alpha-new until the cutover)
  - sync devtest-alpha → devtest-alpha-new: copies the home incl. .git, ~/.config, ~/.jupyter, ~/.ipython, ~/.local (bin, share); skips caches and other dot-dirs (.cache, .venv, .vscode-server, .ssh, .claude), venvs, site-packages and ~/.local/lib (pip --user), HF caches, ~/.local/share/uv, the claude/codex that setup installs and Claude Code's own state, shell rc files + history (the API keys follow by name), zsh's completion cache, ~/.name
  (no rename of devtest-alpha, no proxy change)
[dry-run] nothing changed.
"#;

const MIGRATE_COPY_QUIET: &str = r#"
devtest-alpha-new: a new 1×RTX A4000 · RunPod COMMUNITY
Create devtest-alpha-new and sync devtest-alpha onto it (no rename, no proxy change)? [y/N] y
[created] devtest-alpha-new · 1×RTX A4000 COMMUNITY · $0.29/h
[devtest-alpha-new] waiting for SSH…
[devtest-alpha-new] setup…
[devtest-alpha-new] setup ✓ (0s)
[devtest-alpha-new] sync from devtest-alpha…
✓ devtest-alpha-new is built and synced from devtest-alpha (devtest-alpha untouched)
Next: `arena pods migrate cutover devtest-alpha` (re-run `migrate copy` first to re-sync)
"#;

const MIGRATE_COPY_LOUD: &str = r#"
migrate copy devtest-alpha → devtest-alpha-new:
  target spec: 
  note: GPU NVIDIA RTX A4000: config GPU_TYPE — the source's couldn't be read (pass --gpu to choose)
Will create devtest-alpha-new and sync devtest-alpha's files onto it — no rename, no proxy change. (copies the home incl. .git, ~/.config, ~/.jupyter, ~/.ipython, ~/.local (bin, share); skips caches and other dot-dirs (.cache, .venv, .vscode-server, .ssh, .claude), venvs, site-packages and ~/.local/lib (pip --user), HF caches, ~/.local/share/uv, the claude/codex that setup installs and Claude Code's own state, shell rc files + history (the API keys follow by name), zsh's completion cache, ~/.name)
Proceed? [y/N] y
[1/3] creating devtest-alpha-new…
      [created] devtest-alpha-new id=r-new on 1×RTX A4000 COMMUNITY (price unknown)
      created id=r-new
[2/3] waiting for devtest-alpha-new to come up and stabilize…
      provisioning devtest-alpha-new (its per-host API keys follow at the cutover, under devtest-alpha)…
Broadcast tokens: Hugging Face ✗ (✓ exported on each pod; ✗ skipped)
Claude Code: no accounts (CLAUDE_TOKENS_FILE / CLAUDE_CODE_OAUTH_TOKEN unset) — skipped
Provisioning 1 pod(s) over SSH (budgets: copy 60s, config 300s, hetzner script 1800s)…
[1/1] ✓ devtest-alpha-new

Done: 1 provisioned, 0 failed.
API keys: none generated in <keys>/ — skipping (run `arena keys gen --all` or drop in <provider>_api_keys.csv, then re-run setup / `pods copy-keys`).
[3/3] syncing devtest-alpha → devtest-alpha-new…
      copied (direct pod-to-pod)

✓ devtest-alpha-new is built and synced (participant still on devtest-alpha, untouched).
  Test it directly:  ssh -p 22001 root@10.0.0.2  (use a fleet key)
  Re-run `arena pods migrate copy devtest-alpha` to re-sync any changes, then
  `arena pods migrate cutover devtest-alpha` to switch over (proxy is verified + auto-reverts).
"#;

const CUTOVER_DRY_QUIET: &str = r#"
[dry-run] would cut devtest-alpha over to devtest-alpha-new (a final sync, names swapped, the proxy written but not checked: arena doesn't reload nginx; devtest-alpha-old kept)
"#;

const CUTOVER_DRY_LOUD: &str = r#"
migrate cutover devtest-alpha (dry-run):
  1. final delta sync devtest-alpha → devtest-alpha-new (refused if devtest-alpha was reset since devtest-alpha-new's last sync)
  2. rename devtest-alpha → devtest-alpha-old, then devtest-alpha-new → devtest-alpha (+ ~/.name on both, API keys under devtest-alpha)
  3. proxy apply (write-only: <keys>/proxy.conf is written, nginx is NOT reloaded by arena — so nothing can be verified through it; the new pod's own health check stands, and nothing auto-reverts)
  devtest-alpha-old kept (delete later with `migrate finish devtest-alpha`).
[dry-run] nothing changed.
"#;

const CUTOVER_QUIET: &str = r#"
Cut devtest-alpha over to devtest-alpha-new (a final sync, names swapped, the proxy written but not checked: arena doesn't reload nginx; devtest-alpha-old kept)? [y/N] y
[devtest-alpha-new] final sync from devtest-alpha…
[devtest-alpha-new] health check…
[swapped] devtest-alpha is the new pod; the original is devtest-alpha-old
‼ the proxy is write-only: reload nginx yourself, then check devtest-alpha's port reaches the new pod (<keys>/proxy.conf)
✓ devtest-alpha is on the new pod; devtest-alpha-old kept (roll back: `arena pods migrate revert devtest-alpha`; done: `arena pods migrate finish devtest-alpha`)
"#;

const CUTOVER_LOUD: &str = r#"
Cut over devtest-alpha to devtest-alpha-new: final delta sync devtest-alpha → devtest-alpha-new (refused if devtest-alpha was reset since devtest-alpha-new's last sync); then swap names and proxy apply (write-only: <keys>/proxy.conf is written, nginx is NOT reloaded by arena — so nothing can be verified through it; the new pod's own health check stands, and nothing auto-reverts). devtest-alpha-old kept.
Proceed? [y/N] y
[1/4] final delta sync devtest-alpha → devtest-alpha-new…
      copied (direct pod-to-pod)
[2/4] verifying devtest-alpha-new health…
      ssh ok; gpu: NVIDIA RTX A4000
[3/4] swapping names…
      note: devtest-alpha's MACHINE_NAME env var still says devtest-alpha-new — env is fixed at create (only a reimage, which wipes the disk, resets it); ~/.name carries alpha.
[4/4] re-pointing proxy and verifying devtest-alpha through it…
[proxy] after migrate cutover: +0 ~0 -0 =0 (unchanged; 1 pod(s) not forwarded yet (no SSH endpoint))
      [proxy] write-only: devtest-alpha's forward to the new pod is written to <keys>/proxy.conf, but arena doesn't reload nginx — reload it yourself, then check devtest-alpha's port reaches the new pod (roll back with `arena pods migrate revert devtest-alpha`).

✓ Cut over: devtest-alpha is now the new pod. Original parked as devtest-alpha-old (kept).
  If something's wrong, roll back:  arena pods migrate revert devtest-alpha
  When you're happy, remove the old pod:  arena pods migrate finish devtest-alpha
"#;

const FINISH_DRY_QUIET: &str = r#"
[dry-run] would terminate devtest-alpha-old (the original pod; irreversible, once the proxy routes devtest-alpha to the live pod)
"#;

const FINISH_DRY_LOUD: &str = r#"
migrate finish devtest-alpha (dry-run): would sync the proxy, confirm devtest-alpha's port routes to the live pod, then terminate devtest-alpha-old (id=r-alpha).
"#;

const FINISH_QUIET: &str = r#"
Terminate devtest-alpha-old (the original pod; irreversible, once the proxy routes devtest-alpha to the live pod)? [y/N] y
✓ Terminated devtest-alpha-old. Migration of devtest-alpha complete.
"#;

const FINISH_LOUD: &str = r#"
Permanently terminate devtest-alpha-old (id=r-alpha)? This deletes the original pod.
Proceed? [y/N] y
[proxy] after migrate finish: +0 ~0 -0 =0 (unchanged)
✓ Terminated devtest-alpha-old. Migration of devtest-alpha complete.
"#;

