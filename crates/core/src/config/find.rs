//! Which config file a run reads, the fleet's default provider, and where the files a fleet
//! keeps beside its config are (`keys/`, `arena-plan.json`, `backup/`): one answer for `arena`
//! and `arena-tui`, so they can't drift. Before 2026-10-31 `arena` read only `--config` (a
//! forgotten one read ARENA's prod path), `arena-tui` only `ARENA_CONFIG`, a fleet off RunPod
//! needed `--provider` on every command, and `keys/`, the plan and `./backup` followed the
//! current directory, so a second fleet's wrapper had to `cd` first.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::Config;

/// The built-in default config: ARENA's read-only prod copy on its control box. Looked at last
/// ([`resolve_config`]), so a bare `arena` there still reads what it always did.
pub const LEGACY_CONFIG: &str = "/home/dev/prod-ro/config.env";

/// The environment variable naming a config file: below `--config`, above the per-user file.
pub const CONFIG_VAR: &str = "ARENA_CONFIG";

/// The per-user config, under `$HOME`: read when it exists and no file was named.
pub const USER_CONFIG: &str = ".config/arena/config.env";

/// Where the config file a run reads came from ([`resolve_config`]), highest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    /// `--config <file>`.
    Flag,
    /// `ARENA_CONFIG` (set and not empty).
    Env,
    /// `~/.config/arena/config.env`, which exists.
    User,
    /// [`LEGACY_CONFIG`], whether it exists or not.
    Default,
}

impl ConfigSource {
    /// How `config which` names it.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Flag => "--config",
            Self::Env => "ARENA_CONFIG",
            Self::User => "~/.config/arena/config.env (no --config or ARENA_CONFIG)",
            Self::Default => "the built-in default (no --config, ARENA_CONFIG or ~/.config/arena/config.env)",
        }
    }
}

/// `~/.config/arena/config.env` under `home` (`$HOME`); `None` when that's unset or empty.
pub fn user_config(home: Option<&OsStr>) -> Option<PathBuf> {
    home.filter(|h| !h.is_empty()).map(|h| Path::new(h).join(USER_CONFIG))
}

/// Which config file to read, and why: `flag` (`--config`), else `env` (`ARENA_CONFIG`; empty =
/// unset), else the per-user file under `home` when it `exists`, else [`LEGACY_CONFIG`] (whether
/// it exists or not: reading it then says where else to put one, [`load_context`]). A file
/// named by the flag or the variable is taken as named, even if missing. Pure.
pub fn resolve_config(flag: Option<&Path>, env: Option<&OsStr>, home: Option<&OsStr>, exists: impl Fn(&Path) -> bool) -> (PathBuf, ConfigSource) {
    if let Some(f) = flag {
        return (f.to_path_buf(), ConfigSource::Flag);
    }
    if let Some(e) = env.filter(|e| !e.is_empty()) {
        return (PathBuf::from(e), ConfigSource::Env);
    }
    if let Some(u) = user_config(home).filter(|u| exists(u)) {
        return (u, ConfigSource::User);
    }
    (PathBuf::from(LEGACY_CONFIG), ConfigSource::Default)
}

/// [`resolve_config`] for this process: its `ARENA_CONFIG`, `$HOME` and files.
pub fn find_config(flag: Option<&Path>) -> (PathBuf, ConfigSource) {
    resolve_config(flag, std::env::var_os(CONFIG_VAR).as_deref(), std::env::var_os("HOME").as_deref(), Path::is_file)
}

/// The context a failed read of the config gets: the path, and — for the built-in default, which
/// nobody named — the ways to name another.
pub fn load_context(path: &Path, source: ConfigSource) -> String {
    match source {
        ConfigSource::Flag => format!("loading config {}", path.display()),
        ConfigSource::Env | ConfigSource::User => format!("loading config {} (from {})", path.display(), source.describe()),
        ConfigSource::Default => format!(
            "loading config {}, the built-in default — pass --config <file>, set {CONFIG_VAR}=<file>, or copy \
             config.env.example to ~/{USER_CONFIG}",
            path.display()
        ),
    }
}

/// What `config which` says of the places that lost to `read` (from `source`): an `ARENA_CONFIG`
/// naming another file (it looks as if it should win: `arena` ignored it before 2026-10-31,
/// PR #2), and a per-user file that `--config` or `ARENA_CONFIG` passed over. `same` says whether
/// two paths are one file. Pure.
pub fn passed_over(
    read: &Path,
    source: ConfigSource,
    env: Option<&OsStr>,
    home: Option<&OsStr>,
    exists: impl Fn(&Path) -> bool,
    same: impl Fn(&Path, &Path) -> bool,
) -> Vec<String> {
    let mut out = Vec::new();
    if source == ConfigSource::Flag {
        if let Some(e) = env.filter(|e| !e.is_empty()) {
            if !same(Path::new(e), read) {
                out.push(format!("{CONFIG_VAR} names {}, not read: --config wins", Path::new(e).display()));
            }
        }
    }
    if matches!(source, ConfigSource::Flag | ConfigSource::Env) {
        if let Some(u) = user_config(home).filter(|u| exists(u) && !same(u, read)) {
            out.push(format!("{} exists too, not read: {} wins", u.display(), source.describe()));
        }
    }
    out
}

/// The config key naming the fleet's provider: what `create`/`up` make pods on, and whose key a
/// listing needs, when `--provider` doesn't say. A Hetzner-only fleet sets `PROVIDER=hetzner`.
pub const PROVIDER_KEY: &str = "PROVIDER";

/// The provider when neither `--provider` nor [`PROVIDER_KEY`] says.
pub const DEFAULT_PROVIDER: &str = "runpod";

/// Where the fleet's provider came from ([`pick_provider`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderSource {
    /// Chosen for this run: `--provider`, or the dashboard's `ARENA_PROVIDER`.
    Chosen,
    /// The config's `PROVIDER`.
    Config,
    /// [`DEFAULT_PROVIDER`].
    Default,
}

/// The fleet's provider, and why: `chosen` (`--provider`; the dashboard's `ARENA_PROVIDER`, which
/// `arena tui` sets to its own) when not empty, else the config's `PROVIDER` (trimmed, any case),
/// else `runpod`. The others configured still list as before
/// ([`crate::provider::build_fleet`]: this one is the primary). Whether it names a provider is
/// `build_fleet`'s check, and `config check`'s row. Pure.
pub fn pick_provider(chosen: Option<&str>, cfg: &Config) -> (String, ProviderSource) {
    if let Some(c) = chosen.map(str::trim).filter(|c| !c.is_empty()) {
        return (c.to_string(), ProviderSource::Chosen);
    }
    match cfg.get(PROVIDER_KEY).map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => (p.to_ascii_lowercase(), ProviderSource::Config),
        None => (DEFAULT_PROVIDER.to_string(), ProviderSource::Default),
    }
}

/// Where `keys gen` writes, and `setup`/`copy-keys` read, the per-machine API-key CSVs.
pub const KEYS_DIR: &str = "keys";
/// The provisioning plan `plan check`/`plan show` read when `--file` doesn't say.
pub const PLAN_FILE: &str = "arena-plan.json";
/// Where `pods pull`/`pods backup` copy pods to when `LOCAL_BACKUP_DIR` is unset.
pub const BACKUP_DIR: &str = "backup";

/// How a fleet's file was found ([`fleet_file`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// Next to the config file.
    Beside,
    /// Not made yet: it will be, next to the config file.
    New,
    /// Only in the current directory, where arena looked before 2026-10-31, and it holds this
    /// fleet's files ([`holds_fleet`]): still used, with a warning ([`FleetFile::warning`]) naming
    /// where it belongs (`beside`).
    CurrentDir { beside: PathBuf },
    /// Not beside the config, and the current directory's (`here`) isn't this fleet's — another
    /// fleet's, say, run from the same directory: not used (it would hand this fleet the other's
    /// keys file, or mix their backups). This fleet's is made beside the config, with a warning.
    NotOurs { here: PathBuf },
    /// The current directory, as before and without a warning: there's no config file (a parsed
    /// text), or it's the read-only prod copy itself ([`LEGACY_CONFIG`]), whose folder holds only
    /// that file — prod's `keys/` and `./backup` are where its operators and its cron (in `/root`)
    /// run. Another config in that folder is treated like any other.
    AsBefore,
}

/// A fleet file's path and how it was found ([`fleet_file`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetFile {
    pub path: PathBuf,
    pub found: Found,
}

impl FleetFile {
    /// The one line said when the file was found only in the current directory, or when the
    /// current directory's was passed over as another fleet's; `None` otherwise.
    pub fn warning(&self) -> Option<String> {
        match &self.found {
            Found::CurrentDir { beside } => Some(format!(
                "warning: using {} in the current directory: arena now looks next to the config first — move it to {}",
                self.path.display(),
                beside.display()
            )),
            Found::NotOurs { here } => Some(format!(
                "warning: not using {} in the current directory: it isn't this fleet's (by MACHINE_NAME_PREFIX) — using {}",
                here.display(),
                self.path.display()
            )),
            _ => None,
        }
    }

    /// How `config which` says where it is.
    pub fn describe(&self) -> String {
        let p = self.path.display();
        match &self.found {
            Found::Beside => format!("{p}"),
            Found::New => format!("{p} (not made yet)"),
            Found::CurrentDir { beside } => format!("{p} — in the current directory only (⚠ move it to {})", beside.display()),
            Found::NotOurs { here } => format!("{p} (not made yet; ⚠ {} in the current directory is another fleet's)", here.display()),
            Found::AsBefore => format!("{p} (the current directory, as before: the prod copy's folder isn't a fleet's)"),
        }
    }
}

/// Where the fleet file `rel` (`keys`, `arena-plan.json`, `backup`) is, for the config file at
/// `config` (absolute): next to it when it `exists` there; else in the current directory when it's
/// only there and `ours` (holds this fleet's files, [`holds_fleet`]) — where arena looked before,
/// kept working, with a warning; else next to the config, to be made there (warned about when the
/// current directory's was another fleet's). Without a config file, or for the read-only prod copy
/// ([`Found::AsBefore`]): the current directory, as before. Pure but for `exists` and `ours`.
pub fn fleet_file(config: Option<&Path>, rel: &str, exists: impl Fn(&Path) -> bool, ours: impl Fn(&Path) -> bool) -> FleetFile {
    let here = Path::new(".").join(rel);
    let Some(dir) = config.filter(|c| *c != Path::new(LEGACY_CONFIG)).and_then(Path::parent) else {
        return FleetFile { path: here, found: Found::AsBefore };
    };
    let beside = dir.join(rel);
    if exists(&beside) {
        FleetFile { path: beside, found: Found::Beside }
    } else if !exists(&here) {
        FleetFile { path: beside, found: Found::New }
    } else if ours(&here) {
        FleetFile { path: here, found: Found::CurrentDir { beside } }
    } else {
        FleetFile { path: beside, found: Found::NotOurs { here } }
    }
}

/// The OpenRouter keys CSV the keys commands read and write, inside a keys dir: a link to the
/// current fleet's `<prefix>_openrouter_keys.csv` (`keys gen` points it at its own).
pub const OPENROUTER_KEYS_LINK: &str = "openrouter_api_keys.csv";

/// Whether `here` (the current directory's fleet file `rel`) is the fleet named `prefix`'s — so
/// falling back to it can't hand one fleet another's keys file or mix their backups (two fleets on
/// one box, run from the same directory). `keys/` is judged by its [`OPENROUTER_KEYS_LINK`], the
/// one file `keys which`, `rename`, `terminate --revoke-key` and `copy-keys` read and `keys gen`
/// re-points: a link must point at this prefix's `<prefix>_openrouter_keys.csv` (dangling or not);
/// a plain file (the layout before per-prefix files) must have a row for one of its machines
/// (`<prefix>-…,`); with neither, this prefix's file, or a row of its in another `*.csv`, says so.
/// Anything else (unreadable) isn't. `backup/`: one of its pods one level down (`<label>/<prefix>-…`,
/// `big/<prefix>-…`; pod folders are named per machine, so fleets don't overwrite each other's). A
/// plan names no fleet: any. Listings, the link and the keys CSVs' host column only.
pub fn holds_fleet(rel: &str, here: &Path, prefix: &str) -> bool {
    let mine = format!("{prefix}-");
    let entries = |dir: &Path| std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).collect::<Vec<_>>();
    let named_mine = |p: &Path| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(&mine));
    let has_my_row = |p: &Path| std::fs::read_to_string(p).is_ok_and(|t| t.lines().any(|l| l.starts_with(&mine) && l.contains(',')));
    match rel {
        KEYS_DIR => {
            let own = format!("{prefix}_openrouter_keys.csv");
            let active = here.join(OPENROUTER_KEYS_LINK);
            match std::fs::symlink_metadata(&active) {
                Ok(m) if m.file_type().is_symlink() => {
                    std::fs::read_link(&active).is_ok_and(|t| t.file_name().is_some_and(|n| n == own.as_str()))
                }
                Ok(m) if m.is_file() => has_my_row(&active),
                Ok(_) => false,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    here.join(&own).is_file()
                        || entries(here).iter().filter(|p| p.extension().is_some_and(|x| x == "csv")).any(|p| has_my_row(p))
                }
                Err(_) => false,
            }
        }
        BACKUP_DIR => entries(here).iter().filter(|p| p.is_dir()).any(|d| entries(d).iter().any(|p| named_mine(p))),
        _ => true,
    }
}

impl Config {
    /// [`fleet_file`] for this config's file (made absolute against the current directory, so
    /// `--config config.env` and its full path find the same files).
    pub fn fleet_file(&self, rel: &str) -> FleetFile {
        let config = self.source().map(|p| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()));
        let prefix = self.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
        fleet_file(config.as_deref(), rel, Path::exists, |here| holds_fleet(rel, here, prefix))
    }

    /// Where `pods pull`, `pods backup`, the reaper and the dashboard copy pods to:
    /// `LOCAL_BACKUP_DIR`, else the fleet's `backup/` ([`Config::fleet_file`]) — with how that was
    /// found, so a caller can say its warning.
    pub fn local_backup_dir(&self) -> (String, Option<FleetFile>) {
        if let Some(d) = self.get("LOCAL_BACKUP_DIR").map(str::trim).filter(|d| !d.is_empty()) {
            return (d.to_string(), None);
        }
        let f = self.fleet_file(BACKUP_DIR);
        (f.path.display().to_string(), Some(f))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--config` beats `ARENA_CONFIG`, which beats `~/.config/arena/config.env` (only when it
    /// exists), which beats the built-in default (even when it doesn't). Empty means unset.
    #[test]
    fn the_config_is_the_flag_then_the_variable_then_the_user_file_then_the_default() {
        let user = "/h/.config/arena/config.env";
        // (flag, ARENA_CONFIG, HOME, the user file exists) → (file, source)
        type Case = (Option<&'static str>, Option<&'static str>, Option<&'static str>, bool, &'static str, ConfigSource);
        let table: &[Case] = &[
            (Some("/f.env"), Some("/e.env"), Some("/h"), true, "/f.env", ConfigSource::Flag),
            (Some("rel.env"), None, None, false, "rel.env", ConfigSource::Flag),
            (None, Some("/e.env"), Some("/h"), true, "/e.env", ConfigSource::Env),
            (None, Some("/missing.env"), Some("/h"), true, "/missing.env", ConfigSource::Env),
            (None, Some(""), Some("/h"), true, user, ConfigSource::User),
            (None, None, Some("/h"), true, user, ConfigSource::User),
            (None, None, Some("/h"), false, LEGACY_CONFIG, ConfigSource::Default),
            (None, None, Some(""), true, LEGACY_CONFIG, ConfigSource::Default),
            (None, None, None, true, LEGACY_CONFIG, ConfigSource::Default),
        ];
        for &(flag, env, home, user_exists, want, source) in table {
            let (env, home) = (env.map(OsStr::new), home.map(OsStr::new));
            let got = resolve_config(flag.map(Path::new), env, home, |p| user_exists && p == Path::new(user));
            assert_eq!(got, (PathBuf::from(want), source), "{flag:?} {env:?} {home:?} {user_exists}");
        }
    }

    /// A read of the built-in default says how to name another file; a named one says where from.
    #[test]
    fn a_failed_read_says_where_the_file_came_from() {
        let p = Path::new("/x/config.env");
        assert_eq!(load_context(p, ConfigSource::Flag), "loading config /x/config.env");
        assert_eq!(load_context(p, ConfigSource::Env), "loading config /x/config.env (from ARENA_CONFIG)");
        let d = load_context(Path::new(LEGACY_CONFIG), ConfigSource::Default);
        assert!(d.contains("--config") && d.contains("ARENA_CONFIG=") && d.contains("~/.config/arena/config.env"), "{d}");
    }

    /// `config which` names the sources that lost: an `ARENA_CONFIG` naming another file than
    /// `--config`'s (PR #2's case), and a user file passed over — never the file that was read.
    #[test]
    fn config_which_names_what_lost() {
        let read = Path::new("/f.env");
        let user = Path::new("/h/.config/arena/config.env");
        let same = |a: &Path, b: &Path| a == b;
        let home = Some(OsStr::new("/h"));
        let over = |source, env: Option<&str>, user_exists: bool| {
            passed_over(read, source, env.map(OsStr::new), home, |p| user_exists && p == user, same)
        };
        assert_eq!(
            over(ConfigSource::Flag, Some("/e.env"), true),
            ["ARENA_CONFIG names /e.env, not read: --config wins", "/h/.config/arena/config.env exists too, not read: --config wins"]
        );
        assert!(over(ConfigSource::Flag, Some("/f.env"), false).is_empty(), "the variable names the file read");
        assert!(over(ConfigSource::Flag, Some(""), false).is_empty());
        assert_eq!(over(ConfigSource::Env, Some("/f.env"), true), ["/h/.config/arena/config.env exists too, not read: ARENA_CONFIG wins"]);
        assert!(over(ConfigSource::User, None, true).is_empty() && over(ConfigSource::Default, None, false).is_empty());
        assert!(passed_over(user, ConfigSource::Flag, None, home, |_| true, same).is_empty(), "--config named the user file");
    }

    /// `--provider` (or the dashboard's `ARENA_PROVIDER`) beats the config's `PROVIDER`, which
    /// beats `runpod`; empty means unset, and the config's value is trimmed and lowercased.
    #[test]
    fn the_provider_is_the_flag_then_provider_then_runpod() {
        let table: &[(Option<&str>, &str, &str, ProviderSource)] = &[
            (Some("vast"), "PROVIDER=hetzner", "vast", ProviderSource::Chosen),
            (Some("runpod"), "PROVIDER=hetzner", "runpod", ProviderSource::Chosen),
            (Some(""), "PROVIDER=hetzner", "hetzner", ProviderSource::Config),
            (None, "PROVIDER=hetzner", "hetzner", ProviderSource::Config),
            (None, "PROVIDER=\" Hetzner \"", "hetzner", ProviderSource::Config),
            (None, "PROVIDER=hetz", "hetz", ProviderSource::Config),
            (None, "PROVIDER=", "runpod", ProviderSource::Default),
            (None, "IMAGE=x", "runpod", ProviderSource::Default),
        ];
        for &(chosen, text, want, source) in table {
            assert_eq!(pick_provider(chosen, &Config::parse(text)), (want.to_string(), source), "{chosen:?} {text}");
        }
    }

    /// keys/, the plan and backup/ are found next to the config; else in the current directory
    /// when it holds this fleet's (warned); else made next to the config (warned when the current
    /// directory's is another fleet's). No config file, or the read-only prod copy: the current
    /// directory, as before, unwarned.
    #[test]
    fn a_fleet_file_is_beside_the_config_else_in_the_current_directory() {
        let cfg = Some(Path::new("/srv/p2/config.env"));
        let beside = PathBuf::from("/srv/p2/keys");
        let here = PathBuf::from("./keys");
        let prod = Some(Path::new(LEGACY_CONFIG));
        // (config, exists beside, exists here, here is this fleet's) → (path, found)
        type Case<'a> = (Option<&'a Path>, bool, bool, bool, &'a Path, Found);
        let table: &[Case] = &[
            (cfg, true, true, true, &beside, Found::Beside),
            (cfg, true, true, false, &beside, Found::Beside),
            (cfg, true, false, false, &beside, Found::Beside),
            (cfg, false, true, true, &here, Found::CurrentDir { beside: beside.clone() }),
            (cfg, false, true, false, &beside, Found::NotOurs { here: here.clone() }),
            (cfg, false, false, false, &beside, Found::New),
            (None, true, true, false, &here, Found::AsBefore),
            (None, false, false, false, &here, Found::AsBefore),
            (prod, true, true, false, &here, Found::AsBefore),
            (prod, false, false, false, &here, Found::AsBefore),
            (Some(Path::new("/home/dev/prod-ro/p2.env")), false, true, false, Path::new("/home/dev/prod-ro/keys"), Found::NotOurs { here: here.clone() }),
            (Some(Path::new("/home/dev/prod-ro/p2.env")), false, true, true, &here, Found::CurrentDir { beside: "/home/dev/prod-ro/keys".into() }),
            (Some(Path::new("/home/dev/prod-ro-copy/config.env")), false, false, false, Path::new("/home/dev/prod-ro-copy/keys"), Found::New),
        ];
        for (config, there, cwd, ours, want, found) in table {
            let got = fleet_file(*config, KEYS_DIR, |p| if p.starts_with(".") { *cwd } else { *there }, |_| *ours);
            assert_eq!(got, FleetFile { path: want.to_path_buf(), found: found.clone() }, "{config:?} {there} {cwd} {ours}");
        }
    }

    /// The current directory's keys/ is this fleet's when its keys link points at this prefix's
    /// file — whichever fleet's files sit beside it (both fleets' here: review r2) — or, with no
    /// link, when this prefix's file or a CSV row for one of its machines is there. backup/ with
    /// one of its pods a level down. Never another prefix's alone (`arena9-…` isn't `arena-…`'s).
    /// A plan is anyone's.
    #[test]
    fn whose_files_the_current_directory_holds() {
        let d = std::env::temp_dir().join(format!("arena-holds-fleet-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let (keys, backup) = (d.join("keys"), d.join("backup"));
        std::fs::create_dir_all(&keys).unwrap();
        std::fs::create_dir_all(backup.join("w1d2").join("arena9-apple")).unwrap();
        std::fs::create_dir_all(backup.join("big").join("arena9-apple")).unwrap();
        std::fs::write(keys.join("arena9_openrouter_keys.csv"), "# keys\narena9-apple,sk-or-x\n").unwrap();
        let no_link = [holds_fleet(KEYS_DIR, &keys, "arena9"), holds_fleet(KEYS_DIR, &keys, "arena"), holds_fleet(BACKUP_DIR, &backup, "arena9"), holds_fleet(BACKUP_DIR, &backup, "arena")];
        // p2's file beside arena9's, the link at p2's (p2's `keys gen` ran last): only p2's now.
        std::fs::write(keys.join("p2_openrouter_keys.csv"), "p2-apple,sk-or-y\n").unwrap();
        std::os::unix::fs::symlink("p2_openrouter_keys.csv", keys.join(OPENROUTER_KEYS_LINK)).unwrap();
        std::fs::create_dir_all(backup.join("w1d3").join("p2-apple")).unwrap();
        let linked = [holds_fleet(KEYS_DIR, &keys, "p2"), holds_fleet(KEYS_DIR, &keys, "arena9"), holds_fleet(BACKUP_DIR, &backup, "p2")];
        // An older layout: the CSV itself, no link — its rows say whose, whatever per-prefix file
        // sits beside it (review r3: p2's file next to arena9's active CSV isn't p2's dir).
        let old = d.join("old");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join(OPENROUTER_KEYS_LINK), "arena9-apple,sk-or-x\n").unwrap();
        std::fs::write(old.join("p2_openrouter_keys.csv"), "p2-apple,sk-or-y\n").unwrap();
        let legacy = [holds_fleet(KEYS_DIR, &old, "arena9"), holds_fleet(KEYS_DIR, &old, "p2")];
        // A dangling link still names whose it is; no active CSV: this prefix's file says so.
        let (dangling, none) = (d.join("dangling"), d.join("none"));
        std::fs::create_dir_all(&dangling).unwrap();
        std::os::unix::fs::symlink("p2_openrouter_keys.csv", dangling.join(OPENROUTER_KEYS_LINK)).unwrap();
        std::fs::create_dir_all(&none).unwrap();
        std::fs::write(none.join("p2_openrouter_keys.csv"), "").unwrap();
        let other = [holds_fleet(KEYS_DIR, &dangling, "p2"), holds_fleet(KEYS_DIR, &dangling, "arena9"), holds_fleet(KEYS_DIR, &none, "p2"), holds_fleet(KEYS_DIR, &none, "arena9")];
        let (plan, empty) = (holds_fleet(PLAN_FILE, &d.join(PLAN_FILE), "p2"), holds_fleet(BACKUP_DIR, &d.join("nothing"), "p2"));
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(no_link, [true, false, true, false]);
        assert_eq!(linked, [true, false, true]);
        assert_eq!(legacy, [true, false]);
        assert_eq!(other, [true, false, true, false]);
        assert!(plan && !empty);
    }

    /// Only the current-directory fallback, and passing another fleet's over, warn: one line naming
    /// both places.
    #[test]
    fn only_the_fallback_warns() {
        let f = fleet_file(Some(Path::new("/srv/p2/config.env")), BACKUP_DIR, |p| p.starts_with("."), |_| true);
        assert_eq!(
            f.warning().as_deref(),
            Some("warning: using ./backup in the current directory: arena now looks next to the config first — move it to /srv/p2/backup")
        );
        let f = fleet_file(Some(Path::new("/srv/p2/config.env")), KEYS_DIR, |p| p.starts_with("."), |_| false);
        assert_eq!(
            f.warning().as_deref(),
            Some("warning: not using ./keys in the current directory: it isn't this fleet's (by MACHINE_NAME_PREFIX) — using /srv/p2/keys")
        );
        for found in [Found::Beside, Found::New, Found::AsBefore] {
            assert_eq!(FleetFile { path: "x".into(), found }.warning(), None);
        }
    }

    /// `LOCAL_BACKUP_DIR` when set; else the fleet's `backup/`: `./backup` for a parsed config (as
    /// before), and next to a loaded one.
    #[test]
    fn the_backup_dir_is_the_key_else_the_fleets() {
        assert_eq!(Config::parse("LOCAL_BACKUP_DIR=\" /srv/b \"").local_backup_dir(), ("/srv/b".to_string(), None));
        let (dir, found) = Config::parse("LOCAL_BACKUP_DIR=").local_backup_dir();
        assert_eq!((dir.as_str(), found.map(|f| f.found)), ("./backup", Some(Found::AsBefore)));
        let fleet = std::env::temp_dir().join(format!("arena-fleet-files-{}", std::process::id()));
        std::fs::create_dir_all(fleet.join("backup")).unwrap();
        let c = Config::parse("").with_source(&fleet.join("config.env"));
        let (dir, found) = c.local_backup_dir();
        let _ = std::fs::remove_dir_all(&fleet);
        assert_eq!((PathBuf::from(dir), found.map(|f| f.found)), (fleet.join("backup"), Some(Found::Beside)));
    }
}
