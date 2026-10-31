//! `config check`'s `PROVIDER` row and `not set` rows, `config which`'s fleet files, and the
//! fleet a config's `PROVIDER` builds.

use arena_core::config::{ConfigSource, ProviderSource};
use arena_core::Config;

use super::{config_report, fleet_file_rows, provider_key_row, Picked};

const BASE: &str = "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple)\nRUNPOD_API_KEY=not-a-real-key\n\
                    HETZNER_API_KEY=not-a-real-key\nGPU_TYPE=\"NVIDIA RTX A4000\"\nIMAGE=img:1\n";

/// The `PROVIDER` row: `·` unset (runpod unless `--provider` says), ✓ with a known provider (and
/// when `--provider` overrode it this run), ✗ for a name that isn't one — which fails the check.
#[test]
fn config_check_says_the_fleets_provider() {
    let key = format!("{:<24}", "PROVIDER");
    // (config line, the run's provider) → (row, what the check fails on)
    let table: &[(&str, &str, String, Option<&str>)] = &[
        ("", "runpod", format!("  · {key} not set — runpod unless --provider says"), None),
        ("PROVIDER=", "vast", format!("  · {key} not set — runpod unless --provider says"), None),
        ("PROVIDER=hetzner", "hetzner", format!("  ✓ {key} hetzner — create/up's provider when --provider doesn't say"), None),
        (
            "PROVIDER=Hetzner",
            "vast",
            format!("  ✓ {key} hetzner — create/up's provider when --provider doesn't say (--provider vast wins this run)"),
            None,
        ),
        (
            "PROVIDER=hetz",
            "hetz",
            format!("  ✗ {key} `hetz` isn't a provider (runpod, vast, hetzner, shadeform)"),
            Some("PROVIDER (`hetz` isn't a provider (runpod, vast, hetzner, shadeform))"),
        ),
    ];
    for (line, selected, row, bad) in table {
        let got = provider_key_row(&Config::parse(line), selected);
        assert_eq!(got, (row.clone(), bad.map(String::from)), "{line:?} {selected}");
    }
    // In the report: under the providers, and a bad one fails the check by name.
    let (report, missing) = config_report(&Config::parse(&format!("{BASE}PROVIDER=hetzner\n")), "hetzner");
    assert!(report.contains("Providers (selected: hetzner):") && report.contains("✓ PROVIDER "), "{report}");
    assert!(!missing.iter().any(|m| m.starts_with("PROVIDER")), "{missing:?}");
    let (_, missing) = config_report(&Config::parse(&format!("{BASE}PROVIDER=hetz\n")), "hetz");
    assert!(missing.iter().any(|m| m.starts_with("PROVIDER (`hetz`")), "{missing:?}");
}

/// An optional key that isn't set reads `not set` (PR #1): `(missing)` is a required key's, the
/// one that fails the check.
#[test]
fn an_unset_optional_key_reads_not_set() {
    let (report, _) = config_report(&Config::parse(BASE), "runpod");
    let row = report.lines().find(|l| l.contains(" SSH_PROXY_HOST ")).unwrap_or_default();
    assert_eq!(row.trim(), format!("· {:<28} not set", "SSH_PROXY_HOST"), "{report}");
    let (report, missing) = config_report(&Config::parse("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple)\n"), "vast");
    assert!(report.contains("✗ VAST_API_KEY") && missing.contains(&"VAST_API_KEY".to_string()), "{report}");
}

/// `config which` lists keys/, the plan and the backup dir with where each is: beside the config
/// (made or not), `LOCAL_BACKUP_DIR` as written, and `./…` for a config parsed from text.
#[test]
fn config_which_lists_the_files_beside_the_config() {
    let dir = std::env::temp_dir().join(format!("arena-which-files-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("keys")).unwrap();
    let d = dir.display();
    let rows = fleet_file_rows(&Config::parse("").with_source(&dir.join("config.env")));
    let with_key = fleet_file_rows(&Config::parse("LOCAL_BACKUP_DIR=/srv/b").with_source(&dir.join("config.env")));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        rows,
        [
            format!("  keys/            {d}/keys"),
            format!("  arena-plan.json  {d}/arena-plan.json (not made yet)"),
            format!("  backup/          {d}/backup (not made yet)"),
        ]
    );
    assert_eq!(with_key[2], "  backup/          /srv/b (LOCAL_BACKUP_DIR)");
    let parsed = fleet_file_rows(&Config::parse(""));
    assert!(parsed[0].starts_with("  keys/            ./keys (the current directory, as before"), "{parsed:?}");
}

/// A bad `PROVIDER` fails the fleet naming the key and the file; `--provider`'s own error is
/// unchanged; a good one builds a fleet whose primary it is.
#[test]
fn a_config_provider_builds_the_fleet() {
    let cfg = Config::parse(BASE);
    let picked = |provider: &str, provider_source| Picked {
        config: "/srv/p2/config.env".into(),
        config_source: ConfigSource::Flag,
        provider: provider.into(),
        provider_source,
    };
    let e = picked("hetz", ProviderSource::Config).fleet(&cfg).err().unwrap();
    let text = format!("{e:#}");
    assert!(text.starts_with("the config's PROVIDER=\"hetz\" (/srv/p2/config.env): ") && text.contains("unknown provider `hetz`"), "{text}");
    let e = picked("hetz", ProviderSource::Chosen).fleet(&cfg).err().unwrap();
    assert!(!format!("{e:#}").contains("PROVIDER="), "{e:#}");
    let fleet = picked("hetzner", ProviderSource::Config).fleet(&cfg).unwrap();
    assert_eq!(fleet.name(), "hetzner");
}
