//! Golden tests of `config check`'s and `config which`'s output (docs/OUTPUT.md), at default and
//! at `-v`: by default what's wrong — the ✗, ‼ and `?` rows with their `└` lines — and the
//! verdict; `-v` the whole report, as it printed before.

use arena_core::config::{ConfigSource, ProviderSource};
use arena_core::out::To;
use arena_core::Config;

use super::{check_lines, config_check, config_report, which_lines, Picked, Which};
use crate::output_test::rendered;

/// A report in the shapes the sections print (a trimmed cut of the sandbox's, 2026-11-02): ✓ and
/// · rows, a ⚠ with `└` lines, a `warning:` row, the create spec's mark after its label, a `?`
/// row, and a `└` under a fine row.
const REPORT: &str = "\
Providers (selected: runpod):
  ✓ RUNPOD_API_KEY           (set)   ← selected
  · PROVIDER                 not set — runpod unless --provider says
  · HETZNER_SSH_KEY              devtest (default: MACHINE_NAME_PREFIX)
      └ every new server gets SSH key `devtest`: it must be in the Hetzner project under that name

Create spec (generic key, else RUNPOD_* fallback):
  GPU_TYPE / RUNPOD_GPU_TYPE   ✗ (missing)
  IMAGE / RUNPOD_DOCKER_IMAGE  ✓ nickypro/arena-env:9.1
  resolved disk                60GB
  ⚠ persistent volume          0GB — work is lost on pod restart

Backup archive (optional, `arena archive`):
  · ARCHIVE_HETZNER_API_KEY      (missing — falls back to HETZNER_API_KEY)
  warning: ARCHIVE_HETZNER_API_KEY is unset — the archive uses the fleet's HETZNER_API_KEY

Setup readiness (local, read-only):
  ⚠ shared SSH key             /srv/keys/devtest_key
      └ not readable by this user — dashboard metrics / SSH will fail
  ✓ admin SSH key              /srv/keys/admin

Hetzner image (HETZNER_IMAGE=arena, for cx23):
  ? couldn't look it up in 20s
";

/// What `lines` show at `verbose`, joined as a terminal shows them (colour is off in tests).
fn shown(lines: &[(To, String)], verbose: bool) -> String {
    lines.iter().filter(|(to, _)| to.shows(verbose)).map(|(_, l)| format!("{l}\n")).collect()
}

#[test]
fn config_check_by_default_says_only_whats_wrong() {
    let _plain = arena_core::out::capture(false);
    let lines = check_lines(REPORT);
    assert_eq!(
        shown(&lines, false),
        "\
✗ GPU_TYPE / RUNPOD_GPU_TYPE (missing)
‼ persistent volume 0GB — work is lost on pod restart
‼ ARCHIVE_HETZNER_API_KEY is unset — the archive uses the fleet's HETZNER_API_KEY
‼ shared SSH key /srv/keys/devtest_key
  └ not readable by this user — dashboard metrics / SSH will fail
? couldn't look it up in 20s
"
    );
    assert_eq!(shown(&lines, true), REPORT, "-v: the report as it was");
}

/// A whole run: `-v` is the report and the verdict as before; by default every line is a
/// problem row, its `└`, or the verdict — the same verdict line, the same error.
#[tokio::test]
async fn config_check_run_default_and_verbose() {
    let text = "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple)\nRUNPOD_API_KEY=not-a-real-key\n\
                GPU_TYPE=\"NVIDIA RTX A4000\"\nIMAGE=img:1\nASK_AI=off\n";
    let cfg = Config::parse(text);
    let (report, missing) = config_report(&cfg, "runpod");
    assert!(missing.is_empty(), "{missing:?}");
    let verdict = "OK — required keys for provider `runpod` are present.\n";
    let loud = rendered(true, &[], async { config_check(&cfg, "runpod", &[]) }).await;
    assert_eq!(loud, format!("{report}\n{verdict}"));
    let quiet = rendered(false, &[], async { config_check(&cfg, "runpod", &[]) }).await;
    assert!(quiet.ends_with(verdict), "{quiet}");
    for l in quiet.lines().filter(|l| format!("{l}\n") != verdict) {
        assert!(["✗ ", "‼ ", "? ", "  └ "].iter().any(|m| l.starts_with(m)), "`{l}` isn't a problem row:\n{quiet}");
    }
    assert!(quiet.lines().count() < loud.lines().count() / 4, "{quiet}");
    // A missing required key: its ✗ row by default, and the error as before.
    let cfg = Config::parse("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple)\nASK_AI=off\n");
    let quiet = rendered(false, &[], async { config_check(&cfg, "vast", &[]) }).await;
    assert!(quiet.contains("✗ VAST_API_KEY (empty) ← selected\n"), "{quiet}");
    assert!(quiet.ends_with("Error: missing or invalid keys: VAST_API_KEY\n"), "{quiet}");
    assert!(!quiet.contains("OK —") && !quiet.contains("✓"), "{quiet}");
}

fn picked(source: ConfigSource, provider_source: ProviderSource) -> Picked {
    Picked { config: "/srv/p2/config.env".into(), config_source: source, provider: "runpod".into(), provider_source }
}

#[test]
fn config_which_default_and_verbose() {
    let _plain = arena_core::out::capture(false);
    let dir = std::env::temp_dir().join(format!("arena-which-golden-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("keys")).unwrap();
    let cfg = Config::parse("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom)\n").with_source(&dir.join("config.env"));
    let abs = dir.join("config.env");
    let quiet_env = Which { abs: &abs, readable: true, writable: true, lost: &[], from_env: &[], ignored: &[] };
    let p = picked(ConfigSource::Flag, ProviderSource::Default);
    let lines = which_lines(&cfg, &p, &quiet_env);
    let d = dir.display();
    assert_eq!(
        shown(&lines, false),
        format!("{d}/config.env · --config\nprovider runpod (default) · 1 key · 2 machine names\n")
    );
    assert_eq!(
        shown(&lines, true),
        format!(
            "Active config: {d}/config.env
  ✓ readable  ·  writable
  source: --config

Loaded: provider runpod (the default: no --provider, no PROVIDER in the config) · 1 key(s) · 2 machine name(s)

Files kept beside the config (else found in the current directory, as before):
  keys/            {d}/keys
  arena-plan.json  {d}/arena-plan.json (not made yet)
  backup/          {d}/backup (not made yet)

Environment overrides: none (all values come from the file)
"
        )
    );
    // What needs a look shows by default: a read-only file, one that lost, the environment.
    let lost = ["~/.config/arena/config.env exists but --config wins".to_string()];
    let loud_env = Which { abs: &abs, readable: true, writable: false, lost: &lost, from_env: &["MACHINE_NAME_PREFIX"], ignored: &["REAP_ENABLED"] };
    let lines = which_lines(&cfg, &picked(ConfigSource::User, ProviderSource::Config), &loud_env);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        shown(&lines, false),
        format!(
            "{d}/config.env (read-only: `config set` can't write here)
provider runpod · 1 key · 2 machine names
‼ ~/.config/arena/config.env exists but --config wins
from the environment (wins over the file): MACHINE_NAME_PREFIX
ignored from the environment (file-only): REAP_ENABLED
"
        )
    );
}
