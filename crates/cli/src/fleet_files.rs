//! The files a fleet keeps beside its config — `keys/`, `arena-plan.json`, `backup/` — as the
//! commands use them: found next to the config, else in the current directory with a one-line
//! warning on stderr, said once per run ([`arena_core::config::fleet_file`]).

use std::path::PathBuf;
use std::sync::Mutex;

use arena_core::config::{FleetFile, KEYS_DIR, PLAN_FILE};
use arena_core::Config;

/// Say `f`'s warning ([`FleetFile::warning`]) on stderr, the first time this run.
pub(crate) fn warn(f: &FleetFile) {
    static SAID: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    let Some(w) = f.warning() else { return };
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if !said.contains(&f.path) {
        said.push(f.path.clone());
        eprintln!("{w}");
    }
}

/// The fleet file `rel`'s path ([`Config::fleet_file`]), warned about when it's only in the
/// current directory.
pub(crate) fn path(cfg: &Config, rel: &str) -> PathBuf {
    let f = cfg.fleet_file(rel);
    warn(&f);
    f.path
}

/// Where `keys gen` writes, and `setup`/`copy-keys` read, the per-machine API-key CSVs: the
/// fleet's `keys/` (`./keys` for a config parsed from text, as before).
pub(crate) fn keys_dir(cfg: &Config) -> String {
    path(cfg, KEYS_DIR).display().to_string()
}

/// The plan `plan check`/`plan show` read when `--file` doesn't say: the fleet's
/// `arena-plan.json`.
pub(crate) fn plan_file(cfg: &Config) -> PathBuf {
    path(cfg, PLAN_FILE)
}

/// [`Config::local_backup_dir`], its fallback warned about.
pub(crate) fn backup_dir(cfg: &Config) -> String {
    let (dir, found) = cfg.local_backup_dir();
    if let Some(f) = found {
        warn(&f);
    }
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config parsed from text has no folder: `./keys` and `./backup`, as the constants were.
    /// A loaded one finds them beside it, made or not.
    #[test]
    fn a_parsed_config_keeps_the_old_paths_and_a_loaded_one_its_own() {
        let parsed = Config::parse("MACHINE_NAME_PREFIX=devtest");
        assert_eq!((keys_dir(&parsed).as_str(), backup_dir(&parsed).as_str()), ("./keys", "./backup"));
        assert_eq!(plan_file(&parsed), PathBuf::from("./arena-plan.json"));
        let dir = std::env::temp_dir().join(format!("arena-fleet-files-cli-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("keys")).unwrap();
        let loaded = Config::parse("LOCAL_BACKUP_DIR=/srv/b").with_source(&dir.join("config.env"));
        let got = (keys_dir(&loaded), backup_dir(&loaded), plan_file(&loaded));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got, (dir.join("keys").display().to_string(), "/srv/b".to_string(), dir.join("arena-plan.json")));
    }

    /// What `keys gen` leaves in a keys dir (`ensure_cohort_keys_file`) is what the current-directory
    /// fallback recognises as the fleet's: the link at its prefix's file, and no other prefix's.
    #[test]
    fn keys_gen_s_layout_is_what_the_fallback_recognises() {
        use arena_core::config::holds_fleet;
        let dir = std::env::temp_dir().join(format!("arena-fleet-files-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = crate::keys::ensure_cohort_keys_file(&dir, "p2").unwrap();
        std::fs::write(&file, crate::keys::cohort_keys_header("p2")).unwrap();
        let got = (holds_fleet(KEYS_DIR, &dir, "p2"), holds_fleet(KEYS_DIR, &dir, "arena9"));
        crate::keys::ensure_cohort_keys_file(&dir, "arena9").unwrap();
        let after = (holds_fleet(KEYS_DIR, &dir, "p2"), holds_fleet(KEYS_DIR, &dir, "arena9"));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!((got, after), ((true, false), (false, true)), "the link says whose the dir is now");
    }
}
