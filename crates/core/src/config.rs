//! Tolerant parser for the existing `config.env` format.
//!
//! The legacy file mixes simple `KEY=value` pairs (some quoted, some with inline
//! `# comments`) with one bash array, `MACHINE_NAME_LIST=( "a" "b" ... )`, that
//! may span many lines. We parse both so the Rust tooling reads the *same* config
//! the bash/python scripts use — no migration required.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub values: HashMap<String, String>,
    pub machine_names: Vec<String>,
    /// What the FILE itself says, before the environment's overrides — for the settings only
    /// the file may decide ([`Config::file_value`]).
    file: HashMap<String, String>,
    /// Whether the FILE sets `MACHINE_NAME_LIST` (an empty `( )` too): only then may the
    /// environment override the list, as for any other key ([`Config::apply_overrides`]).
    file_sets_names: bool,
    /// The file this was loaded from ([`Config::load`]) — what a reap re-reads `REAP_ENABLED`
    /// from right before a terminate (`arena_core::reap`, RS8). `None` for a parsed text.
    source: Option<PathBuf>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("reading {}: {e}", path.display())))?;
        let mut cfg = Self::parse_with_env(&text, |k| std::env::var(k).ok());
        cfg.source = Some(path.to_path_buf());
        Ok(cfg)
    }

    /// [`Self::parse`], then the environment's overrides as [`Self::load`] applies them, each
    /// variable read through `lookup` (the process environment, for `load`) — so a test sees
    /// exactly what a variable in the environment does without setting one process-wide.
    pub fn parse_with_env<F: Fn(&str) -> Option<String>>(text: &str, lookup: F) -> Self {
        let mut cfg = Self::parse(text);
        cfg.apply_overrides(lookup);
        cfg
    }

    /// The file this config was loaded from, if any.
    pub fn source(&self) -> Option<&Path> {
        self.source.as_deref()
    }

    /// This config, as read from `path` by the caller itself (no environment applied) — a
    /// test's hermetic stand-in for [`Config::load`].
    pub fn with_source(mut self, path: &Path) -> Self {
        self.source = Some(path.to_path_buf());
        self
    }

    /// A few arena-specific keys that are commonly *absent* from a shared config but
    /// that an operator legitimately needs to set per-run without editing the read-only
    /// prod file (e.g. the iteration start date). These may be *introduced* from the
    /// environment, not just overridden.
    const ENV_INTRODUCIBLE: &'static [&'static str] = &[
        "ARENA_START_DATE",
        "EXTRA_SSH_KEYS",
        // Broadcast tokens (distributed by `copy-keys`/`setup`): supplying them via the
        // environment avoids editing the shared, read-only prod config for a run.
        "HF_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
        // `SSH_PROXY_RELOAD_CMD=` (empty) = write the proxy config, never reload nginx. A
        // wrapper sharing a box with the production nginx (the sandbox's `arena-dev`) exports
        // it empty so no lifecycle sync can reload the system nginx, whatever its config file
        // says — absent from the file, the default reload would run.
        "SSH_PROXY_RELOAD_CMD",
        // Which RunPod REST API to use (`v1`/`v2`; unset = v2): lets an operator roll back to
        // the retiring v1 for one command — or force v2 over a file that still says v1 —
        // without editing the shared, read-only prod config (`RUNPOD_API=v1 arena pods list`).
        "RUNPOD_API",
        // Where the health cache lives (`snapshot::health_cache_path`): lets the sandbox, a
        // cron job or a test point it somewhere explicit without editing the shared config.
        "ARENA_STATE_DIR",
        // The per-pod wall-clock budget of a backup rsync (`pull::backup_timeout`): an
        // operator can give one big first pull longer (or a test less) without editing the
        // shared config.
        "BACKUP_TIMEOUT_SECS",
        // How many hours of runway an account may have before `arena balance` and the
        // footers warn (`balance::BalancePolicy`): `BALANCE_WARN_HOURS=100000 arena balance
        // --check` tries the alarm path, `=0` silences it for one run, without editing the
        // shared config.
        "BALANCE_WARN_HOURS",
        // The stand-in $/h per GPU for a running pod nobody priced (`balance::stand_in_price`).
        "BALANCE_UNPRICED_PER_GPU",
        // The backup archive's per-run knobs (`archive::ArchiveSettings`): sizes for one `archive
        // up`, a retention date, push bandwidth and budgets — none of them secret. (Its token,
        // ARCHIVE_HETZNER_API_KEY, belongs in the config file.)
        "ARCHIVE_SNAP_GB",
        "ARCHIVE_BIG_GB",
        "ARCHIVE_KEEP_UNTIL",
        "ARCHIVE_BWLIMIT",
        "ARCHIVE_SNAP_TIMEOUT_SECS",
        "ARCHIVE_BIG_TIMEOUT_SECS",
        // The most `set-branch --hard` saves on a pod before its reset (`backup::discard_max_kb`):
        // a non-secret tuning knob an operator — or a live test — lowers for one `--hard`
        // (`DISCARD_MAX_GB=0.001 arena pods set-branch main <pod> --hard`) without editing the
        // shared config (Live 6 couldn't reach the "too big to save" path: the sandbox's file
        // doesn't set it).
        "DISCARD_MAX_GB",
        // Each AI's model and effort for `pods ask` and the dashboard's `?` (`ask::ai::Pick`):
        // `ASK_CODEX_MODEL=gpt-6-luna arena pods ask apple` tries a model for one run without
        // editing the shared config. Not `ASK_AI` itself: `off` there is the config owner's call.
        "ASK_CLAUDE_MODEL",
        "ASK_CLAUDE_EFFORT",
        "ASK_CODEX_MODEL",
        "ASK_CODEX_EFFORT",
        // Placement near the proxy (`near::Near`): prod's config predates them, so `NEAR=any
        // arena pods up …` (cheapest anywhere for one run) or a different `NEAR_MAX_EXTRA_PCT`
        // must work without editing the shared config.
        "NEAR",
        "NEAR_MAX_EXTRA_PCT",
        // GPU types RunPod's public catalog lacks (`gpu::UNLISTED`'s kind): `RUNPOD_EXTRA_GPU_TYPES=
        // "<id>" arena pods create --gpu "<id>"` tries one for one run, no config edit.
        "RUNPOD_EXTRA_GPU_TYPES",
    ];

    /// Keys starting with this are **file-only**: the environment can neither override nor
    /// introduce them. The auto-reaper's settings (`REAP_ENABLED`, its caps, its notify
    /// command — `arena_core::reap`): a reap terminates pods, so whether it may run at all, and
    /// how much it may do, must be exactly what the config file says. Without this a stray
    /// `REAP_ENABLED=1` in a shell or a cron line would switch it on over a file saying `0`
    /// (any key in the file is env-overridable — the rule below).
    pub const FILE_ONLY_PREFIX: &'static str = "REAP_";

    /// Whether `key` is file-only ([`Self::FILE_ONLY_PREFIX`]).
    pub fn is_file_only(key: &str) -> bool {
        key.starts_with(Self::FILE_ONLY_PREFIX)
    }

    /// Let environment variables override values from the file: any key already in the
    /// config can be overridden by an env var of the same name (e.g.
    /// `SHARED_SSH_KEY_PATH=~/.ssh/key arena-tui`), and the [`Self::ENV_INTRODUCIBLE`]
    /// keys may be *added* even if the file omits them (e.g.
    /// `ARENA_START_DATE=2026-05-25 arena backup`). This is how an operator configures a
    /// run *without editing the shared, read-only prod config*; a stray env var still
    /// can't introduce an arbitrary new setting. File-only keys ([`Self::is_file_only`]) are
    /// never touched. `MACHINE_NAME_LIST` is such a key too, though the file's bash array is
    /// parsed into [`Self::machine_names`], not `values`: a file that sets it takes the
    /// environment's list instead, in the same syntax (`MACHINE_NAME_LIST='( apple @staff-box )'`;
    /// the parentheses may be left out) — live, the sandbox couldn't try a non-cohort pod
    /// without editing its config (bsec B3). Not introducible: a file without a list keeps none.
    fn apply_overrides<F: Fn(&str) -> Option<String>>(&mut self, lookup: F) {
        for (k, v) in self.values.iter_mut() {
            if Self::is_file_only(k) {
                continue;
            }
            if let Some(override_val) = lookup(k) {
                *v = override_val;
            }
        }
        if self.file_sets_names {
            if let Some(list) = lookup(NAME_LIST_KEY) {
                let list = list.trim();
                let inner = list.strip_prefix('(').map_or(list, |rest| rest.split(')').next().unwrap_or(""));
                self.machine_names = name_list_entries(inner);
            }
        }
        for &k in Self::ENV_INTRODUCIBLE {
            if !self.values.contains_key(k) {
                if let Some(val) = lookup(k) {
                    self.values.insert(k.to_string(), val);
                }
            }
        }
    }

    pub fn parse(text: &str) -> Self {
        let mut values = HashMap::new();
        let mut machine_names = Vec::new();
        let mut file_sets_names = false;
        let mut lines = text.lines();

        while let Some(line) = lines.next() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            // Bash array, possibly multi-line, terminated by ')'.
            if let Some(rest) = trimmed.strip_prefix("MACHINE_NAME_LIST=(") {
                let mut buf = rest.to_string();
                while !buf.contains(')') {
                    match lines.next() {
                        Some(l) => {
                            buf.push('\n');
                            buf.push_str(l);
                        }
                        None => break,
                    }
                }
                machine_names.extend(name_list_entries(buf.split(')').next().unwrap_or("")));
                file_sets_names = true;
                continue;
            }

            if let Some((k, v)) = trimmed.split_once('=') {
                values.insert(k.trim().to_string(), strip_value(v));
            }
        }

        Config { file: values.clone(), values, machine_names, file_sets_names, source: None }
    }

    /// Whether the config FILE sets `MACHINE_NAME_LIST` — so an env var of that name, when set,
    /// is the list in effect ([`Self::apply_overrides`]; `config which` lists it).
    pub fn file_sets_name_list(&self) -> bool {
        self.file_sets_names
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    /// `key` as the config FILE sets it, whatever the environment says (`None`: the file doesn't
    /// set it). For settings whose authority is the file alone — the reaper's state dir.
    pub fn file_value(&self, key: &str) -> Option<&str> {
        self.file.get(key).map(String::as_str)
    }

    pub fn require(&self, key: &str) -> Result<&str> {
        self.get(key)
            .ok_or_else(|| Error::Config(format!("missing required key `{key}`")))
    }

    pub fn get_parsed<T: std::str::FromStr>(&self, key: &str) -> Option<T> {
        self.get(key).and_then(|s| s.parse().ok())
    }
}

/// Return `text` with `key` set to `value` (quoted): replace the first `KEY=…` line if
/// present, else append `KEY="value"`. Comments and other lines are preserved. Intended
/// for simple scalar keys (e.g. API keys) — not the `MACHINE_NAME_LIST` array.
pub fn upsert_line(text: &str, key: &str, value: &str) -> String {
    let needle = format!("{key}=");
    let mut replaced = false;
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        if !replaced && line.trim_start().starts_with(&needle) {
            out.push(format!("{key}=\"{value}\""));
            replaced = true;
        } else {
            out.push(line.to_string());
        }
    }
    if !replaced {
        out.push(format!("{key}=\"{value}\""));
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// The key of the bash array [`Config::machine_names`] is parsed from.
pub const NAME_LIST_KEY: &str = "MACHINE_NAME_LIST";

/// The names inside a `MACHINE_NAME_LIST=( … )` array (what's between the parentheses):
/// whitespace-separated, each unquoted, a `#…` word dropped. The file's and the environment's
/// list read alike. Pure.
fn name_list_entries(inner: &str) -> Vec<String> {
    inner
        .split_whitespace()
        .map(|tok| tok.trim().trim_matches(['"', '\'']))
        .filter(|name| !name.is_empty() && !name.starts_with('#'))
        .map(String::from)
        .collect()
}

/// Strip surrounding quotes and trailing ` # inline comments` from a raw value.
fn strip_value(raw: &str) -> String {
    let s = raw.trim();
    // Quoted value: take the content between the first matching quotes.
    for q in ['"', '\''] {
        if let Some(rest) = s.strip_prefix(q) {
            if let Some(end) = rest.find(q) {
                return rest[..end].to_string();
            }
        }
    }
    // Unquoted: drop an inline comment if present.
    let s = match s.find(" #") {
        Some(idx) => s[..idx].trim(),
        None => s,
    };
    s.trim_matches(['"', '\'']).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped `config.env.example` parses, with the original arena-infra's names, each one
    /// once (a duplicate would hand two machines one proxy port) and fit for a host name.
    #[test]
    fn the_example_config_s_names_parse_unique_and_host_safe() {
        let c = Config::parse(include_str!("../../../config.env.example"));
        let names = &c.machine_names;
        assert_eq!(names.len(), 55, "{names:?}");
        assert_eq!((names[0].as_str(), names[54].as_str()), ("apple", "zebra"));
        let unique: std::collections::HashSet<&String> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "a name listed twice");
        for n in names {
            assert!(!n.is_empty() && n.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit()), "{n:?}");
        }
        assert_eq!(c.get("MACHINE_NAME_PREFIX"), Some("arena8"));
    }

    #[test]
    fn parses_mixed_config() {
        let text = r#"
# comment
RUNPOD_API_KEY=rpa_secret
RUNPOD_GPU_TYPE="NVIDIA RTX A4000" # with comment
MACHINE_NAME_PREFIX="arena8"
RUNPOD_NUM_GPUS=1
MAX_PARALLEL=10
MACHINE_NAME_LIST=(
    "apple"
    "autumn"
    "bloom"
)
"#;
        let c = Config::parse(text);
        assert_eq!(c.get("RUNPOD_API_KEY"), Some("rpa_secret"));
        assert_eq!(c.get("RUNPOD_GPU_TYPE"), Some("NVIDIA RTX A4000"));
        assert_eq!(c.get("MACHINE_NAME_PREFIX"), Some("arena8"));
        assert_eq!(c.get_parsed::<u32>("RUNPOD_NUM_GPUS"), Some(1));
        assert_eq!(c.machine_names, vec!["apple", "autumn", "bloom"]);
    }

    #[test]
    fn env_overrides_existing_keys_only() {
        let mut c = Config::parse("SHARED_SSH_KEY_PATH=/root/.ssh/k\nIMAGE=base:1");
        // A lookup that overrides a present key and offers one that isn't in the file.
        c.apply_overrides(|k| match k {
            "SHARED_SSH_KEY_PATH" => Some("/home/dev/.ssh/k".to_string()),
            "BRAND_NEW_KEY" => Some("ignored".to_string()),
            _ => None,
        });
        assert_eq!(c.get("SHARED_SSH_KEY_PATH"), Some("/home/dev/.ssh/k"));
        assert_eq!(c.get("IMAGE"), Some("base:1")); // untouched
        assert_eq!(c.get("BRAND_NEW_KEY"), None); // env can't introduce an arbitrary key
    }

    /// Live (bsec B3): `MACHINE_NAME_LIST='( @devtest-bs1 )' arena-dev config check` still said
    /// `6 names` — the file's bash array lives in `machine_names`, which the overrides never
    /// touched. Now the environment's list wins when the file sets one, in the file's syntax (or
    /// without the parentheses); like every key outside ENV_INTRODUCIBLE, it can't introduce one.
    #[test]
    fn env_overrides_the_name_list_the_file_sets() {
        let file = "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(\n  \"apple\"\n  \"bloom\"\n  @staff-box\n)\nIMAGE=base:1\n";
        let cases: &[(&str, &[&str])] = &[
            ("( @devtest-bs1 )", &["@devtest-bs1"]),
            ("(apple \"cloud\" 'dune' #off @x)", &["apple", "cloud", "dune", "@x"]),
            ("  apple bloom  ", &["apple", "bloom"]),
            ("()", &[]),
            ("", &[]),
        ];
        for (env, want) in cases {
            let mut c = Config::parse(file);
            assert!(c.file_sets_name_list());
            c.apply_overrides(|k| (k == NAME_LIST_KEY).then(|| env.to_string()));
            assert_eq!(c.machine_names, *want, "{env:?}");
            assert_eq!((c.get("IMAGE"), c.get(NAME_LIST_KEY)), (Some("base:1"), None), "the rest as it was");
        }
        // No env var: the file's list.
        let mut c = Config::parse(file);
        c.apply_overrides(|_| None);
        assert_eq!(c.machine_names, ["apple", "bloom", "@staff-box"]);
        // A file without a list (or with an empty one, which still sets it): introduced never, overridden yes.
        let mut c = Config::parse("MACHINE_NAME_PREFIX=devtest\n");
        assert!(!c.file_sets_name_list());
        c.apply_overrides(|k| (k == NAME_LIST_KEY).then(|| "( apple )".to_string()));
        assert!(c.machine_names.is_empty());
        let mut c = Config::parse("MACHINE_NAME_LIST=()\n");
        assert!(c.file_sets_name_list() && c.machine_names.is_empty());
        c.apply_overrides(|k| (k == NAME_LIST_KEY).then(|| "( apple )".to_string()));
        assert_eq!(c.machine_names, ["apple"]);
    }

    #[test]
    fn upsert_replaces_or_appends() {
        let text = "# comment\nRUNPOD_API_KEY=\"old\"\nMACHINE_NAME_PREFIX=\"arena8\"\n";
        // replace existing
        let r = upsert_line(text, "RUNPOD_API_KEY", "rpa_new");
        assert!(r.contains("RUNPOD_API_KEY=\"rpa_new\""));
        assert!(!r.contains("\"old\""));
        assert!(r.contains("# comment")); // other lines preserved
        assert_eq!(Config::parse(&r).get("RUNPOD_API_KEY"), Some("rpa_new"));
        // append new
        let a = upsert_line(text, "VAST_API_KEY", "vk_123");
        assert!(a.trim_end().ends_with("VAST_API_KEY=\"vk_123\""));
        assert_eq!(Config::parse(&a).get("VAST_API_KEY"), Some("vk_123"));
    }

    #[test]
    fn env_can_introduce_allowlisted_keys() {
        // ARENA_START_DATE is absent from the file but may be set from the environment.
        let mut c = Config::parse("IMAGE=base:1");
        assert_eq!(c.get("ARENA_START_DATE"), None);
        c.apply_overrides(|k| (k == "ARENA_START_DATE").then(|| "2026-05-25".to_string()));
        assert_eq!(c.get("ARENA_START_DATE"), Some("2026-05-25"));
    }

    #[test]
    fn env_can_force_proxy_write_only() {
        // Absent from the file (→ the default nginx reload), an *empty* env value still
        // lands, and it also overrides a reload command the file does set.
        for file in ["IMAGE=base:1", "SSH_PROXY_RELOAD_CMD=\"nginx -s reload\""] {
            let mut c = Config::parse(file);
            c.apply_overrides(|k| (k == "SSH_PROXY_RELOAD_CMD").then(String::new));
            assert_eq!(c.get("SSH_PROXY_RELOAD_CMD"), Some(""), "{file}");
        }
    }

    #[test]
    fn env_can_introduce_runpod_api() {
        use crate::provider::RunpodApi;
        let api = |c: &Config| RunpodApi::from_config(c).unwrap();
        // Absent from the file, the default (v2) — and `RUNPOD_API=v1` from the environment
        // still selects v1 for one command; `v2` too, spelled out.
        let mut c = Config::parse("IMAGE=base:1");
        assert_eq!(api(&c), RunpodApi::V2);
        c.apply_overrides(|k| (k == "RUNPOD_API").then(|| "v1".to_string()));
        assert_eq!((c.get("RUNPOD_API"), api(&c)), (Some("v1"), RunpodApi::V1));
        let mut c = Config::parse("IMAGE=base:1");
        c.apply_overrides(|k| (k == "RUNPOD_API").then(|| "v2".to_string()));
        assert_eq!((c.get("RUNPOD_API"), api(&c)), (Some("v2"), RunpodApi::V2));
        // It overrides a file value both ways (roll back to v1, or force v2) — and an empty
        // one, as unset, means the default again.
        let mut c = Config::parse("RUNPOD_API=v2");
        c.apply_overrides(|k| (k == "RUNPOD_API").then(|| "v1".to_string()));
        assert_eq!((c.get("RUNPOD_API"), api(&c)), (Some("v1"), RunpodApi::V1));
        let mut c = Config::parse("RUNPOD_API=v1");
        c.apply_overrides(|k| (k == "RUNPOD_API").then(|| "v2".to_string()));
        assert_eq!((c.get("RUNPOD_API"), api(&c)), (Some("v2"), RunpodApi::V2));
        let mut c = Config::parse("RUNPOD_API=v1");
        c.apply_overrides(|k| (k == "RUNPOD_API").then(String::new));
        assert_eq!((c.get("RUNPOD_API"), api(&c)), (Some(""), RunpodApi::V2));
    }

    #[test]
    fn env_can_introduce_the_state_dir() {
        // Where the health cache lives can be pointed elsewhere per run (a cron job, the
        // sandbox) without touching the shared, read-only config.
        let mut c = Config::parse("IMAGE=base:1");
        c.apply_overrides(|k| (k == "ARENA_STATE_DIR").then(|| "/srv/arena-state".to_string()));
        assert_eq!(c.get("ARENA_STATE_DIR"), Some("/srv/arena-state"));
        // Likewise the backup rsync's per-pod budget.
        let mut c = Config::parse("IMAGE=base:1");
        c.apply_overrides(|k| (k == "BACKUP_TIMEOUT_SECS").then(|| "600".to_string()));
        assert_eq!(c.get("BACKUP_TIMEOUT_SECS"), Some("600"));
        // And the balance warning threshold (`BALANCE_WARN_HOURS=100000 arena balance --check`).
        let mut c = Config::parse("IMAGE=base:1");
        c.apply_overrides(|k| (k == "BALANCE_WARN_HOURS").then(|| "100000".to_string()));
        assert_eq!(c.get("BALANCE_WARN_HOURS"), Some("100000"));
        let mut c = Config::parse("IMAGE=base:1");
        c.apply_overrides(|k| (k == "BALANCE_UNPRICED_PER_GPU").then(|| "5".to_string()));
        assert_eq!(c.get("BALANCE_UNPRICED_PER_GPU"), Some("5"));
    }

    /// Live 6: the "too big to save" path of `set-branch --hard` couldn't be tried — the sandbox's
    /// file doesn't set `DISCARD_MAX_GB`, and the environment couldn't introduce it. Now it can,
    /// for one command, and overrides a file's value too; what `--hard` then reads is the
    /// environment's ([`crate::backup::discard_max_kb`]). `parse_with_env` is `load`'s own path.
    #[test]
    fn env_can_introduce_the_discard_limit() {
        let env = |v: &'static str| move |k: &str| (k == "DISCARD_MAX_GB").then(|| v.to_string());
        let c = Config::parse_with_env("IMAGE=base:1", env("0.001"));
        assert_eq!((c.get("DISCARD_MAX_GB"), crate::backup::discard_max_kb(&c)), (Some("0.001"), Ok(1049)));
        let c = Config::parse_with_env("DISCARD_MAX_GB=20\nIMAGE=base:1", env("0.5"));
        assert_eq!(crate::backup::discard_max_kb(&c), Ok(512 * 1024));
        // A bad one from the environment is as bad as from the file: `--hard` refuses it.
        let c = Config::parse_with_env("IMAGE=base:1", env("lots"));
        assert!(crate::backup::discard_max_kb(&c).unwrap_err().contains("`lots`"));
        // Without it: the file's, else the default.
        assert_eq!(crate::backup::discard_max_kb(&Config::parse_with_env("IMAGE=base:1", |_| None)), Ok(1024 * 1024));
        assert!(Config::ENV_INTRODUCIBLE.contains(&"DISCARD_MAX_GB") && !Config::is_file_only("DISCARD_MAX_GB"));
    }

    /// REVIEW RS3 (fails today): the reaper's whole authority AND every veto (`reap hold`,
    /// `reap pin`, `reap clear`) live in `$ARENA_STATE_DIR/<prefix>/reap.json`. The config FILE —
    /// what root's reap cron reads — names that dir, but an operator shell that exports another
    /// ARENA_STATE_DIR (allowed: "lets the sandbox, a cron job or a test point it somewhere
    /// explicit") silently overrides it. `reap hold` / `reap pin` typed in that shell then land
    /// in a state file the cron never reads — and print success without naming the file.
    #[test]
    fn review_rs3_env_must_not_redirect_the_reapers_state_dir() {
        let mut c = Config::parse("ARENA_STATE_DIR=/var/lib/arena\nMACHINE_NAME_PREFIX=arena9");
        c.apply_overrides(|k| (k == "ARENA_STATE_DIR").then(|| "/home/dev/.arena-state".to_string()));
        // Fix (RS3): the reaper never follows the environment's dir — it refuses, naming both.
        let e = crate::snapshot::state_dir_explicit(&c).unwrap_err();
        assert!(e.contains("/home/dev/.arena-state") && e.contains("/var/lib/arena") && e.contains("file's only"), "{e}");
        // The same value from both: fine. The environment alone (no file value): refused too.
        let mut same = Config::parse("ARENA_STATE_DIR=/var/lib/arena\nMACHINE_NAME_PREFIX=arena9");
        same.apply_overrides(|k| (k == "ARENA_STATE_DIR").then(|| "/var/lib/arena".to_string()));
        assert_eq!(crate::snapshot::state_dir_explicit(&same).unwrap(), std::path::Path::new("/var/lib/arena/arena9"));
        let mut env_only = Config::parse("MACHINE_NAME_PREFIX=arena9");
        env_only.apply_overrides(|k| (k == "ARENA_STATE_DIR").then(|| "/var/lib/arena".to_string()));
        assert!(crate::snapshot::state_dir_explicit(&env_only).unwrap_err().contains("isn't set in the config file"));
        // The health cache still follows the environment, as documented.
        assert!(crate::snapshot::health_cache_path_for(&c).unwrap().starts_with("/home/dev/.arena-state"));
        // A read that must find the reaper's record whatever the shell exports (`pods replace`
        // checking for a reap in progress) uses the file's, and never the environment's alone.
        assert_eq!(crate::snapshot::state_dir_in_file(&c).unwrap(), std::path::Path::new("/var/lib/arena/arena9"));
        assert!(crate::snapshot::state_dir_in_file(&env_only).is_err());
        assert!(crate::snapshot::state_dir_in_file(&Config::parse("ARENA_STATE_DIR=rel/dir")).unwrap_err().contains("absolute"));
    }

    /// RS8: a loaded config remembers its file (what a reap re-reads `REAP_ENABLED` from right
    /// before a terminate); a parsed text has none.
    #[test]
    fn a_loaded_config_knows_its_file() {
        let p = std::env::temp_dir().join(format!("arena-config-source-{}.env", std::process::id()));
        std::fs::write(&p, "REAP_ENABLED=1\n").unwrap();
        let c = Config::load(&p).unwrap();
        let _ = std::fs::remove_file(&p);
        assert_eq!(c.source(), Some(p.as_path()));
        assert_eq!(Config::parse("REAP_ENABLED=1").source(), None);
    }

    /// The reaper's switch and limits come from the file alone: the environment can neither
    /// turn it on over a file that says `0`, nor introduce it, nor raise a cap.
    #[test]
    fn reap_keys_are_file_only() {
        let env = |k: &str| match k {
            "REAP_ENABLED" => Some("1".to_string()),
            "REAP_MAX_PER_RUN" => Some("5".to_string()),
            "IMAGE" => Some("env:2".to_string()),
            _ => None,
        };
        let mut c = Config::parse("REAP_ENABLED=0\nREAP_MAX_PER_RUN=1\nIMAGE=base:1");
        c.apply_overrides(env);
        assert_eq!(c.get("REAP_ENABLED"), Some("0"), "env must not override the file");
        assert_eq!(c.get("REAP_MAX_PER_RUN"), Some("1"));
        assert_eq!(c.get("IMAGE"), Some("env:2"), "other keys still follow the env");
        let mut c = Config::parse("IMAGE=base:1");
        c.apply_overrides(env);
        assert_eq!(c.get("REAP_ENABLED"), None, "env must not introduce it");
        assert!(Config::is_file_only("REAP_NOTIFY_CMD") && !Config::is_file_only("ARENA_STATE_DIR"));
        assert!(!Config::ENV_INTRODUCIBLE.iter().any(|k| Config::is_file_only(k)));
    }

    #[test]
    fn env_can_introduce_broadcast_tokens() {
        // A broadcast token absent from the read-only file can be supplied per-run via env.
        let mut c = Config::parse("IMAGE=base:1");
        c.apply_overrides(|k| (k == "CLAUDE_CODE_OAUTH_TOKEN").then(|| "cc_secret".to_string()));
        assert_eq!(c.get("CLAUDE_CODE_OAUTH_TOKEN"), Some("cc_secret"));
    }
}
