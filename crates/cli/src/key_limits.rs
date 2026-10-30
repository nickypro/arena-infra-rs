//! `arena keys limit|pause|resume`: raise or set machines' OpenRouter key caps (or take one away,
//! `--no-cap`; or change when it starts over, `--reset`), pause keys and resume them — in place
//! (`PATCH /keys/{hash}`: the secret, the usage and the pod's copy of the key all stay), so a
//! participant who hit their cap is back in seconds, with nothing to copy onto the pod. The
//! dashboard's action menu runs these very commands (its "ai …" rows), so the CLI and the TUI
//! share one path. What a cap change means for one key is core's
//! ([`arena_core::openrouter::plan_cap`]); here: which keys a machine has, the dry run, the
//! confirm, the calls, and what OpenRouter says afterwards.
//!
//! A machine's key is found by name as for `rotate`/`revoke` ([`crate::machine_key_names`]);
//! targets resolve as for `gen` ([`crate::keys_targets`]). Of several keys going by one name, a
//! cap change and a resume touch the one the dashboard shows ([`or::shown_key`]: the enabled one,
//! then the newest — a resume never wakes an old, rotated-out key), a pause every enabled one.

use anyhow::{Context, Result};
use arena_core::openrouter::{self as or, CapChange, KeyApi, KeyInfo, KeyUpdate, LimitReset};
use arena_core::provider::Provider;
use arena_core::selector::{Naming, SelectArgs};
use arena_core::Config;

/// `keys limit`'s arguments.
#[derive(clap::Args, Debug)]
pub(crate) struct LimitArgs {
    /// The machines (names, ids, ranges, as for `gen`), then the cap: `10` (USD) sets it, `+5`
    /// raises each key's own cap by $5 (with --no-cap: the machines only)
    #[arg(value_name = "TARGET… USD|+USD")]
    args: Vec<String>,
    /// Take the cap away (give no amount): nothing then bounds what the key spends
    #[arg(long)]
    no_cap: bool,
    /// When the cap starts over: daily, weekly, monthly (00:00 UTC; weeks from Monday) or never
    /// (`+0 --reset daily` keeps the cap)
    #[arg(long, value_name = "WHEN", value_parser = ["daily", "weekly", "monthly", "never"], hide_possible_values = true)]
    reset: Option<String>,
    /// Set a cap under what the key has spent in its period anyway (its calls fail at once)
    #[arg(long)]
    force: bool,
    /// Preview only: change nothing.
    #[arg(long, visible_aliases = ["dryrun", "dry"])]
    dry_run: bool,
    /// Print one JSON object: each key before and after
    #[arg(long)]
    json: bool,
    // Last: its help heading ("Target selection") would take the flags after it.
    #[command(flatten)]
    opts: crate::SelectOpts,
}

/// `keys pause|resume`'s arguments.
#[derive(clap::Args, Debug)]
pub(crate) struct PauseArgs {
    /// Preview only: change nothing.
    #[arg(long, visible_aliases = ["dryrun", "dry"])]
    dry_run: bool,
    /// Print one JSON object: each key before and after
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    sel: crate::Select,
}

/// What the command does to each key.
#[derive(Debug, Clone, PartialEq)]
enum Verb {
    /// The cap: a change of it (`None`: kept — only `reset`), when it starts over (`None`: as it
    /// is), and whether a tightening under the spend goes ahead.
    Limit { change: Option<CapChange>, reset: Option<Option<LimitReset>>, force: bool },
    Pause,
    Resume,
}

impl Verb {
    fn word(&self) -> &'static str {
        match self {
            Verb::Limit { .. } => "limit",
            Verb::Pause => "pause",
            Verb::Resume => "resume",
        }
    }
}

/// `arena keys limit`.
pub(crate) async fn limit(a: LimitArgs, provider: &dyn Provider, cfg: &Config, api: &dyn KeyApi, dir: &std::path::Path, yes: bool) -> Result<()> {
    let (targets, change) = split_amount(a.args, a.no_cap)?;
    let reset = a.reset.as_deref().map(LimitReset::parse).transpose()?;
    let verb = Verb::Limit { change: Some(change), reset, force: a.force };
    run(verb, a.opts.args(&targets), a.dry_run, a.json, provider, cfg, api, dir, yes).await
}

/// `arena keys pause` (`resume: false`) or `arena keys resume`.
pub(crate) async fn pause(a: PauseArgs, resume: bool, provider: &dyn Provider, cfg: &Config, api: &dyn KeyApi, dir: &std::path::Path, yes: bool) -> Result<()> {
    let verb = if resume { Verb::Resume } else { Verb::Pause };
    run(verb, a.sel.args(), a.dry_run, a.json, provider, cfg, api, dir, yes).await
}

/// `limit`'s positionals: the targets, then the amount — the last one, always, unless `--no-cap`
/// (then there is none, and every one is a target). Never guessed from what parses: a numeric id
/// (Hetzner's, Vast's) is a target too. Pure.
fn split_amount(mut args: Vec<String>, no_cap: bool) -> Result<(Vec<String>, CapChange)> {
    if no_cap {
        return Ok((args, CapChange::NoCap));
    }
    let Some(last) = args.pop() else {
        anyhow::bail!("give the cap after the machines: `arena keys limit apple 10` sets it, `+5` raises it (--no-cap takes it away)");
    };
    let change = or::parse_cap(&last).map_err(|e| anyhow::anyhow!("the last argument is the cap: {e}"))?;
    Ok((args, change))
}

/// What happens to one key, or to a machine without one.
#[derive(Debug, Clone)]
struct Row {
    machine: String,
    /// The key acted on (`None`: the machine has none).
    key: Option<KeyInfo>,
    /// The change (`None`: nothing to do, and `skip` says why).
    update: Option<KeyUpdate>,
    /// `cap $5.00 → $10.00 · spent $0.42 this month`, `on → off`.
    words: String,
    skip: Option<String>,
    /// Said besides: other keys by the name left as they are, a cap a resume doesn't lift.
    note: Option<String>,
}

/// `on`/`off`, as the dashboard's cell says a disabled key.
fn on_off(disabled: bool) -> &'static str {
    if disabled {
        "off"
    } else {
        "on"
    }
}

/// `k` as it would be after `u`.
fn applied(k: &KeyInfo, u: &KeyUpdate) -> KeyInfo {
    let mut out = k.clone();
    if let Some(l) = u.limit {
        out.limit = l;
        out.limit_remaining = None;
    }
    if let Some(r) = u.limit_reset {
        out.limit_reset = r.map(|r| r.as_str().to_string());
        out.limit_remaining = None;
    }
    if let Some(d) = u.disabled {
        out.disabled = d;
    }
    out
}

/// Whether a key read back after the change says what was asked (a cap to the cent).
fn took(now: &KeyInfo, want: &KeyInfo) -> bool {
    let cap = match (now.limit, want.limit) {
        (Some(a), Some(b)) => (a - b).abs() < 0.005,
        (a, b) => a.is_none() && b.is_none(),
    };
    cap && LimitReset::of(now) == LimitReset::of(want) && now.disabled == want.disabled
}

/// A key at its cap after this, said for a pause/resume row: a resume doesn't lift a cap.
fn cap_note(machine: &str, k: &KeyInfo) -> Option<String> {
    let p = or::plan_cap(k, None, None, true).ok()?;
    p.still_at_cap().then(|| {
        format!(
            "at its cap ({} of {}) — `arena keys limit {machine} +5` raises it",
            p.spent.map(|s| arena_core::fleet::fmt_money("$", s)).unwrap_or_else(|| "?".into()),
            p.after.map(|a| arena_core::fleet::fmt_money("$", a)).unwrap_or_default()
        )
    })
}

/// Each machine's rows for `verb`, from the listing `keys`; or every refusal (a cap change that
/// can't go ahead — [`or::plan_cap`]), so that nothing changes unless everything can. Pure.
fn plan(verb: &Verb, machines: &[String], keys: &[KeyInfo], naming: &Naming) -> std::result::Result<Vec<Row>, Vec<String>> {
    let mut rows = Vec::new();
    let mut refused = Vec::new();
    for m in machines {
        let names = crate::machine_key_names(naming, m);
        let theirs: Vec<&KeyInfo> = keys.iter().filter(|k| k.name.as_deref().is_some_and(|n| names.iter().any(|t| t == n))).collect();
        let row = |key: Option<&KeyInfo>, update, words: String, skip: Option<String>, note| Row {
            machine: m.clone(),
            key: key.cloned(),
            update,
            words,
            skip,
            note,
        };
        let Some(shown) = or::shown_key(theirs.iter().copied()) else {
            rows.push(row(None, None, String::new(), Some(format!("no key (`arena keys gen {m}` mints one)")), None));
            continue;
        };
        let others = |n: usize, what: &str| (n > 0).then(|| format!("{n} other {what} by this name left as it is — `arena keys list --all`"));
        match verb {
            Verb::Limit { change, reset, force } => match or::plan_cap(shown, *change, *reset, *force) {
                Err(why) => refused.push(format!("{m}: {why}")),
                Ok(p) => {
                    let note = if shown.disabled {
                        Some(format!("it's paused — `arena keys resume {m}` turns it back on"))
                    } else {
                        others(theirs.len() - 1, "key")
                    };
                    let skip = (!p.changes()).then(|| "unchanged".to_string());
                    rows.push(row(Some(shown), p.changes().then(|| p.update()), p.words(), skip, note));
                }
            },
            Verb::Pause => {
                let on: Vec<&KeyInfo> = theirs.iter().copied().filter(|k| !k.disabled).collect();
                if on.is_empty() {
                    rows.push(row(Some(shown), None, "off".into(), Some("already off".into()), None));
                }
                for k in on {
                    rows.push(row(Some(k), Some(KeyUpdate { disabled: Some(true), ..Default::default() }), "on → off".into(), None, None));
                }
            }
            Verb::Resume if !shown.disabled => {
                let note = cap_note(m, shown);
                rows.push(row(Some(shown), None, "on".into(), Some("already on".into()), note));
            }
            Verb::Resume => {
                let update = KeyUpdate { disabled: Some(false), ..Default::default() };
                let note = cap_note(m, &applied(shown, &update)).or_else(|| others(theirs.len() - 1, "disabled key"));
                rows.push(row(Some(shown), Some(update), "off → on".into(), None, note));
            }
        }
    }
    if refused.is_empty() {
        Ok(rows)
    } else {
        Err(refused)
    }
}

/// One row as a line: `devtest-a: cap $0.10 → $0.20 · spent $0.00 in all`, `devtest-b: no key …`,
/// with its note.
fn line(r: &Row) -> String {
    let mut s = match (&r.skip, r.words.is_empty()) {
        (Some(why), true) => format!("{}: {why}", r.machine),
        (Some(why), false) => format!("{}: {} — {why}", r.machine, r.words),
        (None, _) => format!("{}: {}", r.machine, r.words),
    };
    if let Some(n) = &r.note {
        s.push_str(&format!(" ({n})"));
    }
    s
}

/// A key's state for `--json`.
fn state(k: &KeyInfo) -> serde_json::Value {
    let reset = LimitReset::of(k);
    serde_json::json!({
        "limit": k.limit,
        "reset": reset.map(|r| r.as_str()),
        "disabled": k.disabled,
        "spent": or::spent_against(k, reset),
    })
}

/// How one row came out.
#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    WouldChange,
    Changed,
    Unchanged,
    NoKey,
    Failed(String),
}

impl Outcome {
    fn word(&self) -> &'static str {
        match self {
            Outcome::WouldChange => "would change",
            Outcome::Changed => "changed",
            Outcome::Unchanged => "unchanged",
            Outcome::NoKey => "no key",
            Outcome::Failed(_) => "failed",
        }
    }
}

/// The `--json` object: the verb, whether it was a dry run, each row (`before`/`after` the key's
/// state — `after` as OpenRouter lists it now, else as asked — and how it came out), the counts,
/// and why the keys couldn't be read back, if they couldn't. Pure.
fn json(verb: &Verb, dry_run: bool, done: &[(Row, Outcome, Option<KeyInfo>)], reread: Option<&str>) -> serde_json::Value {
    let keys: Vec<serde_json::Value> = done
        .iter()
        .map(|(r, o, now)| {
            let mut v = serde_json::json!({
                "machine": r.machine,
                "name": r.key.as_ref().and_then(|k| k.name.clone()),
                "hash": r.key.as_ref().map(|k| k.hash.clone()),
                "before": r.key.as_ref().map(state),
                "after": now.as_ref().map(state),
                "result": o.word(),
            });
            if let Outcome::Failed(e) = o {
                v["error"] = serde_json::json!(e);
            }
            if let Some(n) = &r.note {
                v["note"] = serde_json::json!(n);
            }
            v
        })
        .collect();
    let count = |f: fn(&Outcome) -> bool| done.iter().filter(|(_, o, _)| f(o)).count();
    let mut v = serde_json::json!({
        "action": verb.word(),
        "dry_run": dry_run,
        "keys": keys,
        "changed": count(|o| matches!(o, Outcome::Changed | Outcome::WouldChange)),
        "failed": count(|o| matches!(o, Outcome::Failed(_))),
    });
    if let Some(e) = reread {
        v["reread_error"] = serde_json::json!(e);
    }
    v
}

/// The confirm's first line.
fn head(verb: &Verb, n: usize) -> String {
    match verb {
        Verb::Limit { .. } => format!("Will change {n} OpenRouter key cap(s):"),
        Verb::Pause => format!("Will PAUSE {n} OpenRouter key(s) — their AI calls fail until `arena keys resume`:"),
        Verb::Resume => format!("Will resume {n} OpenRouter key(s):"),
    }
}

/// Plan `verb` for the machines `sel` names (one key listing serves the targets and the plan),
/// preview it (`dry_run`), confirm, change each key, read the keys back, and say each key's before
/// → after — a line each, or one JSON object (`json`: the confirm and the notes on stderr). A
/// refusal stops it before anything changes; a key that fails doesn't stop the others, and fails
/// the command at the end.
#[allow(clippy::too_many_arguments)]
async fn run(verb: Verb, sel: SelectArgs, dry_run: bool, json_out: bool, provider: &dyn Provider, cfg: &Config, api: &dyn KeyApi, dir: &std::path::Path, yes: bool) -> Result<()> {
    let naming = Naming::from_config(cfg);
    let csv = std::fs::read_to_string(dir.join(crate::OPENROUTER_KEYS_LINK)).ok();
    let existing = api.list_keys().await.context("listing OpenRouter keys")?;
    let known = crate::key_holders(&naming, &existing, csv.as_deref());
    let (names, _) = crate::keys_targets(provider, cfg, &sel, &known).await?;
    let say = |s: &str| if json_out { eprintln!("{s}") } else { println!("{s}") };
    if names.is_empty() {
        say("(no target machines)");
        return Ok(());
    }
    let rows = plan(&verb, &names, &existing, &naming).map_err(|refused| {
        anyhow::anyhow!("nothing changed — refused:\n  {}", refused.join("\n  "))
    })?;
    let todo: Vec<&Row> = rows.iter().filter(|r| r.update.is_some()).collect();
    let skipped = |r: &Row| if r.key.is_none() { Outcome::NoKey } else { Outcome::Unchanged };
    if dry_run || todo.is_empty() {
        let done: Vec<(Row, Outcome, Option<KeyInfo>)> = rows
            .iter()
            .map(|r| {
                let after = r.key.as_ref().map(|k| r.update.as_ref().map_or_else(|| k.clone(), |u| applied(k, u)));
                (r.clone(), if r.update.is_some() { Outcome::WouldChange } else { skipped(r) }, after)
            })
            .collect();
        if json_out {
            println!("{}", serde_json::to_string_pretty(&json(&verb, dry_run, &done, None))?);
        } else {
            if dry_run {
                println!("Dry-run — would change {} key(s):", todo.len());
            } else {
                println!("(nothing to change)");
            }
            for r in &rows {
                println!("  {}", line(r));
            }
        }
        return Ok(());
    }
    let lines: Vec<String> = todo.iter().map(|r| format!("  {}", line(r))).collect();
    if !crate::confirm(yes, &format!("{}\n{}", head(&verb, todo.len()), lines.join("\n")))? {
        say("aborted.");
        return Ok(());
    }
    let mut done: Vec<(Row, Outcome, Option<KeyInfo>)> = Vec::new();
    for r in &rows {
        let outcome = match (&r.key, &r.update) {
            (Some(k), Some(u)) => match api.update_key(&k.hash, u).await {
                Ok(()) => Outcome::Changed,
                Err(e) => Outcome::Failed(e.to_string()),
            },
            _ => skipped(r),
        };
        done.push((r.clone(), outcome, None));
    }
    // What OpenRouter says now: each changed key read back, and a change it didn't take said.
    let reread = api.list_keys().await.map_err(|e| e.to_string());
    let mut failed = 0;
    for (r, outcome, now) in &mut done {
        let Some(k) = &r.key else { continue };
        let want = r.update.as_ref().map_or_else(|| k.clone(), |u| applied(k, u));
        *now = match &reread {
            Ok(keys) => keys.iter().find(|n| n.hash == k.hash).cloned(),
            Err(_) => Some(want.clone()),
        };
        if *outcome == Outcome::Changed && reread.is_ok() && !now.as_ref().is_some_and(|n| took(n, &want)) {
            *outcome = Outcome::Failed(match now.as_ref() {
                Some(n) => format!(
                    "OpenRouter answered OK but now says: {}, {}",
                    or::plan_cap(n, None, None, true).map(|p| p.words()).unwrap_or_default(),
                    on_off(n.disabled)
                ),
                None => "OpenRouter answered OK but the key isn't listed any more".into(),
            });
        }
        failed += usize::from(matches!(outcome, Outcome::Failed(_)));
    }
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json(&verb, false, &done, reread.as_ref().err().map(String::as_str)))?);
    } else {
        for (r, outcome, _) in &done {
            match outcome {
                Outcome::Changed => println!("✓ {}", line(r)),
                Outcome::Failed(e) => eprintln!("✗ {}: {e}", r.machine),
                _ => println!("= {}", line(r)),
            }
        }
        let changed = done.iter().filter(|(_, o, _)| *o == Outcome::Changed).count();
        println!("\nChanged {changed}, unchanged {}, failed {failed}.", done.len() - changed - failed);
        if let Err(e) = &reread {
            eprintln!("warning: the keys couldn't be read back ({e}) — `arena keys list` shows what they say now");
        }
    }
    if failed > 0 {
        anyhow::bail!("{failed} key(s) failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena_core::{Pod, PodSpec};
    use std::sync::Mutex;

    /// Three machines on a fake fleet (`devtest-alpha`, `-bravo`, `-cloud`); `delta` is a
    /// MACHINE_NAME_LIST name with no pod.
    #[derive(Default)]
    struct Fleet;

    #[async_trait::async_trait]
    impl Provider for Fleet {
        fn name(&self) -> &'static str {
            "hetzner"
        }
        fn describe(&self, _spec: &PodSpec) -> String {
            String::new()
        }
        async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
            Ok(["alpha", "bravo", "cloud"]
                .iter()
                .enumerate()
                .map(|(i, n)| Pod { id: format!("{}", 100 + i), name: format!("devtest-{n}"), provider: "hetzner".into(), status: "running".into(), ..Pod::default() })
                .collect())
        }
        async fn create_pod(&self, _spec: &PodSpec) -> arena_core::Result<Pod> {
            unreachable!("keys never create pods")
        }
        async fn stop_pod(&self, _id: &str) -> arena_core::Result<()> {
            unreachable!()
        }
        async fn restart_pod(&self, _id: &str) -> arena_core::Result<()> {
            unreachable!()
        }
        async fn terminate_pod(&self, _id: &str) -> arena_core::Result<()> {
            unreachable!()
        }
    }

    fn cfg() -> Config {
        Config::parse("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(alpha bravo cloud delta)\n")
    }

    /// Keys on a fake OpenRouter: listed, changed in place as asked (each call recorded); a key
    /// named in `broken` fails its update, one in `ignored` "takes" it without changing.
    #[derive(Default)]
    struct Keys {
        keys: Mutex<Vec<KeyInfo>>,
        calls: Mutex<Vec<String>>,
        broken: Vec<&'static str>,
        ignored: Vec<&'static str>,
    }

    impl Keys {
        fn with(keys: Vec<KeyInfo>) -> Self {
            Keys { keys: Mutex::new(keys), ..Default::default() }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn key(&self, hash: &str) -> KeyInfo {
            self.keys.lock().unwrap().iter().find(|k| k.hash == hash).cloned().unwrap()
        }
    }

    #[async_trait::async_trait]
    impl KeyApi for Keys {
        async fn list_keys(&self) -> arena_core::Result<Vec<KeyInfo>> {
            Ok(self.keys.lock().unwrap().clone())
        }
        async fn create_key(&self, _name: &str, _limit: Option<f64>) -> arena_core::Result<or::CreatedKey> {
            unreachable!("limit/pause/resume never mint")
        }
        async fn delete_key(&self, _hash: &str) -> arena_core::Result<()> {
            unreachable!("limit/pause/resume never delete")
        }
        async fn rename_key(&self, _hash: &str, _new_name: &str) -> arena_core::Result<()> {
            unreachable!()
        }
        async fn update_key(&self, hash: &str, u: &KeyUpdate) -> arena_core::Result<()> {
            self.calls.lock().unwrap().push(format!("{hash} {}", or::update_body(u)?));
            let mut keys = self.keys.lock().unwrap();
            let k = keys.iter_mut().find(|k| k.hash == hash).expect("a listed key");
            let name = k.name.clone().unwrap_or_default();
            if self.broken.contains(&name.as_str()) {
                return Err(arena_core::Error::provider("openrouter update key HTTP 500"));
            }
            if !self.ignored.contains(&name.as_str()) {
                *k = applied(k, u);
            }
            Ok(())
        }
    }

    /// A key: `name`, a `limit` cap (none: `None`) with `spent` of it spent (all told; no
    /// reset), enabled unless `off`, created `at`.
    fn key(hash: &str, name: &str, limit: Option<f64>, spent: f64, off: bool, at: &str) -> KeyInfo {
        KeyInfo {
            hash: hash.into(),
            name: Some(name.into()),
            limit,
            limit_remaining: limit.map(|l| (l - spent).max(0.0)),
            usage: Some(spent),
            usage_daily: Some(0.0),
            usage_weekly: Some(0.0),
            usage_monthly: Some(spent),
            disabled: off,
            created_at: Some(at.into()),
            ..Default::default()
        }
    }

    fn fleet_keys() -> Vec<KeyInfo> {
        vec![
            key("ha", "devtest-alpha", Some(0.1), 0.0, false, "2026-10-30T10:00:00Z"),
            key("hb", "devtest-bravo", Some(5.0), 4.9, false, "2026-10-30T10:00:00Z"),
            // cloud: an old disabled key (rotated out), and the one in use.
            key("hc-old", "devtest-cloud", Some(5.0), 5.0, true, "2026-10-01T10:00:00Z"),
            key("hc", "devtest-cloud", Some(5.0), 1.0, false, "2026-10-30T10:00:00Z"),
            key("hx", "arena9-alpha", Some(5.0), 5.0, false, "2026-10-30T10:00:00Z"),
        ]
    }

    async fn keys_cmd(argv: &[&str], api: &Keys) -> std::result::Result<(), String> {
        use clap::Parser;
        let cli = crate::Cli::try_parse_from([&["arena", "keys"][..], argv].concat()).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        let crate::Cmd::Keys(k) = cli.cmd else { unreachable!() };
        let dir = std::env::temp_dir().join(format!("arena-key-limits-none-{}", std::process::id()));
        crate::keys_with(k, &Fleet, std::sync::Arc::new(arena_core::remote::FakeRemote::new()), &cfg(), api, &dir, true)
            .await
            .map_err(|e| e.to_string())
    }

    /// `keys limit`: an absolute cap and a raise, each on exactly the machines named, each a
    /// PATCH of only the cap; a range and `--all`; the amount always the last positional.
    #[tokio::test]
    async fn limit_sets_and_raises_exactly_the_named_machines_caps() {
        let api = Keys::with(fleet_keys());
        keys_cmd(&["limit", "alpha", "0.2"], &api).await.unwrap();
        assert_eq!(api.calls(), [r#"ha {"limit":0.2}"#]);
        keys_cmd(&["limit", "alpha", "+0.05"], &api).await.unwrap();
        assert_eq!(api.key("ha").limit, Some(0.25), "a raise of the cap it has now, without float noise");
        // A range: bravo $5 → $10, cloud's enabled key $5 → $10 (the disabled one untouched).
        api.calls.lock().unwrap().clear();
        keys_cmd(&["limit", "bravo..cloud", "+5"], &api).await.unwrap();
        assert_eq!(api.calls(), [r#"hb {"limit":10.0}"#, r#"hc {"limit":10.0}"#]);
        assert_eq!(api.key("hc-old").limit, Some(5.0));
        // --all: every current pod; a cap already there is left alone (no PATCH).
        api.calls.lock().unwrap().clear();
        keys_cmd(&["limit", "--all", "10"], &api).await.unwrap();
        assert_eq!(api.calls(), [r#"ha {"limit":10.0}"#]);
        // A reset change alone (`+0`), and no cap at all only with --no-cap.
        api.calls.lock().unwrap().clear();
        keys_cmd(&["limit", "alpha", "+0", "--reset", "daily"], &api).await.unwrap();
        keys_cmd(&["limit", "alpha", "--no-cap"], &api).await.unwrap();
        assert_eq!(api.calls(), [r#"ha {"limit_reset":"daily"}"#, r#"ha {"limit":null}"#]);
        // Another cohort's key is never one of ours.
        assert_eq!(api.key("hx").limit, Some(5.0));
    }

    /// Refusals come before any change: a cap under what a key spent (unless --force), a raise of
    /// a key without a cap, an amount that isn't one, a missing amount; a dry run changes nothing.
    #[tokio::test]
    async fn limit_refuses_up_front_and_changes_nothing() {
        let api = Keys::with(fleet_keys());
        // bravo spent $4.90: a $4 cap is refused — and so is alpha's fine $1 with it.
        let e = keys_cmd(&["limit", "alpha", "bravo", "4"], &api).await.unwrap_err();
        assert!(e.contains("nothing changed") && e.contains("devtest-bravo: a $4.00 cap is under what it has spent in all ($4.90)"), "{e}");
        assert!(!e.contains("devtest-alpha"), "{e}");
        assert!(api.calls().is_empty());
        keys_cmd(&["limit", "bravo", "4", "--force"], &api).await.unwrap();
        assert_eq!(api.calls(), [r#"hb {"limit":4.0}"#]);
        api.calls.lock().unwrap().clear();
        for (argv, says) in [
            (&["limit", "alpha", "none"][..], "the last argument is the cap: no cap is `--no-cap`"),
            (&["limit", "alpha", "bravo"], "the last argument is the cap: `bravo` isn't an amount"),
            (&["limit", "+5"], "name the machines"),
            (&["limit", "--all"], "give the cap after the machines"),
            (&["limit"], "give the cap after the machines"),
            (&["limit", "alpha", "--no-cap", "--reset", "hourly"], "invalid value 'hourly'"),
        ] {
            let e = match keys_cmd_parse(argv) {
                Err(e) => e,
                Ok(()) => keys_cmd(argv, &api).await.unwrap_err(),
            };
            assert!(e.contains(says), "{argv:?}: {e}");
        }
        let open = Keys::with(vec![key("hn", "devtest-alpha", None, 1.0, false, "2026-10-30T10:00:00Z")]);
        let e = keys_cmd(&["limit", "alpha", "+5"], &open).await.unwrap_err();
        assert!(e.contains("devtest-alpha: it has no cap to raise"), "{e}");
        for argv in [&["limit", "alpha", "+5", "--dry-run"][..], &["limit", "--all", "1", "--dry-run", "--force", "--json"]] {
            keys_cmd(argv, &api).await.unwrap();
        }
        assert!(api.calls().is_empty() && open.calls().is_empty(), "{:?}", api.calls());
    }

    /// Whether clap takes `argv` (`keys …`): its error, if not.
    fn keys_cmd_parse(argv: &[&str]) -> std::result::Result<(), String> {
        use clap::Parser;
        crate::Cli::try_parse_from([&["arena", "keys"][..], argv].concat()).map(drop).map_err(|e| e.to_string())
    }

    /// pause: every enabled key of the machine (cloud's old disabled one is already off); resume:
    /// the one the dashboard shows — never an older, rotated-out key; a key already in that state
    /// is left alone; a machine without a key is said, not failed.
    #[tokio::test]
    async fn pause_and_resume_touch_the_right_keys() {
        let api = Keys::with(fleet_keys());
        keys_cmd(&["pause", "alpha..cloud", "delta"], &api).await.unwrap();
        assert_eq!(api.calls(), [r#"ha {"disabled":true}"#, r#"hb {"disabled":true}"#, r#"hc {"disabled":true}"#]);
        api.calls.lock().unwrap().clear();
        keys_cmd(&["pause", "alpha"], &api).await.unwrap();
        assert!(api.calls().is_empty(), "already off");
        keys_cmd(&["resume", "cloud", "alpha"], &api).await.unwrap();
        assert_eq!(api.calls(), [r#"ha {"disabled":false}"#, r#"hc {"disabled":false}"#], "the newest of cloud's, not hc-old");
        assert!(api.key("hc-old").disabled);
        api.calls.lock().unwrap().clear();
        keys_cmd(&["enable", "alpha"], &api).await.unwrap();
        keys_cmd(&["disable", "--all", "--dry-run"], &api).await.unwrap();
        assert!(api.calls().is_empty(), "already on; a dry run: {:?}", api.calls());
    }

    /// A key that fails doesn't stop the others and fails the command; a change OpenRouter
    /// answered OK for but didn't make is a failure too (read back).
    #[tokio::test]
    async fn a_failed_or_unmade_change_fails_the_command_after_the_rest() {
        let api = Keys { broken: vec!["devtest-alpha"], ..Keys::with(fleet_keys()) };
        let e = keys_cmd(&["limit", "alpha", "bravo", "+1"], &api).await.unwrap_err();
        assert_eq!(e, "1 key(s) failed");
        assert_eq!(api.key("hb").limit, Some(6.0), "bravo went ahead");
        let api = Keys { ignored: vec!["devtest-bravo"], ..Keys::with(fleet_keys()) };
        let e = keys_cmd(&["pause", "bravo"], &api).await.unwrap_err();
        assert_eq!(e, "1 key(s) failed");
    }

    /// The command lines the dashboard's AI key rows run (arena-tui's `aikey::args`), which it
    /// builds without this parser, parse as it means them: the pods targets and the amount the
    /// cap — the last positional — `--yes` global, rotate's `--copy` there.
    #[test]
    fn the_dashboards_key_command_lines_parse_as_it_means_them() {
        use clap::Parser;
        let full = |argv: &[&str]| {
            crate::Cli::try_parse_from(["arena", "--config", "/c.env", "--provider", "hetzner"].iter().chain(argv).copied()).unwrap_or_else(|e| panic!("{argv:?}: {e}"))
        };
        let c = full(&["keys", "limit", "devtest-alpha", "devtest-bravo", "+5", "--yes"]);
        let crate::Cmd::Keys(crate::KeysCmd::Limit(a)) = c.cmd else { panic!() };
        assert!(c.yes && !a.no_cap && !a.force && !a.dry_run && a.reset.is_none());
        let (targets, change) = split_amount(a.args, a.no_cap).unwrap();
        assert_eq!((targets, change), (vec!["devtest-alpha".to_string(), "devtest-bravo".into()], CapChange::Raise(5.0)));
        for (verb, resume) in [("pause", false), ("resume", true)] {
            let c = full(&["keys", verb, "devtest-alpha", "--yes"]);
            let (a, is_resume) = match c.cmd {
                crate::Cmd::Keys(crate::KeysCmd::Pause(a)) => (a, false),
                crate::Cmd::Keys(crate::KeysCmd::Resume(a)) => (a, true),
                _ => panic!("{verb}"),
            };
            assert!(c.yes && is_resume == resume && a.sel.targets == ["devtest-alpha"] && !a.dry_run, "{verb}");
        }
        let c = full(&["keys", "rotate", "devtest-alpha", "--copy", "--yes"]);
        let crate::Cmd::Keys(crate::KeysCmd::Rotate { copy, dry_run, sel, limit }) = c.cmd else { panic!() };
        assert!(c.yes && copy && !dry_run && limit.is_none() && sel.targets == ["devtest-alpha"]);
    }

    /// The planner, row by row: words, skips and notes.
    #[test]
    fn plan_words_each_machines_key() {
        let cfg = cfg();
        let naming = Naming::from_config(&cfg);
        let machines: Vec<String> = ["alpha", "bravo", "cloud", "delta"].iter().map(|n| format!("devtest-{n}")).collect();
        let lines_of = |verb: Verb, machines: &[String], keys: &[KeyInfo]| plan(&verb, machines, keys, &naming).unwrap().iter().map(line).collect::<Vec<_>>();
        let lines = |verb: Verb, keys: &[KeyInfo]| lines_of(verb, &machines, keys);
        let raise = Verb::Limit { change: Some(CapChange::Raise(5.0)), reset: None, force: false };
        assert_eq!(
            lines(raise, &fleet_keys()),
            [
                "devtest-alpha: cap $0.10 → $5.10 · spent $0.00 in all",
                "devtest-bravo: cap $5.00 → $10.00 · spent $4.90 in all",
                "devtest-cloud: cap $5.00 → $10.00 · spent $1.00 in all (1 other key by this name left as it is — `arena keys list --all`)",
                "devtest-delta: no key (`arena keys gen devtest-delta` mints one)",
            ]
        );
        // A paused key's cap can change; the line says it stays paused.
        let mut keys = fleet_keys();
        keys[0].disabled = true;
        let same = Verb::Limit { change: Some(CapChange::To(0.1)), reset: None, force: false };
        assert_eq!(lines_of(same, &machines[..1], &keys)[0], "devtest-alpha: cap $0.10 · spent $0.00 in all — unchanged (it's paused — `arena keys resume devtest-alpha` turns it back on)");
        // Resuming a key at its cap says the cap still holds it; an enabled $0 cap too.
        keys[1].disabled = true;
        keys[1].usage = Some(5.0);
        keys[1].usage_monthly = Some(5.0);
        let resumed = lines(Verb::Resume, &keys);
        assert_eq!(resumed[0], "devtest-alpha: off → on");
        assert_eq!(resumed[1], "devtest-bravo: off → on (at its cap ($5.00 of $5.00) — `arena keys limit devtest-bravo +5` raises it)");
        assert_eq!(resumed[2], "devtest-cloud: on — already on");
        let zero = vec![key("hz", "devtest-alpha", Some(0.0), 0.0, false, "2026-10-30T10:00:00Z")];
        assert!(lines(Verb::Resume, &zero)[0].starts_with("devtest-alpha: on — already on (at its cap ($0.00 of $0.00)"), "{:?}", lines(Verb::Resume, &zero));
        assert_eq!(lines(Verb::Pause, &keys)[0], "devtest-alpha: off — already off");
    }

    /// `--json`: each key's before and after (as read back) and how it came out.
    #[test]
    fn json_says_before_after_and_the_outcome() {
        let k = key("ha", "devtest-alpha", Some(0.1), 0.0, false, "2026-10-30T10:00:00Z");
        let u = KeyUpdate { limit: Some(Some(0.2)), ..Default::default() };
        let row = Row { machine: "devtest-alpha".into(), key: Some(k.clone()), update: Some(u.clone()), words: String::new(), skip: None, note: None };
        let none = Row { machine: "devtest-delta".into(), key: None, update: None, words: String::new(), skip: Some("no key".into()), note: None };
        let verb = Verb::Limit { change: Some(CapChange::To(0.2)), reset: None, force: false };
        let v = json(&verb, false, &[(row.clone(), Outcome::Changed, Some(applied(&k, &u))), (none, Outcome::NoKey, None)], None);
        assert_eq!(
            v,
            serde_json::json!({
                "action": "limit", "dry_run": false, "changed": 1, "failed": 0,
                "keys": [
                    {"machine": "devtest-alpha", "name": "devtest-alpha", "hash": "ha", "result": "changed",
                     "before": {"limit": 0.1, "reset": null, "disabled": false, "spent": 0.0},
                     "after": {"limit": 0.2, "reset": null, "disabled": false, "spent": 0.0}},
                    {"machine": "devtest-delta", "name": null, "hash": null, "result": "no key", "before": null, "after": null},
                ],
            })
        );
        let v = json(&Verb::Pause, true, &[(row, Outcome::Failed("HTTP 500".into()), None)], Some("timed out"));
        assert_eq!((v["action"].as_str(), v["failed"].as_u64(), v["keys"][0]["error"].as_str()), (Some("pause"), Some(1), Some("HTTP 500")));
        assert_eq!(v["reread_error"], "timed out");
    }
}
