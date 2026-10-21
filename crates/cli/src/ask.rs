//! `pods ask`: ask claude or codex about the selected pods' current state.
//!
//! The work is `arena_core::ask`'s (the bundles, the masking, the prompt, the no-tools run, the
//! saved runs); here is the command's shape: the question among the targets, the dry run that
//! prints exactly the prompt and sends nothing, the first-use note, the answer on stdout and
//! everything else on stderr (so `arena pods ask … > answer.txt` keeps only the answer).

use std::future::Future;
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
/// knows every switch that takes its tools away — and codex only once that list, with them, shows
/// nothing else on that isn't known to give it no tool (it then also turns off each web search
/// feature, and each of `CODEX_OFF_WHEN_LISTED`, it lists).
fn ai_command_line(ai: Ai, model: Option<&str>) -> String {
    let args = match ai {
        Ai::Claude => ai::claude_args(model),
        Ai::Codex => {
            let tools: Vec<String> = ai::CODEX_TOOL_FEATURES.iter().map(|f| f.to_string()).collect();
            ai::codex_args(std::path::Path::new("<empty temp dir>"), std::path::Path::new("<answer file>"), model, &tools)
        }
    };
    let words: Vec<String> = args.iter().map(|a| arena_core::ssh::shell_word(a)).collect();
    let more = match ai {
        Ai::Claude => "",
        Ai::Codex => {
            " and, with them, shows nothing else on that isn't known to give it no tool (each web search feature, and each \
             other one arena_core::ask::ai::CODEX_OFF_WHEN_LISTED names, that it lists is disabled too)"
        }
    };
    format!(
        "{} {}` — once `{} {}` confirms it knows each of those no-tools switches{more}",
        ai.name(),
        words.join(" "),
        ai.name(),
        ai::discovery_args(ai).join(" ")
    )
}

/// What stops the AI while it answers: the operator's Ctrl-C (`ctrl_c`, the command's latched
/// one), a hangup — the SSH session to this box dropped — or a TERM (`kill`, `timeout`): review
/// ai-review claude #2, either signal killed arena outright, and the AI, in a session of its own,
/// ran on as root with no timeout. Handled like Ctrl-C instead: `pods ask` stops the AI and what it
/// started, says why and saves the run. Both are listened for from this call on (registered here,
/// not on a first poll, so none slips in before). Resolves to the cause, for the message.
pub(crate) fn stop_signals(ctrl_c: impl Future<Output = ()>) -> impl Future<Output = &'static str> {
    use tokio::signal::unix::{signal, Signal, SignalKind};
    let (hangup, term) = (signal(SignalKind::hangup()).ok(), signal(SignalKind::terminate()).ok());
    // A listener's next signal; never, without one (or once its driver is gone).
    async fn next(listener: Option<Signal>) {
        if let Some(mut s) = listener {
            if s.recv().await.is_some() {
                return;
            }
        }
        std::future::pending::<()>().await
    }
    async move {
        tokio::select! {
            () = ctrl_c => "Ctrl-C",
            () = next(hangup) => "a hangup",
            () = next(term) => "SIGTERM",
        }
    }
}

/// `pods ask`: every selected pod's bundle (stopped and unreachable ones from their listing)
/// in one prompt, sent to `run.ai` with no tools; the answer on `out`, progress and notes on
/// `note`. A dry run prints the exact prompt and sends — and saves — nothing. A real run saves
/// the prompt and the answer (or why there is none) under the fleet's state dir and fails when
/// the AI did. `stop` (listened for once the AI is started: [`stop_signals`] — Ctrl-C, a hangup,
/// a TERM) stops the AI, its cause in the message — the terminal's own signals don't reach it, in
/// a session of its own (`arena_core::remote::run_local_input`).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_ask<S: Future<Output = &'static str>>(
    remote: Arc<dyn Remote>,
    cfg: &Config,
    sel: &Selected,
    run: AskRun,
    env: &AiEnv,
    stop: impl FnOnce() -> S,
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
    let stop = stop();
    let outcome = tokio::select! {
        outcome = ai::ask(run.ai, &prompt, run.model.as_deref(), run.timeout, env) => outcome,
        why = stop => Err(format!("interrupted ({why}): {} was stopped", run.ai.name())),
    };
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

    /// A Ctrl-C that never comes.
    fn never() -> std::future::Pending<&'static str> {
        std::future::pending()
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
        handle_ask(fake_alpha(), &c, &sel, run(Ai::Claude, true, Some("anything odd?")), &env, never, &mut |l| out.push(l.to_string()), &mut |l| notes.push(l.to_string()))
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
        handle_ask(fake_alpha(), &c, &sel, run(Ai::Codex, false, None), &env, never, &mut |l| out.push(l.to_string()), &mut |l| notes.push(l.to_string()))
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
        handle_ask(fake_alpha(), &c, &sel, run(Ai::Claude, false, Some("ok?")), &env, never, &mut |l| out.push(l.to_string()), &mut |l| notes.push(l.to_string()))
            .await
            .unwrap();
        assert_eq!(out, ["ANSWER from claude"]);
        assert!(!notes.iter().any(|n| n.starts_with("note:")), "{notes:?}");
    }

    /// Ctrl-C while the AI is thinking (review ai-review claude #4): the AI runs in a session of
    /// its own, out of the terminal's reach, so `pods ask` stops it — and what it started — and
    /// says so; the run is saved with why there's no answer.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn ctrl_c_stops_the_ai_and_what_it_started() {
        use arena_core::ask::ai::fixtures::{claude_help, on_path, stub};
        let t = Tmp::new("ctrl-c");
        let bin = t.0.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let (leader, left) = (t.0.join("leader.pid"), t.0.join("left.pid"));
        stub(
            &bin,
            "claude",
            &format!(
                "if [ \"$1\" = --help ]; then cat <<'EOF'\n{}EOF\nexit 0; fi\nsleep 60 & echo $! > {}\necho $$ > {}\ncat > /dev/null\nexec sleep 60",
                claude_help(&[]),
                left.display(),
                leader.display()
            ),
        );
        let env = AiEnv { path: Some(on_path(&bin)), home: None, tmp: t.0.clone(), vars: Vec::new() };
        let c = cfg(&t.0.join("state"));
        let sel = Selected::all(vec![pod("alpha", Some(22001))]);
        let leader_seen = leader.clone();
        let ctrl_c = move || async move {
            while !leader_seen.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            "Ctrl-C"
        };
        let started = Instant::now();
        let mut notes = Vec::new();
        let e = handle_ask(fake_alpha(), &c, &sel, run(Ai::Claude, false, None), &env, ctrl_c, &mut |_| {}, &mut |l| notes.push(l.to_string())).await.unwrap_err();
        assert!(e.to_string().contains("interrupted (Ctrl-C): claude was stopped"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
        let saved = notes.iter().find_map(|n| n.strip_prefix("saved: ")).expect("saved");
        assert_eq!(std::fs::read_to_string(std::path::Path::new(saved).join("answer.md")).unwrap(), "(no answer: interrupted (Ctrl-C): claude was stopped)\n");
        let alive = |p: &std::path::Path| {
            let pid = std::fs::read_to_string(p).unwrap().trim().to_string();
            std::fs::read_to_string(format!("/proc/{pid}/stat")).ok().and_then(|s| s.rsplit(')').next().and_then(|r| r.split_whitespace().next()).map(|st| st != "Z" && st != "X")).unwrap_or(false)
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while (alive(&leader) || alive(&left)) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!alive(&leader) && !alive(&left), "the AI, or what it started, is still running");
    }

    /// Review ai-review claude #2 (lane ASK7 item H): a hangup (the operator's SSH session to this
    /// box dropped) or a TERM stops the AI like Ctrl-C — `pods ask` says so, the AI and what it
    /// started are gone, the run is saved — where either signal used to kill arena outright and
    /// leave the AI, in a session of its own, running. The signal is sent to this test process
    /// itself once the stub AI runs; `stop_signals` (what `pods ask` listens with) has its handler
    /// registered by then, and one is made first here so no stray signal can end the test binary.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_hangup_or_a_term_stops_the_ai_like_ctrl_c() {
        use arena_core::ask::ai::fixtures::{claude_help, on_path, stub};
        use tokio::signal::unix::{signal, SignalKind};
        let _held = (signal(SignalKind::hangup()).unwrap(), signal(SignalKind::terminate()).unwrap());
        for (sig, why) in [("-HUP", "a hangup"), ("-TERM", "SIGTERM")] {
            let t = Tmp::new(&format!("signal{sig}"));
            let bin = t.0.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let (leader, left) = (t.0.join("leader.pid"), t.0.join("left.pid"));
            stub(
                &bin,
                "claude",
                &format!(
                    "if [ \"$1\" = --help ]; then cat <<'EOF'\n{}EOF\nexit 0; fi\nsleep 60 & echo $! > {}\necho $$ > {}\ncat > /dev/null\nexec sleep 60",
                    claude_help(&[]),
                    left.display(),
                    leader.display()
                ),
            );
            let env = AiEnv { path: Some(on_path(&bin)), home: None, tmp: t.0.clone(), vars: Vec::new() };
            let c = cfg(&t.0.join("state"));
            let sel = Selected::all(vec![pod("alpha", Some(22001))]);
            let leader_seen = leader.clone();
            let stop = move || {
                let stopping = stop_signals(std::future::pending());
                tokio::spawn(async move {
                    while !leader_seen.exists() {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let sent = std::process::Command::new("kill").args([sig, &std::process::id().to_string()]).status().unwrap();
                    assert!(sent.success());
                });
                stopping
            };
            let started = Instant::now();
            let mut notes = Vec::new();
            let e = handle_ask(fake_alpha(), &c, &sel, run(Ai::Claude, false, None), &env, stop, &mut |_| {}, &mut |l| notes.push(l.to_string())).await.unwrap_err();
            assert_eq!(e.to_string(), format!("interrupted ({why}): claude was stopped"));
            assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
            let saved = notes.iter().find_map(|n| n.strip_prefix("saved: ")).expect("saved");
            assert_eq!(std::fs::read_to_string(std::path::Path::new(saved).join("answer.md")).unwrap(), format!("(no answer: interrupted ({why}): claude was stopped)\n"));
            let alive = |p: &std::path::Path| {
                let pid = std::fs::read_to_string(p).unwrap().trim().to_string();
                std::fs::read_to_string(format!("/proc/{pid}/stat")).ok().and_then(|s| s.rsplit(')').next().and_then(|r| r.split_whitespace().next()).map(|st| st != "Z" && st != "X")).unwrap_or(false)
            };
            let deadline = Instant::now() + Duration::from_secs(5);
            while (alive(&leader) || alive(&left)) && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert!(!alive(&leader) && !alive(&left), "{why}: the AI, or what it started, is still running");
        }
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
        let e = handle_ask(fake_alpha(), &c, &sel, run(Ai::Claude, false, None), &env, never, &mut |_| {}, &mut |l| notes.push(l.to_string())).await.unwrap_err();
        assert!(e.to_string().contains("claude not found") && e.to_string().contains("--ai codex"), "{e}");
        let saved = notes.iter().find_map(|n| n.strip_prefix("saved: ")).expect("saved");
        assert!(std::fs::read_to_string(std::path::Path::new(saved).join("answer.md")).unwrap().starts_with("(no answer: claude not found"));
    }
}
