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
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("reading {}: {e}", path.display())))?;
        let mut cfg = Self::parse(&text);
        cfg.apply_overrides(|k| std::env::var(k).ok());
        Ok(cfg)
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
    ];

    /// Let environment variables override values from the file: any key already in the
    /// config can be overridden by an env var of the same name (e.g.
    /// `SHARED_SSH_KEY_PATH=~/.ssh/key arena-tui`), and the [`Self::ENV_INTRODUCIBLE`]
    /// keys may be *added* even if the file omits them (e.g.
    /// `ARENA_START_DATE=2026-05-25 arena backup`). This is how an operator configures a
    /// run *without editing the shared, read-only prod config*; a stray env var still
    /// can't introduce an arbitrary new setting.
    fn apply_overrides<F: Fn(&str) -> Option<String>>(&mut self, lookup: F) {
        for (k, v) in self.values.iter_mut() {
            if let Some(override_val) = lookup(k) {
                *v = override_val;
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
                let inner = buf.split(')').next().unwrap_or("");
                for tok in inner.split_whitespace() {
                    let name = tok.trim().trim_matches(['"', '\'']);
                    if !name.is_empty() && !name.starts_with('#') {
                        machine_names.push(name.to_string());
                    }
                }
                continue;
            }

            if let Some((k, v)) = trimmed.split_once('=') {
                values.insert(k.trim().to_string(), strip_value(v));
            }
        }

        Config { values, machine_names }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    pub fn require(&self, key: &str) -> Result<&str> {
        self.get(key)
            .ok_or_else(|| Error::Config(format!("missing required key `{key}`")))
    }

    pub fn get_parsed<T: std::str::FromStr>(&self, key: &str) -> Option<T> {
        self.get(key).and_then(|s| s.parse().ok())
    }
}

/// The read-only prod copy on the ARENA host. When it exists (and nothing more specific
/// was asked for) it's the config — so the production host behaves exactly as before.
pub const PROD_CONFIG: &str = "/home/dev/prod-ro/config.env";

/// The environment variable naming a config file (below `--config`, above the defaults).
pub const CONFIG_ENV_VAR: &str = "ARENA_CONFIG";

/// Where the active config path came from, so `config which` can say so accurately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    /// An explicit `--config <path>`.
    Flag,
    /// The `ARENA_CONFIG` environment variable.
    Env,
    /// The default read-only prod copy ([`PROD_CONFIG`]), which exists on this host.
    ProdDefault,
    /// The per-user default, `$XDG_CONFIG_HOME/arena/config.env` (else
    /// `~/.config/arena/config.env`), used when the prod copy isn't present.
    UserDefault,
}

impl ConfigSource {
    /// A short human description, e.g. for `config which`.
    pub fn describe(self) -> &'static str {
        match self {
            ConfigSource::Flag => "--config flag",
            ConfigSource::Env => "ARENA_CONFIG environment variable",
            ConfigSource::ProdDefault => "default (read-only prod copy)",
            ConfigSource::UserDefault => "default (user config; no prod copy on this host)",
        }
    }
}

/// The per-user config path: `$XDG_CONFIG_HOME/arena/config.env` when that's set to an
/// absolute path (per the XDG spec, relative values are ignored), else
/// `$HOME/.config/arena/config.env`. `None` when neither variable is usable.
pub fn user_config_path() -> Option<PathBuf> {
    user_config_path_with(|k| std::env::var_os(k))
}

fn user_config_path_with<F: Fn(&str) -> Option<std::ffi::OsString>>(var: F) -> Option<PathBuf> {
    let nonempty = |k: &str| var(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let base = nonempty("XDG_CONFIG_HOME")
        .filter(|p| p.is_absolute())
        .or_else(|| nonempty("HOME").map(|h| h.join(".config")))?;
    Some(base.join("arena").join("config.env"))
}

/// Resolve which config file to load — the one resolver shared by the CLI and the TUI so
/// they can't drift. Precedence: `flag` (`--config`) > `ARENA_CONFIG` (empty = unset) >
/// [`PROD_CONFIG`] if it exists > [`user_config_path`] if it exists. An explicit flag/env
/// path is returned as-is even if missing (loading it then names that path in the error);
/// if neither default exists the error lists both paths tried and how to point elsewhere.
pub fn resolve_config_path(flag: Option<&Path>) -> Result<(PathBuf, ConfigSource)> {
    resolve_config_path_with(
        flag,
        std::env::var_os(CONFIG_ENV_VAR),
        user_config_path(),
        |p| p.is_file(),
    )
}

fn resolve_config_path_with<F: Fn(&Path) -> bool>(
    flag: Option<&Path>,
    env: Option<std::ffi::OsString>,
    user: Option<PathBuf>,
    exists: F,
) -> Result<(PathBuf, ConfigSource)> {
    if let Some(p) = flag {
        return Ok((p.to_path_buf(), ConfigSource::Flag));
    }
    if let Some(e) = env.filter(|e| !e.is_empty()) {
        return Ok((PathBuf::from(e), ConfigSource::Env));
    }
    let prod = PathBuf::from(PROD_CONFIG);
    if exists(&prod) {
        return Ok((prod, ConfigSource::ProdDefault));
    }
    if let Some(u) = user.as_ref().filter(|u| exists(u)) {
        return Ok((u.clone(), ConfigSource::UserDefault));
    }
    let tried = match &user {
        Some(u) => format!("{PROD_CONFIG} and {}", u.display()),
        None => format!("{PROD_CONFIG} (no $XDG_CONFIG_HOME/$HOME for a user config)"),
    };
    Err(Error::Config(format!(
        "no config file found (tried {tried}); pass --config <path> or set \
         {CONFIG_ENV_VAR}=<path> (see config.env.example)"
    )))
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
    fn env_can_introduce_broadcast_tokens() {
        // A broadcast token absent from the read-only file can be supplied per-run via env.
        let mut c = Config::parse("IMAGE=base:1");
        c.apply_overrides(|k| (k == "CLAUDE_CODE_OAUTH_TOKEN").then(|| "cc_secret".to_string()));
        assert_eq!(c.get("CLAUDE_CODE_OAUTH_TOKEN"), Some("cc_secret"));
    }

    #[test]
    fn user_config_path_prefers_absolute_xdg_then_home() {
        let vars = |xdg: Option<&str>, home: Option<&str>| {
            let (xdg, home) = (xdg.map(String::from), home.map(String::from));
            move |k: &str| match k {
                "XDG_CONFIG_HOME" => xdg.clone().map(Into::into),
                "HOME" => home.clone().map(Into::into),
                _ => None,
            }
        };
        let got = |x, h| user_config_path_with(vars(x, h));
        assert_eq!(got(Some("/x"), Some("/h")), Some(PathBuf::from("/x/arena/config.env")));
        // Unset, empty or relative XDG_CONFIG_HOME falls back to ~/.config.
        let home = Some(PathBuf::from("/h/.config/arena/config.env"));
        assert_eq!(got(None, Some("/h")), home);
        assert_eq!(got(Some(""), Some("/h")), home);
        assert_eq!(got(Some("rel"), Some("/h")), home);
        assert_eq!(got(None, None), None);
    }

    #[test]
    fn resolve_config_precedence() {
        let user = Some(PathBuf::from("/h/.config/arena/config.env"));
        let all = |_: &Path| true;
        // --config beats ARENA_CONFIG beats the defaults (even when those exist).
        let (p, s) =
            resolve_config_path_with(Some(Path::new("/f.env")), Some("/e.env".into()), user.clone(), all).unwrap();
        assert_eq!((p.to_str().unwrap(), s), ("/f.env", ConfigSource::Flag));
        let (p, s) = resolve_config_path_with(None, Some("/e.env".into()), user.clone(), all).unwrap();
        assert_eq!((p.to_str().unwrap(), s), ("/e.env", ConfigSource::Env));
        // Empty ARENA_CONFIG is treated as unset; the prod copy wins when present.
        let (p, s) = resolve_config_path_with(None, Some("".into()), user.clone(), all).unwrap();
        assert_eq!((p.to_str().unwrap(), s), (PROD_CONFIG, ConfigSource::ProdDefault));
    }

    #[test]
    fn resolve_config_falls_back_to_user_config_then_errors() {
        let user = PathBuf::from("/h/.config/arena/config.env");
        // No prod copy on this host -> the user config.
        let only_user = |p: &Path| p == Path::new("/h/.config/arena/config.env");
        let (p, s) = resolve_config_path_with(None, None, Some(user.clone()), only_user).unwrap();
        assert_eq!((p, s), (user.clone(), ConfigSource::UserDefault));
        // Neither exists -> an error naming both paths and how to point elsewhere.
        let e = resolve_config_path_with(None, None, Some(user), |_| false).unwrap_err().to_string();
        assert!(e.contains(PROD_CONFIG), "{e}");
        assert!(e.contains("/h/.config/arena/config.env"), "{e}");
        assert!(e.contains("--config") && e.contains("ARENA_CONFIG"), "{e}");
    }
}
