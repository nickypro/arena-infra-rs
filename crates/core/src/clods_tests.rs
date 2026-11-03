//! clods on the pods: the embedded files against `clods/`, the installer and its summary, the
//! spread's counter, `install`, the stage over a scripted remote (configured, unconfigured, a
//! tokens file that can't be used, a push that fails) — and the real path: the installer run by
//! bash in a scratch `$HOME` per machine, with fake tokens.

use super::*;
use crate::remote::{FakeReply, FakeRemote, RemoteCall};
use crate::ssh::SshOutput;
use async_trait::async_trait;
use std::collections::HashMap;

/// Fake tokens: never a real one in a test.
fn tok(n: u32) -> String {
    format!("sk-ant-oat01-FAKE-{n:04}-xxxxxxxxxxxxxxxxxxxx")
}

struct TmpDir(PathBuf);

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tmp(tag: &str) -> TmpDir {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("arena-clods-test-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    TmpDir(d)
}

#[cfg(unix)]
fn write_mode(path: &Path, text: &str, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// `n` fake accounts, the third pinned to 5 (so the numbers are 1, 2, 5, 6, …).
fn tokens_text(n: u32) -> String {
    (1..=n).map(|i| if i == 3 { format!("5 u{i}@example.com -> {}\n", tok(i)) } else { format!("u{i}@example.com -> {}\n", tok(i)) }).collect()
}

/// A clods folder in `d` with `n` accounts, and a config naming it (and a state dir of its own).
#[cfg(unix)]
fn config_in(d: &TmpDir, n: u32) -> Config {
    let dir = d.0.join("clods");
    std::fs::create_dir_all(&dir).unwrap();
    write_mode(&dir.join(TOKENS), &tokens_text(n), 0o600);
    Config::parse(&format!("MACHINE_NAME_PREFIX=devtest\nARENA_STATE_DIR={}\nCLODS_DIR={}\n", d.0.join("state").display(), dir.display()))
}

fn pod(name: &str, ip: &str) -> (Pod, SshTarget) {
    let p = Pod {
        provider: "hetzner".into(),
        id: name.into(),
        name: name.into(),
        status: "running".into(),
        ssh_ip: Some(ip.into()),
        ssh_port: Some(22),
        ..Pod::default()
    };
    let t = SshTarget::from_pod(&p, &Config::parse("")).unwrap();
    (p, t)
}

fn summary_out(line: &str) -> FakeReply {
    FakeReply::stdout(&format!("{line}\n"))
}

// ---- the embedded files --------------------------------------------------------------

/// The binary carries what setup.sh installs on a machine: its `tool_files`, no more, no less.
#[test]
fn the_embedded_files_are_what_setup_sh_installs() {
    let setup = FILES.iter().find(|(n, _)| *n == "setup.sh").unwrap().1;
    let line = setup.lines().find(|l| l.starts_with("tool_files=")).expect("setup.sh's tool_files");
    let listed: Vec<&str> = line.trim_start_matches("tool_files=").trim_matches('"').split_whitespace().collect();
    assert_eq!(FILES.iter().map(|(n, _)| *n).collect::<Vec<_>>(), listed);
    for (name, text) in FILES {
        assert!(text.ends_with('\n'), "{name} ends with a newline (the installer's heredoc keeps it so)");
    }
}

/// `clods/` is public: no token and no real address in what the binary carries (example ones,
/// `someone@example.com`, and placeholders, `sk-ant-oat01-...`, are fine).
#[test]
fn the_embedded_files_hold_no_token_or_address() {
    let word = |c: char| c.is_ascii_alphanumeric() || "._%+-".contains(c);
    for (name, text) in FILES {
        for (at, _) in text.match_indices("sk-ant-") {
            let run = text[at + 7..].chars().take_while(|c| c.is_ascii_alphanumeric() || "-_".contains(*c)).count();
            assert!(run < 20, "{name}: a token at {at}");
        }
        for (at, _) in text.match_indices('@') {
            let user = text[..at].chars().rev().take_while(|c| word(*c)).count();
            let domain: String = text[at + 1..].chars().take_while(|c| c.is_ascii_alphanumeric() || ".-".contains(*c)).collect();
            let domain = domain.trim_end_matches('.');
            if user > 0 && domain.contains('.') {
                assert!(domain.starts_with("example."), "{name}: an address @{domain}");
            }
        }
    }
}

// ---- the installer and its summary ---------------------------------------------------

#[test]
fn the_installer_carries_the_files_and_the_tokens_and_runs_setup_sh() {
    let tokens = tokens_text(2);
    let s = installer_with("__END__", &tokens, 7);
    for (name, text) in FILES {
        assert!(s.contains(&format!("cat > \"$clod_tmp/{name}\" <<'__END__'\n{text}__END__\n")), "{name}");
    }
    assert!(s.contains(&format!("cat > \"$clod_tmp/tokens\" <<'__END__'\n{tokens}__END__\n")));
    assert!(s.ends_with(
        "bash \"$clod_tmp/setup.sh\" --prune --keep-default --summary --spread 7 \"$clod_tmp/tokens\" </dev/null\n}\nmain \"$@\"\n"
    ));
    assert!(s.contains("umask 077\n"));
    // A tokens file without a last newline still ends its heredoc.
    assert!(installer_with("__END__", "a@x -> t", 0).contains("<<'__END__'\na@x -> t\n__END__\n"));
}

/// The heredocs' end word is never a line of what they hold: a tokens file could hold anything.
#[test]
fn the_installers_end_word_is_no_line_of_its_files() {
    let s = installer(&tokens_text(1), 0);
    let eof = s.lines().find_map(|l| l.strip_prefix("cat > \"$clod_tmp/setup.sh\" <<'")).unwrap().trim_end_matches('\'').to_string();
    assert!(eof.starts_with("__CLODS_"));
    assert_eq!(s.lines().filter(|l| *l == eof).count(), FILES.len() + 1);
}

#[test]
fn the_summary_line_reads() {
    let s = |accounts, default, changed| Some(Summary { accounts, default, changed });
    let cases: &[(&str, Option<Summary>)] = &[
        ("clods: accounts=3 default=2 changed=0\n", s(3, Some(2), false)),
        ("  clod1 …\nclods: accounts=1 default=none changed=1\n", s(1, None, true)),
        ("clods: accounts=3 default=2 changed=0\nclods: accounts=4 default=6 changed=1\n", s(4, Some(6), true)),
        ("clods: accounts=x default=2 changed=0\n", None),
        ("clods: accounts=3 changed=0\n", None),
        ("nothing\n", None),
        ("", None),
    ];
    for (text, want) in cases {
        assert_eq!(&parse_summary(text), want, "{text:?}");
    }
}

#[test]
fn the_lines_say_the_accounts_and_the_fix() {
    let _c = crate::out::capture(false);
    {
        let ok = Outcome { name: "devtest-a".into(), result: Ok(Summary { accounts: 3, default: Some(2), changed: false }) };
        assert_eq!(ok.brief(), "[devtest-a] claude: clods, 3 accounts");
        assert_eq!(ok.detail(), "[devtest-a] claude: clods, 3 accounts, default claude2 (unchanged)");
        let one = Outcome { name: "b".into(), result: Ok(Summary { accounts: 1, default: None, changed: true }) };
        assert_eq!(one.detail(), "[b] claude: clods, 1 account, default none (normal login) (updated)");
        let bad = Outcome { name: "c".into(), result: Err("exit 1: setup.sh: needs python3".into()) };
        assert_eq!(bad.brief(), "‼ [c] claude: clods not pushed — exit 1: setup.sh: needs python3 (retry: arena pods setup --clods-only c)");
        assert_eq!(bad.detail(), bad.brief());
    }
}

// ---- the push over a scripted remote --------------------------------------------------

#[tokio::test]
async fn a_push_sends_the_installer_on_stdin_and_reads_the_summary() {
    let (_, t) = pod("a", "10.0.0.1");
    let r = FakeRemote::new();
    r.script("10.0.0.1:22", [summary_out("clods: accounts=2 default=1 changed=1")]);
    let script = installer(&tokens_text(2), 0);
    let got = push(&r, &t, &script, TIMEOUT).await.unwrap();
    assert_eq!(got, Summary { accounts: 2, default: Some(1), changed: true });
    // The command is `bash -s`: the tokens are only ever on its stdin.
    assert_eq!(r.calls(), [RemoteCall::Exec { host: "10.0.0.1:22".into(), cmd: COMMAND.into(), timeout: Some(TIMEOUT) }]);
    assert_eq!(r.inputs(), [("10.0.0.1:22".to_string(), script.into_bytes())]);
}

#[tokio::test]
async fn a_failed_push_says_why_with_no_token_in_it() {
    let (_, t) = pod("a", "10.0.0.1");
    let r = FakeRemote::new();
    r.script(
        "10.0.0.1:22",
        [
            FakeReply::exit(1, "setup.sh: warning\nsetup.sh: needs python3 (to edit the JSON config files)\n"),
            FakeReply::exit(2, &format!("bash: line 3: {}: command not found\n", tok(1))),
            FakeReply::stdout("hello\n"),
            FakeReply::error("ssh: connect to host 10.0.0.1 port 22: Connection refused"),
            FakeReply::hang(),
        ],
    );
    let script = installer(&tokens_text(1), 0);
    let p = || push(&r, &t, &script, Duration::from_secs(5));
    assert_eq!(p().await.unwrap_err(), "exit 1: setup.sh: needs python3 (to edit the JSON config files)");
    assert_eq!(p().await.unwrap_err(), "exit 2: bash: line 3: <token>: command not found");
    assert_eq!(p().await.unwrap_err(), "its setup.sh said nothing arena can read (hello)");
    assert!(p().await.unwrap_err().contains("Connection refused"));
    assert_eq!(p().await.unwrap_err(), "timed out after 5s");
}

// ---- the stage: configured or not ------------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn unconfigured_or_no_tokens_file_is_nothing_at_all() {
    let d = tmp("unconfigured");
    let pods = [pod("a", "10.0.0.1")];
    let r = FakeRemote::new();
    // Unset, empty, or a folder with no tokens file: nothing pushed, nothing to say.
    for text in [String::new(), "CLODS_DIR=\n".into(), format!("CLODS_DIR={}\n", d.0.display())] {
        assert!(stage(&r, &Config::parse(&text), &pods).await.is_none(), "{text:?}");
    }
    assert!(r.calls().is_empty());
    // Configured, no pods: nothing either.
    assert!(stage(&r, &config_in(&d, 2), &[]).await.is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn a_tokens_file_that_cant_be_used_pushes_nothing_and_says_why() {
    let d = tmp("unusable");
    let cfg = config_in(&d, 2);
    let path = d.0.join("clods").join(TOKENS);
    let pods = [pod("a", "10.0.0.1")];
    let r = FakeRemote::new();
    async fn why(r: &FakeRemote, cfg: &Config, pods: &[(Pod, SshTarget)]) -> String {
        match stage(r, cfg, pods).await {
            Some(Staged::Unusable(why)) => why,
            other => panic!("{other:?}"),
        }
    }
    write_mode(&path, &tokens_text(2), 0o644);
    assert_eq!(why(&r, &cfg, &pods).await, format!("{0} is mode 0644: other users of this box can get at its tokens (fix: chmod 600 {0})", path.display()));
    write_mode(&path, "# none yet\n", 0o600);
    assert!(why(&r, &cfg, &pods).await.ends_with("holds no account (one `email -> sk-ant-oat01-…` per line)"));
    write_mode(&path, &format!("1 a@x -> {}\n1 b@x -> {}\n", tok(1), tok(2)), 0o600);
    let e = why(&r, &cfg, &pods).await;
    assert!(e.contains("account 1 twice") && !e.contains("sk-ant"), "{e}");
    assert!(r.calls().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn the_stage_pushes_every_pod_its_turn_in_name_order() {
    let d = tmp("stage");
    let cfg = config_in(&d, 3);
    let pods = [pod("devtest-c", "10.0.0.3"), pod("devtest-a", "10.0.0.1"), pod("devtest-b", "10.0.0.2")];
    let r = FakeRemote::new();
    for ip in ["10.0.0.1", "10.0.0.2", "10.0.0.3"] {
        r.script(&format!("{ip}:22"), [summary_out("clods: accounts=3 default=1 changed=1")]);
    }
    let Some(Staged::Pushed(out)) = stage(&r, &cfg, &pods).await else { panic!() };
    assert_eq!(out.iter().map(|o| o.name.as_str()).collect::<Vec<_>>(), ["devtest-c", "devtest-a", "devtest-b"]);
    assert!(out.iter().all(|o| o.result.is_ok()));
    let turn = |ip: &str| {
        let input = r.inputs().into_iter().find(|(h, _)| h == &format!("{ip}:22")).unwrap().1;
        let s = String::from_utf8(input).unwrap();
        assert!(s.contains(&tokens_text(3)), "the tokens file as it is");
        s.rsplit("--spread ").next().unwrap().split(' ').next().unwrap().to_string()
    };
    assert_eq!([turn("10.0.0.1"), turn("10.0.0.2"), turn("10.0.0.3")], ["0", "1", "2"]);
    // The next run goes on from there.
    let r2 = FakeRemote::new();
    stage(&r2, &cfg, &pods[..1]).await.unwrap();
    assert!(String::from_utf8(r2.inputs()[0].1.clone()).unwrap().contains("--summary --spread 3 "));
    // Nothing with a token on a command line.
    assert!(r.calls().iter().chain(r2.calls().iter()).all(|c| !format!("{c:?}").contains("sk-ant")));
}

#[cfg(unix)]
#[tokio::test]
async fn one_failed_push_is_that_pods_outcome_only() {
    let d = tmp("onefails");
    let cfg = config_in(&d, 2);
    let pods = [pod("a", "10.0.0.1"), pod("b", "10.0.0.2")];
    let r = FakeRemote::new();
    r.script("10.0.0.1:22", [FakeReply::exit(1, "setup.sh: needs python3 (to edit the JSON config files)")]);
    r.script("10.0.0.2:22", [summary_out("clods: accounts=2 default=2 changed=0")]);
    let Some(Staged::Pushed(out)) = stage(&r, &cfg, &pods).await else { panic!() };
    assert_eq!(out[0].result, Err("exit 1: setup.sh: needs python3 (to edit the JSON config files)".into()));
    assert_eq!(out[1].result, Ok(Summary { accounts: 2, default: Some(2), changed: false }));
}

// ---- the spread's counter --------------------------------------------------------------

#[test]
fn turns_follow_on_and_a_bad_counter_starts_again() {
    let d = tmp("turns");
    let path = d.0.join("state").join(SPREAD_FILE);
    assert_eq!(take_turns_at(&path, 3).unwrap(), 0);
    assert_eq!(take_turns_at(&path, 1).unwrap(), 3);
    assert_eq!(take_turns_at(&path, 0).unwrap(), 4);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "4\n");
    std::fs::write(&path, "nonsense\n").unwrap();
    assert_eq!(take_turns_at(&path, 2).unwrap(), 0);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "2\n");
}

// ---- arena clods install ----------------------------------------------------------------

#[cfg(unix)]
#[test]
fn install_writes_the_scripts_and_leaves_the_rest_alone() {
    let d = tmp("install");
    let dir = d.0.join(".clods");
    // A dry run on nothing: all new, nothing made.
    assert!(install(&dir, true).unwrap().iter().all(|(_, w)| *w == Wrote::New));
    assert!(!dir.exists());
    let first = install(&dir, false).unwrap();
    assert!(first.iter().all(|(_, w)| *w == Wrote::New));
    assert_eq!(mode_of(&dir), 0o700);
    assert_eq!(mode_of(&dir.join("setup.sh")), 0o755);
    assert_eq!(mode_of(&dir.join("README.md")), 0o644);
    // The user's files, and an old script.
    write_mode(&dir.join(TOKENS), &tokens_text(1), 0o600);
    std::fs::write(dir.join("fleet"), "box 1\n").unwrap();
    std::fs::create_dir_all(dir.join("profiles/claude1")).unwrap();
    std::fs::write(dir.join("profiles/claude1/.account"), "u1@example.com\n").unwrap();
    std::fs::write(dir.join("clods.sh"), "old\n").unwrap();
    let dry = install(&dir, true).unwrap();
    assert_eq!(dry.iter().filter(|(_, w)| *w != Wrote::Same).collect::<Vec<_>>(), [&("clods.sh", Wrote::Updated)]);
    assert_eq!(std::fs::read_to_string(dir.join("clods.sh")).unwrap(), "old\n");
    let again = install(&dir, false).unwrap();
    assert_eq!(again.iter().filter(|(_, w)| *w != Wrote::Same).collect::<Vec<_>>(), [&("clods.sh", Wrote::Updated)]);
    assert_eq!(std::fs::read_to_string(dir.join("clods.sh")).unwrap(), FILES[2].1);
    assert_eq!(std::fs::read_to_string(dir.join(TOKENS)).unwrap(), tokens_text(1));
    assert_eq!(mode_of(&dir.join(TOKENS)), 0o600);
    assert_eq!(std::fs::read_to_string(dir.join("fleet")).unwrap(), "box 1\n");
    assert_eq!(std::fs::read_to_string(dir.join("profiles/claude1/.account")).unwrap(), "u1@example.com\n");
    assert!(install(&dir, false).unwrap().iter().all(|(_, w)| *w == Wrote::Same));
    // A script that can't be run any more is written again.
    write_mode(&dir.join("push.sh"), FILES[1].1, 0o644);
    assert_eq!(install(&dir, false).unwrap().iter().filter(|(_, w)| *w != Wrote::Same).collect::<Vec<_>>(), [&("push.sh", Wrote::Updated)]);
    assert_eq!(mode_of(&dir.join("push.sh")), 0o755);
}

// ---- the real path: bash in a scratch $HOME per machine ------------------------------------

/// Each machine (`ip:22`) is a scratch `$HOME`; a command runs there under bash, its input on
/// stdin — what ssh would do, with clods' real scripts.
struct Homes {
    homes: HashMap<String, PathBuf>,
    tmp: PathBuf,
}

#[async_trait]
impl Remote for Homes {
    async fn exec(&self, t: &SshTarget, cmd: &str, timeout: Option<Duration>) -> crate::Result<SshOutput> {
        self.exec_input(t, cmd, b"", timeout).await
    }

    async fn exec_input(&self, t: &SshTarget, cmd: &str, input: &[u8], _timeout: Option<Duration>) -> crate::Result<SshOutput> {
        use std::io::Write;
        let home = &self.homes[&format!("{}:{}", t.host, t.port)];
        let mut child = std::process::Command::new("bash")
            .arg("-c")
            .arg(cmd)
            .env("HOME", home)
            .env("TMPDIR", &self.tmp)
            .env_remove("CLAUDE_CONFIG_DIR")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("bash");
        child.stdin.take().unwrap().write_all(input).unwrap();
        let out = child.wait_with_output().unwrap();
        Ok(SshOutput {
            success: out.status.success(),
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    async fn copy(&self, _: &SshTarget, _: &str, _: &str, _: Option<Duration>) -> crate::Result<SshOutput> {
        unreachable!("clods copies nothing")
    }

    async fn copy_recursive(&self, _: &SshTarget, _: &str, _: &str, _: Option<Duration>) -> crate::Result<SshOutput> {
        unreachable!("clods copies nothing")
    }
}

fn homes(d: &TmpDir, ips: &[&str]) -> Homes {
    let tmp = d.0.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let homes = ips
        .iter()
        .map(|ip| {
            let h = d.0.join("hosts").join(ip);
            std::fs::create_dir_all(&h).unwrap();
            std::fs::write(h.join(".zshrc"), "# the pod's\n").unwrap();
            std::fs::write(h.join(".bashrc"), "# the pod's\n").unwrap();
            (format!("{ip}:22"), h)
        })
        .collect();
    Homes { homes, tmp }
}

/// Every file under `dir` but the profiles' settings.json that holds a fake token.
fn token_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if e.file_type().unwrap().is_dir() {
                todo.push(p);
            } else if std::fs::read_to_string(&p).is_ok_and(|t| t.contains("sk-ant-oat01-FAKE")) && !p.ends_with("settings.json") {
                out.push(p);
            }
        }
    }
    out
}

/// Each file under `dir`, with its mode and modification time.
fn snapshot(dir: &Path) -> Vec<(PathBuf, u32, std::time::SystemTime)> {
    use std::os::unix::fs::PermissionsExt;
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let m = std::fs::symlink_metadata(e.path()).unwrap();
            out.push((e.path(), m.permissions().mode() & 0o7777, m.modified().unwrap()));
            if m.is_dir() {
                todo.push(e.path());
            }
        }
    }
    out.sort();
    out
}

fn pushed(staged: Option<Staged>) -> Vec<(String, Result<Summary, String>)> {
    match staged {
        Some(Staged::Pushed(out)) => out.into_iter().map(|o| (o.name, o.result)).collect(),
        other => panic!("{other:?}"),
    }
}

/// The whole path for real: the stage reads the tokens file, builds each pod's installer, and bash
/// runs it in each pod's `$HOME` — every account on each, the defaults spread in name order, files
/// 0600 in 0700 folders, the hook in both rc files, nothing with a token left outside the
/// profiles; a re-push changes nothing, not even a file's time; an account taken out of `tokens`
/// loses its token on the pods, and a pod whose default it was gets the next turn.
#[cfg(unix)]
#[tokio::test]
async fn the_real_push_in_a_scratch_home() {
    let d = tmp("real");
    let cfg = config_in(&d, 3);
    let r = homes(&d, &["10.0.0.1", "10.0.0.2", "10.0.0.3", "10.0.0.4"]);
    let pods = [pod("devtest-a", "10.0.0.1"), pod("devtest-b", "10.0.0.2"), pod("devtest-c", "10.0.0.3"), pod("devtest-d", "10.0.0.4")];
    let s = |accounts, default, changed| Ok(Summary { accounts, default: Some(default), changed });
    let got = pushed(stage(&r, &cfg, &pods).await);
    let want = [("devtest-a", s(3, 1, true)), ("devtest-b", s(3, 2, true)), ("devtest-c", s(3, 5, true)), ("devtest-d", s(3, 1, true))];
    assert_eq!(got, want.map(|(n, s)| (n.to_string(), s)));
    let home = &r.homes["10.0.0.2:22"];
    let profiles = home.join(".clods/profiles");
    assert_eq!(mode_of(&profiles), 0o700);
    for n in [1, 2, 5] {
        let p = profiles.join(format!("claude{n}"));
        assert_eq!(mode_of(&p), 0o700);
        for f in ["settings.json", ".claude.json", ".account"] {
            assert_eq!(mode_of(&p.join(f)), 0o600, "claude{n}/{f}");
        }
    }
    assert_eq!(mode_of(&profiles.join("default")), 0o600);
    let settings = std::fs::read_to_string(profiles.join("claude5/settings.json")).unwrap();
    assert!(settings.contains(&tok(3)), "claude5 holds account 3's token");
    for (name, text) in FILES {
        assert_eq!(std::fs::read_to_string(home.join(".clods").join(name)).unwrap(), text, "{name}");
    }
    assert!(!home.join(".clods/tokens").exists());
    for rc in [".zshrc", ".bashrc"] {
        let t = std::fs::read_to_string(home.join(rc)).unwrap();
        assert!(t.starts_with("# the pod's\n") && t.lines().filter(|l| l.contains(".clods/clods.sh")).count() == 1, "{rc}: {t}");
    }
    assert_eq!(token_files(&d.0.join("hosts")), Vec::<PathBuf>::new());
    assert_eq!(std::fs::read_dir(&r.tmp).unwrap().count(), 0, "nothing left in TMPDIR");

    // Again: nothing changes on any pod — not a file, not a mode, not a time; nobody's default moves.
    let before = snapshot(&d.0.join("hosts"));
    std::thread::sleep(Duration::from_millis(1100));
    let got = pushed(stage(&r, &cfg, &pods).await);
    let want = [("devtest-a", s(3, 1, false)), ("devtest-b", s(3, 2, false)), ("devtest-c", s(3, 5, false)), ("devtest-d", s(3, 1, false))];
    assert_eq!(got, want.map(|(n, s)| (n.to_string(), s)));
    assert_eq!(snapshot(&d.0.join("hosts")), before);

    // Account 2 taken out of the file: off every pod (its history kept); devtest-b, whose default
    // it was, gets the next turn's account.
    std::fs::write(profiles.join("claude2/history.jsonl"), "kept\n").unwrap();
    let text: String = tokens_text(3).lines().filter(|l| !l.starts_with("u2@")).map(|l| format!("{l}\n")).collect();
    write_mode(&d.0.join("clods").join(TOKENS), &text, 0o600);
    let got = pushed(stage(&r, &cfg, &pods[1..2]).await);
    // Turns 0..4 went to the first push, 4..8 to the second: devtest-b's is 8, and 8 % 2 = 0.
    assert_eq!(got, [("devtest-b".to_string(), s(2, 1, true))]);
    assert!(!profiles.join("claude2/.account").exists());
    assert!(!std::fs::read_to_string(profiles.join("claude2/settings.json")).unwrap().contains("sk-ant"));
    assert_eq!(std::fs::read_to_string(profiles.join("claude2/history.jsonl")).unwrap(), "kept\n");
}
