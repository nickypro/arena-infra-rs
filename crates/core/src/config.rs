//! Tolerant parser for the existing `config.env` format.
//!
//! The legacy file mixes simple `KEY=value` pairs (some quoted, some with inline
//! `# comments`) with one bash array, `MACHINE_NAME_LIST=( "a" "b" ... )`, that
//! may span many lines. We parse both so the Rust tooling reads the *same* config
//! the bash/python scripts use — no migration required.

use std::collections::HashMap;
use std::path::Path;

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
    const ENV_INTRODUCIBLE: &'static [&'static str] = &["ARENA_START_DATE"];

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
}
