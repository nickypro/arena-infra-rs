//! `arena claude` end to end over a listed fleet and a scripted remote: who moves where, which pods
//! get a push (and only their own account's token: one copy each, never a token in a command),
//! `off`, the default push; copy-keys' Claude Code stage; and `config check`'s wording.

use super::*;
use arena_core::claude::{fingerprint, Account};
use arena_core::remote::{FakeRemote, RemoteCall};
use arena_core::PodSpec;
use clap::Parser;

/// Fake tokens: never a real one in a test.
fn tok(n: u32) -> String {
    format!("sk-ant-oat01-FAKE-{n:04}-xxxxxxxxxxxxxxxxxxxx")
}

struct TmpDir(std::path::PathBuf);

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tmp(tag: &str) -> TmpDir {
    let d = std::env::temp_dir().join(format!("arena-claude-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    TmpDir(d)
}

/// A tokens file of accounts 1..=n (owner-only) and a state dir, in `d`; `extra` config lines.
fn config_in(d: &TmpDir, n: u32, extra: &str) -> Config {
    use std::os::unix::fs::PermissionsExt;
    let file = d.0.join("tokens");
    let text: String = (1..=n).map(|i| format!("u{i}@x -> {}\n", tok(i))).collect();
    std::fs::write(&file, text).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    Config::parse(&format!(
        "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=( \"apple\" \"bloom\" \"cloud\" \"dune\" )\nARENA_STATE_DIR={}\nCLAUDE_TOKENS_FILE={}\n{extra}",
        d.0.join("state").display(),
        file.display()
    ))
}

fn pod(n: u16, name: &str, status: &str) -> Pod {
    Pod {
        provider: "hetzner".into(),
        id: format!("{n}"),
        name: format!("devtest-{name}"),
        status: status.into(),
        ssh_ip: Some("10.0.0.1".into()),
        ssh_port: Some(22000 + n),
        ..Pod::default()
    }
}

fn host(n: u16) -> String {
    format!("10.0.0.1:{}", 22000 + n)
}

/// A provider listing a fixed fleet.
struct Listed(Vec<Pod>);

#[async_trait::async_trait]
impl Provider for Listed {
    fn name(&self) -> &'static str {
        "hetzner"
    }
    fn describe(&self, _spec: &PodSpec) -> String {
        String::new()
    }
    async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
        Ok(self.0.clone())
    }
    async fn create_pod(&self, _spec: &PodSpec) -> arena_core::Result<Pod> {
        unimplemented!("not exercised")
    }
    async fn stop_pod(&self, _id: &str) -> arena_core::Result<()> {
        Ok(())
    }
    async fn restart_pod(&self, _id: &str) -> arena_core::Result<()> {
        Ok(())
    }
    async fn terminate_pod(&self, _id: &str) -> arena_core::Result<()> {
        Ok(())
    }
}

/// `arena claude <args> --yes` against `fleet`.
async fn run(args: &[&str], fleet: &Listed, fake: &Arc<FakeRemote>, cfg: &Config) -> Result<()> {
    let cli = crate::Cli::try_parse_from(["arena", "claude"].iter().chain(args).chain(&["--yes"])).unwrap_or_else(|e| panic!("{args:?}: {e}"));
    let crate::cli::Cmd::Claude(c) = cli.cmd else { panic!("not claude") };
    handle_claude(c, fleet, fake.clone(), cfg, true).await
}

/// Per pod: how many token files were copied to it, and the commands run there.
fn sent(fake: &FakeRemote, n: u16) -> (usize, Vec<String>) {
    let calls = fake.calls_to(&host(n));
    let copies = calls.iter().filter(|c| matches!(c, RemoteCall::Copy { remote, .. } if remote.starts_with(".arena-claude-token-"))).count();
    let cmds = calls.into_iter().filter_map(|c| if let RemoteCall::Exec { cmd, .. } = c { Some(cmd) } else { None }).collect();
    (copies, cmds)
}

fn no_token_in_any_command(fake: &FakeRemote) {
    for c in fake.calls() {
        if let RemoteCall::Exec { cmd, .. } = c {
            assert!(!cmd.contains("sk-ant"), "a token in a command: {cmd}");
        }
    }
}

fn state(cfg: &Config) -> State {
    claude::load(&claude::state_path(cfg).unwrap()).unwrap()
}

/// What was last pushed to a pod: the account (none: taken off) and its token's fingerprint.
type PushedTo = Option<(Option<String>, Option<String>)>;

/// The account a pod is on, and what was last pushed to it.
fn on(cfg: &Config, p: &Pod) -> (Option<String>, PushedTo) {
    let s = state(cfg);
    let a = s.of(p).cloned();
    (a.as_ref().and_then(|a| a.account.clone()), a.and_then(|a| a.pushed).map(|p| (p.account, p.fp)))
}

#[tokio::test]
async fn push_spreads_assign_moves_off_removes_and_each_pod_gets_only_its_own_token() {
    let d = tmp("e2e");
    let cfg = config_in(&d, 2, "");
    let fleet = Listed(vec![pod(1, "apple", "running"), pod(2, "bloom", "running"), pod(3, "cloud", "running")]);
    let fp = |n| Some(fingerprint(&tok(n)));

    // The default push: every cohort pod is due (none has an account yet) — spread, pushed.
    let fake = Arc::new(FakeRemote::new());
    run(&["push"], &fleet, &fake, &cfg).await.unwrap();
    for (n, acct) in [(1, 1), (2, 2), (3, 1)] {
        assert_eq!(sent(&fake, n).0, 1, "one token file to pod {n}");
        assert_eq!(on(&cfg, &fleet.0[n as usize - 1]), (Some(format!("u{acct}@x")), Some((Some(format!("u{acct}@x")), fp(acct)))));
    }
    no_token_in_any_command(&fake);

    // Again: nothing is due, nothing is sent.
    let fake = Arc::new(FakeRemote::new());
    run(&["push"], &fleet, &fake, &cfg).await.unwrap();
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    // Named: pushed again whatever their state (idempotent on the box).
    run(&["push", "apple"], &fleet, &fake, &cfg).await.unwrap();
    assert_eq!((sent(&fake, 1).0, sent(&fake, 2).0), (1, 0));

    // Move apple to #2: only apple gets a push — #2's token, replacing #1's there.
    let fake = Arc::new(FakeRemote::new());
    run(&["assign", "apple", "u2@x"], &fleet, &fake, &cfg).await.unwrap();
    assert_eq!((sent(&fake, 1).0, sent(&fake, 2).0, sent(&fake, 3).0), (1, 0, 0));
    assert_eq!(on(&cfg, &fleet.0[0]), (Some("u2@x".into()), Some((Some("u2@x".into()), fp(2)))));
    assert_eq!(state(&cfg).of(&fleet.0[0]).unwrap().how, claude::How::Pinned);
    no_token_in_any_command(&fake);
    // Already there: nothing to do.
    let fake = Arc::new(FakeRemote::new());
    run(&["assign", "apple", "2"], &fleet, &fake, &cfg).await.unwrap();
    assert!(fake.calls().is_empty());

    // Off: no file, just the command that takes the lines out; recorded as none.
    let fake = Arc::new(FakeRemote::new());
    run(&["off", "bloom"], &fleet, &fake, &cfg).await.unwrap();
    let (copies, cmds) = sent(&fake, 2);
    assert_eq!(copies, 0);
    assert!(matches!(&cmds[..], [c] if c.contains("ARENA_CC=;")), "{cmds:?}");
    assert_eq!(on(&cfg, &fleet.0[1]), (None, Some((None, None))));
    // A default push leaves it off; spread puts it back on one.
    let fake = Arc::new(FakeRemote::new());
    run(&["push"], &fleet, &fake, &cfg).await.unwrap();
    assert!(fake.calls().is_empty());
    run(&["assign", "bloom", "--spread"], &fleet, &fake, &cfg).await.unwrap();
    assert_eq!(on(&cfg, &fleet.0[1]).0, Some("u1@x".into()), "#1 had cloud only, #2 apple");

    // A rotated token (same email, new token): the default push finds exactly those pods.
    let file = d.0.join("tokens");
    std::fs::write(&file, format!("u1@x -> {}\nu2@x -> {}\n", tok(1), tok(22))).unwrap();
    let fake = Arc::new(FakeRemote::new());
    run(&["push"], &fleet, &fake, &cfg).await.unwrap();
    assert_eq!((sent(&fake, 1).0, sent(&fake, 2).0, sent(&fake, 3).0), (1, 0, 0), "only apple is on u2@x");
    assert_eq!(on(&cfg, &fleet.0[0]).1, Some((Some("u2@x".into()), fp(22))));

    // u2@x leaves the file: apple is spread again at the next push.
    std::fs::write(&file, format!("u1@x -> {}\n", tok(1))).unwrap();
    let fake = Arc::new(FakeRemote::new());
    run(&["push"], &fleet, &fake, &cfg).await.unwrap();
    assert_eq!(on(&cfg, &fleet.0[0]), (Some("u1@x".into()), Some((Some("u1@x".into()), fp(1)))));
}

#[tokio::test]
async fn assign_refuses_what_it_cant_mean_and_a_stopped_pod_waits_for_its_setup() {
    let d = tmp("refuse");
    let cfg = config_in(&d, 2, "");
    let fleet = Listed(vec![pod(1, "apple", "running"), pod(2, "bloom", "EXITED")]);
    let fake = Arc::new(FakeRemote::new());
    for (args, says) in [
        (&["assign", "apple"][..], "name the pods to put on apple"),
        (&["assign"][..], "name the pods and the account"),
        (&["assign", "apple", "7"][..], "no account 7 in the pool — there are: #1 u1@x, #2 u2@x, or `none`"),
        (&["assign", "--spread"][..], "name the pods to assign"),
        (&["off"][..], "name the pods to take the Claude Code token off"),
        (&["assign", "--owner", "alice", "1", "2"][..], "--owner alice takes one account"),
    ] {
        let e = run(args, &fleet, &fake, &cfg).await.unwrap_err().to_string();
        assert!(e.contains(says), "{args:?}: {e}");
    }
    assert!(fake.calls().is_empty());
    // A stopped pod: assigned, and said; its token goes on with its next setup.
    run(&["assign", "bloom", "2"], &fleet, &fake, &cfg).await.unwrap();
    assert!(fake.calls().is_empty());
    assert_eq!(on(&cfg, &fleet.0[1]), (Some("u2@x".into()), None));
    // No pool: push refuses, saying how to make one; off still works.
    let none = Config::parse(&format!("MACHINE_NAME_PREFIX=devtest\nARENA_STATE_DIR={}\n", d.0.join("state").display()));
    let e = run(&["push"], &fleet, &fake, &none).await.unwrap_err().to_string();
    assert!(e.starts_with("no Claude Code accounts: set CLAUDE_TOKENS_FILE"), "{e}");
    run(&["off", "apple"], &fleet, &fake, &none).await.unwrap();
    assert!(matches!(&sent(&fake, 1).1[..], [c] if c.contains("ARENA_CC=;")));
}

#[tokio::test]
async fn copy_keys_puts_each_pods_own_token_on_by_scp_never_in_its_command() {
    let d = tmp("copykeys");
    let cfg = config_in(&d, 2, "");
    let pods = vec![pod(1, "apple", "running"), pod(2, "bloom", "running")];
    let sel = Selected { pods: pods.clone(), named: false };
    let fake = Arc::new(FakeRemote::new());
    let no_csv = d.0.join("no-keys").display().to_string();
    crate::handle_copy_keys(fake.clone(), &cfg, &no_csv, None, None, &sel, false, true).await.unwrap();
    for (n, acct) in [(1, "u1@x"), (2, "u2@x")] {
        let (copies, cmds) = sent(&fake, n);
        assert_eq!(copies, 1, "pod {n}");
        // The keys write first (no Claude line in it any more), then the push.
        assert!(!cmds[0].contains("CLAUDE_CODE_OAUTH_TOKEN"), "{}", cmds[0]);
        assert!(cmds.last().unwrap().contains("ENVIRON[\"ARENA_CC\"]"));
        assert_eq!(on(&cfg, &pods[n as usize - 1]).0.as_deref(), Some(acct));
    }
    no_token_in_any_command(&fake);

    // --cc-token: that one token to every pod, nobody moved.
    let fake = Arc::new(FakeRemote::new());
    crate::handle_copy_keys(fake.clone(), &cfg, &no_csv, None, Some(tok(9)), &sel, false, true).await.unwrap();
    assert_eq!((sent(&fake, 1).0, sent(&fake, 2).0), (1, 1));
    assert_eq!(on(&cfg, &pods[1]).1, Some((Some(claude::OVERRIDE_ACCOUNT.into()), Some(fingerprint(&tok(9))))));
    assert_eq!(on(&cfg, &pods[1]).0.as_deref(), Some("u2@x"));
    no_token_in_any_command(&fake);

    // A failed push fails copy-keys, after the rest is written.
    let fake = Arc::new(FakeRemote::new());
    fake.script(&host(2), [arena_core::remote::FakeReply::ok(), arena_core::remote::FakeReply::exit(1, "scp: lost connection")]);
    let e = crate::handle_copy_keys(fake.clone(), &cfg, &no_csv, None, None, &sel, false, true).await.unwrap_err().to_string();
    assert_eq!(e, "1 pod(s) didn't get their Claude Code token (`arena claude push` retries)");
}

#[test]
fn config_check_counts_the_accounts_and_never_shows_a_token() {
    use std::os::unix::fs::PermissionsExt;
    let d = tmp("check");
    let rows = |cfg: &Config| {
        let mut missing = Vec::new();
        let r = config_check_rows(cfg, &mut missing);
        (r.join("\n"), missing)
    };
    let file = d.0.join("tokens");
    let cfg = config_in(&d, 3, &format!("CLAUDE_CODE_OAUTH_TOKEN={}\n", tok(9)));
    std::fs::write(&file, format!("u1@x -> {}\nu2@x -> {}\nu3@x -> {}\nbroken line\n", tok(1), tok(2), tok(3))).unwrap();
    let (text, missing) = rows(&cfg);
    assert_eq!(
        text,
        format!(
            "  ✓ CLAUDE_TOKENS_FILE           3 accounts in {} (one per machine; tokens never shown)\n      └ ⚠ line 4 (`broken …`) isn't `email -> token` or `N email -> token` — skipped\n  · CLAUDE_CODE_OAUTH_TOKEN      (set — unused: CLAUDE_TOKENS_FILE's accounts replace it)",
            file.display()
        )
    );
    assert!(missing.is_empty());

    // Readable by others: ✗, and the check fails on it.
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
    let (text, missing) = rows(&cfg);
    assert!(
        text.starts_with(&format!(
            "  ✗ CLAUDE_TOKENS_FILE           {} is mode 0640: other users of this box can get at its tokens — chmod 600 {}",
            file.display(),
            file.display()
        )),
        "{text}"
    );
    assert_eq!(missing.len(), 1);
    assert!(missing[0].starts_with("CLAUDE_TOKENS_FILE (") && missing[0].contains("chmod 600"), "{missing:?}");

    // No file: config's token is a pool of one; neither: just say so.
    let (text, _) = rows(&Config::parse(&format!("CLAUDE_CODE_OAUTH_TOKEN={}\n", tok(9))));
    assert_eq!(
        text,
        "  · CLAUDE_TOKENS_FILE           (missing — optional: a tokens file puts each machine on its own account)\n  ✓ CLAUDE_CODE_OAUTH_TOKEN      (set — a pool of one: every machine on it)"
    );
    let (text, missing) = rows(&Config::parse(""));
    assert!(text.ends_with("  · CLAUDE_CODE_OAUTH_TOKEN      (missing — no Claude Code token goes to the pods)") && missing.is_empty());
    for cfg in [cfg, Config::parse(&format!("CLAUDE_CODE_OAUTH_TOKEN={}\n", tok(9)))] {
        assert!(!rows(&cfg).0.contains("sk-ant"));
    }
    // In the whole report too, under its own heading.
    let (report, _) = crate::config_cmd::config_report(&config_in(&d, 2, ""), "hetzner");
    assert!(report.contains("\nClaude Code accounts (optional, `arena claude`):\n  ✓ CLAUDE_TOKENS_FILE           2 accounts in"), "{report}");
    assert!(!report.contains("sk-ant"));
}

#[test]
fn an_account_is_a_number_an_email_or_none() {
    let pool = Pool::of(vec![Account::new(1, "a@x", tok(1)), Account::new(4, "b@x", tok(2))], claude::Source::Config);
    assert_eq!(account_arg("4", &pool).unwrap(), Some("b@x".into()));
    assert_eq!(account_arg("#1", &pool).unwrap(), Some("a@x".into()));
    assert_eq!(account_arg("B@X", &pool).unwrap(), Some("b@x".into()));
    assert_eq!(account_arg("None", &pool).unwrap(), None);
    assert_eq!(account_arg("2", &pool).unwrap_err().to_string(), "no account 2 in the pool — there are: #1 a@x, #4 b@x, or `none`");
}
