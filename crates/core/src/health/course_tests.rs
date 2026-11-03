//! Table tests for the course's facts and verdicts (`course`), how a course FAIL sits beside
//! the machine's own verdict, and the script's course mode run for real in a scratch `$HOME`.

use super::*;
use crate::health::{failure_kind_of, parse_deep, render_course_report, render_summary, FailureKind, HealthPolicy};

const NOW: u64 = 1_800_000_000;
const REPO_KEY: &str = "github.com/nickypro/arena-sandbox-materials";
const TODAY: &str = "autocommit-devtest-w4d2-apple";

fn facts(lines: &str) -> DeepFacts {
    parse_deep(&format!("deep_check=3\n{lines}deep_check_end=1\n"))
}

fn pod(name: &str) -> Pod {
    Pod { id: format!("id-{name}"), name: name.into(), provider: "runpod".into(), status: "RUNNING".into(), ..Default::default() }
}

/// devtest's book: today is w4d2, the repo is the sandbox's, nothing else known.
fn book() -> CourseBook {
    CourseBook {
        prefix: "devtest".into(),
        now: NOW,
        day: Some((4, 2)),
        origin: Some("nickypro/arena-sandbox-materials".into()),
        ..CourseBook::default()
    }
}

fn check(f: &DeepFacts, name: &str, book: &CourseBook, which: &str) -> Check {
    course_checks(f, &pod(name), book).into_iter().find(|c| c.name == which).unwrap()
}

fn verdict(f: &DeepFacts, book: &CourseBook, which: &str) -> (Status, String) {
    let c = check(f, "devtest-apple", book, which);
    (c.status, c.detail)
}

/// A healthy machine's course facts, as the script prints them.
fn good_lines() -> String {
    format!(
        "id.name=devtest-apple\nid.hostname=a1b2c3\nid.hostkey=0123456789ab\n\
         repo.path=/root/ARENA_materials\nrepo.state=ok\nrepo.branch={TODAY}\nrepo.op=none\n\
         repo.origin=github.com:nickypro/arena-sandbox-materials.git\nrepo.origin_keys={REPO_KEY} {REPO_KEY}\n\
         push.key=set\npush.exit=0\n\
         claude.path=/usr/local/bin/claude\nclaude.version=2.1.30 (Claude Code)\nclaude.auth=none\n\
         kernel.env=/opt/arena-env\nkernel.ipykernel=yes\nkernel.specs=arena-env,python3\nkernel.registered=arena-env\n\
         kernel.name=arena-env\nkernel.start_secs=1.2\nkernel.torch=ok\nkernel.torch_version=2.9.0+cu130\nkernel.torch_secs=3.1\nkernel.done=1\n\
         uv.path=/usr/local/bin/uv\nuv.version=uv 0.9.5\n"
    )
}

/// `good_lines` with the lines starting `key=` swapped for `new` (or dropped when it's empty).
fn with(key: &str, new: &str) -> String {
    let mut out: Vec<String> = good_lines().lines().filter(|l| !l.starts_with(&format!("{key}="))).map(String::from).collect();
    if !new.is_empty() {
        out.push(new.to_string());
    }
    out.join("\n") + "\n"
}

#[test]
fn the_course_facts_parse_and_unknowns_stay_unknown() {
    let f = facts(&good_lines());
    assert_eq!(f.identity, Some(Identity { name: Some("devtest-apple".into()), hostname: Some("a1b2c3".into()), hostkey: Some("0123456789ab".into()) }));
    let r = f.repo.clone().unwrap();
    assert_eq!((r.state.as_deref(), r.branch.as_deref(), r.op.as_deref()), (Some("ok"), Some(TODAY), None));
    assert_eq!(r.origin_keys, [REPO_KEY], "deduplicated");
    assert_eq!(f.push.as_ref().map(|p| (p.key.as_deref(), p.exit)), Some((Some("set"), Some(0))));
    let k = f.kernel.clone().unwrap();
    assert_eq!((k.start_secs, k.torch_secs, k.torch.as_deref()), (Some(1.2), Some(3.1), Some("ok")));
    assert!(f.other.is_empty(), "every course fact known: {:?}", f.other);
    // A bad hostkey, a negative time, an unknown value: unknown, never a value.
    let f = facts("id.hostkey=zz\nkernel.start_secs=-1\nclaude.fingerprint=XYZ\nrepo.branch=unknown\nclaude.http=abc\n");
    assert_eq!(f.identity.unwrap().hostkey, None);
    assert_eq!(f.kernel.unwrap().start_secs, None);
    let c = f.claude.unwrap();
    assert_eq!((c.fingerprint, c.http), (None, None));
    assert_eq!(f.repo.unwrap().branch, None);
    // `~/.name` as setup writes it: the name.
    assert_eq!(facts("id.name=export MACHINE_NAME=\"devtest-apple\"\n").identity.unwrap().name.as_deref(), Some("devtest-apple"));
    // An unknown course field is kept for --json, not dropped.
    assert_eq!(facts("repo.future=1\n").other.get("repo.future").map(String::as_str), Some("1"));
}

#[test]
fn a_healthy_machine_passes_every_course_check() {
    let f = facts(&good_lines());
    let checks = course_checks(&f, &pod("devtest-apple"), &book());
    let got: Vec<(&str, Status)> = checks.iter().map(|c| (c.name.as_str(), c.status)).collect();
    assert_eq!(
        got,
        [(PROXY, Status::Skip), (BACKUP, Status::Pass), (REPO, Status::Pass), (CLAUDE, Status::Skip), (KERNEL, Status::Pass), (UV, Status::Pass)]
    );
    assert_eq!(checks[2].detail, format!("on {TODAY} (today's)"));
    assert_eq!(checks[3].detail, "v2.1.30, no auth (no arena claude token, clods profile or API key)");
    assert_eq!(checks[4].detail, "arena-env: torch 2.9.0+cu130 in 4.3s");
    assert_eq!(checks[5].detail, "/usr/local/bin/uv (uv 0.9.5)");
    // From a script before these checks: every one a Skip saying so, never a FAIL.
    let old = facts("torch=ok\n");
    for c in course_checks(&old, &pod("devtest-apple"), &book()) {
        assert_eq!(c.status, Status::Skip, "{c:?}");
    }
}

fn id(name: &str, hostkey: &str) -> Identity {
    Identity { name: Some(name.into()), hostname: Some("a1b2c3".into()), hostkey: Some(hostkey.into()) }
}

fn proxied(state: ProxyState, port: Option<u16>, reached: Option<Result<Identity, String>>, write_only: bool) -> CourseBook {
    let ports = [("runpod:id-devtest-apple".to_string(), Port { port, state, reached })].into_iter().collect();
    CourseBook { proxy: Some(ProxyBook { write_only, ports }), ..book() }
}

#[test]
fn proxy_verdicts() {
    let here = "id.name=devtest-apple\nid.hostname=a1b2c3\nid.hostkey=0123456789ab\n".to_string();
    let f = facts(&here);
    let me = id("devtest-apple", "0123456789ab");
    let other = id("devtest-apple", "ffffffffffff");
    let apply = "`arena proxy apply`";
    let cases: Vec<(CourseBook, Status, String)> = vec![
        (book(), Status::Skip, "no proxy configured (SSH_PROXY_HOST)".into()),
        (CourseBook { proxy: Some(ProxyBook::default()), ..book() }, Status::Skip, "not checked here".into()),
        (proxied(ProxyState::None, None, None, false), Status::Warn, format!("no forward on the proxy for it — {apply}")),
        (proxied(ProxyState::Unknown, None, None, false), Status::Skip, "no proxy port for this name (not in MACHINE_NAME_LIST)".into()),
        (proxied(ProxyState::Live, Some(9500), Some(Ok(me.clone())), false), Status::Pass, "port 9500 reaches it".into()),
        (proxied(ProxyState::Unknown, Some(9500), Some(Ok(me.clone())), false), Status::Pass, "port 9500 reaches it".into()),
        (
            proxied(ProxyState::Stale, Some(9500), Some(Ok(me.clone())), false),
            Status::Warn,
            format!("port 9500 reaches it, but the proxy file forwards to an old endpoint — {apply}"),
        ),
        // The route left on the old pod after a replace: same name, another machine.
        (
            proxied(ProxyState::Live, Some(9500), Some(Ok(other)), false),
            Status::Fail,
            format!("port 9500 reaches another machine (devtest-apple) — {apply}"),
        ),
        (
            proxied(ProxyState::Live, Some(9500), Some(Err("connection refused".into())), false),
            Status::Fail,
            format!("port 9500 doesn't reach it: connection refused — {apply}"),
        ),
        (
            proxied(ProxyState::Stale, Some(9500), Some(Err("timed out".into())), false),
            Status::Fail,
            format!("port 9500 forwards to an old endpoint (timed out) — {apply}"),
        ),
        (proxied(ProxyState::Stale, Some(9500), None, false), Status::Fail, format!("port 9500 forwards to an old endpoint — {apply}")),
        (
            proxied(ProxyState::Live, Some(9500), Some(Err("connection refused".into())), true),
            Status::Warn,
            "port 9500: connection refused — write-only proxy here: SSH_PROXY_RELOAD_CMD reloads it".into(),
        ),
        (proxied(ProxyState::Live, Some(9500), None, false), Status::Skip, "port 9500 not tried".into()),
        // A write-only proxy's stale file is still wrong (live: it read as the write-only WARN).
        (
            proxied(ProxyState::Stale, Some(9500), Some(Err("connection refused".into())), true),
            Status::Fail,
            format!("port 9500 forwards to an old endpoint (connection refused) — {apply}"),
        ),
    ];
    for (b, status, detail) in cases {
        assert_eq!(verdict(&f, &b, PROXY), (status, detail.clone()), "{detail}");
    }
    // No identity from the machine itself: answered, not compared.
    let b = proxied(ProxyState::Live, Some(9500), Some(Ok(me)), false);
    assert_eq!(verdict(&facts(""), &b, PROXY).0, Status::Warn);
    // Identity: every part both know agrees, and they share one.
    assert!(id("a", "0123456789ab").same(&Identity { name: None, hostname: None, hostkey: Some("0123456789ab".into()) }));
    assert!(!id("a", "0123456789ab").same(&Identity::default()));
    assert!(!id("a", "0123456789ab").same(&id("b", "0123456789ab")), "another name is another machine");
}

fn cells(entries: &[(&str, BackupCell)]) -> CourseBook {
    CourseBook { backups: Some(Ok(entries.iter().map(|(n, c)| (n.to_string(), c.clone())).collect())), ..book() }
}

fn cell(health: BackupHealth, text: &str, ok_ago: Option<u64>, why: Option<&str>) -> BackupCell {
    BackupCell {
        health,
        ok_at: ok_ago.map(|a| NOW - a),
        tried_at: ok_ago.map(|a| NOW - a),
        text: text.into(),
        why: why.map(String::from),
        counts: true,
    }
}

#[test]
fn backup_verdicts() {
    let setup = "`arena pods setup apple`";
    let ok = facts(&good_lines());
    let push = |exit: &str, err: &str| facts(&with("push.exit", &format!("push.exit={exit}\npush.error={err}")));
    let cases: Vec<(DeepFacts, CourseBook, Status, String)> = vec![
        (facts(&with("repo.state", "repo.state=missing")), book(), Status::Skip, "needs the course repo (see repo)".into()),
        (facts(&with("push.key", "push.key=missing")), book(), Status::Fail, format!("no deploy key on it (GIT_SSH_KEY_REMOTE) — {setup}")),
        (
            facts(&with("push.exit", "").replace("push.key=set", "push.key=none")),
            book(),
            Status::Warn,
            "push not checked: no cohort repo to push to — set ARENA_REPO_OWNER and ARENA_REPO_NAME".into(),
        ),
        (
            push("128", "ERROR: The key you are authenticating with has been marked as read only."),
            book(),
            Status::Fail,
            "GitHub refuses a push: its deploy key is read-only — allow write access for it on GitHub".into(),
        ),
        (push("128", "git@github.com: Permission denied (publickey)."), book(), Status::Fail, format!("GitHub refuses a push: git@github.com: Permission denied (publickey). — {setup}")),
        (push("128", "ERROR: Repository not found."), book(), Status::Fail, format!("GitHub refuses a push: ERROR: Repository not found. — {setup}")),
        (push("128", "Host key verification failed."), book(), Status::Fail, format!("github.com's host key isn't known there — {setup}")),
        (
            push("124", "ssh: connect to host github.com port 22: Connection timed out"),
            book(),
            Status::Warn,
            "push not checked: ssh: connect to host github.com port … — `arena pods test --course apple`".into(),
        ),
        (ok.clone(), book(), Status::Pass, "push ok".into()),
        (ok.clone(), cells(&[("devtest-apple", cell(BackupHealth::Ok, "pushed", Some(720), None))]), Status::Pass, "push ok · last backup 12 min ago".into()),
        (ok.clone(), cells(&[("devtest-apple", cell(BackupHealth::Unknown, "no record", None, Some("no git backup recorded for it yet")))]), Status::Pass, "push ok · no backup recorded yet".into()),
        // On main: the repo check says it (with its fix); the backup's own verdict is the push.
        (ok.clone(), cells(&[("devtest-apple", cell(BackupHealth::Warn, "on main", Some(7200), Some("SKIP main")))]), Status::Pass, "push ok · last backup 2 h ago".into()),
        (
            ok.clone(),
            cells(&[("devtest-apple", cell(BackupHealth::Warn, "pushed", Some(7200), Some("not backed up for 2 h (due every 15 min)")))]),
            Status::Warn,
            "not backed up for 2 h (due every 15 min) — `arena pods backup apple`".into(),
        ),
        (
            ok.clone(),
            cells(&[("devtest-apple", cell(BackupHealth::Fail, "failed", Some(7200), Some("PUSH_FAILED")))]),
            Status::Fail,
            "pushes; its last backup failed: PUSH_FAILED — `arena pods backup apple`".into(),
        ),
        (
            ok.clone(),
            CourseBook { backups: Some(Err("backups.json: Permission denied".into())), ..book() },
            Status::Warn,
            "pushes; the backups' record can't be read: backups.json: Permission denied — `arena backups`".into(),
        ),
        (facts(""), book(), Status::Skip, NOT_REPORTED.into()),
    ];
    for (f, b, status, detail) in cases {
        assert_eq!(verdict(&f, &b, BACKUP), (status, detail.clone()), "{detail}");
    }
}

#[test]
fn repo_verdicts() {
    let init = "`arena pods init-branches apple`";
    let on = |b: &str| facts(&with("repo.branch", &format!("repo.branch={b}")));
    let cases: Vec<(DeepFacts, CourseBook, Status, String)> = vec![
        (facts(&with("repo.state", "repo.state=missing")), book(), Status::Fail, "no course repo at /root/ARENA_materials — `arena pods setup apple`".into()),
        (
            facts(&with("repo.state", "repo.state=error: fatal: detected dubious ownership in repository at '/root/ARENA_materials'")),
            book(),
            Status::Warn,
            "can't read /root/ARENA_materials: fatal: detected dubious ownership in re… — `arena pods ask apple`".into(),
        ),
        (
            facts(&with("repo.origin_keys", "repo.origin_keys=github.com/arena-education/arena_materials").replace("repo.origin=github.com:nickypro", "repo.origin=github.com:ARENA-education")),
            book(),
            Status::Fail,
            "origin isn't nickypro/arena-sandbox-materials: no backups — `arena pods setup apple` (it's github.com:ARENA-education/arena-sandbox-materials.git)".into(),
        ),
        (
            facts(&with("repo.origin_keys", "").replace("repo.origin=github.com:nickypro/arena-sandbox-materials.git\n", "")),
            book(),
            Status::Fail,
            "origin isn't nickypro/arena-sandbox-materials: no backups — `arena pods setup apple` (it's none)".into(),
        ),
        (
            facts(&with("repo.op", "repo.op=rebase")),
            book(),
            Status::Warn,
            "a rebase in progress: no backups — there, `git rebase --continue` or `--abort`".into(),
        ),
        (
            facts(&with("repo.op", "repo.op=merge unmerged")),
            book(),
            Status::Warn,
            "a merge in progress (conflicts): no backups — there, `git commit` or `git merge --abort`".into(),
        ),
        (
            facts(&with("repo.branch", "repo.detached=0d515fc")),
            book(),
            Status::Warn,
            format!("detached HEAD (at 0d515fc): no backups — {init}"),
        ),
        (on("main"), book(), Status::Warn, format!("on main: no backups — {init}")),
        (on("autocommit-devtest-w4d1-apple"), book(), Status::Warn, format!("on autocommit-devtest-w4d1-apple (an older day's) — {init}")),
        (on("my-project"), book(), Status::Warn, format!("on my-project (not today's) — {init}")),
        (facts(&good_lines()), book(), Status::Pass, format!("on {TODAY} (today's)")),
        // No ARENA_START_DATE: no day to hold it to, and no origin configured: none to compare.
        (on("my-project"), CourseBook { day: None, origin: None, ..book() }, Status::Pass, "on my-project".into()),
        (on("main"), CourseBook { day: None, ..book() }, Status::Warn, format!("on main: no backups — {init}")),
    ];
    for (f, b, status, detail) in cases {
        assert_eq!(verdict(&f, &b, REPO), (status, detail.clone()), "{detail}");
    }
    assert_eq!(book().todays_branch("devtest-apple").as_deref(), Some(TODAY));
}

fn claude_lines(extra: &str) -> DeepFacts {
    facts(&with("claude.auth", extra))
}

#[test]
fn claude_verdicts() {
    let push = "`arena claude push apple`";
    let fp = crate::health::fingerprint("sk-ant-oat01-mine");
    let pushed = |fp: &str| CourseBook {
        claude: ClaudeBook {
            pushed: [("runpod:id-devtest-apple".to_string(), (Some("a@x".to_string()), Some(fp.to_string())))].into_iter().collect(),
            pool: [(fp.to_string(), "a@x".to_string())].into_iter().collect(),
        },
        ..book()
    };
    let oauth = |extra: &str| claude_lines(&format!("claude.auth=oauth\nclaude.source=shell\nclaude.fingerprint={fp}\n{extra}"));
    let cases: Vec<(DeepFacts, CourseBook, Status, String)> = vec![
        (facts(""), book(), Status::Skip, NOT_REPORTED.into()),
        (facts(&with("claude.path", "claude.path=missing")), book(), Status::Fail, "not installed (`claude` isn't on root's PATH) — `arena pods setup apple`".into()),
        (
            facts(&with("claude.version", "claude.version_error=timed out after 10s")),
            book(),
            Status::Fail,
            "`claude --version` fails: timed out after 10s — `arena pods setup apple`".into(),
        ),
        (facts(&good_lines()), pushed(&fp), Status::Warn, format!("v2.1.30: no token, though arena claude pushed one — {push} (a@x)")),
        (oauth(""), pushed(&fp), Status::Pass, "v2.1.30, arena claude's a@x".into()),
        (oauth(""), pushed("000000000000"), Status::Warn, format!("v2.1.30: not the token arena claude pushed — {push} (a@x)")),
        (oauth(""), book(), Status::Pass, format!("v2.1.30, a token {fp}, not arena claude's")),
        (
            claude_lines(&format!("claude.auth=oauth\nclaude.source=clods\nclaude.profile=2\nclaude.fingerprint={fp}\n")),
            pushed(&fp),
            Status::Pass,
            format!("v2.1.30, clods profile 2, token {fp}"),
        ),
        (
            claude_lines("claude.auth=bearer\nclaude.source=shell\nclaude.base_host=openrouter.ai\nclaude.same_as=openrouter\nclaude.call=openrouter\nclaude.http=200\n"),
            book(),
            Status::Pass,
            "v2.1.30, OpenRouter: the AI key: accepted".into(),
        ),
        (
            claude_lines("claude.auth=bearer\nclaude.base_host=openrouter.ai\nclaude.same_as=openrouter\nclaude.call=openrouter\nclaude.http=401\nclaude.error=User not found.\n"),
            book(),
            Status::Fail,
            "v2.1.30: the AI key rejected (HTTP 401) — `arena pods test --keys apple` says why (OpenRouter)".into(),
        ),
        (
            claude_lines("claude.auth=api_key\nclaude.fingerprint=aa4dca9da125\nclaude.call=models\nclaude.http=401\nclaude.error=invalid x-api-key\n"),
            book(),
            Status::Fail,
            "v2.1.30: ANTHROPIC_API_KEY rejected (HTTP 401) — `arena pods copy-keys apple` (aa4dca9da125)".into(),
        ),
        (
            claude_lines("claude.auth=api_key\nclaude.fingerprint=aa4dca9da125\nclaude.call=models\nclaude.http=200\n"),
            book(),
            Status::Pass,
            "v2.1.30, ANTHROPIC_API_KEY aa4dca9da125: accepted".into(),
        ),
        (
            claude_lines("claude.auth=api_key\nclaude.fingerprint=aa4dca9da125\nclaude.call=models\nclaude.http=0\nclaude.error=timed out\n"),
            book(),
            Status::Warn,
            "v2.1.30: ANTHROPIC_API_KEY not checked (timed out) — `arena pods test --course apple` (aa4dca9da125)".into(),
        ),
        // `--claude-call`: one Messages call on the account.
        (oauth("claude.call=messages\nclaude.http=200\n"), pushed(&fp), Status::Pass, "v2.1.30, arena claude's a@x: accepted".into()),
        (
            oauth("claude.call=messages\nclaude.http=401\nclaude.error=OAuth access token is invalid.\n"),
            pushed(&fp),
            Status::Fail,
            format!("v2.1.30: its token rejected (HTTP 401) — renew it, then {push} (arena claude's a@x)"),
        ),
        (
            oauth("claude.call=messages\nclaude.http=429\n"),
            pushed(&fp),
            Status::Warn,
            "v2.1.30: its token at its limit (HTTP 429) — wait, or `arena claude assign apple N` (arena claude's a@x)".into(),
        ),
        (
            oauth("claude.call=messages\nclaude.http=400\nclaude.error=This credential is only authorized for use with Claude Code\n"),
            pushed(&fp),
            Status::Pass,
            "v2.1.30, arena claude's a@x: accepted (for Claude Code only)".into(),
        ),
        (claude_lines("claude.auth=login\nclaude.call=none\n"), book(), Status::Pass, "v2.1.30, a /login".into()),
        // `--claude-call` for an auth it can't ask about: said, never a quiet pass.
        (
            claude_lines("claude.auth=login\nclaude.call=skipped\n"),
            book(),
            Status::Warn,
            "v2.1.30, a /login: not asked (--claude-call can't for it) — there, `claude -p hi` (uses the account)".into(),
        ),
        // "Claude Code" in a refusal is a refusal: only the subscription token's own answer passes.
        (
            claude_lines("claude.auth=api_key\nclaude.fingerprint=aa4dca9da125\nclaude.call=messages\nclaude.http=403\nclaude.error=Your organization is not permitted to use Claude Code\n"),
            book(),
            Status::Fail,
            "v2.1.30: ANTHROPIC_API_KEY rejected (HTTP 403) — `arena pods copy-keys apple` (aa4dca9da125)".into(),
        ),
        (
            oauth("claude.call=messages\nclaude.http=403\nclaude.error=Your organization is not permitted to use Claude Code\n"),
            pushed(&fp),
            Status::Fail,
            format!("v2.1.30: its token rejected (HTTP 403) — renew it, then {push} (arena claude's a@x)"),
        ),
        (
            claude_lines("claude.auth=bearer\nclaude.base_host=gw.example\nclaude.fingerprint=aa4dca9da125\nclaude.call=models\nclaude.http=302\nclaude.error=redirects to evil.example (not followed)\n"),
            book(),
            Status::Warn,
            "v2.1.30: ANTHROPIC_AUTH_TOKEN answered HTTP 302 — `arena pods test --course apple` (gw.example, aa4dca9da125: redirects to evil.example (not followed))".into(),
        ),
        (
            claude_lines("claude.auth_error=no python3 to read Claude Code's settings\n"),
            book(),
            Status::Warn,
            "v2.1.30; its auth couldn't be read: no python3 to read Claude C… — `arena pods setup apple`".into(),
        ),
    ];
    for (f, b, status, detail) in cases {
        assert_eq!(verdict(&f, &b, CLAUDE), (status, detail.clone()), "{detail}");
    }
}

#[test]
fn kernel_verdicts() {
    let k = |key: &str, new: &str| facts(&with(key, new));
    let register = "`arena pods run apple '/opt/arena-env/bin/python -m ipykernel install --user'`";
    let cases: Vec<(DeepFacts, Status, String)> = vec![
        (facts(""), Status::Skip, NOT_REPORTED.into()),
        (k("kernel.env", "kernel.env=none"), Status::Fail, "no course env (none of the agent notes' places) — `arena pods setup apple`".into()),
        (
            k("kernel.ipykernel", "kernel.ipykernel=error: ModuleNotFoundError: No module named 'ipykernel'"),
            Status::Warn,
            "`arena pods run apple 'uv pip install --python /opt/arena-env/bin/python ipykernel'` (no ipykernel in /opt/arena-env: notebooks can't use it)".into(),
        ),
        (k("kernel.specs", "kernel.specs=none"), Status::Warn, format!("{register} (no Jupyter kernel runs /opt/arena-env)")),
        (
            k("kernel.start_secs", "kernel.start=timed out after 20s"),
            Status::Warn,
            "the arena-env kernel didn't start: timed out after 20s — `arena pods test --course apple`".into(),
        ),
        (
            k("kernel.torch", "kernel.torch=error: ModuleNotFoundError: No module named 'torch'"),
            Status::Fail,
            "can't import torch in arena-env: No module named 'torch' — `arena pods setup apple`".into(),
        ),
        (
            k("kernel.torch", "kernel.torch=timed out after 25s"),
            Status::Warn,
            "the arena-env kernel: import torch timed out after 25s — `arena pods test --course apple`".into(),
        ),
        (
            k("kernel.registered", "kernel.registered=none"),
            Status::Warn,
            format!("{register} (works, torch 2.9.0+cu130, but no kernel is registered for /opt/arena-env)"),
        ),
        (
            facts(&with("kernel.ipykernel", "").replace("kernel.specs=arena-env,python3\n", "")),
            Status::Warn,
            "not checked: the kernel check didn't report — `arena pods test --course apple`".into(),
        ),
        // A spec that runs another env's python (two venvs on one interpreter): its torch isn't the
        // course env's.
        (
            k("kernel.done", "kernel.prefix=/root/other-venv"),
            Status::Warn,
            format!("{register} (the arena-env kernel runs /root/other-venv, not /opt/arena-env)"),
        ),
        (
            k("kernel.specs", "kernel.specs=none\nkernel.own_args=arena-env"),
            Status::Warn,
            "not started: the arena-env kernel has its own arguments (its kernel.json) — `arena pods ask apple`".into(),
        ),
        (facts(&good_lines()), Status::Pass, "arena-env: torch 2.9.0+cu130 in 4.3s".into()),
    ];
    for (f, status, detail) in cases {
        assert_eq!(verdict(&f, &book(), KERNEL), (status, detail.clone()), "{detail}");
    }
    // No uv: the env's pip.
    let f = facts(&with("kernel.ipykernel", "kernel.ipykernel=error: ModuleNotFoundError: No module named 'ipykernel'").replace("uv.path=/usr/local/bin/uv", "uv.path=missing"));
    assert!(verdict(&f, &book(), KERNEL).1.starts_with("`arena pods run apple '/opt/arena-env/bin/python -m pip install ipykernel'`"));
}

#[test]
fn uv_verdicts() {
    assert_eq!(
        verdict(&facts(&with("uv.path", "uv.path=missing")), &book(), UV),
        (Status::Warn, "not on PATH in a login shell — `arena pods setup apple --notes-only`".into())
    );
    assert_eq!(verdict(&facts(&with("uv.version", "")), &book(), UV), (Status::Pass, "/usr/local/bin/uv".into()));
}

/// Whether `cell` keeps everything up to its last command (its closing backtick) within the
/// NOTES clip (109 characters and `…`).
fn command_fits(cell: &str) -> bool {
    let upto = cell.rfind('`').map_or(cell.chars().count(), |i| cell[..=i].chars().count());
    upto <= 109
}

/// Every course verdict's command fits the table's NOTES cell (clipped at 110 characters, the
/// check's name included), for a long machine name too.
#[test]
fn every_verdict_fits_the_notes_cell() {
    let long = "devtest-lightning";
    let mut lines: Vec<String> = Vec::new();
    for (k, v) in [
        ("repo.state", "repo.state=missing"),
        ("repo.op", "repo.op=cherry-pick unmerged"),
        ("repo.op", "repo.op=sequence"),
        ("repo.branch", "repo.branch=main"),
        ("repo.branch", "repo.detached=0d515fc"),
        ("repo.branch", "repo.branch=autocommit-devtest-w4d1-lightning-and-a-long-tail-on-it"),
        ("push.key", "push.key=missing"),
        ("push.exit", "push.exit=128\npush.error=git@github.com: Permission denied (publickey)."),
        ("push.exit", "push.exit=124\npush.error=ssh: connect to host github.com port 22: Connection timed out"),
        ("claude.path", "claude.path=missing"),
        ("claude.version", "claude.version_error=Error: Cannot find module '/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js'"),
        ("claude.auth", "claude.auth=api_key\nclaude.fingerprint=aa4dca9da125\nclaude.call=models\nclaude.http=0\nclaude.error=timed out"),
        ("kernel.env", "kernel.env=none"),
        ("kernel.ipykernel", "kernel.ipykernel=error: ModuleNotFoundError: No module named 'ipykernel'"),
        ("kernel.specs", "kernel.specs=none"),
        ("kernel.start_secs", "kernel.start=timed out after 20s"),
        ("kernel.torch", "kernel.torch=error: ImportError: libcudnn.so.9: cannot open shared object file"),
        ("kernel.torch", "kernel.torch=timed out after 25s"),
        ("kernel.registered", "kernel.registered=none"),
        ("uv.path", "uv.path=missing"),
        ("push.key", "push.key=none"),
        ("repo.state", "repo.state=error: fatal: not a git repository (or any of the parent directories): .git"),
        ("claude.auth", "claude.auth_error=no python3 to read Claude Code's settings"),
        ("kernel.specs", "kernel.specs=error: PermissionError: [Errno 13] Permission denied: '/usr/local/share/jupyter/kernels'"),
        ("kernel.done", "kernel.prefix=/root/somewhere/else/entirely/.venv"),
        ("claude.auth", "claude.auth=oauth\nclaude.source=shell\nclaude.call=messages\nclaude.http=429"),
        ("claude.auth", "claude.auth=api_key\nclaude.call=messages\nclaude.http=429"),
        ("claude.auth", "claude.auth=bearer\nclaude.base_host=gateway.example\nclaude.call=messages\nclaude.http=502\nclaude.error=Bad gateway"),
        ("claude.auth", "claude.auth=none"),
    ] {
        lines.push(with(k, v).replace("/opt/arena-env", "/root/ARENA_materials/.venv"));
    }
    lines.push(with("repo.origin_keys", "repo.origin_keys=github.com/someone/else").replace("repo.origin=github.com:nickypro/arena-sandbox-materials.git", "repo.origin=https://github.com/someone-with-a-long-name/a-fork-of-the-course-materials.git"));
    let mut books = vec![book()];
    for (state, reached) in [
        (ProxyState::Stale, Some(Err("connection refused".to_string()))),
        (ProxyState::Live, Some(Err("ssh: connect to host proxy.example.org port 9532: No route to host".to_string()))),
        (ProxyState::Live, Some(Ok(id("devtest-lightning-old-and-long", "ffffffffffff")))),
        (ProxyState::None, None),
    ] {
        let ports = [(format!("runpod:id-{long}"), Port { port: (state != ProxyState::None).then_some(65000), state, reached })].into_iter().collect();
        books.push(CourseBook { proxy: Some(ProxyBook { write_only: false, ports }), ..book() });
    }
    books.push(CourseBook {
        backups: Some(Ok([(long.to_string(), cell(BackupHealth::Fail, "failed", Some(60), Some("BIGSTUCK 120000000 checkpoints/model.safetensors")))].into_iter().collect())),
        ..book()
    });
    books.push(CourseBook {
        backups: Some(Ok([(long.to_string(), cell(BackupHealth::Warn, "pushed", Some(9000), Some("not backed up for 2 h 30 min (due every 15 min)")))].into_iter().collect())),
        ..book()
    });
    books.push(CourseBook { backups: Some(Err("/home/dev/.local/state/arena/devtest/backups.json: Permission denied".into())), ..book() });
    let write_only = [(format!("runpod:id-{long}"), Port { port: Some(65000), state: ProxyState::Live, reached: Some(Err("connection refused".into())) })];
    books.push(CourseBook { proxy: Some(ProxyBook { write_only: true, ports: write_only.into_iter().collect() }), ..book() });
    let pushed = [(format!("runpod:id-{long}"), (Some("someone.with.a.long.address@arena.education".to_string()), Some("000000000000".to_string())))];
    books.push(CourseBook { claude: ClaudeBook { pushed: pushed.into_iter().collect(), pool: Default::default() }, ..book() });
    let mut bad = std::collections::BTreeSet::new();
    for l in &lines {
        for b in &books {
            for c in course_checks(&facts(l), &pod(long), b) {
                let cell = format!("{}: {}", c.name, c.detail);
                if !command_fits(&cell) {
                    bad.insert(format!("{} chars: {cell}", cell.chars().count()));
                }
                // Every FAIL and WARN says what to run (or do): after its dash, or first.
                if matches!(c.status, Status::Fail | Status::Warn) && !(c.detail.contains(" — ") || c.detail.starts_with('`')) {
                    bad.insert(format!("no remedy: {cell}"));
                }
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.into_iter().collect::<Vec<_>>().join("\n"));
}

/// A course FAIL is the course's, never the machine's: `Course` (the kernel's `Software`), the
/// machine's own verdict ignores it, and the summary says so.
#[test]
fn a_course_failure_is_not_the_machines() {
    for name in [PROXY, BACKUP, REPO, CLAUDE, UV] {
        assert_eq!(failure_kind_of(name, None), FailureKind::Course, "{name}");
    }
    assert_eq!(failure_kind_of(KERNEL, None), FailureKind::Software);
    let f = facts(&format!("torch=ok\n{}", with("repo.state", "repo.state=missing")));
    let policy = HealthPolicy { course: Some(book()), ..HealthPolicy::default() };
    let h = PodHealth::course_checked(&pod("devtest-apple"), f.clone(), &policy);
    assert_eq!((h.status, h.failure_kind(), h.machine_status(), h.course_status()), (Status::Fail, Some(FailureKind::Course), Status::Pass, Some(Status::Fail)));
    assert!(render_summary(std::slice::from_ref(&h)).iter().any(|l| l.starts_with("course setup failure, not the machine: devtest-apple")), "{:?}", render_summary(&[h]));
    // In a deep check the machine's checks decide what it is; the course's sit beside them.
    let deep = PodHealth::checked(&pod("devtest-apple"), f, &policy);
    assert!(deep.checks.iter().any(|c| c.name == REPO && c.status == Status::Fail));
    assert_eq!(deep.machine_status(), crate::health::overall(&deep.checks.iter().filter(|c| !is_course_check(&c.name)).cloned().collect::<Vec<_>>()));
    // The health cache keeps every course check, and its public issue is `course setup`.
    let rec = crate::snapshot::HealthRecord::from_health(&deep, NOW);
    assert_eq!(rec.course.len(), 6);
    assert!(rec.issues.contains(&crate::snapshot::Issue::CourseSetup), "{:?}", rec.issues);
    // Without a course book: no course lines at all.
    let none = PodHealth::checked(&pod("devtest-apple"), facts(&good_lines()), &HealthPolicy::default());
    assert!(none.checks.iter().all(|c| !is_course_check(&c.name)));
}

#[test]
fn the_course_report_has_a_cell_each() {
    let book = proxied(ProxyState::Live, Some(9500), Some(Ok(id("devtest-apple", "0123456789ab"))), false);
    let policy = HealthPolicy { course: Some(book), ..HealthPolicy::default() };
    let good = PodHealth::course_checked(&pod("devtest-apple"), facts(&good_lines()), &policy);
    let bad = PodHealth::course_checked(&pod("devtest-bloom"), facts(&with("repo.branch", "repo.branch=main").replace("kernel.torch=ok", "kernel.torch=error: ModuleNotFoundError: x")), &policy);
    let out = render_course_report(&[good, bad], false);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "NAME           RESULT  PROXY  BACKUP  REPO   CLAUDE  KERNEL    NOTES", "{out}");
    assert_eq!(lines[1], "devtest-apple  pass    :9500  ok      today  none    4.3s", "{out}");
    assert!(lines[2].starts_with("devtest-bloom  fail    -      ok      main   none    no torch  kernel: can't import torch in arena-env"), "{out}");
    assert!(out.contains("software/setup failure, not the host: devtest-bloom"), "{out}");
}

#[test]
fn the_identity_command_reads_back_through_the_parser() {
    assert!(!IDENTITY_SH.contains('\''), "it goes inside single quotes");
    if !std::path::Path::new("/bin/sh").exists() {
        return;
    }
    let home = std::env::temp_dir().join(format!("arena-course-id-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join(".name"), "devtest-apple\r\n").unwrap();
    let out = std::process::Command::new("sh").arg("-c").arg(identity_command()).env("HOME", &home).output().unwrap();
    let _ = std::fs::remove_dir_all(&home);
    let f = parse_deep(&String::from_utf8_lossy(&out.stdout));
    assert!(f.started && f.complete, "{out:?}");
    let id = f.identity.unwrap();
    assert_eq!(id.name.as_deref(), Some("devtest-apple"));
    assert!(id.hostname.is_some());
}

// ---- the script's course mode, run for real in a scratch $HOME (no network) ----

fn have(tool: &str) -> bool {
    std::process::Command::new("sh").args(["-c", &format!("command -v {tool}")]).output().is_ok_and(|o| o.status.success())
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(["-c", "user.email=t@t", "-c", "user.name=t", "-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(ok.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&ok.stderr));
}

/// Every file under `dir` with its size and mtime: what a read-only run must leave as it was.
fn listing(dir: &std::path::Path) -> Vec<(std::path::PathBuf, u64, std::time::SystemTime)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let m = e.metadata().unwrap();
            if m.is_dir() {
                stack.push(e.path());
            }
            out.push((e.path(), m.len(), m.modified().unwrap()));
        }
    }
    out.sort();
    out
}

/// The course mode end to end on this box: a repo on today's branch with a local bare origin
/// (the dry-run push succeeds, nothing is sent), a rebase marker, a stub `claude` and `uv`, an
/// API key answered by a stub service, an env without ipykernel — every fact reported, no secret
/// printed, nothing under `$HOME` changed and no temp dir left.
#[cfg(target_os = "linux")]
#[test]
fn the_course_mode_reports_and_leaves_everything_as_it_was() {
    if !(have("bash") && have("git") && have("python3") && have("base64") && have("timeout") && have("sha256sum")) {
        eprintln!("bash/git/python3/base64/timeout/sha256sum missing — skipping");
        return;
    }
    const KEY: &str = "sk-ant-api03-COURSE-TEST-SECRET-0123456789";
    let root = std::env::temp_dir().join(format!("arena-course-run-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (home, bin, env) = (root.join("home"), root.join("bin"), root.join("env"));
    for d in [&home, &bin, &env.join("bin")] {
        std::fs::create_dir_all(d).unwrap();
    }
    let origin = root.join("origin.git");
    git(&root, &["init", "-q", "--bare", origin.to_str().unwrap()]);
    let repo = home.join("ARENA_materials");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("notes.md"), "the participant's work\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "work"]);
    git(&repo, &["checkout", "-q", "-b", TODAY]);
    git(&repo, &["remote", "add", "origin", origin.to_str().unwrap()]);
    std::fs::write(home.join(".name"), "devtest-apple\n").unwrap();
    crate::remote::write_test_script(bin.join("claude"), "#!/bin/sh\necho '2.1.30 (Claude Code)'\n").unwrap();
    crate::remote::write_test_script(bin.join("uv"), "#!/bin/sh\necho 'uv 0.9.5'\n").unwrap();
    // ssh as git runs it for the push: its arguments logged, GitHub's refusal for the key said.
    let ssh_log = root.join("ssh.log");
    crate::remote::write_test_script(
        bin.join("ssh"),
        format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\necho 'git@github.com: Permission denied (publickey).' >&2\nexit 255\n", ssh_log.display()),
    )
    .unwrap();
    // The kernel helper's errors scrubbed before they're clipped: a fake ipykernel whose import
    // error echoes a secret the environment holds, across the 300-character clip.
    let fakes = root.join("pylib");
    std::fs::create_dir_all(fakes.join("ipykernel")).unwrap();
    std::fs::write(fakes.join("ipykernel/__init__.py"), format!("raise ImportError('{} ' + __import__('os').environ['COURSE_TEST_API_KEY'])\n", "y".repeat(250))).unwrap();
    // The course env: a python with no ipykernel.
    std::os::unix::fs::symlink(String::from_utf8(std::process::Command::new("sh").args(["-c", "command -v python3"]).output().unwrap().stdout).unwrap().trim(), env.join("bin/python")).unwrap();
    // The Anthropic models list, answering this key 401.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            let mut buf = [0u8; 4096];
            let n = conn.read(&mut buf).unwrap_or(0);
            log.lock().unwrap().push(String::from_utf8_lossy(&buf[..n]).into());
            let body = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
            let _ = write!(conn, "HTTP/1.1 401 X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        }
    });
    // A participant's git config that rewrites GitHub's SSH URL into one carrying a credential:
    // the push never reads it (its own empty repo, no global or system config).
    std::fs::write(home.join(".gitconfig"), "[url \"https://ghp_COURSE0TEST0PAT@127.0.0.1:9/\"]\n\tinsteadOf = git@github.com:\n").unwrap();
    git(&repo, &["config", "url.https://ghp_COURSE0TEST0PAT@127.0.0.1:9/.pushInsteadOf", "git@github.com:"]);
    let gitdir = repo.join(".git");
    std::fs::create_dir_all(gitdir.join("rebase-merge")).unwrap();
    let before = listing(&home);
    let origin_before = listing(&origin);
    let tmp_before = std::fs::read_dir("/tmp").map(|d| d.flatten().filter(|e| e.file_name().to_string_lossy().starts_with("arena_deep.")).count()).unwrap_or(0);
    let args = CheckArgs {
        repo: repo.display().to_string(),
        envs: vec!["~/nope".into(), env.display().to_string()],
        git_key: None,
        claude_call: false,
        origin: Some("nickypro/arena-sandbox-materials".into()),
    };
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(crate::health::course_check_inner_command(&args))
        .env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()))
        .env("HOME", &home)
        .env("ANTHROPIC_API_KEY", KEY)
        .env("ARENA_TEST_ANTHROPIC_URL", &url)
        .env("PYTHONPATH", &fakes)
        .env("COURSE_TEST_API_KEY", "course-test-secret-0123456789abcdefghijklmnopqrstuvwxyz")
        .env("no_proxy", "*")
        .env("NO_PROXY", "*")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
        .env_remove("ANTHROPIC_BASE_URL")
        .output()
        .unwrap();
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout).to_string(), String::from_utf8_lossy(&out.stderr).to_string());
    let after = listing(&home);
    let origin_after = listing(&origin);
    let tmp_after = std::fs::read_dir("/tmp").map(|d| d.flatten().filter(|e| e.file_name().to_string_lossy().starts_with("arena_deep.")).count()).unwrap_or(0);
    let requests = seen.lock().unwrap().clone();
    let ssh_calls = std::fs::read_to_string(&ssh_log).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&root);

    assert!(!stdout.contains(KEY) && !stderr.contains(KEY), "the key printed:\n{stdout}\n{stderr}");
    for piece in ["ghp_COURSE0TEST0PAT", "course-test-secret", "ijklmnopqrstuvwxyz"] {
        assert!(!stdout.contains(piece) && !stderr.contains(piece), "{piece} printed:\n{stdout}\n{stderr}");
    }
    // The push went over ssh to GitHub's own URL, the rewrites never read.
    assert!(ssh_calls.contains("git@github.com") && ssh_calls.contains("git-receive-pack 'nickypro/arena-sandbox-materials.git'"), "{ssh_calls:?}");
    for line in stdout.lines() {
        assert!(line.contains('='), "stray stdout line {line:?}");
    }
    assert_eq!(before, after, "the run changed something under $HOME:\n{stdout}");
    assert_eq!(origin_before, origin_after, "the dry run sent something");
    assert!(tmp_after <= tmp_before, "a temp dir left behind");
    let f = parse_deep(&stdout);
    assert!(f.started && f.complete, "{stdout}\n{stderr}");
    assert_eq!(f.identity.as_ref().and_then(|i| i.name.as_deref()), Some("devtest-apple"));
    let r = f.repo.clone().unwrap();
    assert_eq!((r.state.as_deref(), r.branch.as_deref(), r.op.as_deref()), (Some("ok"), Some(TODAY), Some("rebase")), "{stdout}");
    let p = f.push.clone().unwrap();
    assert_eq!((p.key.as_deref(), p.exit), (Some("default"), Some(128)), "{stdout}\n{stderr}");
    assert!(p.error.as_deref().is_some_and(|e| e.contains("Permission denied (publickey)")), "{stdout}");
    let c = f.claude.clone().unwrap();
    assert_eq!(
        (c.version.as_deref(), c.auth.as_deref(), c.fingerprint.as_deref(), c.call.as_deref(), c.http),
        (Some("2.1.30 (Claude Code)"), Some("api_key"), Some(crate::health::fingerprint(KEY).as_str()), Some("models"), Some(401)),
        "{stdout}"
    );
    assert!(requests.iter().any(|r| r.starts_with("GET /v1/models") && r.to_ascii_lowercase().contains(&format!("x-api-key: {}", KEY.to_ascii_lowercase()))), "{requests:?}");
    let k = f.kernel.clone().unwrap();
    assert_eq!(k.env.as_deref(), Some(env.display().to_string().as_str()), "the first env with a python");
    assert!(k.ipykernel.as_deref().is_some_and(|i| i.contains("ImportError") && i.contains("[secret]")), "{stdout}");
    assert_eq!(f.uv.as_ref().and_then(|u| u.version.as_deref()), Some("uv 0.9.5"));
    // Judged: the rebase WARNs, the rejected key FAILs, the kernel WARNs; the machine is fine.
    let h = PodHealth::course_checked(&pod("devtest-apple"), f, &HealthPolicy { course: Some(CourseBook { origin: None, ..book() }), ..HealthPolicy::default() });
    let got: Vec<(&str, Status)> = h.checks.iter().map(|c| (c.name.as_str(), c.status)).collect();
    assert_eq!(
        got,
        [(PROXY, Status::Skip), (BACKUP, Status::Fail), (REPO, Status::Warn), (CLAUDE, Status::Fail), (KERNEL, Status::Warn), (UV, Status::Pass)],
        "{:#?}",
        h.checks
    );
    assert_eq!(h.failure_kind(), Some(FailureKind::Course));
}

/// The course command is the deep script in its course mode, with the repo, the envs, the key
/// and the paid call's switch as its arguments, quoted.
#[test]
fn the_course_command_carries_its_arguments() {
    let args = CheckArgs {
        repo: "/root/ARENA materials".into(),
        envs: vec!["/opt/arena-env".into(), "~/x".into()],
        git_key: Some("/root/.ssh/id".into()),
        claude_call: true,
        origin: Some("nickypro/arena-sandbox-materials".into()),
    };
    let cmd = crate::health::course_check_inner_command(&args);
    assert!(
        cmd.ends_with(" arena-deep-check course '/root/ARENA materials' '/opt/arena-env:~/x' '/root/.ssh/id' 1 'nickypro/arena-sandbox-materials' </dev/null"),
        "{cmd}"
    );
    assert!(crate::health::deep_check_inner_command_for(&args).contains(" arena-deep-check deep '/root/ARENA materials'"));
    let script = crate::health::deep_check_script();
    for mark in ["@ARENA_IDENTITY_SH@", "@ARENA_ORIGIN_SH@", "@ARENA_BUSY_OP_SH@", "@ARENA_CLAUDE_PY@", "@ARENA_KERNEL_PY@"] {
        assert!(!script.contains(mark), "{mark} left in the script");
    }
    let cfg = Config::parse(
        "MACHINE_NAME_PREFIX=devtest\nBACKUP_REPO_PATH=/root/ARENA_materials\nGIT_SSH_KEY_REMOTE=/root/.ssh/id_ed25519\nARENA_REPO_OWNER=o\nARENA_REPO_NAME=n\n",
    );
    let gpu = CheckArgs::for_pod(&cfg, &pod("devtest-apple"));
    assert_eq!(gpu.origin.as_deref(), Some("o/n"));
    assert_eq!((gpu.repo.as_str(), gpu.git_key.as_deref(), gpu.envs.first().map(String::as_str)), ("/root/ARENA_materials", Some("/root/.ssh/id_ed25519"), Some("/opt/arena-env")));
    let vm = CheckArgs::for_pod(&cfg, &Pod { provider: "hetzner".into(), ..pod("devtest-apple") });
    assert_eq!(vm.envs.first().map(String::as_str), Some("/root/ARENA_materials/.venv"), "a bare VM's venv first");
}

/// The Claude helper scrubs a secret out of what a service says before anything is clipped: a
/// gateway that echoes an opaque token (no `sk-` to match) from a settings file, past both clip
/// points, prints none of it — and `--claude-call` asks Messages with an API key too.
#[cfg(target_os = "linux")]
#[test]
fn the_claude_helper_never_prints_an_echoed_secret() {
    if !(have("bash") && have("python3") && have("base64") && have("timeout")) {
        eprintln!("bash/python3/base64/timeout missing — skipping");
        return;
    }
    const OPAQUE: &str = "OPAQUE-gateway-token-0123456789abcdefghijklmnopqrstuvwxyz";
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            let mut buf = [0u8; 8192];
            let n = conn.read(&mut buf).unwrap_or(0);
            log.lock().unwrap().push(String::from_utf8_lossy(&buf[..n]).into());
            // The first echo straddles the helper's 160-character clip, the second the script's 300.
            let pad = "x".repeat(110);
            let body = format!(r#"{{"error":{{"message":"{pad} bad token {OPAQUE} {pad} {OPAQUE}"}}}}"#);
            let _ = write!(conn, "HTTP/1.1 401 X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        }
    });
    let root = std::env::temp_dir().join(format!("arena-claude-scrub-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (home, bin) = (root.join("home"), root.join("bin"));
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    crate::remote::write_test_script(bin.join("claude"), "#!/bin/sh\necho '2.1.30 (Claude Code)'\n").unwrap();
    std::fs::write(home.join(".claude/settings.json"), format!(r#"{{"env":{{"ANTHROPIC_AUTH_TOKEN":"{OPAQUE}","ANTHROPIC_BASE_URL":"{url}"}}}}"#)).unwrap();
    let args = CheckArgs { repo: "/nonexistent".into(), envs: vec![], git_key: None, claude_call: true, origin: None };
    let run = |extra: &[(&str, &str)]| {
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c")
            .arg(crate::health::course_check_inner_command(&args))
            .env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()))
            .env("HOME", &home)
            .env("no_proxy", "*")
            .env("NO_PROXY", "*")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("ANTHROPIC_AUTH_TOKEN")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
            .env_remove("ANTHROPIC_BASE_URL");
        for (k, v) in extra {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr)
    };
    let out = run(&[]);
    for piece in [OPAQUE, &OPAQUE[..20], &OPAQUE[OPAQUE.len() - 20..]] {
        assert!(!out.contains(piece), "{piece} printed:\n{out}");
    }
    let c = parse_deep(&out).claude.unwrap();
    assert_eq!((c.auth.as_deref(), c.source.as_deref(), c.call.as_deref(), c.http), (Some("bearer"), Some("settings"), Some("messages"), Some(401)), "{out}");
    assert!(c.error.as_deref().is_some_and(|e| e.contains("[secret]")), "{out}");
    // An API key with `--claude-call`: Messages, with its x-api-key.
    std::fs::remove_file(home.join(".claude/settings.json")).unwrap();
    let out = run(&[("ANTHROPIC_API_KEY", "sk-ant-api03-PAID-CALL-TEST-0123456789"), ("ARENA_TEST_ANTHROPIC_URL", url.as_str())]);
    let c = parse_deep(&out).claude.unwrap();
    assert_eq!((c.auth.as_deref(), c.call.as_deref()), (Some("api_key"), Some("messages")), "{out}");
    let requests = seen.lock().unwrap().clone();
    let _ = std::fs::remove_dir_all(&root);
    assert!(requests.iter().any(|r| r.starts_with("POST /v1/messages") && r.to_ascii_lowercase().contains("x-api-key: sk-ant-api03-paid-call-test")), "{requests:?}");
    assert!(requests.iter().any(|r| r.starts_with("POST /v1/messages") && r.contains(&format!("Bearer {OPAQUE}"))), "{requests:?}");
}

/// The Claude helper never follows a redirect: it would carry the credential's header wherever
/// it points (another host, plain HTTP). The 3xx is its answer, the second host asked nothing.
#[cfg(target_os = "linux")]
#[test]
fn the_claude_helper_never_follows_a_redirect() {
    if !(have("bash") && have("python3") && have("base64") && have("timeout")) {
        eprintln!("bash/python3/base64/timeout missing — skipping");
        return;
    }
    use std::io::{Read, Write};
    let elsewhere = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let there = format!("http://{}/v1/models", elsewhere.local_addr().unwrap());
    let reached = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = reached.clone();
    std::thread::spawn(move || {
        for conn in elsewhere.incoming() {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            drop(conn);
        }
    });
    let gateway = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", gateway.local_addr().unwrap());
    std::thread::spawn(move || {
        for conn in gateway.incoming() {
            let Ok(mut conn) = conn else { continue };
            let mut buf = [0u8; 4096];
            let _ = conn.read(&mut buf);
            let _ = write!(conn, "HTTP/1.1 302 Found\r\nLocation: {there}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    });
    let home = std::env::temp_dir().join(format!("arena-claude-redirect-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let args = CheckArgs { repo: "/nonexistent".into(), envs: vec![], git_key: None, claude_call: false, origin: None };
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(crate::health::course_check_inner_command(&args))
        .env("HOME", &home)
        .env("no_proxy", "*")
        .env("NO_PROXY", "*")
        .env("ANTHROPIC_API_KEY", "sk-ant-api03-REDIRECT-TEST-0123456789")
        .env("ARENA_TEST_ANTHROPIC_URL", &url)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
        .env_remove("ANTHROPIC_BASE_URL")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&home);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let c = parse_deep(&stdout).claude.unwrap();
    assert_eq!((c.call.as_deref(), c.http), (Some("models"), Some(302)), "{stdout}");
    assert!(c.error.as_deref().is_some_and(|e| e.contains("not followed")), "{stdout}");
    assert_eq!(reached.load(std::sync::atomic::Ordering::SeqCst), 0, "the redirect was followed");
}
