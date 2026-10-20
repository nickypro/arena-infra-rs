//! `pods ask`: ask claude or codex about the selected pods' current state.
//!
//! The work is `arena_core::ask`'s (the bundles, the masking, the prompt, the no-tools run, the
//! saved runs); here is the command's shape: the question among the targets, the dry run that
//! prints exactly the prompt and sends nothing, the first-use note, the answer on stdout and
//! everything else on stderr (so `arena pods ask … > answer.txt` keeps only the answer).

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use arena_core::ask::{self, ai, Ai, AiEnv};
use arena_core::remote::Remote;
use arena_core::Config;

use super::Selected;

/// Where a handler's lines go: stdout/stderr for real, a recorder in tests.
pub(crate) type Say<'a> = &'a mut dyn FnMut(&str);

/// Whether a target word is the question instead: it has a space or a `?`, which no pod name,
/// id, range or selector flag has.
fn is_question(word: &str) -> bool {
    word.contains(char::is_whitespace) || word.contains('?')
}

/// Split `pods ask`'s positional words into the targets and the question (at most one — quote
/// it), `-q` being the other way to give it.
pub(crate) fn split_question(words: &[String], flag: Option<String>) -> Result<(Vec<String>, Option<String>), String> {
    let (questions, targets): (Vec<&String>, Vec<&String>) = words.iter().partition(|w| is_question(w));
    let positional = match questions.as_slice() {
        [] => None,
        [q] => Some(q.to_string()),
        _ => return Err("one question at a time — quote it: arena pods ask apple \"why is it slow?\"".into()),
    };
    let question = match (flag, positional) {
        (Some(_), Some(_)) => return Err("the question twice: give it with -q or as a quoted word, not both".into()),
        (q, p) => q.or(p).filter(|q| !q.trim().is_empty()),
    };
    Ok((targets.into_iter().cloned().collect(), question))
}

/// What one `pods ask` was asked to do.
pub(crate) struct AskRun {
    pub ai: Ai,
    pub model: Option<String>,
    pub timeout: Duration,
    pub dry_run: bool,
    /// `None`: [`ask::default_question`].
    pub question: Option<String>,
}

/// The command line the AI runs with, for the dry run (the prompt goes on its stdin), and the
/// check that comes first: it is run only once `claude --help` / `codex features list` shows it
/// knows every switch that takes its tools away.
fn ai_command_line(ai: Ai, model: Option<&str>) -> String {
    let args = match ai {
        Ai::Claude => ai::claude_args(model),
        Ai::Codex => ai::codex_args(std::path::Path::new("<empty temp dir>"), std::path::Path::new("<answer file>"), model),
    };
    let words: Vec<String> = args.iter().map(|a| arena_core::ssh::shell_word(a)).collect();
    format!(
        "{} {}` — once `{} {}` confirms it knows each of those no-tools switches",
        ai.name(),
        words.join(" "),
        ai.name(),
        ai::discovery_args(ai).join(" ")
    )
}

/// `pods ask`: every selected pod's bundle (stopped and unreachable ones from their listing)
/// in one prompt, sent to `run.ai` with no tools; the answer on `out`, progress and notes on
/// `note`. A dry run prints the exact prompt and sends — and saves — nothing. A real run saves
/// the prompt and the answer (or why there is none) under the fleet's state dir and fails when
/// the AI did.
pub(crate) async fn handle_ask(
    remote: Arc<dyn Remote>,
    cfg: &Config,
    sel: &Selected,
    run: AskRun,
    env: &AiEnv,
    out: Say<'_>,
    note: Say<'_>,
) -> Result<()> {
    // Before any pod is read: a secret pattern this build can't apply means no bundle at all.
    ask::redact::ready().map_err(|e| anyhow::anyhow!("can't mask the bundles ({e}) — nothing was read or sent"))?;
    let pods = &sel.pods;
    if pods.is_empty() {
        anyhow::bail!("no pods to ask about");
    }
    let now = arena_core::snapshot::unix_now();
    let question = run.question.clone().unwrap_or_else(|| ask::default_question(pods.len()));
    note(&format!("reading {} pod(s)…", pods.len()));
    let bundles = ask::bundles(&remote, cfg, pods, now).await;
    let prompt = ask::prompt(&question, &bundles, cfg, now);
    if run.dry_run {
        out(&prompt);
        note(&format!(
            "[dry-run] nothing sent: the prompt above ({} bytes) would go to {} on the stdin of `{}",
            prompt.len(),
            run.ai.vendor(),
            ai_command_line(run.ai, run.model.as_deref())
        ));
        return Ok(());
    }
    let runs = ask::runs_dir(cfg);
    let first = match &runs {
        Ok(dir) => ask::first_use(dir),
        Err(_) => true,
    };
    if first {
        note(&ask::privacy_note(run.ai));
    }
    let names: Vec<&str> = pods.iter().map(|p| p.name.as_str()).collect();
    let about = match names.as_slice() {
        [one] => one.to_string(),
        many => format!("{} pods", many.len()),
    };
    note(&format!("asking {} about {about}…", run.ai.name()));
    let started = Instant::now();
    let outcome = ai::ask(run.ai, &prompt, run.model.as_deref(), run.timeout, env).await;
    if let Ok(answer) = &outcome {
        out(answer);
    }
    let saved = runs.and_then(|dir| {
        let record = ask::Run {
            ai: run.ai,
            model: run.model.as_deref(),
            pods,
            prompt: &prompt,
            outcome: &outcome,
            asked_at: now,
            took: started.elapsed(),
        };
        ask::save_run(&dir, &record).map_err(|e| format!("{}: {e}", dir.display()))
    });
    match saved {
        Ok(dir) => note(&format!("saved: {}", dir.display())),
        Err(e) => note(&format!("⚠ the prompt and answer weren't saved: {e}")),
    }
    outcome.map(|_| ()).map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena_core::ask::ai::fixtures::recording_stubs;
    use arena_core::ask::bundle::fixtures::busy;
    use arena_core::remote::FakeRemote;
    use arena_core::Pod;

    fn pod(name: &str, port: Option<u16>) -> Pod {
        Pod {
            id: format!("id-devtest-{name}"),
            name: format!("devtest-{name}"),
            provider: "runpod".into(),
            status: "RUNNING".into(),
            ssh_ip: port.map(|_| "10.0.0.1".into()),
            ssh_port: port,
            ..Default::default()
        }
    }

    /// A temp dir, removed after.
    struct Tmp(std::path::PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Tmp {
            let d = std::env::temp_dir().join(format!("arena-cli-ask-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Tmp(d)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn cfg(state: &std::path::Path) -> Config {
        Config::parse(&format!(
            "MACHINE_NAME_PREFIX=devtest\nSHARED_SSH_KEY_PATH=/nonexistent/devtest_key\nARENA_STATE_DIR={}\nLOCAL_BACKUP_DIR=/nonexistent/b\n",
            state.display()
        ))
    }

    fn fake_alpha() -> Arc<dyn Remote> {
        let fake = FakeRemote::new();
        fake.script("10.0.0.1:22001", [busy("", "", "pytest")]);
        Arc::new(fake)
    }

    fn run(ai: Ai, dry_run: bool, question: Option<&str>) -> AskRun {
        AskRun { ai, model: None, timeout: Duration::from_secs(20), dry_run, question: question.map(String::from) }
    }

    #[test]
    fn the_question_is_the_word_with_a_space_or_a_question_mark() {
        let w = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let ok = |words: &[&str], flag: Option<&str>| split_question(&w(words), flag.map(String::from)).unwrap();
        assert_eq!(ok(&["apple", "bloom"], None), (w(&["apple", "bloom"]), None));
        assert_eq!(ok(&["apple", "why is it slow"], None), (w(&["apple"]), Some("why is it slow".into())));
        assert_eq!(ok(&["status?", "apple..cloud"], None), (w(&["apple..cloud"]), Some("status?".into())));
        assert_eq!(ok(&["apple"], Some("disk?")), (w(&["apple"]), Some("disk?".into())));
        assert_eq!(ok(&["apple"], Some("  ")), (w(&["apple"]), None), "blank = the default");
        assert!(split_question(&w(&["a b", "c d"]), None).unwrap_err().contains("one question"));
        assert!(split_question(&w(&["a b"]), Some("x".into())).unwrap_err().contains("not both"));
    }

    /// `--dry-run` prints exactly the prompt — every pod's bundle, the question — and never
    /// runs the AI (a recording stub on its PATH stays untouched) or saves anything.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_dry_run_prints_the_prompt_and_sends_nothing() {
        let t = Tmp::new("dry");
        let env = recording_stubs(&t.0);
        let c = cfg(&t.0.join("state"));
        let sel = Selected::all(vec![pod("alpha", Some(22001)), pod("bravo", None)]);
        let (mut out, mut notes) = (Vec::new(), Vec::new());
        handle_ask(fake_alpha(), &c, &sel, run(Ai::Claude, true, Some("anything odd?")), &env, &mut |l| out.push(l.to_string()), &mut |l| notes.push(l.to_string()))
            .await
            .unwrap();
        assert_eq!(out.len(), 1, "the prompt, once");
        let prompt = &out[0];
        assert!(prompt.contains("The operator's question: anything odd?") && prompt.contains("## devtest-alpha") && prompt.contains("## devtest-bravo"), "{prompt}");
        assert!(notes.last().unwrap().starts_with("[dry-run] nothing sent: the prompt above"), "{notes:?}");
        assert!(
            notes.last().unwrap().contains(
                "to Anthropic on the stdin of `claude -p --safe-mode --tools '' --strict-mcp-config --mcp-config '{\"mcpServers\":{}}' \
                 --disable-slash-commands --no-session-persistence --output-format text` — once `claude --help` confirms it knows each of \
                 those no-tools switches"
            ),
            "{notes:?}"
        );
        assert!(!t.0.join("rec").join("claude.argv").exists() && !t.0.join("rec").join("claude.stdin").exists(), "the AI ran");
        assert!(!t.0.join("state").exists(), "a dry run saves nothing");
    }

    /// A real run: the note on first use only, the stub gets the very prompt a dry run prints,
    /// the answer goes to `out`, the run is saved under the state dir.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_run_sends_the_prompt_prints_the_answer_and_saves_both() {
        let t = Tmp::new("run");
        let env = recording_stubs(&t.0);
        let c = cfg(&t.0.join("state"));
        let sel = Selected::all(vec![pod("alpha", Some(22001))]);
        let (mut out, mut notes) = (Vec::new(), Vec::new());
        handle_ask(fake_alpha(), &c, &sel, run(Ai::Codex, false, None), &env, &mut |l| out.push(l.to_string()), &mut |l| notes.push(l.to_string()))
            .await
            .unwrap();
        assert_eq!(out, ["ANSWER from codex"]);
        assert!(notes.iter().any(|n| n.starts_with("note: `pods ask` sends") && n.contains("to OpenAI through your `codex` login")), "{notes:?}");
        assert!(notes.iter().any(|n| n == "asking codex about devtest-alpha…"), "{notes:?}");
        let sent = std::fs::read_to_string(t.0.join("rec").join("codex.stdin")).unwrap();
        assert!(sent.contains("What is the current state of this pod?") && sent.contains("## devtest-alpha"), "{sent}");
        let saved = notes.iter().find_map(|n| n.strip_prefix("saved: ")).expect("saved");
        let dir = std::path::Path::new(saved);
        assert!(dir.starts_with(t.0.join("state").join("devtest").join("ask")), "{saved}");
        assert_eq!(std::fs::read_to_string(dir.join("prompt.txt")).unwrap(), sent);
        assert_eq!(std::fs::read_to_string(dir.join("answer.md")).unwrap(), "ANSWER from codex\n");

        // The second time: no note.
        let (mut out, mut notes) = (Vec::new(), Vec::new());
        handle_ask(fake_alpha(), &c, &sel, run(Ai::Claude, false, Some("ok?")), &env, &mut |l| out.push(l.to_string()), &mut |l| notes.push(l.to_string()))
            .await
            .unwrap();
        assert_eq!(out, ["ANSWER from claude"]);
        assert!(!notes.iter().any(|n| n.starts_with("note:")), "{notes:?}");
    }

    /// The AI missing: an error naming the other one — and the failed run is still saved.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_missing_ai_fails_naming_the_other() {
        let t = Tmp::new("missing");
        let env = AiEnv { path: Some(t.0.clone().into_os_string()), home: None, tmp: t.0.clone(), vars: Vec::new() };
        let c = cfg(&t.0.join("state"));
        let sel = Selected::all(vec![pod("alpha", Some(22001))]);
        let mut notes = Vec::new();
        let e = handle_ask(fake_alpha(), &c, &sel, run(Ai::Claude, false, None), &env, &mut |_| {}, &mut |l| notes.push(l.to_string())).await.unwrap_err();
        assert!(e.to_string().contains("claude not found") && e.to_string().contains("--ai codex"), "{e}");
        let saved = notes.iter().find_map(|n| n.strip_prefix("saved: ")).expect("saved");
        assert!(std::fs::read_to_string(std::path::Path::new(saved).join("answer.md")).unwrap().starts_with("(no answer: claude not found"));
    }
}
