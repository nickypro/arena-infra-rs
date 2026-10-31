//! `arena config check|which|set`: what config.env sets, where each key came from, and the
//! report `config check` prints (every section's rows).

use anyhow::{Context, Result};
use arena_core::config::{ConfigSource, ProviderSource, PROVIDER_KEY};
use arena_core::sshconfig::Audience;
use arena_core::{Config, PodSpec};
use clap::Subcommand;

use crate::cron::reap_cron_installed;
use crate::pods::backup::RSYNC;
use crate::pods::health::health_cache_path;
use crate::ssh_config::{same_file, ssh_config_check_row, ssh_config_check_row_as};
use crate::{archive, image, money, reap};

#[derive(Subcommand)]
pub(crate) enum ConfigCmd {
    /// Check that the needed config keys are set (read-only)
    ///
    /// Never prints secret values. Exits non-zero if a required key is missing.
    Check,
    /// Set a key in config.env (never echoes the value)
    ///
    /// Replaces the key's line in the --config file, else appends KEY="value".
    /// Give only KEY to read the value from stdin (keeps it out of `ps`):
    ///   printf %s "$TOKEN" | arena config set HF_TOKEN
    /// With no KEY it prompts interactively (needs a terminal).
    #[command(verbatim_doc_comment)]
    Set {
        /// Config key, e.g. RUNPOD_API_KEY. Omit to choose interactively.
        key: Option<String>,
        /// Value to store (will be quoted). Omit to read from stdin / be prompted.
        value: Option<String>,
    },
    /// Show the active config file and a summary of what's loaded
    Which,
}

/// The config file a run reads and the fleet's provider, each with where it came from
/// ([`arena_core::config::find_config`], [`arena_core::config::pick_provider`]).
pub(crate) struct Picked {
    pub config: std::path::PathBuf,
    pub config_source: ConfigSource,
    pub provider: String,
    pub provider_source: ProviderSource,
}

impl Picked {
    /// The fleet-wide provider, [`Picked::provider`] its primary. A bad `PROVIDER` in the config
    /// is named as that (`--provider`'s own error names the flag's value already).
    pub(crate) fn fleet(&self, cfg: &Config) -> Result<Box<dyn arena_core::provider::Provider>> {
        let fleet = arena_core::provider::build_fleet(&self.provider, cfg, true);
        match self.provider_source {
            ProviderSource::Config => fleet.with_context(|| format!("the config's {PROVIDER_KEY}=\"{}\" ({})", self.provider, self.config.display())),
            _ => Ok(fleet?),
        }
    }

    /// How `config which` says where the provider came from.
    fn provider_from(&self) -> &'static str {
        match self.provider_source {
            ProviderSource::Chosen => "--provider",
            ProviderSource::Config => "the config's PROVIDER",
            ProviderSource::Default => "the default: no --provider, no PROVIDER in the config",
        }
    }
}

pub(crate) fn handle_config(cmd: ConfigCmd, cfg: &Config, picked: &Picked) -> Result<()> {
    let (provider_name, config_path) = (picked.provider.as_str(), picked.config.as_path());
    match cmd {
        ConfigCmd::Check => config_check(cfg, provider_name, &[]),
        ConfigCmd::Which => config_which(cfg, picked),
        ConfigCmd::Set { key, value } => {
            let (key, value) = match (key, value) {
                (Some(k), Some(v)) => (k, v),
                // Key given but no value: read it from stdin when piped (script-friendly,
                // and keeps secrets out of argv/`ps`), or prompt at a terminal.
                (Some(k), None) => {
                    let v = read_value_for(&k)?;
                    (k, v)
                }
                // No key: pick interactively (needs a terminal).
                (None, _) => interactive_config_set(cfg)?,
            };
            let text = std::fs::read_to_string(config_path)
                .with_context(|| format!("reading {}", config_path.display()))?;
            let updated = arena_core::config::upsert_line(&text, &key, &value);
            std::fs::write(config_path, updated)
                .with_context(|| format!("writing {}", config_path.display()))?;
            // Never echo the value (it may be a secret).
            println!("set {key} in {}", config_path.display());
            Ok(())
        }
    }
}

/// Report one config key: prints a ✓/✗/· line (never the value if `secret`) and
/// records it in `missing` when it's required but absent/empty. An absent optional key reads
/// "not set": "(missing)" is for a required one, which fails the check (PR #1).
fn cfg_row(out: &mut String, cfg: &Config, missing: &mut Vec<String>, key: &str, required: bool, secret: bool) {
    let ok = cfg.get(key).map(|v| !v.is_empty()).unwrap_or(false);
    let mark = if ok { "✓" } else if required { "✗" } else { "·" };
    let shown = if !ok {
        if required { "(missing)" } else { "not set" }.to_string()
    } else if secret {
        "(set)".to_string()
    } else {
        cfg.get(key).unwrap_or("").to_string()
    };
    use std::fmt::Write as _;
    let _ = writeln!(out, "  {mark} {key:<28} {shown}");
    if required && !ok {
        missing.push(key.to_string());
    }
}

/// `config check`'s entry for a value its parser refused, as `KEY (why)` — the key read off
/// the error, which names it first (`KEY must be …`, `KEY="x" is not …`, `KEY: …`) — added
/// once per key: a bad CUDA value breaks two rows (the create spec's and the deep check's
/// driver floor) but is one key to fix.
fn note_invalid(missing: &mut Vec<String>, e: &arena_core::Error) {
    let msg = match e {
        arena_core::Error::Config(m) => m.clone(),
        other => other.to_string(),
    };
    note_invalid_text(missing, &msg);
}

/// [`note_invalid`] for a parser that reports a plain message (`KEY …` first).
pub(crate) fn note_invalid_text(missing: &mut Vec<String>, msg: &str) {
    let msg = msg.to_string();
    let key: String = msg.chars().take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_').collect();
    if key.is_empty() {
        missing.push(msg);
        return;
    }
    if missing.iter().any(|m| m.split([' ', '(']).next() == Some(key.as_str())) {
        return;
    }
    let mut why = msg[key.len()..].trim_start_matches(['=', ':', ' ']).trim();
    // The value itself is on its ✗ row.
    if let Some(i) = why.rfind(" (got ").filter(|_| why.ends_with(')')) {
        why = &why[..i];
    }
    missing.push(format!("{key} ({why})"));
}

/// The keys offered in the interactive `config set` picker, with whether each is a
/// secret (so we show "set" rather than the value). Order roughly by how often they're
/// set per run.
const SETTABLE_KEYS: &[(&str, bool)] = &[
    ("RUNPOD_API_KEY", true),
    ("VAST_API_KEY", true),
    ("HETZNER_API_KEY", true),
    ("SHADEFORM_API_KEY", true), // opt-in overflow GPU VMs (`--provider shadeform`)
    ("HF_TOKEN", true), // broadcast to pods for gated repos (Llama 3 …)
    ("CLAUDE_CODE_OAUTH_TOKEN", true), // broadcast to pods for Claude Code access
    ("OPENROUTER_PROVISIONING_KEY", true), // mints per-machine OpenRouter keys (`arena keys`)
    ("OPENROUTER_WORKSPACE", false), // the OpenRouter workspace arena's keys live in (id or slug)
    ("ARENA_START_DATE", false),
    ("MACHINE_NAME_PREFIX", false),
    ("SHARED_SSH_KEY_PATH", false),
    ("ADMIN_SSH_KEY_PATH", false), // optional: a key only the proxy box holds, every machine lets in
    ("ARCHIVE_HETZNER_API_KEY", true), // the backup archive's own Hetzner project (`arena archive`)
    ("ARCHIVE_KEEP_UNTIL", false),
    ("ARCHIVE_HOST", false),
];

/// How a key currently reads for the picker: "not set", "set" (secret), or its value.
fn key_state(cfg: &Config, key: &str, secret: bool) -> String {
    match cfg.get(key).filter(|v| !v.is_empty()) {
        None => "· not set".to_string(),
        Some(_) if secret => "✓ set".to_string(),
        Some(v) => format!("✓ {v}"),
    }
}

/// Read the value for `key` when it wasn't given on the command line: from **stdin** if
/// it's piped (so scripts can do `printf %s "$TOKEN" | arena config set KEY` — the secret
/// never lands in argv / `ps` / shell history), or by prompting at a terminal. Trailing
/// newline is stripped; an empty value is an error.
fn read_value_for(key: &str) -> Result<String> {
    use std::io::{IsTerminal, Read, Write};
    if std::io::stdin().is_terminal() {
        eprint!("value for {key}: ");
        std::io::stderr().flush().ok();
        let mut s = String::new();
        std::io::stdin().read_line(&mut s)?;
        let s = s.trim().to_string();
        if s.is_empty() {
            anyhow::bail!("empty value — nothing set");
        }
        Ok(s)
    } else {
        // Piped: take all of stdin, trimming a trailing newline (the usual `echo`/here-string).
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).context("reading value from stdin")?;
        let s = s.trim_end_matches(['\n', '\r']).to_string();
        if s.is_empty() {
            anyhow::bail!(
                "no value for {key}: pass it as an argument or pipe it \
                 (e.g. `printf %s \"$VALUE\" | arena config set {key}`)"
            );
        }
        Ok(s)
    }
}

/// Interactively pick a config key and read its value (for `config set` with no args).
/// Shows which keys are already set (secrets as "set", others as their value) so the
/// operator can see at a glance what's configured. Requires a terminal.
fn interactive_config_set(cfg: &Config) -> Result<(String, String)> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("config set needs a key and value non-interactively: arena config set KEY VALUE");
    }
    let read = |prompt: &str| -> Result<String> {
        eprint!("{prompt}");
        std::io::stderr().flush().ok();
        let mut s = String::new();
        std::io::stdin().read_line(&mut s)?;
        Ok(s.trim().to_string())
    };

    eprintln!("Which key to set?  (✓ = already set in this config)\n");
    for (i, (k, secret)) in SETTABLE_KEYS.iter().enumerate() {
        eprintln!("  {:>2}) {k:<22} {}", i + 1, key_state(cfg, k, *secret));
    }
    eprintln!("  {:>2}) other (type the key name)", SETTABLE_KEYS.len() + 1);
    let choice = read("\n> ")?;
    let (key, secret) = match choice.parse::<usize>() {
        Ok(n) if (1..=SETTABLE_KEYS.len()).contains(&n) => {
            let (k, s) = SETTABLE_KEYS[n - 1];
            (k.to_string(), s)
        }
        Ok(n) if n == SETTABLE_KEYS.len() + 1 => (read("key name: ")?, true),
        _ => (choice, true), // a key name typed directly; treat as secret-ish
    };
    if key.is_empty() {
        anyhow::bail!("no key chosen");
    }
    // Flag an overwrite so it's never a surprise.
    if cfg.get(&key).map(|v| !v.is_empty()).unwrap_or(false) {
        eprintln!("({key} is already set — entering a value overwrites it; blank keeps it)");
    }
    let value = read(&format!("value for {key}: "))?;
    if value.is_empty() {
        anyhow::bail!("empty value — nothing set");
    }
    let _ = secret; // (reserved: could mask the input later)
    Ok((key, value))
}

/// `config which`: show the active config file (path, readable/writable) and where that path came
/// from (`--config`, `ARENA_CONFIG`, `~/.config/arena/config.env` or the built-in default — and
/// any of them that lost), a one-line summary of what parsed with where the provider came from,
/// where the fleet's own files are, and any keys currently being supplied by the environment
/// (which silently override the file) so it's clear where values are coming from.
fn config_which(cfg: &Config, picked: &Picked) -> Result<()> {
    let config_path = picked.config.as_path();
    let abs = std::fs::canonicalize(config_path).unwrap_or_else(|_| config_path.to_path_buf());
    let meta = std::fs::metadata(&abs).ok();
    let readable = std::fs::File::open(&abs).is_ok();
    let writable = meta
        .as_ref()
        .map(|m| !m.permissions().readonly())
        .unwrap_or(false);

    println!("Active config: {}", abs.display());
    println!(
        "  {}  ·  {}",
        if readable { "✓ readable" } else { "✗ not readable" },
        if writable { "writable" } else { "read-only (config set can't write here)" },
    );
    println!("  source: {}", picked.config_source.describe());
    let canonical = |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let lost = arena_core::config::passed_over(
        config_path,
        picked.config_source,
        std::env::var_os(arena_core::config::CONFIG_VAR).as_deref(),
        std::env::var_os("HOME").as_deref(),
        std::path::Path::is_file,
        |a, b| canonical(a) == canonical(b),
    );
    for l in lost {
        println!("  ⚠ {l}");
    }
    println!("\nLoaded: provider {} ({}) · {} key(s) · {} machine name(s)",
        picked.provider, picked.provider_from(), cfg.values.len(), cfg.machine_names.len());

    println!("\nFiles kept beside the config (else found in the current directory, as before):");
    for line in fleet_file_rows(cfg) {
        println!("{line}");
    }

    let (from_env, ignored) = env_keys(cfg, |k| std::env::var(k).is_ok());
    if !ignored.is_empty() {
        println!("\nIgnored from the environment (file-only, {}*): {}", Config::FILE_ONLY_PREFIX, ignored.join(", "));
    }
    if from_env.is_empty() {
        println!("\nEnvironment overrides: none (all values come from the file)");
    } else {
        println!("\nEnvironment overrides in effect (env wins over the file):");
        for k in from_env {
            println!("  • {k}");
        }
    }
    Ok(())
}

/// `config which`'s rows for the files a fleet keeps beside its config: `keys/`, the plan, and
/// the backup dir (`LOCAL_BACKUP_DIR` when set) — where each is, and how it was found
/// ([`arena_core::config::FleetFile::describe`]).
fn fleet_file_rows(cfg: &Config) -> Vec<String> {
    use arena_core::config::{KEYS_DIR, PLAN_FILE};
    let mut rows: Vec<String> = [(KEYS_DIR, "keys/"), (PLAN_FILE, PLAN_FILE)]
        .iter()
        .map(|(rel, label)| format!("  {label:<16} {}", cfg.fleet_file(rel).describe()))
        .collect();
    let keys = cfg.fleet_file(KEYS_DIR).path;
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    rows.extend(arena_core::config::keys_link_note(&keys, prefix).map(|n| format!("  {:<16} ⚠ {n}", "")));
    rows.push(match cfg.local_backup_dir() {
        (dir, None) => format!("  {:<16} {dir} (LOCAL_BACKUP_DIR)", "backup/"),
        (_, Some(f)) => format!("  {:<16} {}", "backup/", f.describe()),
    });
    rows
}

/// What `config which` says of the environment (`is_set`: whether a variable of that name is set):
/// the keys whose value now comes from it — every key of the loaded config with a variable of its
/// name, whether that overrode the file's value or introduced one the file lacks
/// (`Config::ENV_INTRODUCIBLE`: `DISCARD_MAX_GB`, …); env overrides the file at load, easy to
/// forget, so it's said — and the name list when the file has one (parsed apart from `values`:
/// the environment's wins then). Then the file-only keys whose variable is ignored (the reaper's:
/// it looks like it should win). Each sorted. Pure.
fn env_keys(cfg: &Config, is_set: impl Fn(&str) -> bool) -> (Vec<&str>, Vec<&str>) {
    let mut from_env: Vec<&str> = cfg.values.keys().map(String::as_str).filter(|k| !Config::is_file_only(k) && is_set(k)).collect();
    let list = arena_core::config::NAME_LIST_KEY;
    if cfg.file_sets_name_list() && is_set(list) {
        from_env.push(list);
    }
    from_env.sort_unstable();
    let mut ignored: Vec<&str> = cfg.values.keys().map(String::as_str).filter(|k| Config::is_file_only(k) && is_set(k)).collect();
    ignored.sort_unstable();
    (from_env, ignored)
}

/// The `config check` row for `CREATE_EXTRA_JSON` (`None` when unset), and whether it
/// parses: its top-level keys only — never the values, which may carry a token. Whether a key
/// is one arena sets itself depends on the provider, so that's checked at create. Pure.
fn create_extra_row(cfg: &Config) -> Option<(String, bool)> {
    let key = "CREATE_EXTRA_JSON";
    cfg.get(key)?;
    Some(match arena_core::apiextra::combine(cfg.get(key), None) {
        Ok(None) => (format!("  · {key:<28} (empty)"), true),
        Ok(Some(extra)) => (
            format!(
                "  ✓ {key:<28} keys [{}] — merged into every create body (fields arena sets are refused at create)",
                extra.keys().map(String::as_str).collect::<Vec<_>>().join(", ")
            ),
            true,
        ),
        Err(e) => (format!("  ✗ {key:<28} {}", e.to_string().trim_start_matches("config error: ")), false),
    })
}

/// The `config check` row for `PROVIDER` (the fleet's provider when `--provider` doesn't say),
/// and what the check fails on: a name that isn't a provider. `selected`: the provider this run
/// uses, to say when `--provider` overrode the key. Pure.
fn provider_key_row(cfg: &Config, selected: &str) -> (String, Option<String>) {
    use arena_core::config::{pick_provider, DEFAULT_PROVIDER};
    use arena_core::provider::multi::KNOWN;
    let key = PROVIDER_KEY;
    let (name, source) = pick_provider(None, cfg);
    if source == ProviderSource::Default {
        return (format!("  · {key:<24} not set — {DEFAULT_PROVIDER} unless --provider says"), None);
    }
    if !KNOWN.contains(&name.as_str()) {
        let why = format!("`{name}` isn't a provider ({})", KNOWN.join(", "));
        return (format!("  ✗ {key:<24} {why}"), Some(format!("{key} ({why})")));
    }
    let wins = if name == selected { String::new() } else { format!(" (--provider {selected} wins this run)") };
    (format!("  ✓ {key:<24} {name} — create/up's provider when --provider doesn't say{wins}"), None)
}

/// The `config check` row for `RUNPOD_API`, and whether it's valid. Pure, for testing.
/// v2 is the default (unset or empty, said so). An explicit v1 still works but gets a
/// warning with the retirement date — past it, every RunPod call fails — and the way back.
fn runpod_api_row(cfg: &Config) -> (String, bool) {
    use arena_core::provider::{RunpodApi, RUNPOD_V1_REMEDY, RUNPOD_V1_RETIREMENT};
    let key = "RUNPOD_API";
    match RunpodApi::from_config(cfg) {
        Ok(RunpodApi::V2) => (
            format!(
                "  ✓ {key:<24} v2{} (api.runpod.io/v2)",
                if cfg.get(key).is_some_and(|v| !v.trim().is_empty()) { "" } else { " (default)" }
            ),
            true,
        ),
        Ok(RunpodApi::V1) => (
            format!(
                "  ⚠ {key:<24} v1 (rest.runpod.io/v1) — RunPod retires REST v1 on {RUNPOD_V1_RETIREMENT}; {RUNPOD_V1_REMEDY}"
            ),
            true,
        ),
        Err(e) => (format!("  ✗ {key:<24} {e}"), false),
    }
}

/// The `config check` lines for the host CUDA constraint (finding #10), and whether it's
/// usable on the RunPod generation in use. Pure, for testing.
/// - What creates send: a floor (`≥ 13.0`, from `MIN_CUDA_VERSION` or the lowest of
///   `ALLOWED_CUDA_VERSIONS`) or an exact set (`EXACT_CUDA_VERSIONS`).
/// - ⚠ an exact set — RunPod matches it exactly, so a host on a newer CUDA is "no capacity";
///   ⚠ an `ALLOWED_CUDA_VERSIONS` list (it used to be an exact set; now only its lowest
///   version counts); ⚠ keys shadowed by a stronger one.
/// - ✗ (not ok) on RunPod v1 (only ever an explicit `RUNPOD_API=v1`) when its schema can't
///   express the constraint (13.1+).
/// - ✗ alone, in place of the ✓/⚠ row, for a value that isn't a version (`13.x`): creates
///   are refused, so nothing about it is fine (live BPF S5 printed `✓ ≥ 13.x` above the ✗).
///
/// The `Err` is what `config check`'s final error lists: the key and why it's invalid, or
/// what RunPod v1 can't express.
fn cuda_rows(cfg: &Config, runpod_v1: bool) -> (Vec<String>, std::result::Result<(), String>) {
    let key = "host CUDA";
    let cuda = arena_core::pod::cuda_from_config(cfg);
    let mut out = Vec::new();
    let bad = cuda.exact.iter().chain(&cuda.floor).find(|v| arena_core::pod::cuda_key(v).is_none());
    match (&cuda.floor, cuda.exact.is_empty()) {
        _ if bad.is_some() => out.push(format!(
            "  ✗ {key:<27}`{}` isn't a version like 13.0 (from {}) — every create would be refused",
            bad.map(String::as_str).unwrap_or_default(),
            cuda.source.unwrap_or_default()
        )),
        (_, false) => out.push(format!(
            "  ⚠ {key:<27}exactly {} ({}) — RunPod matches this exactly: hosts on a newer CUDA are \"no capacity\"; \
             MIN_CUDA_VERSION sets a floor instead",
            cuda.exact.join(", "),
            cuda.source.unwrap_or_default()
        )),
        (Some(floor), true) => out.push(format!("  ✓ {key:<27}≥ {floor} (a floor, from {})", cuda.source.unwrap_or_default())),
        (None, true) => out.push(format!("  · {key:<27}any (no MIN_CUDA_VERSION / ALLOWED_CUDA_VERSIONS)")),
    }
    if bad.is_none() && cuda.source.is_some_and(|s| s.ends_with("ALLOWED_CUDA_VERSIONS")) {
        let raw = cfg.get(cuda.source.unwrap_or_default()).unwrap_or_default();
        if raw.split(',').filter(|v| !v.trim().is_empty()).count() > 1 {
            out.push(format!(
                "      └ {} lists several versions: only the lowest counts now (a floor) — EXACT_CUDA_VERSIONS for an exact set",
                cuda.source.unwrap_or_default()
            ));
        }
    }
    for shadowed in cuda.set.iter().skip(1) {
        out.push(format!("      └ {shadowed} is ignored: {} takes precedence", cuda.set[0]));
    }
    if let Some(bad) = bad {
        // The v1 check below would only say the same again.
        return (out, Err(format!("{} (`{bad}` isn't a version like 13.0)", cuda.source.unwrap_or("the host CUDA key"))));
    }
    let mut ok = Ok(());
    if runpod_v1 {
        use arena_core::provider::RUNPOD_V1_REMEDY;
        let spec = PodSpec { allowed_cuda: cuda.exact.clone(), min_cuda: cuda.floor.clone(), ..PodSpec::from_config(cfg) };
        match arena_core::provider::runpod::v1_cuda_versions(&spec) {
            Ok(Some(_)) if cuda.exact.is_empty() => out.push(format!(
                "      └ RUNPOD_API=v1 has no real floor: it sends its list's versions ≥ the floor, newest {} — \
                 {RUNPOD_V1_REMEDY} to reach newer hosts",
                arena_core::provider::runpod::V1_CUDA_VERSIONS[0]
            )),
            Ok(_) => {}
            Err(e) => {
                out.push(format!("  ✗ {key:<27}{e}"));
                ok = Err(format!("a host CUDA constraint RunPod v1 can express, or {RUNPOD_V1_REMEDY}"));
            }
        }
    }
    (out, ok)
}

/// The `config check` row for `VAST_ALLOW_UNVERIFIED`, and whether it's valid. Pure, for
/// testing. Allowing unverified hosts is a warning: they're the flakier part of the market.
fn vast_hosts_row(cfg: &Config) -> (String, bool) {
    let key = "VAST_ALLOW_UNVERIFIED";
    match arena_core::provider::vast::unverified_allowed(cfg) {
        Ok(false) => (format!("  ✓ {key:<24} no (vast rents verified hosts only)"), true),
        Ok(true) => (format!("  ⚠ {key:<24} yes (vast may rent unverified/deverified hosts)"), true),
        Err(e) => (format!("  ✗ {key:<24} {e}"), false),
    }
}

/// `config check`'s Shadeform rows — its options as the provider reads them (no request: the
/// SSH key's presence on the account is checked at create) — and the refusal when one is
/// invalid (its ✗ row says which; the whole check then fails). Pure, for testing. Never shows
/// the API key, and an invalid value is described, never echoed when long.
fn shadeform_rows(cfg: &Config) -> (Vec<String>, Option<arena_core::Error>) {
    use arena_core::provider::shadeform::{Clouds, ShadeformOpts, DEFAULT_KEY};
    let o = match ShadeformOpts::from_config(cfg) {
        Ok(o) => o,
        Err(e) => {
            let why = match &e {
                arena_core::Error::Config(m) => m.clone(),
                other => other.to_string(),
            };
            return (vec![format!("  ✗ SHADEFORM_*                  {why}")], Some(e));
        }
    };
    let set = |k: &str| cfg.get(k).is_some_and(|v| !v.trim().is_empty());
    let default = |k: &str| if set(k) { "" } else { " (default)" };
    let mut out = Vec::new();
    out.push(match &o.clouds {
        Clouds::All => "  ⚠ SHADEFORM_CLOUDS             * — any cloud: ports, images, drivers and boot times differ per cloud; \
                        widen one cloud at a time after a live test"
            .to_string(),
        Clouds::Only(c) => format!("  ✓ SHADEFORM_CLOUDS             {}{}", c.join(", "), default("SHADEFORM_CLOUDS")),
    });
    out.push(if o.regions.is_empty() {
        "  · SHADEFORM_REGIONS            (any region)".to_string()
    } else {
        format!("  ✓ SHADEFORM_REGIONS            {}", o.regions.join(", "))
    });
    out.push(format!(
        "  ✓ SHADEFORM_MAX_PRICE          {}/h per pod{} — the ceiling under any --max-price",
        arena_core::fleet::fmt_money("$", o.max_price),
        default("SHADEFORM_MAX_PRICE")
    ));
    out.push(if o.ssh_key == DEFAULT_KEY {
        "  ⚠ SHADEFORM_SSH_KEY            default — the account's default key (on a new account the Shadeform \
         Managed Key, whose private half Shadeform holds and returns to any API key holder)"
            .to_string()
    } else {
        format!(
            "  ✓ SHADEFORM_SSH_KEY            {}{} (must be uploaded to the Shadeform account under that name — checked at create)",
            o.ssh_key,
            default("SHADEFORM_SSH_KEY")
        )
    });
    out.push(format!("  · SHADEFORM_MAX_BOOT_SECS      {}{}", o.max_boot_secs, default("SHADEFORM_MAX_BOOT_SECS")));
    out.push(format!("  · SHADEFORM_READY_MINS         {}{}", o.ready_mins, default("SHADEFORM_READY_MINS")));
    out.push(format!(
        "  · SHADEFORM_ALLOW_BAREMETAL    {}{}",
        if o.allow_baremetal { "yes" } else { "no" },
        default("SHADEFORM_ALLOW_BAREMETAL")
    ));
    out.push(
        "      └ shadeform: no stop (a VM bills until terminated), no volumes; what happens at a $0 wallet is \
         undocumented — keep Auto Top Up on"
            .to_string(),
    );
    (out, None)
}

/// `config check`'s `ENDPOINT_TIMEOUT_MINS` row: how long `pods up` lets a pod be up without an
/// SSH endpoint before it replaces it, `(default)` when unset — or a ✗ row and the error the
/// check fails on. Pure.
fn endpoint_timeout_row(cfg: &Config) -> (String, Option<arena_core::Error>) {
    use arena_core::pipeline::{endpoint_timeout_mins, ENDPOINT_TIMEOUT_KEY};
    let set = cfg.get(ENDPOINT_TIMEOUT_KEY).is_some_and(|v| !v.trim().is_empty());
    match endpoint_timeout_mins(cfg, None) {
        Ok(m) => {
            let (mark, default) = if set { ("✓", "") } else { ("·", " (default)") };
            let what = "`pods up` replaces a pod up that long without an SSH endpoint (--endpoint-timeout)";
            (format!("  {mark} {ENDPOINT_TIMEOUT_KEY:<28} {m} min{default} — {what}"), None)
        }
        Err(e) => {
            let why = match &e {
                arena_core::Error::Config(m) => m.clone(),
                other => other.to_string(),
            };
            (format!("  ✗ {ENDPOINT_TIMEOUT_KEY:<28} {why}"), Some(e))
        }
    }
}

/// `config check`'s rows for placement near the proxy (`NEAR`, `NEAR_MAX_EXTRA_PCT`): where
/// create/up try first and how much more they may pay for it, defaults marked — and the
/// refusal when either can't be read (its ✗ row says why). Pure, for testing.
fn near_rows(cfg: &Config) -> (Vec<String>, Option<arena_core::Error>) {
    use arena_core::near::{Near, EXTRA_KEY, NEAR_KEY};
    let set = |k: &str| cfg.get(k).is_some();
    let mark = |k: &str| if set(k) { ("✓", "") } else { ("·", " (default)") };
    match Near::from_config(cfg) {
        Err(e) => {
            let why = match &e {
                arena_core::Error::Config(m) => m.clone(),
                other => other.to_string(),
            };
            let key = if why.starts_with(EXTRA_KEY) { EXTRA_KEY } else { NEAR_KEY };
            (vec![format!("  ✗ {key:<28} {why}")], Some(e))
        }
        Ok(None) => (vec![format!("  ✓ {NEAR_KEY:<28} any — off: create/up try every option anywhere, cheapest first")], None),
        Ok(Some(n)) => {
            let (m, d) = mark(NEAR_KEY);
            let (m2, d2) = mark(EXTRA_KEY);
            let label = n.region.label();
            (
                vec![
                    format!("  {m} {NEAR_KEY:<28} {label}{d} — create/up try {label} first (NEAR=any: anywhere)"),
                    format!("  {m2} {EXTRA_KEY:<28} {}{d2} — when it costs at most this much more than the cheapest option", n.margin()),
                    "      └ RunPod: SECURE pods only (COMMUNITY hosts have no data centre); Vast: offers by country — \
                     `arena offers` shows the order"
                        .to_string(),
                ],
                None,
            )
        }
    }
}

/// The `HETZNER_*` keys that shape a Hetzner create ([`hetzner_rows`]).
const HETZNER_OPTION_KEYS: [&str; 4] = ["HETZNER_SERVER_TYPE", "HETZNER_LOCATION", "HETZNER_IMAGE", "HETZNER_SSH_KEY"];

/// `config check`'s Hetzner rows: the create options as the provider reads them, defaults
/// marked — and the SSH key every new server gets, the one that must already exist in the
/// Hetzner project. Pure, and no request (`config check` makes none): whether the project has
/// that key is checked at create, whose error names the keys it has (live, an evals node:
/// the default key is named after the prefix, `arena9`, and the project only had `arena8`).
pub(crate) fn hetzner_rows(cfg: &Config) -> Vec<String> {
    let o = arena_core::provider::hetzner::HetznerOpts::from_config(cfg);
    let set = |k: &str| cfg.get(k).is_some_and(|v| !v.trim().is_empty());
    let row = |key: &str, value: &str, note: &str| {
        let (mark, default) = if set(key) { ("✓", "") } else { ("·", " (default)") };
        format!("  {mark} {key:<28} {value}{default}{note}")
    };
    let key = o.ssh_keys.join(", ");
    let key_row = if o.ssh_keys_from == "HETZNER_SSH_KEY" {
        format!("  ✓ {:<28} {key}", "HETZNER_SSH_KEY")
    } else {
        format!("  · {:<28} {key} (default: MACHINE_NAME_PREFIX)", "HETZNER_SSH_KEY")
    };
    vec![
        row("HETZNER_SERVER_TYPE", &o.server_type, " — `pods create/up --server-type` overrides it"),
        row("HETZNER_LOCATION", &o.locations.describe(), ""),
        row(
            "HETZNER_IMAGE",
            &o.image,
            if arena_core::image::Setting::parse(&o.image) == arena_core::image::Setting::Arena {
                " — the newest arena-built snapshot that fits, else ubuntu-24.04 (below; `arena image list`)"
            } else {
                ""
            },
        ),
        key_row,
        format!(
            "      └ every new server gets SSH key `{key}`: it must be in the Hetzner project under that name — checked at \
             create (`arena api get hetzner ssh_keys` lists the project's keys)"
        ),
    ]
}

/// `config check`'s look at what `HETZNER_IMAGE=arena` resolves to — the one live lookup the
/// check makes, and only for that setting (read-only, bounded): the snapshot it picks for
/// HETZNER_SERVER_TYPE and how old it is, a ⚠ when it's over `image::STALE_DAYS` days old or its
/// requirements differ from the repo's now, or Ubuntu and why. Advice only: it never fails the
/// check (a create fails on its own if the lookup does).
pub(crate) async fn hetzner_image_rows(cfg: &Config) -> Vec<String> {
    use arena_core::image::Setting;
    let opts = arena_core::provider::hetzner::HetznerOpts::from_config(cfg);
    let setting = Setting::parse(&opts.image);
    if setting != Setting::Arena {
        return Vec::new();
    }
    let mut out = vec![format!("\nHetzner image (HETZNER_IMAGE=arena, for {}):", opts.server_type)];
    let Ok(cloud) = image::hetzner(cfg) else {
        out.push("  ⚠ HETZNER_API_KEY isn't set — can't look the image up".into());
        return out;
    };
    let lookup = tokio::time::timeout(image::LOOKUP_TIMEOUT, image::resolve(&cloud, cfg, &setting, &opts.server_type));
    let (resolved, repo) = tokio::join!(lookup, image::repo_requirements(cfg));
    match resolved {
        Ok(Ok((r, _))) => out.extend(image::resolution_lines(&r, arena_core::snapshot::unix_now(), &repo).into_iter().map(|l| format!("  {l}"))),
        Ok(Err(e)) => out.push(format!("  ? couldn't look it up: {e:#}")),
        Err(_) => out.push(format!("  ? couldn't look it up in {}s", image::LOOKUP_TIMEOUT.as_secs())),
    }
    out
}

/// `config check`'s `VM_SWAP_GB` row: the swap a bare VM's setup makes, `(default)` when unset,
/// or a ✗ row and the error the check fails on. Pure.
fn swap_row(cfg: &Config) -> (String, Option<arena_core::Error>) {
    let set = cfg.get("VM_SWAP_GB").is_some_and(|v| !v.trim().is_empty());
    match arena_core::setup::swap_gb(cfg) {
        Ok(gb) => {
            let what = if gb == 0 { "no swap".to_string() } else { format!("{gb} GB of swap") };
            let (mark, default) = if set { ("✓", "") } else { ("·", " (default)") };
            (format!("  {mark} {:<28} {what}{default} — on a bare VM (Hetzner, Shadeform), made by setup", "VM_SWAP_GB"), None)
        }
        Err(e) => (format!("  ✗ {:<28} {e}", "VM_SWAP_GB"), Some(e)),
    }
}

/// `config check`'s rows for `ASK_AI`: which AI `pods ask` (and the dashboard's `?`) sends to
/// — `(default)` when unset — or `off`; where its binary is, or a ⚠ when it isn't installed for
/// this user (advice: nothing fails until it's used). Then each AI's model and effort that is set
/// (`ASK_CLAUDE_MODEL` … `ASK_CODEX_EFFORT`). A value its parser refuses is a ✗ row and one of the
/// errors the check fails on. Pure but for the binary lookup in `env`.
fn ask_ai_rows(cfg: &Config, env: &arena_core::ask::AiEnv) -> (Vec<String>, Vec<String>) {
    use arena_core::ask::{ai, Ai, Setting};
    let key = ai::ASK_AI;
    let (mut rows, mut bad) = match Setting::from_config(cfg) {
        Err(e) => (vec![format!("  ✗ {key:<28} {e}")], vec![e]),
        Ok(Setting::Off) => (vec![format!("  ✓ {key:<28} off — `pods ask` and the dashboard's `?` refuse: nothing goes to an AI")], Vec::new()),
        Ok(Setting::Use(a)) => {
            let (mark, default) = if Setting::is_default(cfg) { ("·", " (default)") } else { ("✓", "") };
            let mut rows = vec![format!(
                "  {mark} {key:<28} {}{default} — `pods ask` sends pods' state (keys masked) to {}; `off` turns it off",
                a.name(),
                a.vendor()
            )];
            rows.push(match ai::find_bin(a.name(), env) {
                Some(p) => format!("      └ {}: {}", a.name(), p.display()),
                None => format!("  ⚠ {:<28} {}", format!("{} binary", a.name()), ai::missing(a)),
            });
            (rows, Vec::new())
        }
    };
    for a in [Ai::Claude, Ai::Codex] {
        for (k, what) in [(ai::model_key(a), "model"), (ai::effort_key(a), "effort")] {
            let Some(v) = cfg.get(k).map(str::trim).filter(|v| !v.is_empty()) else { continue };
            let checked = if what == "model" { ai::check_model(v, k) } else { ai::check_effort(v, k).map(|e| e.name().to_string()) };
            match checked {
                Ok(v) => rows.push(format!("  ✓ {k:<28} {v} — {}'s {what}", a.name())),
                Err(e) => {
                    rows.push(format!("  ✗ {k:<28} {e}"));
                    bad.push(e);
                }
            }
        }
    }
    (rows, bad)
}

/// Print a config checklist for the selected provider + proxy + backup, never showing
/// secret values. Returns an error if a required key is missing.
/// `extra`: rows looked up live before the check ([`hetzner_image_rows`]), after the report.
pub(crate) fn config_check(cfg: &Config, provider_name: &str, extra: &[String]) -> Result<()> {
    let (report, missing) = config_report(cfg, provider_name);
    print!("{report}");
    for l in extra {
        println!("{l}");
    }
    if missing.is_empty() {
        println!("\nOK — required keys for provider `{provider_name}` are present.");
        Ok(())
    } else {
        // Set but invalid counts too (each such entry says why): "missing" alone misled.
        anyhow::bail!("missing or invalid keys: {}", missing.join(", "))
    }
}

/// [`config_check`]'s report — every section's rows — and what it fails on: the keys
/// missing, or set to a value their parser refuses. Nothing is printed, so a test holds the
/// ✗ rows against that list: a ✗ row means the check fails (a missing `GPU_TYPE`/`IMAGE`
/// aside), a ⚠ row is advice.
pub(crate) fn config_report(cfg: &Config, provider_name: &str) -> (String, Vec<String>) {
    let mut out = String::new();
    // Every row goes into `out`: within this function `println!` is this one, so a row added
    // here later lands in the report, in order.
    macro_rules! println {
        () => {
            out.push('\n')
        };
        ($($arg:tt)*) => {{
            use std::fmt::Write as _;
            let _ = writeln!(out, $($arg)*);
        }};
    }
    let mut missing: Vec<String> = Vec::new();

    // Show every provider's API key (set/empty), marking the selected one. Only the
    // selected provider's key is *required* (counts toward `missing`).
    println!("Providers (selected: {provider_name}):");
    for (name, key) in [
        ("runpod", "RUNPOD_API_KEY"),
        ("vast", "VAST_API_KEY"),
        ("hetzner", "HETZNER_API_KEY"),
        ("shadeform", "SHADEFORM_API_KEY"),
    ] {
        let selected = name == provider_name;
        let set = cfg.get(key).map(|v| !v.is_empty()).unwrap_or(false);
        let mark = if set { "✓" } else if selected { "✗" } else { "·" };
        println!(
            "  {mark} {key:<24} {}{}",
            if set { "(set)" } else { "(empty)" },
            if selected { "   ← selected" } else { "" }
        );
        if selected && !set {
            missing.push(key.to_string());
        }
    }
    let (line, bad) = provider_key_row(cfg, provider_name);
    println!("{line}");
    if let Some(why) = bad {
        missing.push(why);
    }
    // Which RunPod REST generation `runpod` talks to — shown whenever RunPod is in play:
    // v2 by default, and a leftover `RUNPOD_API=v1` (which RunPod retires) is flagged here.
    if provider_name == "runpod" || cfg.get("RUNPOD_API_KEY").is_some_and(|v| !v.is_empty()) || cfg.get("RUNPOD_API").is_some() {
        let (line, ok) = runpod_api_row(cfg);
        println!("{line}");
        if !ok {
            missing.push("RUNPOD_API (must be v1 or v2)".into());
        }
    }
    if provider_name == "vast" || cfg.get("VAST_API_KEY").is_some_and(|v| !v.is_empty()) || cfg.get("VAST_ALLOW_UNVERIFIED").is_some() {
        let (line, ok) = vast_hosts_row(cfg);
        println!("{line}");
        if !ok {
            missing.push("VAST_ALLOW_UNVERIFIED (must be 1/true/yes or 0/false/no)".into());
        }
    }
    let shadeform_in_play = provider_name == "shadeform"
        || cfg.get("SHADEFORM_API_KEY").is_some_and(|v| !v.is_empty())
        || arena_core::provider::shadeform::OPTION_KEYS.iter().any(|k| cfg.get(k).is_some());
    if shadeform_in_play {
        let (lines, bad) = shadeform_rows(cfg);
        lines.iter().for_each(|l| println!("{l}"));
        if let Some(e) = bad {
            note_invalid(&mut missing, &e);
        }
    }
    // As for Shadeform: whenever Hetzner is in play — `pods up --provider hetzner` reads the
    // same config whatever the default provider is.
    let hetzner_in_play = provider_name == "hetzner"
        || cfg.get("HETZNER_API_KEY").is_some_and(|v| !v.is_empty())
        || HETZNER_OPTION_KEYS.iter().any(|k| cfg.get(k).is_some());
    // The swap the bare-VM setup makes (Hetzner, and Shadeform's GPU VMs).
    if hetzner_in_play || shadeform_in_play || cfg.get("VM_SWAP_GB").is_some() {
        if hetzner_in_play {
            hetzner_rows(cfg).iter().for_each(|l| println!("{l}"));
        }
        let (line, bad) = swap_row(cfg);
        println!("{line}");
        if let Some(e) = bad {
            note_invalid(&mut missing, &e);
        }
    }

    println!("\nMachine naming:");
    cfg_row(&mut out, cfg, &mut missing, "MACHINE_NAME_PREFIX", false, false);
    println!(
        "  {} MACHINE_NAME_LIST          {} name{}",
        if cfg.machine_names.is_empty() { "✗" } else { "✓" },
        cfg.machine_names.len(),
        if cfg.machine_names.len() == 1 { "" } else { "s" }
    );
    if cfg.machine_names.is_empty() {
        missing.push("MACHINE_NAME_LIST".into());
    }

    if provider_name != "hetzner" {
        println!("\nCreate spec (generic key, else RUNPOD_* fallback):");
        let spec = PodSpec::from_config(cfg);
        let yn = |s: &str| if s.is_empty() { "✗ (missing)".into() } else { format!("✓ {s}") };
        println!("  GPU_TYPE / RUNPOD_GPU_TYPE   {}", yn(&spec.gpu_type));
        println!("  IMAGE / RUNPOD_DOCKER_IMAGE  {}", yn(&spec.image));
        println!("  resolved disk                {}GB", spec.disk_gb);
        if spec.volume_gb == 0 {
            println!("  ⚠ persistent volume          0GB — work is lost on pod restart");
        }
        let v1 = provider_name == "runpod"
            && matches!(arena_core::provider::RunpodApi::from_config(cfg), Ok(arena_core::provider::RunpodApi::V1));
        let (lines, ok) = cuda_rows(cfg, v1);
        lines.iter().for_each(|l| println!("{l}"));
        if let Err(what) = ok {
            missing.push(what);
        }
    }
    if let Some((line, ok)) = create_extra_row(cfg) {
        println!("{line}");
        if !ok {
            missing.push("CREATE_EXTRA_JSON (must be one JSON object)".into());
        }
    }
    // Whatever the selected provider: `--provider runpod|vast` reads the same keys.
    let (lines, bad) = near_rows(cfg);
    lines.iter().for_each(|l| println!("{l}"));
    if let Some(e) = bad {
        note_invalid(&mut missing, &e);
    }
    let (line, bad) = endpoint_timeout_row(cfg);
    println!("{line}");
    if let Some(e) = bad {
        note_invalid(&mut missing, &e);
    }

    println!("\nSSH (backup + dashboard metrics):");
    cfg_row(&mut out, cfg, &mut missing, "SSH_USER", false, false);
    cfg_row(&mut out, cfg, &mut missing, "SHARED_SSH_KEY_PATH", false, false);

    println!("\nProxy (`proxy plan` / `proxy apply`):");
    cfg_row(&mut out, cfg, &mut missing, "SSH_PROXY_HOST", false, false);
    cfg_row(&mut out, cfg, &mut missing, "SSH_PROXY_STARTING_PORT", false, false);
    // Absent and empty mean different things here (default reload vs never reload), so
    // this row can't use `cfg_row`'s set/missing view.
    let reload = arena_core::proxy::reload_cmd_from(cfg);
    let key = "SSH_PROXY_RELOAD_CMD";
    match cfg.get(key) {
        None => println!("  · {key:<28} (default) {reload}"),
        Some(_) if reload.is_empty() => {
            println!("  ✓ {key:<28} (empty) write-only — the config file is written, nginx is never reloaded")
        }
        Some(_) => println!("  ✓ {key:<28} {reload}"),
    }

    println!("\nYour ~/.ssh/config (`ssh-config --install`):");
    let home = std::env::var("HOME").ok();
    let (row, bad) = ssh_config_check_row(cfg, home.as_deref());
    println!("{row}");
    if let Some(e) = bad {
        note_invalid(&mut missing, &arena_core::Error::Config(e));
    }
    // Staff's file on the proxy box: shown once the admin key (or the file) is set.
    let admin_on = [arena_core::ssh::ADMIN_KEY_VAR, Audience::Admin.key()].iter().any(|k| cfg.get(k).is_some_and(|v| !v.trim().is_empty()));
    if admin_on {
        let (row, bad) = ssh_config_check_row_as(Audience::Admin, cfg, home.as_deref());
        println!("{row}");
        if let Some(e) = bad {
            note_invalid(&mut missing, &arena_core::Error::Config(e));
        }
    }

    println!("\nBackup (git `backup` + file `pull`):");
    cfg_row(&mut out, cfg, &mut missing, "ARENA_REPO_NAME", false, false);
    // The `upstream` remote setup adds to each pod's checkout: a default, so not cfg_row's view.
    match arena_core::setup::upstream_url(cfg) {
        Ok(Some(url)) => println!(
            "  · ARENA_UPSTREAM_REPO          {url}{} — each pod's `upstream` remote (setup adds it when missing)",
            if cfg.get("ARENA_UPSTREAM_REPO").is_some_and(|v| !v.trim().is_empty()) { "" } else { " (default)" }
        ),
        Ok(None) => println!("  · ARENA_UPSTREAM_REPO          none — setup adds no `upstream` remote"),
        Err(e) => {
            println!("  ✗ ARENA_UPSTREAM_REPO          {e}");
            note_invalid(&mut missing, &e);
        }
    }
    cfg_row(&mut out, cfg, &mut missing, "GIT_SSH_KEY_REMOTE", false, false);
    cfg_row(&mut out, cfg, &mut missing, "ARENA_START_DATE", false, false); // wNdM label (init-branches / pull)
    // Where `pods pull` rsyncs pod files to locally (config LOCAL_BACKUP_DIR, else the fleet's
    // backup/: next to the config, else ./backup), shown as an absolute path so it's obvious
    // where backups land.
    let (backup_dir, found) = cfg.local_backup_dir();
    let abs = std::fs::canonicalize(&backup_dir)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| {
            // Not created yet — resolve relative to cwd, dropping a leading "./".
            let rel = backup_dir.trim_start_matches("./");
            std::env::current_dir()
                .map(|d| d.join(rel).display().to_string())
                .unwrap_or_else(|_| backup_dir.clone())
        });
    let exists = std::path::Path::new(&backup_dir).is_dir();
    let moved = match found.map(|f| f.found) {
        Some(arena_core::config::Found::CurrentDir { beside }) => format!("  (in the current directory only: move it to {})", beside.display()),
        _ => String::new(),
    };
    println!(
        "  {} local rsync backup dir     {abs}{}{moved}",
        if !moved.is_empty() { "⚠" } else if exists { "✓" } else { "·" },
        if exists { "" } else { "  (created on first `pods pull`)" }
    );
    // The `pods pull` knobs (all optional; shown so it's clear what's configurable).
    println!(
        "  · pull max file size         {} (BACKUP_MAX_SIZE)",
        cfg.get("BACKUP_MAX_SIZE").filter(|s| !s.is_empty()).unwrap_or("50M (default)")
    );
    println!(
        "  · pull remote source         {} (BACKUP_REMOTE_PATH)",
        cfg.get("BACKUP_REMOTE_PATH").filter(|s| !s.is_empty()).unwrap_or("~/ (home, default)")
    );
    // The per-pod rsync budget (a bad value fails every `pods pull`/`backup` up front).
    match arena_core::pull::backup_timeout(cfg.get("BACKUP_TIMEOUT_SECS")) {
        Ok(d) => println!(
            "  · pull time budget per pod   {}{} (BACKUP_TIMEOUT_SECS)",
            arena_core::error::human_duration(&d),
            if cfg.get("BACKUP_TIMEOUT_SECS").is_some_and(|v| !v.trim().is_empty()) { "" } else { " (default)" }
        ),
        Err(e) => {
            println!("  ✗ pull time budget per pod   {e}");
            note_invalid(&mut missing, &e);
        }
    }
    // When `pods backup` finishes an operation left in progress on a pod (a bad value: never).
    let resolve_key = arena_core::resolve::AUTO_RESOLVE_KEY;
    match arena_core::resolve::auto_resolve_hours(cfg.get(resolve_key)) {
        Ok(h) => println!(
            "  · finish a BUSY pod's merge  {} ({resolve_key})",
            arena_core::resolve::auto_resolve_words(h, cfg.get(resolve_key).is_some_and(|v| !v.trim().is_empty()))
        ),
        Err(e) => {
            println!("  ✗ finish a BUSY pod's merge  {e}");
            note_invalid(&mut missing, &e);
        }
    }
    // The most `set-branch --hard` saves on a pod before its reset (review ai-review claude #6: a
    // bad value used to surface only when `--hard` ran, and stops it there before anything else).
    // A value the environment gave (for one run) is said so, as the file doesn't have it.
    match arena_core::backup::discard_max_kb(cfg) {
        Ok(kb) => println!(
            "  · --hard save max per pod    {}{} (DISCARD_MAX_GB)",
            arena_core::backup::discard_max_words(kb),
            match cfg.get("DISCARD_MAX_GB").filter(|v| !v.trim().is_empty()) {
                None => " (default)",
                Some(v) if cfg.file_value("DISCARD_MAX_GB") != Some(v) => " (from the environment)",
                Some(_) => "",
            }
        ),
        Err(e) => {
            println!("  ✗ --hard save max per pod    {e}");
            note_invalid_text(&mut missing, &e);
        }
    }
    // What every copy onto this box leaves free on its disk (a bad value fails every pull and
    // copy up front, and refuses replace/migrate before they create anything). How full the disk
    // is now comes after the report (`disk::check_rows`).
    match arena_core::disk::floor_bytes(cfg) {
        Ok(b) => println!(
            "  · copies leave free here     {}{} (CONTROL_MIN_FREE_GB) — pulls, backup copies, replace/migrate staging, the reaper",
            arena_core::disk::gb(b),
            match cfg.get(arena_core::disk::FLOOR_KEY).filter(|v| !v.trim().is_empty()) {
                None => " (default)",
                Some(v) if cfg.file_value(arena_core::disk::FLOOR_KEY) != Some(v) => " (from the environment)",
                Some(_) => "",
            }
        ),
        Err(e) => {
            println!("  ✗ copies leave free here     {e} — pulls, backup copies and replace/migrate are refused until it's fixed");
            note_invalid_text(&mut missing, &e);
        }
    }

    println!("\nBackup archive (optional, `arena archive`):");
    for line in archive::config_check_lines(cfg, &mut missing) {
        println!("{line}");
    }

    println!("\nAuto-reaper (`pods reap`, opt-in; REAP_* are read from the config file only):");
    let enabled = cfg.get("REAP_ENABLED").map(str::trim) == Some("1");
    let (lines, blockers) = reap::readiness_rows_for(cfg, RSYNC, enabled || reap_cron_installed());
    lines.iter().for_each(|l| println!("{l}"));
    // Only a reaper that's switched on makes its gaps a failure of the check (✗; ⚠ while it
    // only observes); a value set but invalid — a REAP_* one, or a relative ARENA_STATE_DIR in
    // the file — fails it either way, named as `KEY (why)`.
    if enabled {
        missing.extend(blockers);
    }
    if let Err(e) = arena_core::reap::ReapSettings::from_config(cfg) {
        note_invalid_text(&mut missing, &e);
    }
    if let Err(e) = reap::state_path(cfg) {
        if reap::is_invalid_state_dir(&e) {
            note_invalid_text(&mut missing, &e);
        }
    }

    println!("\nDashboard (optional):");
    cfg_row(&mut out, cfg, &mut missing, "PROGRESS_CMD", false, false);

    println!("\nAsking an AI (`pods ask`, the dashboard's `?`):");
    let (rows, bad) = ask_ai_rows(cfg, &arena_core::ask::AiEnv::from_process());
    rows.iter().for_each(|l| println!("{l}"));
    for e in bad {
        note_invalid_text(&mut missing, &e);
    }

    println!("\nEvals / model access (optional, `pods copy-keys` / `arena keys`):");
    cfg_row(&mut out, cfg, &mut missing, "HF_TOKEN", false, true); // broadcast for gated repos (Llama 3 …)
    cfg_row(&mut out, cfg, &mut missing, "CLAUDE_CODE_OAUTH_TOKEN", false, true); // broadcast for Claude Code
    cfg_row(&mut out, cfg, &mut missing, "OPENROUTER_PROVISIONING_KEY", false, true); // mints runtime keys
    // The workspace every key call is scoped to: a value that's neither an id nor a slug
    // would fail each call, so it's a ✗ here.
    match cfg.get("OPENROUTER_WORKSPACE").map(str::trim).filter(|w| !w.is_empty()).map(|w| (w, arena_core::openrouter::workspace_ref(w))) {
        Some((w, Err(e))) => {
            use std::fmt::Write as _;
            let _ = writeln!(out, "  ✗ {:<28} {w} (neither a workspace id nor a slug)", "OPENROUTER_WORKSPACE");
            note_invalid(&mut missing, &e);
        }
        _ => cfg_row(&mut out, cfg, &mut missing, "OPENROUTER_WORKSPACE", false, false),
    }
    cfg_row(&mut out, cfg, &mut missing, "OPENROUTER_KEY_LIMIT", false, false); // USD cap per generated key

    // The balance sources as last seen (the state file only — no request): ⚠ rows are
    // advice, never missing keys; a ✗ row (a balance key set but invalid) fails the check.
    let now = arena_core::snapshot::unix_now();
    balance_section(cfg, &money::BalanceCtx::from_config(cfg), now, &mut missing).iter().for_each(|l| println!("{l}"));

    // Read-only readiness: are the local files this user needs actually there/readable?
    // Answers "is it set up yet?" without touching any API.
    println!("\nSetup readiness (local, read-only):");
    let readable = |p: &str| std::fs::File::open(p).is_ok();
    if let Some(k) = cfg.get("SHARED_SSH_KEY_PATH") {
        // Resolve the same way pods/proxy do (readable ~/.ssh fallback).
        let resolved = arena_core::ssh::resolve_key_path(k);
        let ok = readable(&resolved);
        println!(
            "  {} shared SSH key             {}{}",
            if ok { "✓" } else { "⚠" },
            resolved,
            if ok || resolved == k { String::new() } else { format!("  (configured: {k})") }
        );
        if !ok {
            println!("      └ not readable by this user — dashboard metrics / SSH will fail");
        }
    }
    if let Some(k) = cfg.get("GIT_SSH_KEY_LOCAL") {
        let resolved = arena_core::ssh::resolve_key_path(k);
        println!("  {} git deploy key (local)    {resolved}", if readable(&resolved) { "✓" } else { "⚠" });
        let pubk = format!("{resolved}.pub");
        println!("  {} git deploy key .pub       {pubk}", if readable(&pubk) { "✓" } else { "⚠" });
    }
    admin_key_rows(cfg).iter().for_each(|l| println!("{l}"));
    let plan = cfg.fleet_file(arena_core::config::PLAN_FILE);
    let plan_present = readable(&plan.path.display().to_string());
    println!(
        "  {} provisioning plan          {}",
        if !plan_present { "·" } else if plan.warning().is_some() { "⚠" } else { "✓" },
        if plan_present { plan.describe() } else { "none (optional — `arena plan`)".to_string() }
    );
    println!(
        "  {} iteration start date       {}",
        if cfg.get("ARENA_START_DATE").is_some() { "✓" } else { "⚠" },
        cfg.get("ARENA_START_DATE").unwrap_or("unset — backups can't label wNdM")
    );
    // The per-step setup budgets, as `pods setup`/`up` will use them (SETUP_TIMEOUT_SECS).
    match arena_core::setup::SetupTimeouts::from_config(cfg, None) {
        Ok(t) => println!(
            "  · setup step budgets        copy {}s, config {}s, hetzner script {}s",
            t.copy.as_secs(),
            t.config.as_secs(),
            t.bare_vm.as_secs()
        ),
        Err(e) => {
            println!("  ✗ setup step budgets        {e}");
            note_invalid(&mut missing, &e);
        }
    }
    // The driver floor `pods test --deep` holds pods to (MIN_DRIVER_VERSION, else derived
    // from ALLOWED_CUDA_VERSIONS).
    match arena_core::health::HealthPolicy::from_config(cfg) {
        Ok(p) => match p.min_driver {
            Some(f) => {
                let v = f.version.iter().map(u32::to_string).collect::<Vec<_>>().join(".");
                println!("  · deep-check driver floor   ≥ {v} ({})", f.why)
            }
            None => println!("  · deep-check driver floor   none (no MIN_DRIVER_VERSION / MIN_CUDA_VERSION / ALLOWED_CUDA_VERSIONS)"),
        },
        Err(e) => {
            println!("  ✗ deep-check driver floor   {e}");
            note_invalid(&mut missing, &e);
        }
    }
    // The machines `up --check` remembers as bad (one small local file): where to look when
    // `up` keeps turning a machine away. Informational — never a missing key.
    bad_hosts_section(cfg, now).iter().for_each(|l| println!("{l}"));

    (out, missing)
}

/// `config check`'s admin-key rows (`ADMIN_SSH_KEY_PATH`), beside the shared and deploy keys: `·`
/// unset (optional); `✓` readable here with its public half derivable (every machine authorizes
/// it, arena offers it first); else `⚠` with a `└` line per problem — not readable by this user
/// (fine anywhere but the proxy box), no public half (no machine would authorize it), the same
/// file or key as the shared key or the deploy key (participants or every pod hold those, so it
/// would be no one's alone), or a mode that lets other users of this box at it. Advice only,
/// never a missing key.
pub(crate) fn admin_key_rows(cfg: &Config) -> Vec<String> {
    use arena_core::ssh::{public_key_for, resolve_key_path, ADMIN_KEY_VAR};
    const LABEL: &str = "admin SSH key";
    let Some(configured) = cfg.get(ADMIN_KEY_VAR).filter(|s| !s.trim().is_empty()) else {
        return vec![format!(
            "  · {LABEL:<26} (unset — optional: {ADMIN_KEY_VAR}, a key only the proxy box holds that every machine lets in; \
             staff on that box then reach any machine)"
        )];
    };
    let resolved = resolve_key_path(configured);
    let path = std::path::Path::new(&resolved);
    let readable = std::fs::File::open(path).is_ok();
    let pubkey = public_key_for(configured);
    let blob = |k: &Option<String>| k.as_deref().and_then(arena_core::archive::key_blob).map(str::to_string);
    let mut problems: Vec<String> = Vec::new();
    if !readable {
        problems.push("not readable by this user — arena doesn't offer it here (only the proxy box needs to)".into());
    } else if pubkey.is_none() {
        problems.push("its public half can't be derived (no .pub beside it, and `ssh-keygen -y` failed) — no machine would authorize it".into());
    }
    for (key, who) in [("SHARED_SSH_KEY_PATH", "the shared key: every participant holds it"), ("GIT_SSH_KEY_LOCAL", "the deploy key: every pod holds it")] {
        let Some(other) = cfg.get(key).filter(|s| !s.trim().is_empty()) else { continue };
        let same_file = same_file(path, std::path::Path::new(&resolve_key_path(other)));
        let same_key = blob(&pubkey).is_some_and(|b| blob(&public_key_for(other)).as_deref() == Some(b.as_str()));
        if same_file || same_key {
            problems.push(format!(
                "it is {key} ({who}), so it isn't the proxy box's alone — give it a key of its own (`ssh-keygen -t ed25519 -N '' -f \
                 /root/.ssh/arena_admin_key`)"
            ));
        }
    }
    #[cfg(unix)]
    if let Ok(m) = std::fs::metadata(path) {
        use std::os::unix::fs::PermissionsExt;
        let mode = m.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            problems.push(format!("mode {mode:04o}: other users of this box can get at it (and ssh refuses such a key) — chmod 600 {resolved}"));
        }
    }
    let ok = readable && pubkey.is_some() && problems.is_empty();
    let configured_note = if resolved == configured { String::new() } else { format!("  (configured: {configured})") };
    let mut rows = vec![format!("  {} {LABEL:<26} {resolved}{configured_note}", if ok { "✓" } else { "⚠" })];
    rows.extend(problems.iter().map(|p| format!("      └ {p}")));
    rows
}

/// `config check`'s Balance section: each prepaid account's sources as last seen, from the
/// balance state file alone (no request; ⚠ rows are advice, never missing keys). A balance
/// key set but invalid is a ✗ row and joins `missing` — the check fails on it like on any
/// invalid key, while every other surface just uses the default.
pub(crate) fn balance_section(cfg: &Config, bal: &money::BalanceCtx, now: u64, missing: &mut Vec<String>) -> Vec<String> {
    let mut out = vec!["\nBalance (`arena balance`):".to_string()];
    out.extend(money::balance_config_rows(cfg, bal, now));
    missing.extend(arena_core::balance::invalid_keys(cfg));
    out
}

/// `config check`'s section on the machines `up --check` remembers as bad, from the health
/// cache — which shares its state directory (and its lock) with the balance state file.
/// Nothing when there is no state directory.
pub(crate) fn bad_hosts_section(cfg: &Config, now: u64) -> Vec<String> {
    let Some(Ok(path)) = health_cache_path(cfg) else {
        return Vec::new();
    };
    let (cache, warning) = arena_core::snapshot::HealthCache::load(&path);
    let mut out = vec!["\nMachines `up --check` remembers as bad (`pods up --retry-bad-hosts` ignores them):".to_string()];
    out.extend(warning.map(|w| format!("  {w}")));
    out.extend(arena_core::snapshot::bad_hosts_lines(&cache, now));
    out
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod which_tests;

#[cfg(test)]
mod swap_row_tests {
    use super::*;
    use crate::cli::{Cli, Cmd, PodCmd};

    /// `config check`'s `VM_SWAP_GB` row: the default marked, 0 as "no swap", and a bad value a
    /// ✗ row with the error the check fails on.
    #[test]
    fn the_swap_row_says_the_size_or_why_not() {
        let row = |text: &str| swap_row(&Config::parse(text));
        let (line, bad) = row("");
        assert_eq!(line, "  · VM_SWAP_GB                   8 GB of swap (default) — on a bare VM (Hetzner, Shadeform), made by setup");
        assert!(bad.is_none());
        assert!(row("VM_SWAP_GB=0\n").0.starts_with("  ✓ VM_SWAP_GB                   no swap — "));
        let (line, bad) = row("VM_SWAP_GB=8G\n");
        assert!(line.starts_with("  ✗ VM_SWAP_GB") && line.contains("whole number of GB"), "{line}");
        let mut missing = Vec::new();
        note_invalid(&mut missing, &bad.unwrap());
        assert!(missing.iter().any(|m| m.starts_with("VM_SWAP_GB")), "{missing:?}");
    }

    /// `config check`'s `ENDPOINT_TIMEOUT_MINS` row: the default (5) marked, a set value ✓, and
    /// a bad one a ✗ row with the error the check fails on.
    #[test]
    fn the_endpoint_timeout_row_says_the_minutes_or_why_not() {
        let row = |text: &str| endpoint_timeout_row(&Config::parse(text));
        let (line, bad) = row("");
        assert_eq!(
            line,
            "  · ENDPOINT_TIMEOUT_MINS        5 min (default) — `pods up` replaces a pod up that long without an SSH endpoint \
             (--endpoint-timeout)"
        );
        assert!(bad.is_none());
        assert!(row("ENDPOINT_TIMEOUT_MINS=8\n").0.starts_with("  ✓ ENDPOINT_TIMEOUT_MINS        8 min — "));
        let (line, bad) = row("ENDPOINT_TIMEOUT_MINS=5m\n");
        assert_eq!(line, "  ✗ ENDPOINT_TIMEOUT_MINS        ENDPOINT_TIMEOUT_MINS must be whole minutes from 1 to 120, not \"5m\"");
        let mut missing = Vec::new();
        note_invalid(&mut missing, &bad.unwrap());
        assert!(missing.iter().any(|m| m.starts_with("ENDPOINT_TIMEOUT_MINS")), "{missing:?}");
        // And `up` refuses it before anything is created.
        let e = arena_core::pipeline::endpoint_timeout_mins(&Config::parse("ENDPOINT_TIMEOUT_MINS=5m\n"), None).unwrap_err();
        assert!(e.to_string().contains("whole minutes"), "{e}");
    }

    /// `pods up --endpoint-timeout MINS`: whole minutes 1..=120, refused by clap otherwise.
    #[test]
    fn the_endpoint_timeout_flag_parses() {
        use clap::Parser;
        let parse = |args: &[&str]| Cli::try_parse_from(args).map(|c| c.cmd);
        match parse(&["arena", "pods", "up", "-n", "2", "--endpoint-timeout", "8"]).unwrap() {
            Cmd::Pods(PodCmd::Up { endpoint_timeout, .. }) => assert_eq!(endpoint_timeout, Some(8)),
            _ => panic!(),
        }
        match parse(&["arena", "pods", "up", "-n", "2"]).unwrap() {
            Cmd::Pods(PodCmd::Up { endpoint_timeout, .. }) => assert_eq!(endpoint_timeout, None),
            _ => panic!(),
        }
        for bad in ["0", "121", "4.5", "-1", "five"] {
            assert!(parse(&["arena", "pods", "up", "-n", "2", "--endpoint-timeout", bad]).is_err(), "{bad}");
        }
    }
}
