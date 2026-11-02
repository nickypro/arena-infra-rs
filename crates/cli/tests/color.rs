//! Colour only on a terminal (docs/OUTPUT.md): the built `arena`, failing on a config that isn't
//! there — its `Error:` is red on a terminal — piped, with `NO_COLOR`, with `--color`, and (where
//! util-linux's `script` can give it one) on a terminal.

use std::process::{Command, Stdio};

const ESC: char = '\x1b';

/// `arena <args>` with stdout and stderr piped and the environment cleared but for `env`: what
/// it wrote to stderr.
fn piped(args: &[&str], env: &[(&str, &str)]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_arena"))
        .args(args)
        .env_clear()
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .output()
        .expect("arena runs");
    assert!(!out.status.success(), "the config isn't there: {out:?}");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The same on a terminal (`script` gives it one): what the terminal got, both streams. `None`
/// without `script`.
fn on_terminal(args: &str, env: &[(&str, &str)]) -> Option<String> {
    let has_script = Command::new("sh").args(["-c", "command -v script"]).output().is_ok_and(|o| o.status.success());
    if !has_script {
        eprintln!("no `script` here: the terminal half of this test is skipped");
        return None;
    }
    let cmd = format!("{} {args}", env!("CARGO_BIN_EXE_arena"));
    let out = Command::new("script")
        .args(["-qec", &cmd, "/dev/null"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .output()
        .expect("script runs");
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn colour_is_off_in_a_pipe_and_with_no_color_and_on_on_a_terminal() {
    let missing = std::env::temp_dir().join(format!("arena-color-test-{}/config.env", std::process::id()));
    let config = missing.to_string_lossy().into_owned();
    let args = ["--config", config.as_str(), "pods", "list"];
    let term = [("TERM", "xterm-256color")];

    // A pipe: the same words, no escape codes.
    let plain = piped(&args, &term);
    assert!(plain.starts_with("Error: "), "{plain:?}");
    assert!(!plain.contains(ESC), "a pipe gets no colour: {plain:?}");
    // --color always: colour even there; --color never: none.
    let always = piped(&[&["--color", "always"][..], &args].concat(), &term);
    assert!(always.contains(ESC), "{always:?}");
    assert_eq!(always.replace("\x1b[31m", "").replace("\x1b[1m", "").replace("\x1b[22m", "").replace("\x1b[39m", ""), plain);

    // A terminal: colour, unless NO_COLOR (or --color never, or TERM=dumb).
    let line = format!("--config {config} pods list");
    if let Some(tty) = on_terminal(&line, &term) {
        assert!(tty.contains("Error:") && tty.contains(ESC), "a terminal gets colour: {tty:?}");
        let no_color = on_terminal(&line, &[("TERM", "xterm-256color"), ("NO_COLOR", "1")]).unwrap();
        assert!(no_color.contains("Error:") && !no_color.contains(ESC), "NO_COLOR: {no_color:?}");
        let never = on_terminal(&format!("--color never {line}"), &term).unwrap();
        assert!(!never.contains(ESC), "--color never: {never:?}");
        let dumb = on_terminal(&line, &[("TERM", "dumb")]).unwrap();
        assert!(!dumb.contains(ESC), "TERM=dumb: {dumb:?}");
    }
}
