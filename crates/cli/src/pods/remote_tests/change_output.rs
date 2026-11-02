//! Golden tests of `pods cp`, `pods run` and `pods test`'s output (docs/OUTPUT.md), at default
//! and at `-v`, through the commands against a fake fleet and a scripted `FakeRemote`. `-v` is
//! what each printed before.

use super::*;
use crate::output_test::rendered;

/// `pods <args>` at default and at `-v`, each on a fresh `remote()`, the prompt answered `y`.
async fn both(args: &[&str], f: &Fleet, remote: impl Fn() -> FakeRemote) -> (String, String) {
    let (mut out, cfg) = (Vec::new(), cfg());
    for verbose in [false, true] {
        let run = handle_pods(pods(args), f, Arc::new(remote()), &cfg, false);
        out.push(rendered(verbose, &["y"], run).await);
    }
    let loud = out.pop().unwrap();
    (out.pop().unwrap(), loud)
}

/// cp: the type, both paths and the pods in one question (the 4-line preview is `-v`'s), a line
/// for the pods it reached and one per pod it didn't, with the command that copies it again.
#[tokio::test(start_paused = true)]
async fn cp_says_what_goes_where_in_a_line() {
    let file = TmpFile(std::env::temp_dir().join(format!("arena-cp-golden-{}.txt", std::process::id())));
    std::fs::write(&file.0, "hello").unwrap();
    let local = file.0.to_string_lossy().into_owned();
    let f = fleet_of(&[("apple", 22001), ("bloom", 22002)]);
    let remote = || {
        let fake = FakeRemote::new();
        fake.script(&host(22001), [FakeReply::ok(), FakeReply::ok(), FakeReply::stdout("OK 5\n")]);
        fake.script(&host(22002), [FakeReply::ok(), FakeReply::exit(1, "scp: /root/x/hello.txt: No space left on device")]);
        fake
    };
    let (quiet, loud) = both(&["cp", &local, "/root/x/hello.txt", "-t", "apple", "-t", "bloom"], &f, remote).await;
    let want = format!(
        "Copy file {local} → /root/x/hello.txt on devtest-apple, devtest-bloom? [y/N] y\n\
         ✗ devtest-bloom: scp: /root/x/hello.txt: No space left on device (retry: arena pods cp {local} /root/x/hello.txt -t devtest-bloom)\n\
         ✓ copied {local} → /root/x/hello.txt to devtest-apple\n\
         Error: 1 pod(s) failed to receive the file\n"
    );
    assert_eq!(quiet, want, "default:\n{quiet}");
    let want = format!(
        "Copy preview (same on every pod):\n  input  [file — check the type!]: {local}\n  output [file]:                   /root/x/hello.txt\n  \
         budget: 600s per pod (scales with size × pods; --timeout to change)\n\
         Will copy {local} to /root/x/hello.txt on 2 pod(s): devtest-apple, devtest-bloom\nProceed? [y/N] y\n\
         [1/2] ✓ devtest-apple\n[2/2] ✗ devtest-bloom: scp: /root/x/hello.txt: No space left on device\n\nDone: 1 copied, 1 failed.\n\
         Error: 1 pod(s) failed to receive the file\n"
    );
    assert_eq!(loud, want, "-v:\n{loud}");
    let (quiet, loud) = both(&["cp", &local, "/root/x/hello.txt", "-t", "apple", "--dry-run"], &f, FakeRemote::new).await;
    assert_eq!(quiet, format!("[dry-run] would copy file {local} → /root/x/hello.txt on devtest-apple\n"), "default:\n{quiet}");
    assert!(loud.starts_with("Copy preview (same on every pod):\n") && loud.contains("\nDry-run — would scp to 1 pod(s):\n\n# devtest-apple\nscp "), "-v:\n{loud}");
    assert!(loud.ends_with("\n\nPreview only — run without --dry-run to copy.\n"), "-v:\n{loud}");
}

/// run: one question naming the command and every pod (the shell it's wrapped in is `-v`'s),
/// each pod's output as before, and the tally only for several pods (or with `-v`).
#[tokio::test]
async fn run_says_the_command_and_the_pods_in_a_line() {
    let f = fleet_of(&[("apple", 22001), ("bloom", 22002)]);
    let remote = || {
        let fake = FakeRemote::new();
        fake.on(&host(22001), "hostname", FakeReply::stdout("apple-host\n"));
        fake.on(&host(22002), "hostname", FakeReply::stdout("bloom-host\n"));
        fake
    };
    let (quiet, loud) = both(&["run", "-t", "apple", "hostname"], &f, remote).await;
    assert_eq!(quiet, "Run `hostname` on devtest-apple? [y/N] y\n\n── devtest-apple \napple-host\n", "default:\n{quiet}");
    assert_eq!(loud, "Run `hostname` on 1 pod(s) over SSH: devtest-apple\nProceed? [y/N] y\n\n── devtest-apple \napple-host\n\n1 ok, 0 failed\n", "-v:\n{loud}");
    let (quiet, _) = both(&["run", "-t", "apple", "-t", "bloom", "hostname"], &f, remote).await;
    assert_eq!(
        quiet,
        "Run `hostname` on devtest-apple, devtest-bloom? [y/N] y\n\n── devtest-apple \napple-host\n\n── devtest-bloom \nbloom-host\n\n2 ok, 0 failed\n",
        "default:\n{quiet}"
    );
    let (quiet, loud) = both(&["run", "-t", "apple", "--dry-run", "hostname"], &f, remote).await;
    assert_eq!(quiet, "[dry-run] would run `hostname` on devtest-apple\n", "default:\n{quiet}");
    assert!(loud.starts_with("[dry-run] would run on 1 pod(s) (1800s budget each): devtest-apple\n  "), "-v:\n{loud}");
}

/// `pods test` (plain): a row per pod as before; the tally only for several (or with `-v`).
#[tokio::test]
async fn pods_test_is_a_row_per_pod() {
    let f = fleet_of(&[("apple", 22001)]);
    let remote = || {
        let fake = FakeRemote::new();
        fake.on(&host(22001), "torch", FakeReply::stdout("2.4.0\n"));
        fake
    };
    let (quiet, loud) = both(&["test", "apple"], &f, remote).await;
    assert_eq!(quiet, "devtest-apple          2.4.0\n", "default:\n{quiet}");
    assert_eq!(loud, "devtest-apple          2.4.0\n\n1 ok, 0 failed\n", "-v:\n{loud}");
}
