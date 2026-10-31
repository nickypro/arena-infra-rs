//! Table tests for the Claude Code account pool: the tokens file, the planners (spread, pins, owners,
//! a name reused, a pool of one, an account gone from the file), the view, the state file, the push
//! command (run for real in a scratch `$HOME`) and the push through a scripted remote.

use super::*;
use crate::remote::{FakeRemote, RemoteCall};
use crate::ssh::SshOutput;

const T0: u64 = 1_793_000_000;

/// Fake tokens: never a real one in a test.
fn tok(n: u32) -> String {
    format!("sk-ant-oat01-FAKE-{n:04}-xxxxxxxxxxxxxxxxxxxx")
}

fn pool(n: u32) -> Pool {
    Pool::of((1..=n).map(|i| Account::new(i, format!("u{i}@x"), tok(i))).collect(), Source::File("tokens".into()))
}

fn m(name: &str, id: &str, owner: Option<&str>) -> Machine {
    Machine { provider: "hetzner".into(), id: id.into(), name: name.into(), owner: owner.map(String::from) }
}

fn owners(ms: &[Machine]) -> Owners {
    ms.iter().filter_map(|m| Some(((m.provider.clone(), m.id.clone()), m.owner.clone()?))).collect()
}

/// Each machine's account, by name.
fn on(state: &State, ms: &[Machine]) -> Vec<(String, Option<String>)> {
    ms.iter().map(|m| (m.name.clone(), state.get(&m.provider, &m.id).and_then(|a| a.account.clone()))).collect()
}

fn acct(e: &str) -> Option<String> {
    Some(e.to_string())
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
    let d = std::env::temp_dir().join(format!("arena-claude-test-{tag}-{}-{n}", std::process::id()));
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

// ---- the tokens file ----------------------------------------------------------------

#[test]
fn the_tokens_file_reads_as_clods_writes_it() {
    let text = format!(
        "# accounts\n\nalice@arena.education -> {}\nbob@x={}\n  carol@x   {}  \n7 dave@x -> {}\neve@x -> {}\n",
        tok(1),
        tok(2),
        tok(3),
        tok(7),
        tok(8)
    );
    let p = parse_tokens(&text).unwrap();
    let got: Vec<(u32, &str)> = p.accounts.iter().map(|a| (a.num, a.email.as_str())).collect();
    // A pinned number, and the next line counts on from it (as clods numbers them).
    assert_eq!(got, [(1, "alice@arena.education"), (2, "bob@x"), (3, "carol@x"), (7, "dave@x"), (8, "eve@x")]);
    assert_eq!(p.accounts[3].token(), tok(7));
    assert!(p.skipped.is_empty(), "{:?}", p.skipped);
}

#[test]
fn a_line_that_isnt_an_account_is_skipped_and_named_without_its_token() {
    let text = format!("alice@x -> {}\nbob@x -> sk-ant-oat01-has\"quote\nsk-ant-oat01-NOEMAILxxxxxx\njunk line\nc d e -> {}\n", tok(1), tok(2));
    let p = parse_tokens(&text).unwrap();
    assert_eq!(p.accounts.len(), 1);
    assert_eq!(p.skipped.len(), 4, "{:?}", p.skipped);
    assert!(p.skipped[0].starts_with("line 2 (`bob@x …`)"), "{}", p.skipped[0]);
    assert!(p.skipped[1].starts_with("line 3 (its start isn't shown)"), "{}", p.skipped[1]);
    for s in &p.skipped {
        assert!(!s.contains("sk-ant") && !s.contains("NOEMAIL") && !s.contains("quote"), "{s}");
    }
}

#[test]
fn a_number_or_an_email_twice_and_a_number_0_are_refused() {
    for (text, says) in [
        (format!("a@x -> {}\n1 b@x -> {}\n", tok(1), tok(2)), "line 2: account 1 twice (b@x)"),
        (format!("a@x -> {}\nA@x -> {}\n", tok(1), tok(2)), "line 2: A@x twice — one token per account"),
        (format!("0 a@x -> {}\n", tok(1)), "line 1: account numbers start at 1 (a@x)"),
    ] {
        let e = parse_tokens(&text).unwrap_err();
        assert_eq!(e, says);
        assert!(!e.contains("sk-ant"));
    }
}

#[test]
fn an_accounts_debug_never_shows_its_token() {
    let a = Account::new(1, "a@x", tok(1));
    let shown = format!("{a:?} {:?}", pool(2));
    assert!(!shown.contains("sk-ant") && shown.contains("<redacted>"), "{shown}");
    assert_eq!(a.fingerprint().len(), 12);
    assert_ne!(a.fingerprint(), Account::new(1, "a@x", tok(2)).fingerprint());
}

#[test]
fn the_pool_finds_an_account_by_number_or_email() {
    let p = pool(3);
    assert_eq!(p.find("2").map(|a| a.num), Some(2));
    assert_eq!(p.find("#3").map(|a| a.num), Some(3));
    assert_eq!(p.find("U1@X").map(|a| a.num), Some(1));
    assert!(p.find("4").is_none() && p.find("nobody@x").is_none() && p.find("").is_none());
    assert_eq!(p.listed(), "#1 u1@x, #2 u2@x, #3 u3@x");
}

#[cfg(unix)]
#[test]
fn the_pool_is_the_tokens_file_or_configs_one_token() {
    let d = tmp("pool");
    let file = d.0.join("tokens");
    write_mode(&file, &format!("a@x -> {}\nb@x -> {}\n", tok(1), tok(2)), 0o600);
    let cfg = |text: &str| Config::parse(text);

    // The file: its accounts; a CLAUDE_CODE_OAUTH_TOKEN beside it is unused, and said so.
    let p = load_pool(&cfg(&format!("CLAUDE_TOKENS_FILE={}\nCLAUDE_CODE_OAUTH_TOKEN={}\n", file.display(), tok(9)))).unwrap();
    assert_eq!(p.accounts.len(), 2);
    assert_eq!(p.source, Source::File(file.clone()));
    assert_eq!(p.notes, ["CLAUDE_CODE_OAUTH_TOKEN is set too, and unused: CLAUDE_TOKENS_FILE's accounts replace it"]);
    assert_eq!(p.describe(), format!("2 accounts in {}", file.display()));

    // No file: config's token is a pool of one, so a config that worked before still does.
    let one = load_pool(&cfg(&format!("CLAUDE_CODE_OAUTH_TOKEN={}\n", tok(9)))).unwrap();
    assert_eq!((one.accounts.len(), one.accounts[0].email.as_str(), one.source.clone()), (1, CONFIG_ACCOUNT, Source::Config));
    assert_eq!(one.accounts[0].token(), tok(9));

    // Neither: no pool, not an error.
    assert!(load_pool(&cfg("")).unwrap().is_empty());

    // Refusals, none with a token in them.
    write_mode(&file, &format!("a@x -> {}\n", tok(1)), 0o644);
    let e = load_pool(&cfg(&format!("CLAUDE_TOKENS_FILE={}\n", file.display()))).unwrap_err();
    assert_eq!(
        e,
        format!(
            "CLAUDE_TOKENS_FILE {} is mode 0644: other users of this box can get at its tokens — chmod 600 {}",
            file.display(),
            file.display()
        )
    );
    write_mode(&file, "# nothing yet\n", 0o600);
    let e = load_pool(&cfg(&format!("CLAUDE_TOKENS_FILE={}\n", file.display()))).unwrap_err();
    assert!(e.ends_with("holds no token (one `email -> sk-ant-oat01-…` per line; `claude setup-token` makes one)"), "{e}");
    let e = load_pool(&cfg(&format!("CLAUDE_TOKENS_FILE={}/missing\n", d.0.display()))).unwrap_err();
    assert!(e.starts_with("CLAUDE_TOKENS_FILE ") && e.contains("No such file"), "{e}");
    let e = load_pool(&cfg("CLAUDE_CODE_OAUTH_TOKEN=\"has space$\"\n")).unwrap_err();
    assert_eq!(e, "CLAUDE_CODE_OAUTH_TOKEN isn't a token (letters, digits, `-` and `_` only)");
}

// ---- assigning ----------------------------------------------------------------------

#[test]
fn spread_fills_the_least_loaded_account_first() {
    let ms: Vec<Machine> = (1..=5).map(|i| m(&format!("p{i}"), &i.to_string(), None)).collect();
    let (s, changes) = assign(State::default(), &pool(3), &Owners::new(), &ms, &Want::Ensure, T0);
    assert_eq!(
        on(&s, &ms),
        [("p1".into(), acct("u1@x")), ("p2".into(), acct("u2@x")), ("p3".into(), acct("u3@x")), ("p4".into(), acct("u1@x")), ("p5".into(), acct("u2@x"))]
    );
    assert!(changes.iter().all(|c| c.why == Why::New && c.was == Was::Nothing && c.how == How::Spread));
    // A sixth machine later sees the five: #3 has the fewest.
    let six = m("p6", "6", None);
    let (s, changes) = assign(s, &pool(3), &Owners::new(), std::slice::from_ref(&six), &Want::Ensure, T0 + 1);
    assert_eq!(on(&s, &[six]), [("p6".into(), acct("u3@x"))]);
    assert_eq!(changes.len(), 1);
    // Ensure again: nothing moves, nothing to say.
    let (s2, changes) = assign(s.clone(), &pool(3), &Owners::new(), &ms, &Want::Ensure, T0 + 2);
    assert_eq!((s2, changes), (s, vec![]));
}

#[test]
fn a_pool_of_one_puts_every_machine_on_it_and_no_pool_puts_none() {
    let ms: Vec<Machine> = (1..=3).map(|i| m(&format!("p{i}"), &i.to_string(), None)).collect();
    let one = Pool::of(vec![Account::new(1, CONFIG_ACCOUNT, tok(1))], Source::Config);
    let (s, _) = assign(State::default(), &one, &Owners::new(), &ms, &Want::Ensure, T0);
    assert!(on(&s, &ms).iter().all(|(_, a)| a.as_deref() == Some(CONFIG_ACCOUNT)));
    let (s, changes) = assign(State::default(), &Pool::empty(), &Owners::new(), &ms, &Want::Ensure, T0);
    assert!(s.machines.is_empty() && changes.is_empty());
}

#[test]
fn spread_counts_people_when_spend_has_owners() {
    // alice has two machines, bob and carol one each: alice counts once.
    let ms = [m("a1", "1", Some("alice")), m("a2", "2", Some("alice")), m("b", "3", Some("bob")), m("c", "4", Some("carol"))];
    let (s, changes) = assign(State::default(), &pool(2), &owners(&ms), &ms, &Want::Ensure, T0);
    assert_eq!(
        on(&s, &ms),
        [("a1".into(), acct("u1@x")), ("a2".into(), acct("u1@x")), ("b".into(), acct("u2@x")), ("c".into(), acct("u1@x"))]
    );
    assert_eq!(changes[1].why, Why::WithOwner("alice".into()));
    // alice's third machine, later, joins her account whatever its load.
    let a3 = m("a3", "5", Some("alice"));
    let all: Vec<Machine> = ms.iter().cloned().chain([a3.clone()]).collect();
    let (s, changes) = assign(s, &pool(2), &owners(&all), std::slice::from_ref(&a3), &Want::Ensure, T0);
    assert_eq!(on(&s, &[a3]), [("a3".into(), acct("u1@x"))]);
    assert_eq!(changes[0].because(), "beside alice's other machines");
}

#[test]
fn a_pin_holds_until_the_machine_is_spread_again() {
    let ms: Vec<Machine> = (1..=3).map(|i| m(&format!("p{i}"), &i.to_string(), None)).collect();
    let (s, _) = assign(State::default(), &pool(3), &Owners::new(), &ms, &Want::Ensure, T0);
    // p1 (on #1) pinned to #3.
    let (s, changes) = assign(s, &pool(3), &Owners::new(), &ms[..1], &Want::Pin(acct("u3@x")), T0 + 1);
    assert_eq!(changes.len(), 1);
    assert_eq!((changes[0].was.clone(), changes[0].to.clone(), changes[0].how), (Was::On("u1@x".into()), acct("u3@x"), How::Pinned));
    // Ensure keeps a pin; a new machine sees #1 empty now.
    let p4 = m("p4", "4", None);
    let all: Vec<Machine> = ms.iter().cloned().chain([p4.clone()]).collect();
    let (s, _) = assign(s, &pool(3), &Owners::new(), &all, &Want::Ensure, T0 + 2);
    assert_eq!(on(&s, &all)[0].1, acct("u3@x"));
    assert_eq!(on(&s, &all)[3].1, acct("u1@x"));
    // Spread again: back on the least-loaded (all at one now: the lowest number), by spread.
    let (s, changes) = assign(s, &pool(3), &Owners::new(), &ms[..1], &Want::Spread, T0 + 3);
    assert_eq!(s.get("hetzner", "1").map(|a| (a.account.clone(), a.how)), Some((acct("u1@x"), How::Spread)));
    assert_eq!(changes[0].why, Why::Respread);
    // Pinned to none (`claude off`): off, and Ensure leaves it off.
    let (s, changes) = assign(s, &pool(3), &Owners::new(), &ms[..1], &Want::Pin(None), T0 + 4);
    assert_eq!(changes[0].to, None);
    let (s, changes) = assign(s, &pool(3), &Owners::new(), &ms[..1], &Want::Ensure, T0 + 5);
    assert!(changes.is_empty());
    assert_eq!(s.get("hetzner", "1").map(|a| a.account.clone()), Some(None));
}

#[test]
fn a_name_made_again_is_a_new_machine_and_inherits_nothing() {
    let old = m("apple", "111", None);
    let (s, _) = assign(State::default(), &pool(2), &Owners::new(), std::slice::from_ref(&old), &Want::Pin(acct("u2@x")), T0);
    // apple terminated and made again: a new instance (id 222). Its assignment is its own.
    let new = m("apple", "222", None);
    let (s, changes) = assign(s, &pool(2), &Owners::new(), std::slice::from_ref(&new), &Want::Ensure, T0 + 1);
    assert_eq!(changes[0].was, Was::Nothing);
    assert_eq!(s.get("hetzner", "222").map(|a| (a.account.clone(), a.how)), Some((acct("u1@x"), How::Spread)));
    // A complete listing without 111 forgets it; a failed one keeps it.
    let new_pod = Pod { provider: "hetzner".into(), id: "222".into(), name: "apple".into(), ..Pod::default() };
    let failed = Listing::from_results(vec![("hetzner".into(), Err(crate::Error::provider("boom")))]);
    assert_eq!(prune(s.clone(), &failed).machines.len(), 2);
    let ok = Listing::from_results(vec![("hetzner".into(), Ok(vec![new_pod]))]);
    let s = prune(s, &ok);
    assert_eq!(s.machines.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["222"]);
    // Another provider's listing says nothing about Hetzner's machines.
    let other = Listing::from_results(vec![("runpod".into(), Ok(vec![]))]);
    assert_eq!(prune(s.clone(), &other), s);
}

#[test]
fn a_machine_on_an_account_gone_from_the_file_is_spread_again_and_the_rest_stay() {
    let ms: Vec<Machine> = (1..=4).map(|i| m(&format!("p{i}"), &i.to_string(), None)).collect();
    let (s, _) = assign(State::default(), &pool(3), &Owners::new(), &ms, &Want::Ensure, T0);
    // u2@x leaves the file (its line deleted): #3 keeps its number in clods only if pinned — here
    // the pool is u1, u3 renumbered, which changes nothing: accounts are emails.
    let smaller = Pool::of(vec![Account::new(1, "u1@x", tok(1)), Account::new(2, "u3@x", tok(3))], Source::File("tokens".into()));
    let (s, changes) = assign(s, &smaller, &Owners::new(), &ms, &Want::Ensure, T0 + 1);
    assert_eq!(changes.len(), 1);
    assert_eq!((changes[0].name.as_str(), changes[0].why.clone()), ("p2", Why::Left("u2@x".into())));
    assert_eq!(changes[0].because(), "u2@x is no longer in the pool");
    // p1 and p4 on u1, p3 on u3: p2 goes to u3.
    assert_eq!(on(&s, &ms).iter().map(|(_, a)| a.clone().unwrap()).collect::<Vec<_>>(), ["u1@x", "u3@x", "u3@x", "u1@x"]);
}

#[test]
fn an_owner_pin_takes_their_machines_now_and_later() {
    let ms = [m("a1", "1", Some("alice")), m("a2", "2", Some("alice")), m("b", "3", Some("bob"))];
    let o = owners(&ms);
    let (s, _) = assign(State::default(), &pool(3), &o, &ms, &Want::Ensure, T0);
    let (s, changes) = pin_owner(s, &pool(3), &o, "alice", Some(acct("u3@x")), &ms[..2], T0 + 1);
    assert_eq!(on(&s, &ms[..2]), [("a1".into(), acct("u3@x")), ("a2".into(), acct("u3@x"))]);
    assert!(changes.iter().all(|c| c.how == How::Owner && c.why == Why::Owner("alice".into())));
    // Her next machine goes there too; bob's isn't touched.
    let a3 = m("a3", "4", Some("alice"));
    let all: Vec<Machine> = ms.iter().cloned().chain([a3.clone()]).collect();
    let (s, changes) = assign(s, &pool(3), &owners(&all), std::slice::from_ref(&a3), &Want::Ensure, T0 + 2);
    assert_eq!(changes[0].because(), "alice's pin");
    assert_eq!(s.get("hetzner", "4").unwrap().account, acct("u3@x"));
    assert_eq!(s.get("hetzner", "3").unwrap().account, acct("u2@x"));
    // Pinned to none: all of hers off. The pin off again: spread, together.
    let theirs: Vec<Machine> = all.iter().filter(|m| m.owner.as_deref() == Some("alice")).cloned().collect();
    let (s, _) = pin_owner(s, &pool(3), &owners(&all), "alice", Some(None), &theirs, T0 + 3);
    assert!(on(&s, &theirs).iter().all(|(_, a)| a.is_none()));
    let (s, _) = pin_owner(s, &pool(3), &owners(&all), "alice", None, &theirs, T0 + 4);
    assert!(s.owners.is_empty());
    assert_eq!(on(&s, &theirs).iter().map(|(_, a)| a.clone()).collect::<Vec<_>>(), [acct("u1@x"), acct("u1@x"), acct("u1@x")]);
}

#[test]
fn out_of_date_until_the_current_token_of_its_account_went() {
    let p = pool(2);
    let fp1 = p.get("u1@x").unwrap().fingerprint();
    let a = |account: Option<&str>, pushed: Option<(Option<&str>, Option<&str>)>| Assignment {
        provider: "hetzner".into(),
        id: "1".into(),
        name: "p1".into(),
        account: account.map(String::from),
        how: How::Spread,
        at: T0,
        pushed: pushed.map(|(account, fp)| Pushed { account: account.map(String::from), fp: fp.map(String::from), at: T0 }),
    };
    for (row, want) in [
        (a(Some("u1@x"), None), true),                                  // never pushed
        (a(Some("u1@x"), Some((Some("u1@x"), Some(&fp1)))), false),      // up to date
        (a(Some("u1@x"), Some((Some("u2@x"), Some(&fp1)))), true),       // moved since
        (a(Some("u1@x"), Some((Some("u1@x"), Some("000000000000")))), true), // rotated since
        (a(Some("gone@x"), Some((Some("gone@x"), Some(&fp1)))), true),   // its account left the pool
        (a(None, None), true),                                         // off, never taken off
        (a(None, Some((None, None))), false),                          // off, taken off
        (a(None, Some((Some("u1@x"), Some(&fp1)))), true),               // off, still has it
    ] {
        assert_eq!(out_of_date(&row, &p), want, "{row:?}");
    }
}

// ---- the view -----------------------------------------------------------------------

#[test]
fn the_accounts_view_says_who_is_where() {
    let ms = [m("apple", "1", Some("alice")), m("bloom", "2", Some("alice")), m("cloud", "3", None), m("dune", "4", None), m("east", "5", None)];
    let o = owners(&ms);
    let (s, _) = assign(State::default(), &pool(2), &o, &ms[..3], &Want::Ensure, T0);
    let (s, _) = assign(s, &pool(2), &o, &ms[3..4], &Want::Pin(None), T0);
    let fp = pool(2).get("u1@x").unwrap().fingerprint();
    let s = record_push(s, "hetzner", "1", Some("u1@x"), Some(&fp), T0);
    let v = view(&s, &pool(2), &ms);
    assert_eq!(v.accounts[0].machines, ["apple", "bloom"]);
    assert_eq!(v.accounts[0].people, ["alice"]);
    assert_eq!(v.accounts[0].pending, ["bloom"]);
    assert_eq!(v.accounts[1].machines, ["cloud"]);
    assert_eq!((v.off.clone(), v.unassigned.clone()), (vec!["dune".to_string()], vec!["east".to_string()]));
    let text = render(&v);
    assert_eq!(
        text,
        "Claude Code accounts: 2 accounts in tokens\n\n\
         #  ACCOUNT  MACHINES  PEOPLE  ON IT\n\
         1  u1@x            2       1  apple, bloom*\n\
         2  u2@x            1       -  cloud*\n\
         \nPeople (spend owners): #1 alice\n\
         No token (claude off): dune*\n\
         Not on an account yet: east — `arena claude push` spreads them\n\
         * its box may not have this yet (moved, rotated, or never pushed): `arena claude push` puts it on\n"
    );
    assert!(!text.contains("sk-ant"));
    // An account gone from the pool keeps a row while machines are on it.
    let smaller = Pool::of(vec![Account::new(1, "u1@x", tok(1))], Source::File("tokens".into()));
    let v = view(&s, &smaller, &ms);
    assert_eq!(v.accounts[1].num, None);
    assert!(render(&v).contains("-  u2@x            1       -  cloud*, ← not in the pool: `arena claude push` moves them"), "{}", render(&v));
    let json = serde_json::to_string(&v).unwrap();
    assert!(json.contains("\"email\":\"u2@x\"") && !json.contains("sk-ant"), "{json}");
}

// ---- the state file -----------------------------------------------------------------

#[cfg(unix)]
#[test]
fn the_state_file_is_private_and_a_broken_one_is_left_alone() {
    use std::os::unix::fs::PermissionsExt;
    let d = tmp("state");
    let path = d.0.join("devtest").join(STATE_FILE);
    let ms = [m("apple", "1", None)];
    let (s, changes) = record(&path, |s| assign(s, &pool(2), &Owners::new(), &ms, &Want::Ensure, T0)).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(load(&path).unwrap(), s);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("\"how\": \"spread\"") && !text.contains("sk-ant"), "{text}");
    // Unchanged: not rewritten.
    let before = std::fs::metadata(&path).unwrap().modified().unwrap();
    std::thread::sleep(Duration::from_millis(20));
    record(&path, |s| (s, ())).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), before);
    // A broken file is an error, and stays as it was.
    std::fs::write(&path, "{ not json").unwrap();
    let e = record(&path, |s| assign(s, &pool(2), &Owners::new(), &ms, &Want::Ensure, T0)).unwrap_err();
    assert!(e.contains("isn't readable") && e.contains("fix or move it aside"), "{e}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
}

// ---- the push command, run for real ---------------------------------------------------

/// Run [`push_command`] with bash in `home` as `$HOME`, `token` copied there first (as scp
/// would) when given: (exit code, stdout, stderr).
fn run_push(home: &Path, token: Option<&str>) -> (i32, String, String) {
    let name = ".arena-claude-token-test";
    if let Some(t) = token {
        std::fs::write(home.join(name), format!("{t}\n")).unwrap();
    }
    let cmd = push_command(token.map(|_| name));
    let out = std::process::Command::new("bash")
        .arg("-c")
        .arg(&cmd)
        .env("HOME", home)
        .env_remove("ARENA_CC")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("bash");
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into(), String::from_utf8_lossy(&out.stderr).into())
}

fn read(home: &Path, f: &str) -> String {
    std::fs::read_to_string(home.join(f)).unwrap_or_default()
}

fn token_lines(text: &str) -> Vec<&str> {
    text.lines().filter(|l| l.contains("CLAUDE_CODE_OAUTH_TOKEN")).collect()
}

/// What's in `home` besides the rc files and `~/.claude.json`: nothing should be.
fn leftovers(home: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(home)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| ![".bashrc", ".zshrc", ".claude.json"].contains(&n.as_str()))
        .collect();
    v.sort();
    v
}

#[test]
fn the_push_replaces_every_old_token_line_keeps_the_rest_and_a_repush_changes_nothing() {
    let d = tmp("push");
    let home = &d.0;
    // As copy-keys left it (twice over), between the participant's own lines; .zshrc without a
    // trailing newline.
    std::fs::write(
        home.join(".bashrc"),
        format!("alias ll='ls -l'\nexport CLAUDE_CODE_OAUTH_TOKEN=\"{}\"\nexport HF_TOKEN=\"hf\"\n  export CLAUDE_CODE_OAUTH_TOKEN=\"{}\"\n# mine\n", tok(8), tok(9)),
    )
    .unwrap();
    std::fs::write(home.join(".zshrc"), "source ~/.p10k.zsh").unwrap();
    std::fs::write(home.join(".claude.json"), "{\"theme\": \"dark\"}").unwrap();

    let (code, out, err) = run_push(home, Some(&tok(1)));
    assert_eq!((code, out.trim()), (0, "claude: token set"), "{err}");
    let want = format!("export CLAUDE_CODE_OAUTH_TOKEN=\"{}\"", tok(1));
    for rc in [".bashrc", ".zshrc"] {
        assert_eq!(token_lines(&read(home, rc)), [want.as_str()], "{rc}");
        assert!(read(home, rc).ends_with(&format!("{want}\n")), "{rc}: last");
    }
    assert_eq!(read(home, ".bashrc"), format!("alias ll='ls -l'\nexport HF_TOKEN=\"hf\"\n# mine\n{want}\n"));
    assert_eq!(read(home, ".zshrc"), format!("source ~/.p10k.zsh\n{want}\n"));
    let cj: serde_json::Value = serde_json::from_str(&read(home, ".claude.json")).unwrap();
    assert_eq!((cj["theme"].as_str(), cj["hasCompletedOnboarding"].as_bool()), (Some("dark"), Some(true)));
    assert_eq!(leftovers(home), Vec::<String>::new(), "the token's file and the temp files are gone");

    // The same again: byte for byte the same files, not rewritten.
    let before = (read(home, ".bashrc"), std::fs::metadata(home.join(".bashrc")).unwrap().modified().unwrap());
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(run_push(home, Some(&tok(1))).0, 0);
    assert_eq!((read(home, ".bashrc"), std::fs::metadata(home.join(".bashrc")).unwrap().modified().unwrap()), before);

    // Moved to another account: that token, and only that one.
    assert_eq!(run_push(home, Some(&tok(2))).0, 0);
    let moved = format!("export CLAUDE_CODE_OAUTH_TOKEN=\"{}\"", tok(2));
    assert_eq!(token_lines(&read(home, ".bashrc")), [moved.as_str()]);
    assert_eq!(token_lines(&read(home, ".zshrc")), [moved.as_str()]);

    // Off: no token line anywhere, the rest kept.
    let (code, out, _) = run_push(home, None);
    assert_eq!((code, out.trim()), (0, "claude: token removed"));
    assert_eq!(read(home, ".bashrc"), "alias ll='ls -l'\nexport HF_TOKEN=\"hf\"\n# mine\n");
    assert_eq!(read(home, ".zshrc"), "source ~/.p10k.zsh\n");
    assert_eq!(leftovers(home), Vec::<String>::new());
}

#[test]
fn off_on_a_machine_with_no_rc_files_makes_none_and_a_missing_token_changes_nothing() {
    let d = tmp("push-none");
    let home = &d.0;
    assert_eq!(run_push(home, None).0, 0);
    assert!(leftovers(home).is_empty() && !home.join(".bashrc").exists() && !home.join(".zshrc").exists());
    // A token that didn't arrive (an scp that failed quietly): exit 3, nothing written.
    std::fs::write(home.join(".bashrc"), format!("export CLAUDE_CODE_OAUTH_TOKEN=\"{}\"\n", tok(1))).unwrap();
    let out = std::process::Command::new("bash")
        .arg("-c")
        .arg(push_command(Some(".arena-claude-token-never")))
        .env("HOME", home)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("the token didn't arrive"));
    assert_eq!(token_lines(&read(home, ".bashrc")).len(), 1);
    // A fresh machine (no rc files, no ~/.claude.json): both made, with the token.
    std::fs::remove_file(home.join(".bashrc")).unwrap();
    assert_eq!(run_push(home, Some(&tok(3))).0, 0);
    assert_eq!(token_lines(&read(home, ".zshrc")).len(), 1);
    assert!(read(home, ".claude.json").contains("hasCompletedOnboarding"));
}

#[test]
fn the_push_command_never_carries_a_token() {
    for cmd in [push_command(Some(".arena-claude-token-1")), push_command(None)] {
        assert!(!cmd.contains("sk-ant"), "{cmd}");
        assert!(cmd.contains("ENVIRON[\"ARENA_CC\"]"), "the token reaches awk through its environment");
    }
}

// ---- the push, through a scripted remote ---------------------------------------------

/// A [`FakeRemote`] that also keeps what each copied file held when it was copied (it's deleted
/// right after), and where it was.
#[derive(Default)]
struct Capture {
    fake: FakeRemote,
    copied: std::sync::Mutex<Vec<(String, String, String)>>,
    locals: std::sync::Mutex<Vec<PathBuf>>,
}

#[async_trait::async_trait]
impl Remote for Capture {
    async fn exec(&self, t: &SshTarget, cmd: &str, timeout: Option<Duration>) -> crate::Result<SshOutput> {
        self.fake.exec(t, cmd, timeout).await
    }

    async fn copy(&self, t: &SshTarget, local: &str, remote: &str, timeout: Option<Duration>) -> crate::Result<SshOutput> {
        let held = std::fs::read_to_string(local).unwrap_or_default();
        self.locals.lock().unwrap().push(PathBuf::from(local));
        self.copied.lock().unwrap().push((format!("{}:{}", t.host, t.port), remote.to_string(), held));
        self.fake.copy(t, local, remote, timeout).await
    }

    async fn copy_recursive(&self, t: &SshTarget, local: &str, remote: &str, timeout: Option<Duration>) -> crate::Result<SshOutput> {
        self.fake.copy_recursive(t, local, remote, timeout).await
    }
}

fn pod(name: &str, id: &str, ip: &str) -> (Pod, SshTarget) {
    let p = Pod {
        provider: "hetzner".into(),
        id: id.into(),
        name: name.into(),
        status: "running".into(),
        ssh_ip: Some(ip.into()),
        ssh_port: Some(22),
        ..Pod::default()
    };
    let t = SshTarget::from_pod(&p, &Config::parse("")).unwrap();
    (p, t)
}

/// A config with a tokens file of `n` fake accounts and a state dir of its own, in `d`.
#[cfg(unix)]
fn config_in(d: &TmpDir, n: u32) -> Config {
    let file = d.0.join("tokens");
    let text: String = (1..=n).map(|i| format!("u{i}@x -> {}\n", tok(i))).collect();
    write_mode(&file, &text, 0o600);
    Config::parse(&format!("MACHINE_NAME_PREFIX=devtest\nARENA_STATE_DIR={}\nCLAUDE_TOKENS_FILE={}\n", d.0.join("state").display(), file.display()))
}

#[cfg(unix)]
#[tokio::test]
async fn each_pod_gets_its_own_accounts_token_and_nothing_with_a_token_is_left() {
    let d = tmp("stage");
    let cfg = config_in(&d, 2);
    let pods = [pod("apple", "1", "10.0.0.1"), pod("bloom", "2", "10.0.0.2")];
    let r = Capture::default();
    let out = stage(&r, &cfg, &pods, None, T0).await.unwrap().expect("a pool");
    let lines: Vec<String> = out.iter().map(Outcome::line).collect();
    assert_eq!(lines, ["✓ apple: #1 u1@x (new, by spread)", "✓ bloom: #2 u2@x (new, by spread)"]);

    // apple got #1's token, bloom #2's — one copy each, never the other's.
    let copied = r.copied.lock().unwrap().clone();
    assert_eq!(copied.len(), 2);
    for (host, held) in [("10.0.0.1:22", tok(1)), ("10.0.0.2:22", tok(2))] {
        let mine: Vec<_> = copied.iter().filter(|(h, _, _)| h == host).collect();
        assert_eq!(mine.len(), 1, "{host}");
        assert_eq!(mine[0].2, format!("{held}\n"), "{host}");
        assert!(mine[0].1.starts_with(".arena-claude-token-"), "into $HOME, a name of its own");
    }
    // No token in any command: it went by scp only.
    for c in r.fake.calls() {
        if let RemoteCall::Exec { cmd, .. } = c {
            assert!(!cmd.contains("sk-ant"), "{cmd}");
        }
    }
    // The local copies are gone, their private dir with them.
    let locals = r.locals.lock().unwrap().clone();
    assert_eq!(locals.len(), 2);
    for l in &locals {
        assert!(!l.exists() && !l.parent().unwrap().exists(), "{}", l.display());
    }

    // Recorded: assigned and pushed, with the token's fingerprint.
    let s = load(&state_path(&cfg).unwrap()).unwrap();
    let a = s.get("hetzner", "1").unwrap();
    assert_eq!(a.account, acct("u1@x"));
    assert_eq!(a.pushed.as_ref().map(|p| (p.account.clone(), p.fp.clone())), Some((acct("u1@x"), Some(fingerprint(&tok(1))))));
    let p = load_pool(&cfg).unwrap();
    assert!(!out_of_date(a, &p));

    // Again: the same accounts (nothing new), pushed again.
    let r2 = Capture::default();
    let again = stage(&r2, &cfg, &pods, None, T0 + 60).await.unwrap().unwrap();
    assert_eq!(again.iter().map(Outcome::line).collect::<Vec<_>>(), ["✓ apple: #1 u1@x", "✓ bloom: #2 u2@x"]);
}

#[cfg(unix)]
#[tokio::test]
async fn a_failed_copy_cleans_up_and_records_nothing_and_off_sends_no_file() {
    let d = tmp("stage-fail");
    let cfg = config_in(&d, 2);
    let pods = [pod("apple", "1", "10.0.0.1"), pod("bloom", "2", "10.0.0.2")];
    let r = Capture::default();
    r.fake.script("10.0.0.2:22", [crate::remote::FakeReply::exit(1, "scp: disk full")]);
    let out = stage(&r, &cfg, &pods, None, T0).await.unwrap().unwrap();
    assert_eq!(out[1].line(), "✗ bloom: #2 u2@x (new, by spread) — copying the token: scp: disk full");
    let bloom: Vec<RemoteCall> = r.fake.calls_to("10.0.0.2:22");
    assert!(matches!(&bloom[1], RemoteCall::Exec { cmd, .. } if cmd.starts_with("rm -f \"$HOME/.arena-claude-token-")), "{bloom:?}");
    assert_eq!(bloom.len(), 2, "no push command after a failed copy");
    let s = load(&state_path(&cfg).unwrap()).unwrap();
    assert!(s.get("hetzner", "1").unwrap().pushed.is_some());
    assert!(s.get("hetzner", "2").unwrap().pushed.is_none(), "a failed push isn't recorded");

    // apple off: only the command (no file), which takes the token off; recorded as none.
    let path = state_path(&cfg).unwrap();
    let pool = load_pool(&cfg).unwrap();
    let ms = [Machine::new(&pods[0].0, &Owners::new())];
    let (s, changes) = record(&path, |s| assign(s, &pool, &Owners::new(), &ms, &Want::Pin(None), T0 + 1)).unwrap();
    let r = Capture::default();
    let out = push_all(&r, &path, &pool, &s, &pods[..1], &changes, T0 + 1).await.unwrap();
    assert_eq!(out[0].line(), "✓ apple: token taken off (pinned)");
    assert!(r.copied.lock().unwrap().is_empty());
    assert!(matches!(&r.fake.calls()[..], [RemoteCall::Exec { cmd, .. }] if cmd.contains("ARENA_CC=;")));
    let a = load(&path).unwrap().get("hetzner", "1").cloned().unwrap();
    assert_eq!(a.pushed.map(|p| p.account), Some(None));
}

#[cfg(unix)]
#[tokio::test]
async fn cc_token_goes_to_every_pod_and_moves_no_one() {
    let d = tmp("stage-flag");
    let cfg = config_in(&d, 2);
    let pods = [pod("apple", "1", "10.0.0.1"), pod("bloom", "2", "10.0.0.2")];
    // apple already on #2.
    let (s, _) = record(&state_path(&cfg).unwrap(), |s| {
        assign(s, &load_pool(&cfg).unwrap(), &Owners::new(), &[Machine::new(&pods[0].0, &Owners::new())], &Want::Pin(acct("u2@x")), T0)
    })
    .unwrap();
    let r = Capture::default();
    let out = stage(&r, &cfg, &pods, Some(&tok(5)), T0 + 1).await.unwrap().unwrap();
    assert!(out.iter().all(|o| o.result.is_ok() && o.label.as_deref() == Some("#1 --cc-token")), "{out:?}");
    assert!(r.copied.lock().unwrap().iter().all(|(_, _, held)| *held == format!("{}\n", tok(5))));
    let after = load(&state_path(&cfg).unwrap()).unwrap();
    assert_eq!(after.get("hetzner", "1").unwrap().account, s.get("hetzner", "1").unwrap().account, "no one moved");
    assert!(after.get("hetzner", "2").is_none(), "an unassigned pod stays so: its next push spreads it");
    assert!(out_of_date(after.get("hetzner", "1").unwrap(), &load_pool(&cfg).unwrap()), "apple has --cc-token's, not #2's");
}
