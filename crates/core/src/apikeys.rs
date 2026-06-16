//! Distributing API keys to pods (legacy `copy_api_keys.py`).
//!
//! For evals week the pods need provider API keys exported in the participants' shells.
//! Two shapes:
//!   * **per-host** keys (OpenAI / Anthropic / OpenRouter) come from CSV files
//!     `keys/<provider>_api_keys.csv`, each line `<pod-name>,<key>` — different key per
//!     machine (rate-limit / billing isolation), and
//!   * a **broadcast** key (Hugging Face) is the *same* token on every pod, so the
//!     cohort can pull from gated repos we've been approved for (Llama 3, etc.).
//!
//! This module is pure: CSV parsing + rendering the idempotent shell command that
//! appends `export …` lines to `~/.bashrc` and `~/.zshrc`. The scp/ssh and file IO live
//! in the caller. Appends are idempotent (`grep -qxF || echo`) so re-running is safe —
//! unlike the legacy script, which appended unconditionally and duplicated lines.

/// The per-host providers we know how to distribute, as
/// `(csv_basename, display_name, &[shell env var names])`. The CSV file for each is
/// `keys/<csv_basename>_api_keys.csv`. A provider may set more than one env var (some
/// libraries read a legacy name), so the value is fanned out to each.
pub const PROVIDERS: &[(&str, &str, &[&str])] = &[
    ("openai", "OpenAI", &["OPENAI_API_KEY"]),
    ("anthropic", "Anthropic", &["ANTHROPIC_API_KEY"]),
    ("openrouter", "OpenRouter", &["OPENROUTER_API_KEY"]),
    // Hugging Face is usually broadcast (see `hf_env_vars`), but a per-host CSV is also
    // honored if present. Both env names are set: `transformers`/`hub` read `HF_TOKEN`;
    // older code reads `HUGGING_FACE_HUB_TOKEN`.
    ("huggingface", "Hugging Face", &["HF_TOKEN", "HUGGING_FACE_HUB_TOKEN"]),
];

/// Tokens broadcast identically to *every* pod (same value everywhere), as
/// `(config key, display name, &[env var names to export])`. The value comes from the
/// config key (or a CLI override); each listed env var is set to it. Hugging Face sets
/// two names (`transformers`/`hub` read `HF_TOKEN`, older code `HUGGING_FACE_HUB_TOKEN`);
/// Claude Code reads `CLAUDE_CODE_OAUTH_TOKEN`.
pub const BROADCAST_TOKENS: &[(&str, &str, &[&str])] = &[
    ("HF_TOKEN", "Hugging Face", &["HF_TOKEN", "HUGGING_FACE_HUB_TOKEN"]),
    ("CLAUDE_CODE_OAUTH_TOKEN", "Claude Code", &["CLAUDE_CODE_OAUTH_TOKEN"]),
];

/// Build the `(env name, value)` exports for every [`BROADCAST_TOKENS`] entry that
/// `lookup(config_key)` returns a non-empty value for. `lookup` is where the value comes
/// from — config, or a CLI override layered over it.
pub fn broadcast_env_vars<F: Fn(&str) -> Option<String>>(lookup: F) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (cfg_key, _display, env_names) in BROADCAST_TOKENS {
        if let Some(v) = lookup(cfg_key).filter(|s| !s.is_empty()) {
            for env in *env_names {
                out.push((env.to_string(), v.clone()));
            }
        }
    }
    out
}

/// Parse a `<host>,<key>` CSV into `(host, key)` pairs. Tolerant: trims whitespace,
/// skips blank lines and `#` comments, and ignores a leading header row (`host,...`).
/// Anything after the first comma is treated as the key (keys don't contain commas).
pub fn parse_csv(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((host, key)) = line.split_once(',') else { continue };
        let (host, key) = (host.trim(), key.trim());
        // Skip an obvious header row.
        if host.eq_ignore_ascii_case("host") || host.eq_ignore_ascii_case("hostname") {
            continue;
        }
        if host.is_empty() || key.is_empty() {
            continue;
        }
        out.push((host.to_string(), key.to_string()));
    }
    out
}

/// Upsert a `host,key` row into a per-host keys CSV's text: replace the line for `host`
/// if present, else append `host,key`. Other lines (comments, blanks) are preserved.
/// Used by key generation/rotation to persist a machine's freshly minted key.
pub fn upsert_csv(text: &str, host: &str, key: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut replaced = false;
    for line in text.lines() {
        let is_row = line
            .split_once(',')
            .map(|(h, _)| h.trim() == host)
            .unwrap_or(false);
        if is_row && !replaced {
            out.push(format!("{host},{key}"));
            replaced = true;
        } else {
            out.push(line.to_string());
        }
    }
    if !replaced {
        out.push(format!("{host},{key}"));
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// Render an idempotent shell command that ensures each `export NAME="value"` line is
/// present in both `~/.bashrc` and `~/.zshrc` (creating the files if absent). Safe to
/// re-run: a line already present is not appended again.
pub fn remote_export_command(vars: &[(String, String)]) -> String {
    let mut cmd = String::from("set -e");
    for file in ["$HOME/.bashrc", "$HOME/.zshrc"] {
        cmd.push_str(&format!("; touch {file}"));
        for (name, value) in vars {
            // The whole `export NAME="value"` is one shell word we grep for verbatim.
            let line = format!("export {name}=\"{value}\"");
            let q = shell_quote(&line);
            cmd.push_str(&format!(
                "; (grep -qxF {q} {file} || echo {q} >> {file})"
            ));
        }
    }
    // Claude Code ignores CLAUDE_CODE_OAUTH_TOKEN until onboarding is marked complete, so
    // set hasCompletedOnboarding=true in ~/.claude.json — create it, or merge into an
    // existing one (preserving its other keys).
    if vars.iter().any(|(n, _)| n == "CLAUDE_CODE_OAUTH_TOKEN") {
        cmd.push_str(
            "; python3 -c 'import json,os; p=os.path.expanduser(\"~/.claude.json\"); \
             d=json.load(open(p)) if os.path.isfile(p) and os.path.getsize(p) else {}; \
             d[\"hasCompletedOnboarding\"]=True; json.dump(d,open(p,\"w\"))'",
        );
    }
    cmd
}

/// Single-quote for safe inclusion in a `sh -c` string (POSIX `'\''` escaping).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_host_key_csv_tolerantly() {
        let csv = "\
# evals week
arena8-apple,sk-aaa
arena8-autumn , sk-bbb
\n
host,API_KEY
arena8-bloom,sk-ccc,extra-ignored-no
";
        let rows = parse_csv(csv);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], ("arena8-apple".to_string(), "sk-aaa".to_string()));
        assert_eq!(rows[1], ("arena8-autumn".to_string(), "sk-bbb".to_string()));
        // everything after the first comma is the key
        assert_eq!(rows[2], ("arena8-bloom".to_string(), "sk-ccc,extra-ignored-no".to_string()));
    }

    #[test]
    fn broadcast_builds_exports_for_present_tokens_only() {
        // Only HF set -> both HF env names, no Claude Code.
        let vars = broadcast_env_vars(|k| (k == "HF_TOKEN").then(|| "hf_secret".to_string()));
        assert_eq!(vars.len(), 2);
        assert!(vars.contains(&("HF_TOKEN".into(), "hf_secret".into())));
        assert!(vars.contains(&("HUGGING_FACE_HUB_TOKEN".into(), "hf_secret".into())));
        // Both set -> includes the Claude Code token too.
        let both = broadcast_env_vars(|k| match k {
            "HF_TOKEN" => Some("h".to_string()),
            "CLAUDE_CODE_OAUTH_TOKEN" => Some("cc".to_string()),
            _ => None,
        });
        assert!(both.contains(&("CLAUDE_CODE_OAUTH_TOKEN".into(), "cc".into())));
        assert_eq!(both.len(), 3);
        // Nothing set -> empty.
        assert!(broadcast_env_vars(|_| None).is_empty());
    }

    #[test]
    fn export_command_is_idempotent_and_covers_both_shells() {
        let cmd = remote_export_command(&[("HF_TOKEN".into(), "hf_x".into())]);
        assert!(cmd.contains("$HOME/.bashrc"));
        assert!(cmd.contains("$HOME/.zshrc"));
        // idempotent guard
        assert!(cmd.contains("grep -qxF"));
        // the exact export line is what we look for + append
        assert!(cmd.contains(r#"'export HF_TOKEN="hf_x"'"#));
    }

    #[test]
    fn upsert_csv_replaces_or_appends() {
        let text = "# openrouter keys\narena8-apple,sk-or-old\narena8-bloom,sk-or-b\n";
        let r = upsert_csv(text, "arena8-apple", "sk-or-new");
        assert!(r.contains("arena8-apple,sk-or-new"));
        assert!(!r.contains("sk-or-old"));
        assert!(r.contains("arena8-bloom,sk-or-b")); // others kept
        assert!(r.contains("# openrouter keys"));
        // append a new host
        let a = upsert_csv(text, "arena8-nova", "sk-or-n");
        assert!(a.trim_end().ends_with("arena8-nova,sk-or-n"));
    }

    #[test]
    fn export_command_escapes_single_quotes_in_values() {
        let cmd = remote_export_command(&[("X".into(), "a'b".into())]);
        // the value's quote is POSIX-escaped so the sh -c word stays intact
        assert!(cmd.contains(r#"'\''"#));
    }
}
