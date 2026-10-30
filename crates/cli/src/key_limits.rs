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
    /// The machines (as for `gen`), then the cap: `10` (USD) sets it, `+5` raises it by $5
    ///
    /// The amount is always the last one; with --no-cap there is none.
    #[arg(value_name = "TARGET… USD|+USD")]
    args: Vec<String>,
    /// Take the cap away (give no amount): nothing then bounds what the key spends
    #[arg(long)]
    no_cap: bool,
    /// When the cap starts over: daily, weekly, monthly or never (00:00 UTC)
    ///
    /// Weeks start on Monday. `+0 --reset daily` keeps the cap and changes only this.
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
    /// The keys as the dashboard showed them: one that isn't, or moved, is refused
    #[arg(long, hide = true, value_name = "HASH=CAP/RESET/ON|OFF")]
    expect: Vec<String>,
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
    /// The keys as the dashboard showed them: one that isn't, or moved, is refused
    #[arg(long, hide = true, value_name = "HASH=CAP/RESET/ON|OFF")]
    expect: Vec<String>,
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
    let expect = expected(&a.expect)?;
    run(verb, a.opts.args(&targets), &expect, a.dry_run, a.json, provider, cfg, api, dir, yes).await
}

/// `arena keys pause` (`resume: false`) or `arena keys resume`.
pub(crate) async fn pause(a: PauseArgs, resume: bool, provider: &dyn Provider, cfg: &Config, api: &dyn KeyApi, dir: &std::path::Path, yes: bool) -> Result<()> {
    let verb = if resume { Verb::Resume } else { Verb::Pause };
    let expect = expected(&a.expect)?;
    run(verb, a.sel.args(), &expect, a.dry_run, a.json, provider, cfg, api, dir, yes).await
}

/// How old the keys a machine's plan is made again from may be ([`run`]): a listing serves the
/// machines whose changes start within it, so a fleet-wide run re-reads every few machines, never
/// once per machine nor once for all.
const FRESH_FOR: std::time::Duration = std::time::Duration::from_secs(2);

/// How long a key change waits for another arena's on this box ([`keys_lock`]).
const KEYS_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// The box's key-change lock (`<state dir>/openrouter-keys.lock`), held until the file is dropped:
/// `keys limit|pause|resume` from two terminals, or a terminal and the dashboard, take turns — the
/// second plans from the first's result instead of over it. Waits up to [`KEYS_LOCK_WAIT`]. No
/// state directory: said, and no lock.
async fn keys_lock(cfg: &Config) -> Result<Option<std::fs::File>> {
    let dir = match arena_core::snapshot::state_dir_for(cfg) {
        Ok(d) => d,
        Err(why) => {
            eprintln!("warning: no state directory ({why}) — key changes from other arena commands aren't kept apart");
            return Ok(None);
        }
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("making {}", dir.display()))?;
    let path = dir.join("openrouter-keys.lock");
    let f = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(&path)
    }
    .with_context(|| format!("opening {}", path.display()))?;
    let deadline = std::time::Instant::now() + KEYS_LOCK_WAIT;
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(Some(f)),
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                anyhow::bail!("another arena has been changing OpenRouter keys for {}s ({}) — nothing changed", KEYS_LOCK_WAIT.as_secs(), path.display())
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e).with_context(|| format!("locking {}", path.display())),
        }
    }
}

/// The test a key must pass in the request just before its change ([`or::KeyCheck`]): still the
/// key planned for — the same name, cap, reset, on/off (review codex r3 #2, r4 #1: renamed or
/// raised meanwhile) — and, a cap change, still one [`or::plan_cap`] allows with the spend it has
/// now (r4 #3: spent past a lowered cap while the earlier keys were changed).
fn just_before(verb: &Verb, planned: &KeyInfo) -> impl Fn(&KeyInfo) -> std::result::Result<(), String> + Send + Sync {
    let (was, verb) = (or::Expected::of(planned), verb.clone());
    move |now: &KeyInfo| {
        if !was.holds(now) {
            return Err(format!("changed since it was read (now {}{})", state_words(now), now.name.as_ref().filter(|n| Some(*n) != was.name.as_ref()).map(|n| format!(", named {n}")).unwrap_or_default()));
        }
        match &verb {
            Verb::Limit { change, reset, force } => or::plan_cap(now, *change, *reset, *force).map(drop),
            _ => Ok(()),
        }
    }
}

/// `--expect`'s values ([`or::Expected::parse`]).
fn expected(tokens: &[String]) -> Result<Vec<or::Expected>> {
    tokens.iter().map(|t| or::Expected::parse(t).map_err(|e| anyhow::anyhow!("--expect: {e}"))).collect()
}

/// The dashboard's `--expect` against the plan: each key a row would change must be one it showed,
/// still as shown — else the operator confirmed a change to another key, or another cap (review
/// codex r2 #3). Nothing to check without `--expect` (the CLI's own confirm showed the plan).
/// The refusals. Pure.
fn unexpected(verb: &Verb, rows: &[Row], expect: &[or::Expected]) -> Vec<String> {
    if expect.is_empty() {
        return Vec::new();
    }
    rows.iter()
        .filter(|r| r.update.is_some())
        .filter_map(|r| {
            let k = r.key.as_ref()?;
            // Another of this machine's keys was shown: the name has more than one (the dashboard
            // shows one key a name; a pause acts on every enabled one).
            let another_shown = rows.iter().any(|o| o.machine == r.machine && o.key.as_ref().is_some_and(|ok| ok.hash != k.hash && expect.iter().any(|e| e.hash == ok.hash)));
            match expect.iter().find(|e| e.hash == k.hash) {
                None if another_shown => Some(format!(
                    "{}: it has more than one key and the dashboard showed one — `arena keys list --all`, then `arena keys {} {0}`",
                    r.machine,
                    verb.word()
                )),
                None => Some(format!("{}: its key isn't the one the dashboard showed (rotated meanwhile?) — look again", r.machine)),
                Some(e) if !e.holds(k) => Some(format!("{}: its key changed since the dashboard showed it (now {}) — look again", r.machine, state_words(k))),
                Some(_) => None,
            }
        })
        .collect()
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

/// Whether two readings of a key say the same cap, reset and on/off — the cap to the float's noise
/// ([`or::CAP_EPSILON`]), never a cent's slack (review codex r1 #2: a $0.004 change OpenRouter
/// didn't make went unseen). What a key read back must say after a change ([`run`]), and what it
/// must still say when the change is made.
fn same(now: &KeyInfo, want: &KeyInfo) -> bool {
    let cap = match (now.limit, want.limit) {
        (Some(a), Some(b)) => (a - b).abs() < or::CAP_EPSILON,
        (a, b) => a.is_none() && b.is_none(),
    };
    cap && LimitReset::of(now) == LimitReset::of(want) && now.disabled == want.disabled
}

/// `cap $25.00 (monthly), on` — a key's state, for a message.
fn state_words(k: &KeyInfo) -> String {
    let cap = k.limit.map(|c| format!("cap {}", or::exact_money(c))).unwrap_or_else(|| "no cap".into());
    let reset = LimitReset::of(k).map(|r| format!(" ({})", r.as_str())).unwrap_or_default();
    format!("{cap}{reset}, {}", on_off(k.disabled))
}

/// Why machine `m`'s changes (`was`: its rows as planned from the first listing) mustn't be made
/// now, by the keys read again after the confirm (`fresh`): the plan made again from them must be
/// the same — the same keys under its name, the same changes, each key as it was. Otherwise
/// something moved meanwhile: a cap (review codex r1 #1: `+5` confirmed as `$5 → $10` would set
/// $10 over a cap raised to $25), a key replaced or added under the name (r2 #1: a resume would
/// wake a rotated-out key, a pause miss the new one), a key renamed away (r2 #2) — or the spend,
/// making a cap one [`or::plan_cap`] refuses. `None`: go ahead. Pure.
fn moved(verb: &Verb, m: &str, was: &[&Row], fresh: &[KeyInfo], naming: &Naming) -> Option<String> {
    let again = match plan(verb, &[m.to_string()], fresh, naming) {
        Ok(rows) => rows,
        Err(why) => {
            let why = why.join("; ");
            return Some(format!("{} — nothing done", why.strip_prefix(&format!("{m}: ")).unwrap_or(&why)));
        }
    };
    let changes = |rows: Vec<&Row>| -> Vec<(KeyInfo, KeyUpdate)> {
        rows.into_iter().filter_map(|r| Some((r.key.clone()?, r.update.clone()?))).collect()
    };
    let (was, now) = (changes(was.to_vec()), changes(again.iter().collect()));
    let alike = was.len() == now.len()
        && was.iter().zip(&now).all(|((a, ua), (b, ub))| a.hash == b.hash && a.name == b.name && ua == ub && same(a, b));
    if alike {
        return None;
    }
    let names = crate::machine_key_names(naming, m);
    let theirs: Vec<String> = fresh.iter().filter(|k| k.name.as_deref().is_some_and(|n| names.iter().any(|t| t == n))).map(state_words).collect();
    let now = if theirs.is_empty() { "no key under its name".to_string() } else { theirs.join("; ") };
    Some(format!("its key changed while this ran (now {now}) — nothing done; run the command again"))
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
    /// Asked, answered OK, but the keys couldn't be read back to check it (review codex r7 #3):
    /// fails the command, never passed off as done.
    Unverified,
    Unchanged,
    NoKey,
    Failed(String),
}

impl Outcome {
    fn word(&self) -> &'static str {
        match self {
            Outcome::WouldChange => "would change",
            Outcome::Changed => "changed",
            Outcome::Unverified => "unverified",
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
        "failed": count(|o| matches!(o, Outcome::Failed(_) | Outcome::Unverified)),
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
async fn run(verb: Verb, sel: SelectArgs, expect: &[or::Expected], dry_run: bool, json_out: bool, provider: &dyn Provider, cfg: &Config, api: &dyn KeyApi, dir: &std::path::Path, yes: bool) -> Result<()> {
    let naming = Naming::from_config(cfg);
    let csv = std::fs::read_to_string(dir.join(crate::OPENROUTER_KEYS_LINK)).ok();
    let existing = api.list_keys().await.context("listing OpenRouter keys")?;
    let known = crate::key_holders(&naming, &existing, csv.as_deref());
    let (names, _) = crate::keys_targets(provider, cfg, &sel, &known).await?;
    let say = |s: &str| if json_out { eprintln!("{s}") } else { println!("{s}") };
    if names.is_empty() {
        if json_out {
            println!("{}", serde_json::to_string_pretty(&json(&verb, dry_run, &[], None))?);
        } else {
            println!("(no target machines)");
        }
        return Ok(());
    }
    let rows = plan(&verb, &names, &existing, &naming).map_err(|refused| {
        anyhow::anyhow!("nothing changed — refused:\n  {}", refused.join("\n  "))
    })?;
    let off = unexpected(&verb, &rows, expect);
    anyhow::ensure!(off.is_empty(), "nothing changed — refused:\n  {}", off.join("\n  "));
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
    // One arena changing keys at a time on this box (another operator's command, the dashboard's
    // runs). Then machine by machine — every one named, a skipped one too (review codex r6 #1:
    // resumed meanwhile, a pause's "already off" would have stood) — its plan made again from
    // keys read at most [`FRESH_FOR`] before its changes ([`moved`]: a change confirmed against
    // keys that have moved since isn't made), and each key checked once more in the request just
    // before its change ([`just_before`]). OpenRouter has no conditional update to close the rest.
    let _lock = keys_lock(cfg).await?;
    let mut machines: Vec<&str> = rows.iter().map(|r| r.machine.as_str()).collect();
    machines.dedup();
    let mut fresh: Option<(std::time::Instant, Vec<KeyInfo>)> = None;
    let mut done: Vec<(Row, Outcome, Option<KeyInfo>)> = Vec::new();
    // A resume reads the keys right before each machine's: it can wake a rotated-out key, the one
    // a newer key under the name replaced (review codex r7 #1).
    let fresh_for = if verb == Verb::Resume { std::time::Duration::ZERO } else { FRESH_FOR };
    for m in machines {
        if fresh.as_ref().is_none_or(|(at, _)| at.elapsed() >= fresh_for) {
            let at = std::time::Instant::now();
            fresh = api.list_keys().await.ok().map(|keys| (at, keys));
        }
        let theirs: Vec<&Row> = rows.iter().filter(|r| r.machine == m).collect();
        let stale = match &fresh {
            Some((_, keys)) => moved(&verb, m, &theirs, keys, &naming),
            None => Some("the keys couldn't be read again before changing them — nothing done".into()),
        };
        for r in theirs {
            let outcome = match (&r.key, &r.update, &stale) {
                (_, _, Some(why)) => Outcome::Failed(why.clone()),
                (Some(k), Some(u), None) => match api.update_key(&k.hash, u, Some(&just_before(&verb, k))).await {
                    Ok(()) => Outcome::Changed,
                    Err(e) => Outcome::Failed(e.to_string()),
                },
                _ => skipped(r),
            };
            done.push((r.clone(), outcome, None));
        }
    }
    // What OpenRouter says now: each changed key read back, and a change it didn't take said.
    let reread = api.list_keys().await.map_err(|e| e.to_string());
    for (r, outcome, now) in &mut done {
        let Some(k) = &r.key else { continue };
        let want = r.update.as_ref().map_or_else(|| k.clone(), |u| applied(k, u));
        // Unknown when the keys couldn't be read back: never what was asked, passed off as done
        // (review codex r3 #3).
        *now = reread.as_ref().ok().and_then(|keys| keys.iter().find(|n| n.hash == k.hash).cloned());
        if *outcome == Outcome::Changed && reread.is_err() {
            *outcome = Outcome::Unverified;
        }
        if *outcome == Outcome::Changed && reread.is_ok() && !now.as_ref().is_some_and(|n| same(n, &want)) {
            *outcome = Outcome::Failed(match now.as_ref() {
                Some(n) => format!(
                    "OpenRouter answered OK but now says: {}, {}",
                    or::plan_cap(n, None, None, true).map(|p| p.words()).unwrap_or_default(),
                    on_off(n.disabled)
                ),
                None => "OpenRouter answered OK but the key isn't listed any more".into(),
            });
        }
    }
    // What the keys say afterwards, of a name: after a pause no key under any named machine's name
    // is on (review codex r5 #1, r6 #1: one added or resumed meanwhile), after a resume the key
    // turned on is the one its name shows (r6 #2: a newer one turned up meanwhile, the old one is
    // on again beside it). Anything else is said and fails the command.
    if let Ok(keys) = &reread {
        let failed_hashes: Vec<String> = done.iter().filter(|(_, o, _)| matches!(o, Outcome::Failed(_))).filter_map(|(r, _, _)| r.key.as_ref().map(|k| k.hash.clone())).collect();
        let mut machines: Vec<&str> = rows.iter().map(|r| r.machine.as_str()).collect();
        machines.dedup();
        let mut extra = Vec::new();
        for m in machines {
            let names = crate::machine_key_names(&naming, m);
            let theirs: Vec<&KeyInfo> = keys.iter().filter(|k| k.name.as_deref().is_some_and(|n| names.iter().any(|t| t == n))).collect();
            let row = |k: &KeyInfo| Row { machine: m.to_string(), key: Some(k.clone()), update: None, words: on_off(k.disabled).into(), skip: None, note: None };
            match verb {
                Verb::Pause => {
                    for k in theirs.iter().filter(|k| !k.disabled && !failed_hashes.contains(&k.hash)) {
                        let why = format!("a key under its name is still on (added or resumed while this ran) — `arena keys pause {m}` again");
                        extra.push((row(k), Outcome::Failed(why), Some((*k).clone())));
                    }
                }
                Verb::Resume => {
                    // A key this woke that isn't the one its name shows — a newer one turned up — is
                    // turned off again, if it's still as this left it (review codex r7 #1): a
                    // rotated-out key, perhaps a leaked one, never stays on beside its replacement.
                    let shown = or::shown_key(theirs.iter().copied()).map(|s| s.hash.clone());
                    for (r, outcome, now) in done.iter_mut().filter(|(r, o, _)| r.machine == m && *o == Outcome::Changed) {
                        let (Some(k), Some(u)) = (&r.key, &r.update) else { continue };
                        if shown.as_ref().is_none_or(|s| *s == k.hash) {
                            continue;
                        }
                        let left = applied(k, u);
                        let off = KeyUpdate { disabled: Some(true), ..Default::default() };
                        let was = or::Expected::of(&left);
                        let check = move |n: &KeyInfo| if was.holds(n) { Ok(()) } else { Err(format!("changed since (now {})", state_words(n))) };
                        let newer = "a newer key under its name turned up while this ran";
                        *outcome = Outcome::Failed(match api.update_key(&k.hash, &off, Some(&check)).await {
                            Ok(()) => {
                                *now = Some(applied(&left, &off));
                                format!("{newer}; this older one was turned off again")
                            }
                            Err(e) => format!("{newer}; this older one is on beside it and turning it off again failed ({e}) — `arena keys pause {m}`"),
                        });
                    }
                }
                Verb::Limit { .. } => {}
            }
        }
        done.extend(extra);
    }
    let count = |f: fn(&Outcome) -> bool| done.iter().filter(|(_, o, _)| f(o)).count();
    let failed = count(|o| matches!(o, Outcome::Failed(_)));
    let unverified = count(|o| *o == Outcome::Unverified);
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json(&verb, false, &done, reread.as_ref().err().map(String::as_str)))?);
    } else {
        for (r, outcome, _) in &done {
            match outcome {
                Outcome::Changed => println!("✓ {}", line(r)),
                Outcome::Unverified => println!("? {} — asked, but not read back to check", line(r)),
                Outcome::Failed(e) => eprintln!("✗ {}: {e}", r.machine),
                _ => println!("= {}", line(r)),
            }
        }
        let changed = count(|o| *o == Outcome::Changed);
        let unchecked = if unverified > 0 { format!(", not checked {unverified}") } else { String::new() };
        println!("\nChanged {changed}, unchanged {}, failed {failed}{unchecked}.", done.len() - changed - failed - unverified);
        if let Err(e) = &reread {
            eprintln!("warning: the keys couldn't be read back ({e}) — `arena keys list` shows what they say now");
        }
    }
    if failed > 0 {
        anyhow::bail!("{failed} key(s) failed");
    }
    if unverified > 0 {
        anyhow::bail!("{unverified} change(s) made but not read back to check — `arena keys list` shows what the keys say");
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
        let state = std::env::temp_dir().join(format!("arena-key-limits-state-{}", std::process::id()));
        Config::parse(&format!("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(alpha bravo cloud delta)\nARENA_STATE_DIR={}\n", state.display()))
    }

    /// A change to the key with this hash.
    type Meanwhile = (&'static str, fn(&mut KeyInfo));

    /// Keys on a fake OpenRouter: listed, changed in place as asked (each call recorded); a key
    /// named in `broken` fails its update, one in `ignored` "takes" it without changing; someone
    /// else changes keys between the calls as [`Keys::meanwhile`] says.
    #[derive(Default)]
    struct Keys {
        keys: Mutex<Vec<KeyInfo>>,
        calls: Mutex<Vec<String>>,
        broken: Vec<&'static str>,
        ignored: Vec<&'static str>,
        /// Someone else's change (another operator's, the participant's spend) to a key, made
        /// right after the n-th listing is read (`on_list`), or as the n-th update starts
        /// (`on_update`): `(n, (hash, change))`.
        on_list: Mutex<Vec<(usize, Meanwhile)>>,
        on_update: Mutex<Vec<(usize, Meanwhile)>>,
        lists: Mutex<usize>,
        updates: Mutex<usize>,
        /// Listings (by number) that fail.
        failing_lists: Vec<usize>,
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
        /// The `n`-th's changes of `hooks`, made.
        fn someone_else(&self, hooks: &Mutex<Vec<(usize, Meanwhile)>>, n: usize) {
            for (at, (hash, change)) in hooks.lock().unwrap().iter() {
                if *at == n {
                    self.keys.lock().unwrap().iter_mut().filter(|k| k.hash == *hash).for_each(change);
                }
            }
        }
        /// With someone else's changes: after listing `n` / as update `n` starts.
        fn meanwhile(self, on_list: Vec<(usize, Meanwhile)>, on_update: Vec<(usize, Meanwhile)>) -> Self {
            Keys { on_list: Mutex::new(on_list), on_update: Mutex::new(on_update), ..self }
        }
    }

    #[async_trait::async_trait]
    impl KeyApi for Keys {
        async fn list_keys(&self) -> arena_core::Result<Vec<KeyInfo>> {
            let listed = self.keys.lock().unwrap().clone();
            let n = {
                let mut n = self.lists.lock().unwrap();
                *n += 1;
                *n
            };
            self.someone_else(&self.on_list, n);
            if self.failing_lists.contains(&n) {
                return Err(arena_core::Error::provider("openrouter list keys: timed out"));
            }
            Ok(listed)
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
        async fn update_key(&self, hash: &str, u: &KeyUpdate, check: Option<or::KeyCheck<'_>>) -> arena_core::Result<()> {
            let n = {
                let mut n = self.updates.lock().unwrap();
                *n += 1;
                *n
            };
            self.someone_else(&self.on_update, n);
            let mut keys = self.keys.lock().unwrap();
            let k = keys.iter_mut().find(|k| k.hash == hash).expect("a listed key");
            if let Some(Err(why)) = check.map(|c| c(k)) {
                return Err(arena_core::Error::provider(format!("openrouter update key: refused — {why}")));
            }
            self.calls.lock().unwrap().push(format!("{hash} {}", or::update_body(u)?));
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
        // A $0.004 change it didn't make is seen too (review codex r1 #2: a cent's slack hid it).
        let e = keys_cmd(&["limit", "bravo", "5.004"], &api).await.unwrap_err();
        assert_eq!(e, "1 key(s) failed");
    }

    /// A key that moved between the listing the plan was made from and the change (review codex
    /// r1 #1: `+5` confirmed as `$5 → $10` set $10 over a cap raised to $25 meanwhile) isn't
    /// changed; the others are.
    #[tokio::test]
    async fn a_key_that_moved_since_the_plan_is_left_alone() {
        let api = Keys::with(fleet_keys()).meanwhile(vec![(1, ("hb", |k| k.limit = Some(25.0)))], vec![]);
        let e = keys_cmd(&["limit", "alpha", "bravo", "+5"], &api).await.unwrap_err();
        assert_eq!(e, "1 key(s) failed");
        assert_eq!(api.calls(), [r#"ha {"limit":5.1}"#], "bravo isn't touched");
        assert_eq!(api.key("hb").limit, Some(25.0));
    }

    /// A later key that moves while earlier ones are being changed (review codex r3 #2) is refused
    /// in the request just before its own change; the earlier ones stand.
    #[tokio::test]
    async fn a_later_key_that_moves_mid_run_is_refused_at_its_own_change() {
        let api = Keys::with(fleet_keys()).meanwhile(vec![], vec![(1, ("hc", |k| k.limit = Some(25.0)))]);
        let e = keys_cmd(&["limit", "alpha..cloud", "+5"], &api).await.unwrap_err();
        assert_eq!(e, "1 key(s) failed");
        assert_eq!(api.calls(), [r#"ha {"limit":5.1}"#, r#"hb {"limit":10.0}"#], "cloud's isn't made");
        assert_eq!(api.key("hc").limit, Some(25.0));
        // Renamed to another machine meanwhile (review codex r4 #1): not this machine's to pause.
        let api = Keys::with(fleet_keys()).meanwhile(vec![], vec![(1, ("hc", |k| k.name = Some("devtest-delta".into())))]);
        assert_eq!(keys_cmd(&["pause", "alpha", "cloud"], &api).await.unwrap_err(), "1 key(s) failed");
        assert_eq!(api.calls(), [r#"ha {"disabled":true}"#]);
        // A second enabled key under a later name, added while the first were paused (r5 #1): said,
        // and the command fails.
        let api = Keys::with(fleet_keys()).meanwhile(vec![], vec![(1, ("hc-old", |k| k.disabled = false))]);
        assert_eq!(keys_cmd(&["pause", "alpha", "cloud"], &api).await.unwrap_err(), "1 key(s) failed");
        assert_eq!(api.calls(), [r#"ha {"disabled":true}"#, r#"hc {"disabled":true}"#]);
        assert!(!api.key("hc-old").disabled, "named, not touched: it wasn't in what was confirmed");
        // Resumed meanwhile, between the plan and the change (r6 #1): a pause's "already off" isn't
        // left standing — its machine is planned again and fails.
        let mut keys = fleet_keys();
        keys[1].disabled = true;
        let api = Keys::with(keys).meanwhile(vec![(1, ("hb", |k| k.disabled = false))], vec![]);
        assert_eq!(keys_cmd(&["pause", "alpha", "bravo"], &api).await.unwrap_err(), "1 key(s) failed");
        assert_eq!(api.calls(), [r#"ha {"disabled":true}"#]);
        // A newer key under a later machine's name, made while the earlier ones were resumed (r6 #2):
        // the old one turned on is said, and the command fails.
        let mut keys = fleet_keys();
        keys[0].disabled = true;
        keys[3].disabled = true;
        let newer: Meanwhile = ("hc-old", |k| {
            k.hash = "hc-new".into();
            k.disabled = false;
            k.created_at = Some("2026-10-31T10:00:00Z".into());
        });
        // Made before cloud's keys are read again (a resume reads them right before each machine's):
        // refused there (r7 #1).
        let api = Keys::with(keys.clone()).meanwhile(vec![(2, newer)], vec![]);
        assert_eq!(keys_cmd(&["resume", "alpha", "cloud"], &api).await.unwrap_err(), "1 key(s) failed");
        assert_eq!(api.calls(), [r#"ha {"disabled":false}"#]);
        // Made after that read, before the change: the old key is woken, then found beside the newer
        // one and turned off again (r7 #1).
        let api = Keys::with(keys).meanwhile(vec![(3, newer)], vec![]);
        assert_eq!(keys_cmd(&["resume", "alpha", "cloud"], &api).await.unwrap_err(), "1 key(s) failed");
        assert_eq!(api.calls(), [r#"ha {"disabled":false}"#, r#"hc {"disabled":false}"#, r#"hc {"disabled":true}"#]);
        assert!(api.key("hc").disabled && !api.key("hc-new").disabled);
        // A machine with no key at the plan that has one by the change fails the command too
        // (r7 #2: a keyless row's failure wasn't counted).
        let api = Keys::with(fleet_keys()).meanwhile(vec![(1, ("hx", |k| k.name = Some("devtest-delta".into())))], vec![]);
        assert_eq!(keys_cmd(&["limit", "alpha", "delta", "+5"], &api).await.unwrap_err(), "1 key(s) failed");
        assert_eq!(api.calls(), [r#"ha {"limit":5.1}"#]);
        // Keys that can't be read back: the change was asked, never passed off as checked (r7 #3).
        let api = Keys { failing_lists: vec![3], ..Keys::with(fleet_keys()) };
        let e = keys_cmd(&["pause", "alpha"], &api).await.unwrap_err();
        assert!(e.contains("1 change(s) made but not read back to check"), "{e}");
        assert_eq!(api.calls(), [r#"ha {"disabled":true}"#]);
        // Spent past the new cap meanwhile (r4 #3): a $2 cap over $3 spent is refused at its change.
        let api = Keys::with(fleet_keys()).meanwhile(vec![], vec![(1, ("hc", |k| k.usage = Some(3.0)))]);
        assert_eq!(keys_cmd(&["limit", "alpha", "cloud", "2"], &api).await.unwrap_err(), "1 key(s) failed");
        assert_eq!(api.calls(), [r#"ha {"limit":2.0}"#]);
        assert_eq!(api.key("hc").limit, Some(5.0));
    }

    /// Two key changes on one box take turns: one waits for the other's lock, then goes ahead.
    #[tokio::test]
    async fn key_changes_take_turns_on_the_box() {
        let held = keys_lock(&cfg()).await.unwrap().expect("a state dir in the test config");
        let waiting = tokio::spawn(async move { keys_lock(&cfg()).await.map(|f| f.is_some()) });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(!waiting.is_finished(), "the second waits while the first holds it");
        drop(held);
        assert!(waiting.await.unwrap().unwrap());
    }

    /// What stops a machine's planned change, by the keys read again: a key gone or renamed away,
    /// its cap, reset or on/off moved, a key replaced or added under the name, or (a cap) a spend
    /// grown past the new cap. Pure.
    #[test]
    fn moved_says_why_a_planned_change_waits() {
        let cfg = cfg();
        let naming = Naming::from_config(&cfg);
        let at = "2026-10-30T10:00:00Z";
        let bravo = key("hb", "devtest-bravo", Some(5.0), 1.0, false, at);
        let check = |verb: &Verb, before: &[KeyInfo], after: &[KeyInfo]| {
            let rows = plan(verb, &["devtest-bravo".to_string()], before, &naming).unwrap();
            moved(verb, "devtest-bravo", &rows.iter().collect::<Vec<_>>(), after, &naming)
        };
        let lower = Verb::Limit { change: Some(CapChange::To(2.0)), reset: None, force: false };
        assert_eq!(check(&lower, std::slice::from_ref(&bravo), std::slice::from_ref(&bravo)), None);
        for (now, says) in [
            (vec![], "now no key under its name"),
            (vec![KeyInfo { name: Some("devtest-alpha".into()), ..bravo.clone() }], "now no key under its name"),
            (vec![KeyInfo { limit: Some(25.0), ..bravo.clone() }], "now cap $25.00, on"),
            (vec![KeyInfo { limit: None, ..bravo.clone() }], "now no cap, on"),
            (vec![KeyInfo { limit_reset: Some("daily".into()), ..bravo.clone() }], "now cap $5.00 (daily), on"),
            (vec![KeyInfo { usage: Some(3.0), ..bravo.clone() }], "a $2.00 cap is under what it has spent in all ($3.00)"),
            // A newer key under the name, the old one disabled (a rotation meanwhile).
            (vec![KeyInfo { disabled: true, ..bravo.clone() }, key("hb2", "devtest-bravo", Some(5.0), 0.0, false, "2026-10-31T10:00:00Z")], "now cap $5.00, off; cap $5.00, on"),
        ] {
            let why = check(&lower, std::slice::from_ref(&bravo), &now).unwrap_or_else(|| panic!("{now:?}"));
            assert!(why.contains(says) && why.contains("nothing done"), "{says}: {why}");
        }
        // A resume confirmed for the newest disabled key doesn't wake an older one when a newer
        // one arrived meanwhile (review codex r2 #1)…
        let off = KeyInfo { disabled: true, ..bravo.clone() };
        let newer = KeyInfo { hash: "hb2".into(), created_at: Some("2026-10-31T10:00:00Z".into()), ..off.clone() };
        assert!(check(&Verb::Resume, std::slice::from_ref(&off), &[off.clone(), newer]).is_some());
        // …and a pause confirmed for one enabled key doesn't miss a second one added meanwhile.
        let second = key("hb3", "devtest-bravo", Some(5.0), 0.0, false, "2026-10-31T10:00:00Z");
        assert!(check(&Verb::Pause, std::slice::from_ref(&bravo), &[bravo.clone(), second]).is_some());
        // The spend that grew doesn't stop a pause.
        assert_eq!(check(&Verb::Pause, std::slice::from_ref(&bravo), &[KeyInfo { usage: Some(3.0), ..bravo.clone() }]), None);
    }

    /// The dashboard's `--expect`: a change to a key it didn't show, or one that moved since it
    /// showed it, is refused before anything changes; as shown, it goes ahead.
    #[tokio::test]
    async fn the_dashboards_expectations_bind_the_change_to_the_key_it_showed() {
        let api = Keys::with(fleet_keys());
        let e = keys_cmd(&["limit", "bravo", "10", "--expect", "hb=4/never/on", "--yes"], &api).await.unwrap_err();
        assert!(e.contains("nothing changed") && e.contains("devtest-bravo: its key changed since the dashboard showed it (now cap $5.00, on)"), "{e}");
        let e = keys_cmd(&["pause", "bravo", "--expect", "hother=5/never/on"], &api).await.unwrap_err();
        assert!(e.contains("devtest-bravo: its key isn't the one the dashboard showed"), "{e}");
        assert!(keys_cmd(&["pause", "bravo", "--expect", "hb=5/nope/on"], &api).await.unwrap_err().contains("`hb=5/nope/on` isn't HASH=CAP/RESET/ON|OFF"));
        assert!(api.calls().is_empty());
        keys_cmd(&["limit", "bravo", "10", "--expect", "hb=5/never/on"], &api).await.unwrap();
        assert_eq!(api.calls(), [r#"hb {"limit":10.0}"#]);
        // Two enabled keys under one name: the dashboard showed one, a pause would act on both.
        let mut keys = fleet_keys();
        keys.push(key("ha2", "devtest-alpha", Some(5.0), 0.0, false, "2026-10-29T10:00:00Z"));
        let api = Keys::with(keys);
        let e = keys_cmd(&["pause", "alpha", "--expect", "ha=0.1/never/on"], &api).await.unwrap_err();
        assert!(e.contains("devtest-alpha: it has more than one key and the dashboard showed one — `arena keys list --all`, then `arena keys pause devtest-alpha`"), "{e}");
        assert!(api.calls().is_empty());
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
        let c = full(&["keys", "limit", "devtest-alpha", "devtest-bravo", "+5", "--expect", "ab12=5/monthly/on", "--expect", "cd34=none/never/off", "--yes"]);
        let crate::Cmd::Keys(crate::KeysCmd::Limit(a)) = c.cmd else { panic!() };
        assert!(c.yes && !a.no_cap && !a.force && !a.dry_run && a.reset.is_none());
        assert_eq!(expected(&a.expect).unwrap().iter().map(|e| e.token()).collect::<Vec<_>>(), ["ab12=5/monthly/on", "cd34=none/never/off"]);
        let (targets, change) = split_amount(a.args, a.no_cap).unwrap();
        assert_eq!((targets, change), (vec!["devtest-alpha".to_string(), "devtest-bravo".into()], CapChange::Raise(5.0)));
        for (verb, resume) in [("pause", false), ("resume", true)] {
            let c = full(&["keys", verb, "devtest-alpha", "--expect", "ab12=5/never/off", "--yes"]);
            let (a, is_resume) = match c.cmd {
                crate::Cmd::Keys(crate::KeysCmd::Pause(a)) => (a, false),
                crate::Cmd::Keys(crate::KeysCmd::Resume(a)) => (a, true),
                _ => panic!("{verb}"),
            };
            assert!(c.yes && is_resume == resume && a.sel.targets == ["devtest-alpha"] && !a.dry_run && a.expect == ["ab12=5/never/off"], "{verb}");
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
