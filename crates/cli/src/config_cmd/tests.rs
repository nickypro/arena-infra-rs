/// `config check` says which RunPod API is selected — v2, "(default)" when RUNPOD_API is
/// unset or empty — warns on an explicit v1 with the retirement date and the way back,
/// and fails on a value that's neither.
#[test]
fn config_check_runpod_api_row() {
    use super::runpod_api_row;
    use arena_core::Config;
    let key = format!("{:<24}", "RUNPOD_API");
    for text in ["RUNPOD_API_KEY=k", "RUNPOD_API=", "RUNPOD_API=\"  \""] {
        let row = runpod_api_row(&Config::parse(text));
        assert_eq!(row, (format!("  ✓ {key} v2 (default) (api.runpod.io/v2)"), true), "{text:?}");
    }
    for text in ["RUNPOD_API=v2", "RUNPOD_API=V2"] {
        let row = runpod_api_row(&Config::parse(text));
        assert_eq!(row, (format!("  ✓ {key} v2 (api.runpod.io/v2)"), true), "{text:?}");
    }
    for text in ["RUNPOD_API=v1", "RUNPOD_API=\"V1\""] {
        let row = runpod_api_row(&Config::parse(text));
        let want = format!(
            "  ⚠ {key} v1 (rest.runpod.io/v1) — RunPod retires REST v1 on 2026-11-16; remove RUNPOD_API=v1 (v2 is the default)"
        );
        assert_eq!(row, (want, true), "{text:?}: a warning, not a failure");
    }
    let (line, ok) = runpod_api_row(&Config::parse("RUNPOD_API=v3"));
    assert!(!ok && line.contains("✗") && line.contains("must be `v1` or `v2`"), "{line}");
    // `config check` never shows secret values — not even a key pasted into RUNPOD_API.
    let (line, ok) = runpod_api_row(&Config::parse("RUNPOD_API=rpa_SECRETKEY123"));
    assert!(!ok && line.contains("✗") && !line.contains("SECRETKEY"), "{line}");
}

/// The whole `config check` of a RunPod config without RUNPOD_API: v2, said to be the
/// default, and a CUDA floor above 13.0 is fine (v2 sends a real floor). The same config
/// with an explicit `RUNPOD_API=v1` still passes that row (⚠, not ✗) but fails on the
/// floor v1 can't ask for — both saying how to get back to v2.
#[test]
fn config_check_without_runpod_api_is_v2_and_an_explicit_v1_warns() {
    use super::config_report;
    use arena_core::Config;
    let base = "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple)\nRUNPOD_API_KEY=not-a-real-key\n\
                GPU_TYPE=\"NVIDIA RTX A4000\"\nIMAGE=img:1\nMIN_CUDA_VERSION=13.1\n";
    let row = |report: &str| report.lines().find(|l| l.contains("RUNPOD_API ")).unwrap_or_default().trim_start().to_string();
    let (report, missing) = config_report(&Config::parse(base), "runpod");
    assert!(row(&report).starts_with("✓ RUNPOD_API") && row(&report).ends_with(" v2 (default) (api.runpod.io/v2)"), "{report}");
    assert!(report.contains("✓ host CUDA") && report.contains("≥ 13.1 (a floor, from MIN_CUDA_VERSION)"), "{report}");
    assert!(missing.is_empty() && !report.contains("rest.runpod.io/v1"), "{missing:?}\n{report}");
    let (report, missing) = config_report(&Config::parse(&format!("{base}RUNPOD_API=v1\n")), "runpod");
    assert!(
        row(&report).starts_with("⚠ RUNPOD_API")
            && row(&report).ends_with(" v1 (rest.runpod.io/v1) — RunPod retires REST v1 on 2026-11-16; remove RUNPOD_API=v1 (v2 is the default)"),
        "{report}"
    );
    assert!(report.contains("✗ host CUDA") && report.contains("can't ask for it"), "{report}");
    assert_eq!(missing, ["a host CUDA constraint RunPod v1 can express, or remove RUNPOD_API=v1 (v2 is the default)"]);
}

/// `config check`'s ASK_AI rows: the AI (and `(default)` when unset) with where its data goes,
/// `off`, where the binary is or a ⚠ naming the other option; a bad value is ✗ and the error
/// — a pasted key never echoed.
#[test]
fn config_check_ask_ai_rows() {
    use super::{ask_ai_rows, config_report};
    use arena_core::Config;
    let dir = std::env::temp_dir().join(format!("arena-cli-askrow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let empty = arena_core::ask::AiEnv { path: Some(dir.clone().into_os_string()), home: None, tmp: dir.clone(), vars: Vec::new() };
    let key = format!("{:<28}", "ASK_AI");
    let rows = |text: &str| ask_ai_rows(&Config::parse(text), &empty);
    assert_eq!(
        rows(""),
        (
            vec![
                format!("  · {key} claude (default) — `pods ask` sends pods' state (keys masked) to Anthropic; `off` turns it off"),
                format!("  ⚠ {:<28} claude not found on PATH or in ~/.local/bin — install it and log in, or use codex instead (`--ai codex`, or ASK_AI=codex)", "claude binary"),
            ],
            vec![]
        )
    );
    assert!(rows("ASK_AI=codex").0[0].starts_with(&format!("  ✓ {key} codex — `pods ask` sends pods' state (keys masked) to OpenAI")));
    assert_eq!(rows("ASK_AI=off"), (vec![format!("  ✓ {key} off — `pods ask` and the dashboard's `?` refuse: nothing goes to an AI")], vec![]));
    let (r, e) = rows("ASK_AI=gpt");
    assert_eq!((r, e), (vec![format!("  ✗ {key} ASK_AI must be claude, codex or off (got gpt)")], vec!["ASK_AI must be claude, codex or off (got gpt)".to_string()]));
    // Each AI's model and effort, where set: shown, or ✗ and failed on.
    let (r, e) = rows("ASK_AI=off\nASK_CODEX_MODEL=gpt-6-luna\nASK_CODEX_EFFORT=LOW\nASK_CLAUDE_EFFORT=fast\nASK_CLAUDE_MODEL=\n");
    assert_eq!(
        r[1..],
        [
            format!("  ✗ {:<28} ASK_CLAUDE_EFFORT must be low, medium, high, xhigh or max (got fast)", "ASK_CLAUDE_EFFORT"),
            format!("  ✓ {:<28} gpt-6-luna — codex's model", "ASK_CODEX_MODEL"),
            format!("  ✓ {:<28} low — codex's effort", "ASK_CODEX_EFFORT"),
        ]
    );
    assert_eq!(e, ["ASK_CLAUDE_EFFORT must be low, medium, high, xhigh or max (got fast)"]);
    let (r, _) = rows("ASK_AI=sk-ant-0123456789012345678901234567890");
    assert!(!r[0].contains("sk-ant"), "{r:?}");
    #[cfg(unix)]
    {
        arena_core::ask::ai::fixtures::stub(&dir, "claude", "true");
        assert_eq!(rows("ASK_AI=claude").0[1], format!("      └ claude: {}", dir.join("claude").display()));
    }
    // In the whole report, in its own section; a bad value is a named failure.
    let (report, missing) = config_report(&Config::parse("MACHINE_NAME_PREFIX=devtest
MACHINE_NAME_LIST=(apple)
ASK_AI=gpt
ASK_CODEX_MODEL=-x
"), "hetzner");
    assert!(report.contains("\nAsking an AI (`pods ask`, the dashboard's `?`):\n  ✗ ASK_AI"), "{report}");
    assert!(missing.iter().any(|m| m == "ASK_AI (must be claude, codex or off)"), "{missing:?}");
    assert!(missing.iter().any(|m| m.starts_with("ASK_CODEX_MODEL (isn't a model's name")), "{missing:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// "✗ means fail": every key set to a value its parser refuses shows a ✗ row and is named —
/// once, however many rows it breaks — in `config check`'s "missing or invalid keys"
/// error, and every ✗ row names a key that error names; a valid config has neither,
/// whichever provider is selected. What's only not ready here (a key file this user can't
/// read, no ARENA_START_DATE) is a ⚠ row: advice, never a failure.
#[test]
fn config_check_invalid_values_are_exactly_its_cross_rows_and_its_error_list() {
    use super::config_report;
    use arena_core::Config;
    // Everything required is there (dummy keys: nothing is called), GPU and image too.
    let base = "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple)\nRUNPOD_API_KEY=not-a-real-key\n\
                HETZNER_API_KEY=not-a-real-key\nGPU_TYPE=\"NVIDIA RTX A4000\"\nIMAGE=img:1\n";
    // (what's added to the base, the keys the check must fail on)
    let cases: [(&str, &[&str]); 41] = [
        ("", &[]),
        // Placement near the proxy: off is valid, a region or margin it can't read isn't.
        ("NEAR=any\nNEAR_MAX_EXTRA_PCT=junk", &[]),
        ("NEAR=asia", &["NEAR"]),
        ("NEAR_MAX_EXTRA_PCT=-5", &["NEAR_MAX_EXTRA_PCT"]),
        ("NEAR=north-america\nNEAR_MAX_EXTRA_PCT=40%", &[]),
        ("OPENROUTER_WORKSPACE=a/b", &["OPENROUTER_WORKSPACE"]),
        ("OPENROUTER_WORKSPACE=arena-dev-test", &[]),
        ("ASK_AI=gpt", &["ASK_AI"]),
        ("ASK_CODEX_EFFORT=fast\nASK_CLAUDE_MODEL=a b", &["ASK_CLAUDE_MODEL", "ASK_CODEX_EFFORT"]),
        ("SHARED_SSH_KEY_PATH=/nonexistent/k\nGIT_SSH_KEY_LOCAL=/nonexistent/d", &[]),
        ("BACKUP_TIMEOUT_SECS=soon", &["BACKUP_TIMEOUT_SECS"]),
        // `set-branch --hard`'s save limit (review ai-review claude #6).
        ("DISCARD_MAX_GB=lots", &["DISCARD_MAX_GB"]),
        ("DISCARD_MAX_GB=0", &["DISCARD_MAX_GB"]),
        ("DISCARD_MAX_GB=0.5", &[]),
        // The free space every copy onto this box leaves (disk-guard).
        ("CONTROL_MIN_FREE_GB=lots", &["CONTROL_MIN_FREE_GB"]),
        ("CONTROL_MIN_FREE_GB=100001", &["CONTROL_MIN_FREE_GB"]),
        ("CONTROL_MIN_FREE_GB=0", &[]),
        ("BACKUP_TIMEOUT_SECS=0", &["BACKUP_TIMEOUT_SECS"]),
        ("BUSY_AUTO_RESOLVE_HOURS=3h", &["BUSY_AUTO_RESOLVE_HOURS"]),
        ("BUSY_AUTO_RESOLVE_HOURS=721", &["BUSY_AUTO_RESOLVE_HOURS"]),
        ("BUSY_AUTO_RESOLVE_HOURS=off", &[]),
        ("SETUP_TIMEOUT_SECS=5m", &["SETUP_TIMEOUT_SECS"]),
        ("MIN_DRIVER_VERSION=abc", &["MIN_DRIVER_VERSION"]),
        ("VSCODE_PREINSTALL=maybe", &["VSCODE_PREINSTALL"]),
        ("VSCODE_EXTENSIONS=bogus", &["VSCODE_EXTENSIONS"]),
        // Two ✗ rows (the create spec's, the driver floor's), one key to fix.
        ("ALLOWED_CUDA_VERSIONS=thirteen", &["ALLOWED_CUDA_VERSIONS"]),
        ("RUNPOD_API=v3", &["RUNPOD_API"]),
        ("VAST_ALLOW_UNVERIFIED=maybe", &["VAST_ALLOW_UNVERIFIED"]),
        ("CREATE_EXTRA_JSON='[1]'", &["CREATE_EXTRA_JSON"]),
        ("BALANCE_WARN_HOURS=soon", &["BALANCE_WARN_HOURS"]),
        ("BALANCE_UNPRICED_PER_GPU=free", &["BALANCE_UNPRICED_PER_GPU"]),
        // The backup archive's settings (its section names the first bad one) and state dir.
        ("ARCHIVE_SNAP_GB=5", &["ARCHIVE_SNAP_GB"]),
        ("ARCHIVE_KEEP_UNTIL=soon", &["ARCHIVE_KEEP_UNTIL"]),
        ("ARCHIVE_BWLIMIT=fast", &["ARCHIVE_BWLIMIT"]),
        ("ARCHIVE_BIG_TIMEOUT_SECS=0", &["ARCHIVE_BIG_TIMEOUT_SECS"]),
        ("BACKUP_MAX_SIZE=huge", &["BACKUP_MAX_SIZE"]),
        ("ARENA_STATE_DIR=state", &["ARENA_STATE_DIR"]),
        // The auto-reaper's (file-only) keys: invalid fails, whether it's on or not.
        ("REAP_MAX_PER_RUN=9", &["REAP_MAX_PER_RUN"]),
        ("REAP_ENABLED=maybe", &["REAP_ENABLED"]),
        ("SETUP_TIMEOUT_SECS=0\nMIN_DRIVER_VERSION=x.y\nBACKUP_TIMEOUT_SECS=-1", &["BACKUP_TIMEOUT_SECS", "MIN_DRIVER_VERSION", "SETUP_TIMEOUT_SECS"]),
        (
            "BALANCE_WARN_HOURS=-1\nRUNPOD_API=v9\nVSCODE_EXTENSIONS=x\nCREATE_EXTRA_JSON='[]'",
            &["BALANCE_WARN_HOURS", "CREATE_EXTRA_JSON", "RUNPOD_API", "VSCODE_EXTENSIONS"],
        ),
    ];
    for provider in ["runpod", "hetzner"] {
        for (extra, want) in cases {
            let (report, missing) = config_report(&Config::parse(&format!("{base}{extra}\n")), provider);
            let mut named: Vec<&str> = missing.iter().map(|m| m.split([' ', '(']).next().unwrap_or_default()).collect();
            named.sort_unstable();
            assert_eq!(named, want, "{provider} {extra:?}: {missing:#?}");
            let crossed: Vec<&str> = report.lines().map(str::trim_start).filter(|l| l.starts_with("✗ ")).collect();
            for row in &crossed {
                assert!(want.iter().any(|k| row.contains(k)), "{provider} {extra:?}: a ✗ row for no failing key: {row}");
            }
            for key in want {
                assert!(crossed.iter().any(|row| row.contains(key)), "{provider} {extra:?}: {key} fails with no ✗ row:\n{report}");
            }
        }
    }
    // The entries say why, without the value (that's on its row).
    let (_, missing) = config_report(&Config::parse(&format!("{base}BACKUP_TIMEOUT_SECS=soon\nMIN_DRIVER_VERSION=abc\n")), "runpod");
    assert_eq!(
        missing,
        [
            "BACKUP_TIMEOUT_SECS (must be a whole number of seconds, 1..=86400)",
            "MIN_DRIVER_VERSION (\"abc\" is not a driver version like 580 or 580.65.06)",
        ]
    );
    let (_, missing) = config_report(&Config::parse(&format!("{base}DISCARD_MAX_GB=lots\n")), "runpod");
    assert_eq!(missing, ["DISCARD_MAX_GB (must be a number of GiB above 0, a fraction too, like 1 or 0.5)"]);
    // The save limit is shown, in binary units labelled so, its default said so.
    for (extra, row) in [
        ("", "  · --hard save max per pod    1.0 GiB (default) (DISCARD_MAX_GB)"),
        ("DISCARD_MAX_GB=0.5\n", "  · --hard save max per pod    512 MiB (DISCARD_MAX_GB)"),
        ("DISCARD_MAX_GB=0.05\n", "  · --hard save max per pod    51 MiB (DISCARD_MAX_GB)"),
        (
            "DISCARD_MAX_GB=lots\n",
            "  ✗ --hard save max per pod    DISCARD_MAX_GB must be a number of GiB above 0, a fraction too, like 1 or 0.5 (got `lots`)",
        ),
    ] {
        let (report, _) = config_report(&Config::parse(&format!("{base}{extra}")), "runpod");
        assert!(report.lines().any(|l| l == row), "{extra:?}:\n{report}");
    }
    // The floor every copy onto this box keeps: its default said so, a bad value ✗ and named.
    let tail = " (CONTROL_MIN_FREE_GB) — pulls, backup copies, replace/migrate staging, the reaper";
    for (extra, row) in [
        ("", format!("  · copies leave free here     5 GB (default){tail}")),
        ("CONTROL_MIN_FREE_GB=12\n", format!("  · copies leave free here     12 GB{tail}")),
        ("CONTROL_MIN_FREE_GB=0\n", format!("  · copies leave free here     0 GB{tail}")),
        (
            "CONTROL_MIN_FREE_GB=lots\n",
            "  ✗ copies leave free here     CONTROL_MIN_FREE_GB must be a whole number of GB, 0..=100000 (got `lots`) — pulls, \
             backup copies and replace/migrate are refused until it's fixed"
                .to_string(),
        ),
    ] {
        let (report, missing) = config_report(&Config::parse(&format!("{base}{extra}")), "runpod");
        assert!(report.lines().any(|l| l == row), "{extra:?}:\n{report}");
        assert_eq!(missing.iter().any(|m| m.starts_with("CONTROL_MIN_FREE_GB")), extra.contains("lots"), "{missing:?}");
    }
    // Not ready here is ⚠, and named.
    let (report, _) = config_report(&Config::parse(&format!("{base}SHARED_SSH_KEY_PATH=/nonexistent/k\n")), "runpod");
    assert!(report.contains("  ⚠ shared SSH key") && report.contains("  ⚠ iteration start date       unset"), "{report}");
}

/// Live 6 (FIX6B item 5): `DISCARD_MAX_GB` from the environment, the file without it — as in
/// `DISCARD_MAX_GB=0.001 arena pods set-branch main <pod> --hard`. `config which` lists it among
/// the environment's overrides, and `config check` shows the value (a bad one is ✗ and fails
/// the check); a key the environment may not introduce is listed by neither, and the reaper's
/// file-only keys are said to be ignored. With no variable, nothing is listed.
#[test]
fn a_discard_limit_from_the_environment_shows_in_config_which_and_check() {
    use super::{config_report, env_keys};
    use arena_core::Config;
    let base = "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple)\nRUNPOD_API_KEY=not-a-real-key\n\
                GPU_TYPE=\"NVIDIA RTX A4000\"\nIMAGE=img:1\nREAP_ENABLED=0\n";
    let env = |v: &'static str| move |k: &str| matches!(k, "DISCARD_MAX_GB" | "SOME_NEW_KEY" | "REAP_ENABLED").then(|| v.to_string());
    let cfg = Config::parse_with_env(base, env("0.001"));
    assert_eq!(env_keys(&cfg, |k| env("0.001")(k).is_some()), (vec!["DISCARD_MAX_GB"], vec!["REAP_ENABLED"]));
    let (report, missing) = config_report(&cfg, "runpod");
    assert!(report.lines().any(|l| l == "  · --hard save max per pod    1 MiB (from the environment) (DISCARD_MAX_GB)"), "{report}");
    assert!(missing.is_empty(), "{missing:?}");
    let (report, missing) = config_report(&Config::parse_with_env(base, env("lots")), "runpod");
    assert!(report.contains("\n  ✗ --hard save max per pod    DISCARD_MAX_GB must be a number of GiB above 0"), "{report}");
    assert_eq!(missing, ["DISCARD_MAX_GB (must be a number of GiB above 0, a fraction too, like 1 or 0.5)"]);
    let cfg = Config::parse_with_env(&format!("{base}DISCARD_MAX_GB=2\n"), |_| None);
    assert_eq!(env_keys(&cfg, |_| false), (vec![], vec![]));
    // The file's value, then the environment's over it (Live 7): said where it came from.
    let (report, _) = config_report(&cfg, "runpod");
    assert!(report.lines().any(|l| l == "  · --hard save max per pod    2.0 GiB (DISCARD_MAX_GB)"), "{report}");
    let cfg = Config::parse_with_env(&format!("{base}DISCARD_MAX_GB=2\n"), env("0.5"));
    let (report, _) = config_report(&cfg, "runpod");
    assert!(report.lines().any(|l| l == "  · --hard save max per pod    512 MiB (from the environment) (DISCARD_MAX_GB)"), "{report}");
}

/// Merge of wave 7 reaper ("✗ means fail"): switched on (REAP_ENABLED=1), every ✗ row in the
/// reaper's section is a gap the check fails on — named in the error — and its other rows
/// are ✓/⚠/·; only observing (its cron line in, REAP_ENABLED unset), the same gaps are ⚠
/// and fail nothing.
#[test]
fn config_check_reaper_rows_cross_only_what_fails() {
    use super::config_report;
    use arena_core::Config;
    use crate::{reap, RSYNC};
    let base = "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple)\nRUNPOD_API_KEY=not-a-real-key\n\
                GPU_TYPE=\"NVIDIA RTX A4000\"\nIMAGE=img:1\n";
    let on = Config::parse(&format!("{base}REAP_ENABLED=1\n"));
    let (report, missing) = config_report(&on, "runpod");
    let section: Vec<&str> =
        report.split("Auto-reaper").nth(1).unwrap().lines().skip(1).take_while(|l| !l.trim().is_empty()).collect();
    let (_, blockers) = reap::readiness_rows_for(&on, RSYNC, true);
    let crossed = section.iter().filter(|l| l.trim_start().starts_with("✗ ")).count();
    assert!(!blockers.is_empty(), "this config has gaps (no explicit ARENA_STATE_DIR, a relative backup dir…)");
    // Every ✗ row is a gap, each gap named once (both tier roots relative: two rows, one fix).
    assert!(crossed >= blockers.len(), "{section:#?} {blockers:?}");
    assert!(blockers.iter().all(|b| missing.iter().filter(|m| *m == b).count() == 1), "{blockers:?} {missing:?}");
    // Observing only: the same rows, no ✗, nothing missing for them.
    let observe = Config::parse(base);
    let (lines, gaps) = reap::readiness_rows_for(&observe, RSYNC, true);
    assert!(!gaps.is_empty() && !lines.iter().any(|l| l.contains('✗')), "{lines:#?}");
    assert!(lines.iter().any(|l| l.trim_start().starts_with("⚠ ")), "{lines:#?}");
}

/// Live W7 step 0: with both tier markers missing, `config check` failed with "missing or
/// invalid keys: the backup tier markers, the backup tier markers" — one fix, named twice (so
/// two relative tier roots). Each thing to set up is named once, and the off row counts it once.
#[test]
fn config_check_names_each_reaper_gap_once() {
    use super::config_report;
    use arena_core::Config;
    use crate::{reap, RSYNC};
    let dir = std::env::temp_dir().join(format!("arena-cc-markers-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("snap/big")).unwrap();
    let base = format!(
        "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple)\nRUNPOD_API_KEY=not-a-real-key\nGPU_TYPE=\"NVIDIA RTX A4000\"\n\
         IMAGE=img:1\nARENA_STATE_DIR={d}/state\nARENA_START_DATE=2026-10-05\nRUNPOD_API=v2\n",
        d = dir.display()
    );
    // Both roots absolute, neither marked; then both relative.
    for (extra, gap) in [(format!("LOCAL_BACKUP_DIR={}/snap\n", dir.display()), "the backup tier markers"), (String::new(), "absolute LOCAL_BACKUP_DIR")] {
        let (_, missing) = config_report(&Config::parse(&format!("{base}{extra}REAP_ENABLED=1\n")), "runpod");
        assert_eq!(missing.iter().filter(|m| *m == gap).count(), 1, "{missing:?}");
        let (lines, blockers) = reap::readiness_rows_for(&Config::parse(&format!("{base}{extra}")), RSYNC, false);
        assert_eq!(blockers.iter().filter(|b| *b == gap).count(), 1, "{blockers:?}");
        let n = blockers.len();
        assert!(lines.iter().any(|l| l.contains(&format!(" — {n} thing(s) to set up"))), "{n}: {lines:#?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Finding #10: `config check` says whether creates ask for a CUDA floor or an exact set,
/// flags an exact set (a newer host is "no capacity"), an old multi-version
/// ALLOWED_CUDA_VERSIONS, shadowed keys, and — on RunPod v1 — a constraint its schema can't
/// express (13.1+), which fails the check.
#[test]
fn config_check_cuda_rows() {
    use super::cuda_rows;
    use arena_core::Config;
    let rows = |text: &str, v1: bool| {
        let (lines, ok) = cuda_rows(&Config::parse(text), v1);
        (lines.join("\n"), ok.is_ok())
    };
    let (l, ok) = rows("ALLOWED_CUDA_VERSIONS=\"13.0\"", false);
    assert!(ok && l.contains("✓") && l.contains("≥ 13.0 (a floor, from ALLOWED_CUDA_VERSIONS)") && !l.contains("└"), "{l}");
    let (l, ok) = rows("ALLOWED_CUDA_VERSIONS=\"13.0\"", true);
    assert!(ok && l.contains("has no real floor") && l.contains("remove RUNPOD_API=v1 (v2 is the default) to reach newer hosts"), "{l}");
    let (l, _) = rows("ALLOWED_CUDA_VERSIONS=\"13.0, 12.8\"", false);
    assert!(l.contains("≥ 12.8") && l.contains("lists several versions"), "{l}");
    let (l, ok) = rows("EXACT_CUDA_VERSIONS=13.0", false);
    assert!(ok && l.contains("⚠") && l.contains("exactly 13.0") && l.contains("no capacity"), "{l}");
    let (l, _) = rows("MIN_CUDA_VERSION=13.0\nALLOWED_CUDA_VERSIONS=12.8", false);
    assert!(l.contains("≥ 13.0 (a floor, from MIN_CUDA_VERSION)") && l.contains("ALLOWED_CUDA_VERSIONS is ignored"), "{l}");
    let (l, ok) = rows("MIN_CUDA_VERSION=13.1", true);
    assert!(!ok && l.contains("✗") && l.contains("remove RUNPOD_API=v1 (v2 is the default)"), "{l}");
    assert!(rows("MIN_CUDA_VERSION=13.1", false).1, "v2 sends any floor");
    let (l, ok) = rows("EXACT_CUDA_VERSIONS=13.0,13.2", true);
    assert!(!ok && l.contains("13.2"), "{l}");
    let (l, ok) = rows("", true);
    assert!(ok && l.contains("any"), "{l}");
    let (l, ok) = rows("MIN_CUDA_VERSION=newest", false);
    assert!(!ok && l.contains("`newest` isn't a version"), "{l}");

    // Live BPF S5: an invalid value is one ✗ row — no `✓ ≥ 13.x` above it, no repeat from
    // the v1 check — and the final error names the key as invalid, not "missing".
    for v1 in [true, false] {
        let (lines, ok) = cuda_rows(&Config::parse("RUNPOD_ALLOWED_CUDA_VERSIONS=\"13.0, 13.x\""), v1);
        assert_eq!(lines.len(), 1, "{lines:#?}");
        assert!(
            lines[0].starts_with("  ✗ host CUDA") && lines[0].contains("`13.x` isn't a version like 13.0 (from RUNPOD_ALLOWED_CUDA_VERSIONS)"),
            "{lines:#?}"
        );
        assert_eq!(ok, Err("RUNPOD_ALLOWED_CUDA_VERSIONS (`13.x` isn't a version like 13.0)".to_string()));
    }
    let (_, ok) = cuda_rows(&Config::parse("EXACT_CUDA_VERSIONS=13.0,latest"), false);
    assert_eq!(ok, Err("EXACT_CUDA_VERSIONS (`latest` isn't a version like 13.0)".to_string()));
    // What v1 can't express keeps its own wording.
    let (_, ok) = cuda_rows(&Config::parse("MIN_CUDA_VERSION=13.1"), true);
    assert_eq!(ok, Err("a host CUDA constraint RunPod v1 can express, or remove RUNPOD_API=v1 (v2 is the default)".to_string()));
}

/// `config check` says where create/up try first and how much more they may pay for it —
/// defaults marked, since prod's config has neither key — or that it's off.
#[test]
fn config_check_near_rows() {
    use arena_core::Config;
    let rows = |text: &str| super::near_rows(&Config::parse(text));
    let (lines, bad) = rows("");
    assert!(bad.is_none());
    assert_eq!(lines[0], "  · NEAR                         Europe (default) — create/up try Europe first (NEAR=any: anywhere)");
    assert_eq!(
        lines[1],
        "  · NEAR_MAX_EXTRA_PCT           25% (default) — when it costs at most this much more than the cheapest option"
    );
    assert!(lines[2].contains("RunPod: SECURE pods only (COMMUNITY hosts have no data centre)"), "{lines:?}");
    let (lines, _) = rows("NEAR=europe\nNEAR_MAX_EXTRA_PCT=12.5");
    assert!(lines[0].starts_with("  ✓ NEAR                         Europe —") && lines[1].starts_with("  ✓ NEAR_MAX_EXTRA_PCT           12.5% —"), "{lines:?}");
    let (lines, bad) = rows("NEAR=");
    assert_eq!((lines.len(), bad.is_none()), (1, true));
    assert!(lines[0].contains("any — off"), "{lines:?}");
    let (lines, bad) = rows("NEAR_MAX_EXTRA_PCT=lots");
    assert!(lines[0].starts_with("  ✗ NEAR_MAX_EXTRA_PCT ") && bad.is_some(), "{lines:?}");
    let (lines, _) = rows("NEAR=mars");
    assert!(lines[0].starts_with("  ✗ NEAR ") && lines[0].contains("expected europe, north-america, or any"), "{lines:?}");
    // In the report whatever provider is selected.
    let (report, _) = super::config_report(&Config::parse("NEAR=any"), "hetzner");
    assert!(report.contains("NEAR                         any — off"), "{report}");
}

/// `config check` shows CREATE_EXTRA_JSON's keys (never its values) and fails a value
/// that isn't one JSON object.
#[test]
fn config_check_create_extra_row() {
    use super::create_extra_row;
    use arena_core::Config;
    assert_eq!(create_extra_row(&Config::parse("RUNPOD_API_KEY=k")), None);
    let (line, ok) = create_extra_row(&Config::parse(r#"CREATE_EXTRA_JSON='{"dataCenterIds":["EU-RO-1"],"env":{"T":"hf_SECRET"}}'"#)).unwrap();
    assert!(ok && line.contains("✓") && line.contains("keys [dataCenterIds, env]") && !line.contains("SECRET"), "{line}");
    let (line, ok) = create_extra_row(&Config::parse("CREATE_EXTRA_JSON='[\"hf_SECRET\"]'")).unwrap();
    assert!(!ok && line.contains("✗") && line.contains("must be a JSON object") && !line.contains("SECRET"), "{line}");
    let (line, ok) = create_extra_row(&Config::parse("CREATE_EXTRA_JSON=")).unwrap();
    assert!(ok && line.contains("(empty)"), "{line}");
}

#[test]
fn config_check_vast_hosts_row() {
    use super::vast_hosts_row;
    use arena_core::Config;
    let (line, ok) = vast_hosts_row(&Config::parse("VAST_API_KEY=k"));
    assert!(ok && line.contains("✓") && line.contains("verified hosts only"), "{line}");
    let (line, ok) = vast_hosts_row(&Config::parse("VAST_ALLOW_UNVERIFIED=1"));
    assert!(ok && line.contains("⚠") && line.contains("unverified"), "{line}");
    let (line, ok) = vast_hosts_row(&Config::parse("VAST_ALLOW_UNVERIFIED=on"));
    assert!(!ok && line.contains("✗") && line.contains("must be"), "{line}");
}

/// Shadeform's rows: its options as the provider reads them, the managed-key and any-cloud
/// warnings, an invalid value failing the whole check — and its API key only ever "(set)".
#[test]
fn config_check_shadeform_rows() {
    use super::{config_report, shadeform_rows};
    use arena_core::Config;
    let (lines, bad) = shadeform_rows(&Config::parse("SHADEFORM_API_KEY=sf_secret_value_123\nMACHINE_NAME_PREFIX=devtest"));
    let text = lines.join("\n");
    assert!(bad.is_none(), "{text}");
    for want in [
        "✓ SHADEFORM_CLOUDS             massedcompute (default)",
        "· SHADEFORM_REGIONS            (any region)",
        "✓ SHADEFORM_MAX_PRICE          $1.00/h per pod (default)",
        "✓ SHADEFORM_SSH_KEY            devtest (default) (must be uploaded",
        "· SHADEFORM_READY_MINS         15 (default)",
        "no stop",
    ] {
        assert!(text.contains(want), "{want}\n{text}");
    }
    let (lines, _) = shadeform_rows(&Config::parse("SHADEFORM_CLOUDS=*\nSHADEFORM_SSH_KEY=default\nSHADEFORM_MAX_PRICE=0.7"));
    let text = lines.join("\n");
    assert!(text.contains("⚠ SHADEFORM_CLOUDS             * — any cloud") && text.contains("⚠ SHADEFORM_SSH_KEY            default"), "{text}");
    assert!(text.contains("$0.70/h per pod —"), "{text}");
    let (lines, bad) = shadeform_rows(&Config::parse("SHADEFORM_MAX_PRICE=cheap"));
    assert!(bad.is_some() && lines[0].contains("✗ SHADEFORM_*                  SHADEFORM_MAX_PRICE must be"), "{lines:?}");
    // The whole report: the key is "(set)", never shown; an invalid option fails the check.
    let (report, missing) = config_report(&Config::parse("SHADEFORM_API_KEY=sf_secret_value_123\nSHADEFORM_READY_MINS=0"), "shadeform");
    assert!(!report.contains("sf_secret_value_123"), "{report}");
    assert!(report.contains("✓ SHADEFORM_API_KEY        (set)   ← selected"), "{report}");
    assert!(missing.iter().any(|m| m.starts_with("SHADEFORM_READY_MINS")), "{missing:?}");
    let (_, missing) = config_report(&Config::parse(""), "shadeform");
    assert!(missing.contains(&"SHADEFORM_API_KEY".to_string()), "{missing:?}");
    // Not in play: one "(empty)" row, none of its options.
    let (report, missing) = config_report(&Config::parse("RUNPOD_API_KEY=r"), "runpod");
    assert!(report.contains("· SHADEFORM_API_KEY        (empty)") && !report.contains("SHADEFORM_CLOUDS"), "{report}");
    assert!(!missing.iter().any(|m| m.contains("SHADEFORM")), "{missing:?}");
}

/// REVIEW sf SF8 (cosmetic): two `config check` rows (and the `└` note, and
/// `warn_no_volume`'s Shadeform line, which prints straight to stderr) carry a long run of
/// spaces mid-sentence — a lost `\` line continuation: "differ per cloud;
/// [25 spaces] widen one cloud at a time". Past the aligned key column, no row should.
#[test]
fn review_sf_sf8_shadeform_rows_have_no_stray_indentation() {
    use super::shadeform_rows;
    use arena_core::Config;
    let (lines, _) = shadeform_rows(&Config::parse("SHADEFORM_CLOUDS=*\nSHADEFORM_SSH_KEY=default"));
    for l in &lines {
        let tail: String = l.chars().skip(36).collect();
        assert!(!tail.contains("    "), "a run of spaces inside: {l:?}");
    }
}
