//! `pods rename`: rename pods in place, and bring `~/.name`, the OpenRouter key mapping and the
//! proxy along.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use arena_core::capability::Op;
use arena_core::provider::Provider;
use arena_core::remote::{describe_error, Remote, PROBE_TIMEOUT};
use arena_core::selector::Naming;
use arena_core::ssh::SshTarget;
use arena_core::Config;

use crate::keys::{cohort_keys_file, cohort_keys_header, ensure_cohort_keys_file, machine_key_names, write_secret_file, KeyBook};
use crate::proxy::sync_proxy;
use crate::{confirm, reap};

/// The one thing that can't follow a name, for the renamed pods that have it: a RunPod or Vast
/// container's `MACHINE_NAME` env var ([`arena_core::setup::machine_name_in_env`]) — not a
/// Hetzner or Shadeform VM's, which has none (prod 2026-10-30: the note was printed for a
/// Hetzner VM). `None` when no renamed pod has one. A pod missing from `pods` is assumed to
/// have one: the note is only ever extra. Pure.
pub(crate) fn machine_name_note(renamed: &[&PlannedRename], pods: &[arena_core::Pod]) -> Option<String> {
    let with_env: Vec<&str> = renamed
        .iter()
        .filter(|r| pods.iter().find(|p| p.id == r.id).is_none_or(|p| arena_core::setup::machine_name_in_env(&p.provider)))
        .map(|r| r.new.as_str())
        .collect();
    let whose = match with_env.len() {
        0 => return None,
        n if n == renamed.len() => "each renamed pod's MACHINE_NAME env var keeps".to_string(),
        _ => format!("the MACHINE_NAME env var of {} keeps", with_env.join(", ")),
    };
    Some(format!(
        "note: {whose} the old name — env only changes with a reimage (which wipes the disk); ~/.name carries the new one."
    ))
}

/// `pods rename`: validate every (old → new) pair, confirm, rename via the provider's
/// metadata-only rename, then bring along everything keyed by the machine name (PLAN 1.D):
/// `~/.name` on the pod — and the hostname of a VM named by it (Hetzner) — the OpenRouter key
/// mapping, and the proxy (stable ports follow names). A container's `MACHINE_NAME` env var
/// can't follow — env is fixed at create/reimage — so the summary says so where there is one.
/// A pod whose backend can't rename ([`Provider::supports`]) is refused before anything
/// is asked. Nothing after the provider rename is fatal: the pods are renamed either way, so a
/// pod that can't be reached or a key that can't be moved is reported.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_rename(
    provider: &dyn Provider,
    // How `~/.name` is rewritten: `SshRemote` for real, `FakeRemote` in tests.
    remote: Arc<dyn Remote>,
    cfg: &Config,
    book: &KeyBook<'_>,
    request: RenameRequest,
    skip_proxy: bool,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    // Whole-fleet: a new name held by a pod on a provider that didn't answer isn't free.
    let pods = arena_core::provider::list_complete(provider).await.context("listing pods")?;
    let plan = plan_renames(prefix, &cfg.machine_names, &pods, &request)?;
    if plan.is_empty() {
        println!("nothing to rename.");
        return Ok(());
    }
    // Before anything is asked: a batch with a pod its backend can't rename would stop there.
    let unsupported: Vec<String> = plan
        .iter()
        .filter_map(|r| {
            let pod = pods.iter().find(|p| p.id == r.id)?;
            provider.supports(Op::Rename, Some(pod)).refusal().map(|why| format!("{} ({}): {why}", r.old, pod.provider))
        })
        .collect();
    if !unsupported.is_empty() {
        anyhow::bail!("refusing to rename anything:\n  {}", unsupported.join("\n  "));
    }
    let summary: Vec<String> = plan.iter().map(|r| format!("  {} → {} (id={})", r.old, r.new, r.id)).collect();
    let steps = rename_followup_steps(prefix, &plan, &pods, book, skip_proxy);
    let what = format!("rename {} pod(s):\n{}\nthen:\n{}", plan.len(), summary.join("\n"), steps.join("\n"));
    if dry_run {
        println!("[dry-run] would {what}");
        if let Some(note) = machine_name_note(&plan.iter().collect::<Vec<_>>(), &pods) {
            println!("{note}");
        }
        println!("[dry-run] nothing changed.");
        return Ok(());
    }
    if !confirm(yes, &format!("Will {what}"))? {
        println!("aborted.");
        return Ok(());
    }
    let policy = arena_core::retry::RetryPolicy::default();
    let mut done: Vec<&PlannedRename> = Vec::new();
    let mut failure = None;
    for (i, r) in plan.iter().enumerate() {
        if let Err(e) = arena_core::retry::retrying(&policy, || provider.rename_pod(&r.id, &r.new)).await {
            failure = Some(format!(
                "renaming {} → {} failed: {}\n{}",
                r.old,
                r.new,
                arena_core::lock::explain(&r.old, &e),
                rename_failure_hint(&request, i, plan.len())
            ));
            break;
        }
        println!("[renamed] {} → {}", r.old, r.new);
        // The reaper's record (and a pin by this id) follows the new name.
        if let Some(p) = pods.iter().find(|p| p.id == r.id) {
            reap::renamed(cfg, &p.provider, &r.id, &r.new);
        }
        done.push(r);
    }
    // The pods renamed so far get their ~/.name and key mapping even when the batch stopped:
    // a re-run renames only the rest, so these would otherwise keep the old ones for good.
    let missed = rewrite_name_files(remote, cfg, prefix, &done, &pods).await;
    let key_warnings = move_key_mappings(book, &Naming::from_config(cfg), &done, &pods).await;
    let note = machine_name_note(&done, &pods);
    if let Some(failure) = failure {
        // Deliberately no sync on a partial batch: in a `--from-prefix` rename the pods not
        // renamed yet still carry the old prefix, which has no slot in the current list — a
        // sync would drop their forwards. Left alone, every old forward keeps routing (a
        // renamed pod keeps its port: same list index), until the re-run.
        for line in rename_report(note.as_deref(), &missed, &key_warnings) {
            eprintln!("{line}");
        }
        anyhow::bail!("{failure}");
    }
    if skip_proxy {
        println!("(proxy not synced — run `arena proxy apply`)");
    } else {
        sync_proxy(cfg, provider, "rename").await;
    }
    for line in rename_report(note.as_deref(), &missed, &key_warnings) {
        println!("{line}");
    }
    Ok(())
}

/// The follow-up steps `rename` takes after the provider renames, for its dry run and
/// prompt: whose `~/.name` is rewritten (and who has no endpoint to reach), what moves in
/// the keys CSV and on OpenRouter, and the proxy sync. Reads the local CSV (never printing
/// a key) but makes no API call.
pub(crate) fn rename_followup_steps(
    prefix: &str,
    plan: &[PlannedRename],
    pods: &[arena_core::Pod],
    book: &KeyBook<'_>,
    skip_proxy: bool,
) -> Vec<String> {
    use arena_core::setup::short_name;
    let mut out = Vec::new();
    let shorts: Vec<String> = plan.iter().map(|r| format!("{} ← '{}'", r.new, short_name(prefix, &r.new))).collect();
    out.push(format!("  1. rewrite ~/.name (export MACHINE_NAME='<short>') over SSH: {}", shorts.join(", ")));
    let by_hostname: Vec<&str> = plan
        .iter()
        .filter(|r| pods.iter().any(|p| p.id == r.id && arena_core::setup::hostname_is_machine_name(&p.provider)))
        .map(|r| r.new.as_str())
        .collect();
    if !by_hostname.is_empty() {
        out.push(format!("     and set the hostname (a Hetzner VM is named by it; kept across a reboot): {}", by_hostname.join(", ")));
    }
    let no_endpoint: Vec<&str> = plan
        .iter()
        .filter(|r| !pods.iter().any(|p| p.id == r.id && p.ssh_ip.is_some() && p.ssh_port.is_some()))
        .map(|r| r.new.as_str())
        .collect();
    if !no_endpoint.is_empty() {
        out.push(format!(
            "     no SSH endpoint right now (will be reported — re-run `pods setup` later): {}",
            no_endpoint.join(", ")
        ));
    }
    let csv = book.csv.display();
    match book.read_csv() {
        None => out.push(format!("  2. OpenRouter key mapping: no {csv} — nothing to move locally")),
        Some(text) => {
            let pairs: Vec<(&str, &str)> = plan.iter().map(|r| (r.old.as_str(), r.new.as_str())).collect();
            let done: Vec<&PlannedRename> = plan.iter().collect();
            let cross = cross_cohort_dst(book, prefix);
            let mv = plan_csv_move(&text, cross.as_ref().map(|(_, t)| t.as_str()), &pairs, &names_after(pods, &done));
            out.push(match (&cross, mv.moved.is_empty()) {
                (_, true) => format!("  2. OpenRouter key mapping: no rows for these machines in {csv}"),
                (None, false) => format!("  2. OpenRouter key mapping: move rows in {csv}: {}", mv.moved.join(", ")),
                (Some((dst, _)), false) => format!(
                    "  2. OpenRouter key mapping: move rows out of the previous cohort's file into {} and point {csv} \
                     at it: {}{}",
                    dst.display(),
                    mv.moved.join(", "),
                    if mv.carried.is_empty() { String::new() } else { format!(" (carrying along: {})", mv.carried.join(", ")) }
                ),
            });
            if !mv.taken.is_empty() {
                out.push(format!("     ⚠ already have their own row (left alone): {}", mv.taken.join(", ")));
            }
        }
    }
    out.push(match book.api {
        Some(_) => "     and rename each machine's key on OpenRouter to the new name (if it has one)".into(),
        None => "     (no OPENROUTER_PROVISIONING_KEY: key names on OpenRouter are left as they are)".into(),
    });
    out.push(if skip_proxy { "  3. proxy NOT synced (--skip-proxy)".into() } else { "  3. redeploy the proxy".into() });
    out
}

/// What a rename's SSH follow-ups couldn't do, each `(new name, why)`.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct NameMissed {
    /// Pods whose `~/.name` still has the old name.
    pub name_file: Vec<(String, String)>,
    /// VMs named by their hostname whose hostname is still the old name.
    pub hostname: Vec<(String, String)>,
}

/// Rewrite `~/.name` on each renamed pod to its new short name — exactly the line setup
/// writes ([`arena_core::setup::name_file_command`]) — and, on a VM named by its hostname
/// (Hetzner: [`arena_core::setup::hostname_is_machine_name`]), set that to the new name too
/// ([`arena_core::setup::hostname_command`]): its prompt, login banner and `~/.name` fallback
/// come from it. Concurrently across pods, each command within `PROBE_TIMEOUT`. The endpoint
/// comes from the pre-rename listing (a metadata rename doesn't move it). Never fails the
/// rename: what it couldn't do comes back for the summary.
pub(crate) async fn rewrite_name_files(
    remote: Arc<dyn Remote>,
    cfg: &Config,
    prefix: &str,
    done: &[&PlannedRename],
    pods: &[arena_core::Pod],
) -> NameMissed {
    use arena_core::setup::{hostname_command, hostname_is_machine_name, name_file_command, short_name};
    /// One command's outcome: `Ok` = it ran and succeeded, else why not.
    fn outcome(res: arena_core::Result<arena_core::ssh::SshOutput>) -> std::result::Result<(), String> {
        match res {
            Ok(o) if o.success => Ok(()),
            Ok(o) => Err(format!("exit {}: {}", o.code.unwrap_or(-1), o.stderr.trim())),
            Err(e) => Err(describe_error(&e)),
        }
    }
    fn report(missed: &mut Vec<(String, String)>, what: &str, name: String, res: std::result::Result<(), String>) {
        match res {
            Ok(()) => println!("[{what}] ✓ {name}"),
            Err(why) => {
                eprintln!("[{what}] ✗ {name}: {why}");
                missed.push((name, why));
            }
        }
    }
    let mut missed = NameMissed::default();
    let mut set = tokio::task::JoinSet::new();
    for r in done {
        let pod = pods.iter().find(|p| p.id == r.id);
        let by_hostname = pod.is_some_and(|p| hostname_is_machine_name(&p.provider));
        let Some(target) = pod.and_then(|p| SshTarget::from_pod(p, cfg).ok()) else {
            report(&mut missed.name_file, "~/.name", r.new.clone(), Err("no SSH endpoint".into()));
            if by_hostname {
                report(&mut missed.hostname, "hostname", r.new.clone(), Err("no SSH endpoint".into()));
            }
            continue;
        };
        let cmd = name_file_command(short_name(prefix, &r.new));
        // `None` when the name isn't one: said, never written into /etc.
        let host_cmd = by_hostname.then(|| hostname_command(&r.new));
        let (remote, name) = (remote.clone(), r.new.clone());
        set.spawn(async move {
            let name_file = outcome(remote.exec(&target, &cmd, Some(PROBE_TIMEOUT)).await);
            let hostname = match host_cmd {
                None => None,
                Some(None) => Some(Err(format!("`{name}` isn't a plain hostname — left as it was"))),
                Some(Some(c)) => Some(outcome(remote.exec(&target, &c, Some(PROBE_TIMEOUT)).await)),
            };
            (name, name_file, hostname)
        });
    }
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((name, name_file, hostname)) => {
                report(&mut missed.name_file, "~/.name", name.clone(), name_file);
                if let Some(h) = hostname {
                    report(&mut missed.hostname, "hostname", name, h);
                }
            }
            Err(e) => report(&mut missed.name_file, "~/.name", "?".into(), Err(format!("task crashed: {e}"))),
        }
    }
    missed
}

/// The keys CSV's side of moving key mappings after a rename (pure, so table-tested; the
/// dry run and the real move share it).
#[derive(Debug, Default, PartialEq)]
pub(crate) struct CsvMove {
    /// The new text of the file the rows end up in; `None`: nothing to write.
    pub dst: Option<String>,
    /// Cross-cohort only: the old cohort's file with the moved rows taken out.
    pub src: Option<String>,
    /// `old → new`, per moved row.
    pub moved: Vec<String>,
    /// New names that already have a row of their own: left alone (and, by the caller,
    /// their OpenRouter key too — the two sides never disagree).
    pub taken: Vec<String>,
    /// Cross-cohort only: other current pods whose rows were copied into the new file, so
    /// `copy-keys` (which reads through the link) still finds them.
    pub carried: Vec<String>,
}

/// Plan the CSV half of a rename's key move. `src`: the file the `openrouter_api_keys.csv`
/// link points at now. `dst`: `None` when that is already the current cohort's file — rows
/// are renamed in place; else `Some(text)` of the current cohort's file (the seeded header
/// when it doesn't exist yet): the link points at an *older* cohort's file (a
/// `--from-prefix` rename after a prefix change), and the renamed rows move INTO the current
/// one. Rewriting the old file instead would strand them: the next `keys gen` repoints the
/// link to the current cohort's file and skips these machines (their keys already exist
/// under the new names), so they'd never be delivered again. `live`: every current pod's
/// name after the renames — their rows are carried along so the repointed link loses none.
pub(crate) fn plan_csv_move(src: &str, dst: Option<&str>, pairs: &[(&str, &str)], live: &[String]) -> CsvMove {
    use arena_core::apikeys::{parse_csv, remove_csv_host, rename_csv_host, upsert_csv, CsvRename};
    let mut out = CsvMove::default();
    let Some(dst) = dst else {
        let mut text = src.to_string();
        for (old, new) in pairs {
            match rename_csv_host(&text, old, new) {
                CsvRename::Renamed(t) => {
                    text = t;
                    out.moved.push(format!("{old} → {new}"));
                }
                CsvRename::Taken => out.taken.push(new.to_string()),
                CsvRename::NoRow => {}
            }
        }
        out.dst = (!out.moved.is_empty()).then_some(text);
        return out;
    };
    // The last row wins, as when copy-keys exports them in order.
    let key_of = |text: &str, host: &str| parse_csv(text).into_iter().rev().find(|(h, _)| h == host).map(|(_, k)| k);
    let (mut d, mut s) = (dst.to_string(), src.to_string());
    let mut new_names = Vec::new();
    for (old, new) in pairs {
        let Some(secret) = key_of(&s, old) else { continue };
        if old != new && (key_of(&d, new).is_some() || key_of(&s, new).is_some()) {
            out.taken.push(new.to_string());
            continue;
        }
        d = upsert_csv(&d, new, &secret);
        s = remove_csv_host(&s, old).unwrap_or(s);
        out.moved.push(format!("{old} → {new}"));
        new_names.push(new.to_string());
    }
    if out.moved.is_empty() {
        return out; // nothing of ours in the old file: leave both files and the link alone
    }
    for host in live.iter().filter(|h| !new_names.contains(h)) {
        if let (Some(secret), None) = (key_of(&s, host), key_of(&d, host)) {
            d = upsert_csv(&d, host, &secret);
            out.carried.push(host.clone());
        }
    }
    out.dst = Some(d);
    out.src = Some(s);
    out
}

/// Where a rename's key rows go: `(the current cohort's file, its text or the seeded header)`
/// when the keys link points at another cohort's file (see [`plan_csv_move`]), else `None`
/// (rows move in place). A legacy regular file stays in place, as before.
fn cross_cohort_dst(book: &KeyBook<'_>, prefix: &str) -> Option<(PathBuf, String)> {
    let linked = std::fs::read_link(&book.csv).ok()?;
    let file = cohort_keys_file(prefix);
    if linked.file_name() == Some(std::ffi::OsStr::new(&file)) {
        return None;
    }
    let path = book.csv.with_file_name(&file);
    let text = std::fs::read_to_string(&path).ok().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| cohort_keys_header(prefix));
    Some((path, text))
}

/// Every current pod's name once the renames in `done` are applied (from the pre-rename
/// listing) — whose rows a cross-cohort move carries along.
fn names_after(pods: &[arena_core::Pod], done: &[&PlannedRename]) -> Vec<String> {
    pods.iter().map(|p| done.iter().find(|r| r.id == p.id).map_or_else(|| p.name.clone(), |r| r.new.clone())).collect()
}

/// Move each renamed machine's OpenRouter key mapping to its new name: its row(s) in the
/// keys CSV (into the current cohort's file — see [`plan_csv_move`]) and its key's `name`
/// on OpenRouter (with a provisioning key; found by [`machine_key_names`], as `keys gen`
/// named it), so `copy-keys`, `keys rotate|revoke` and `terminate --revoke-key` — which all
/// go by machine name — keep finding it. Quiet when there's nothing to move (no CSV or no
/// row; no provisioning key or no key under the old name). A new name that already holds
/// its own row or key is left alone and warned about — silently merging two machines' keys
/// is worse than a manual fix — and then neither side moves, so the CSV and OpenRouter stay
/// consistent. Never fails the rename: problems come back as warning lines.
async fn move_key_mappings(
    book: &KeyBook<'_>,
    naming: &Naming<'_>,
    done: &[&PlannedRename],
    pods: &[arena_core::Pod],
) -> Vec<String> {
    let mut warnings = Vec::new();
    if done.is_empty() {
        return warnings;
    }
    let keys = match book.api {
        None => None,
        Some(api) => match api.list_keys().await {
            Ok(k) => Some(k),
            Err(e) => {
                let pairs: Vec<String> = done.iter().map(|r| format!("{} → {}", r.old, r.new)).collect();
                warnings.push(format!(
                    "couldn't list OpenRouter keys ({e}) — key names not moved: {} (rename them on openrouter.ai)",
                    pairs.join(", ")
                ));
                None
            }
        },
    };
    let named = |names: &[String]| -> Vec<&arena_core::openrouter::KeyInfo> {
        keys.iter().flatten().filter(|k| k.name.as_deref().is_some_and(|n| names.iter().any(|w| w == n))).collect()
    };
    // Decide per machine, so a conflict on either side moves neither: OpenRouter first...
    let mut pairs: Vec<(&str, &str)> = Vec::new();
    for r in done {
        let mine = named(&machine_key_names(naming, &r.old));
        if !mine.is_empty() && !named(&machine_key_names(naming, &r.new)).is_empty() {
            warnings.push(format!(
                "{} already has its own OpenRouter key on OpenRouter — left {}'s key mapping as it was; keep one \
                 (`arena keys revoke`/`rotate`) and fix the other by hand",
                r.new, r.old
            ));
            continue;
        }
        pairs.push((r.old.as_str(), r.new.as_str()));
    }
    // ...then the CSV.
    let csv_text = book.read_csv();
    let cross = csv_text.as_ref().and_then(|_| cross_cohort_dst(book, naming.prefix));
    let plan = match &csv_text {
        Some(src) => plan_csv_move(src, cross.as_ref().map(|(_, t)| t.as_str()), &pairs, &names_after(pods, done)),
        None => CsvMove::default(),
    };
    for new in &plan.taken {
        let old = pairs.iter().find(|(_, n)| n == new).map_or("", |(o, _)| *o);
        warnings.push(format!(
            "{new} already has its own OpenRouter key row in the CSV — left {old}'s key mapping as it was; keep one \
             (`arena keys revoke`/`rotate`) and fix the other by hand"
        ));
    }
    if let Some(e) = write_csv_move(book, naming.prefix, cross.as_ref().map(|(p, _)| p.as_path()), &plan) {
        warnings.push(e);
    }
    if let Some(api) = book.api {
        for (old, new) in pairs.iter().filter(|(_, n)| !plan.taken.iter().any(|t| t == n)) {
            for k in named(&machine_key_names(naming, old)) {
                match api.rename_key(&k.hash, new).await {
                    Ok(()) => println!("[keys] OpenRouter key {} → {new}", k.name.as_deref().unwrap_or(old)),
                    Err(e) => warnings.push(format!("renaming the OpenRouter key {old} → {new} failed: {e}")),
                }
            }
        }
    }
    warnings
}

/// Write a [`CsvMove`]: in place through the link, or — cross-cohort (`dst` = the current
/// cohort's file) — the new file first (secrets in two places beats in none), then the link
/// repointed at it, then the moved rows taken out of the old file. Each file keeps (a new
/// one inherits) the old file's mode: they hold secrets. A failure stops there, and comes
/// back as the warning saying what state it left.
fn write_csv_move(book: &KeyBook<'_>, prefix: &str, dst: Option<&std::path::Path>, plan: &CsvMove) -> Option<String> {
    let Some(text) = &plan.dst else { return None };
    let moved = plan.moved.join(", ");
    let Some(dst) = dst else {
        return match book.write_csv(text) {
            Ok(()) => {
                println!("[keys] {}: {moved}", book.csv.display());
                None
            }
            Err(e) => Some(format!("couldn't update {} ({e}) — its rows still use the old names: {moved}", book.csv.display())),
        };
    };
    // Resolved before the link moves: afterwards the link leads to the new file.
    let old_real = std::fs::canonicalize(&book.csv).unwrap_or_else(|_| book.csv.clone());
    if let Err(e) = write_secret_file(dst, text) {
        return Some(format!("couldn't write {} ({e}) — key rows NOT moved (still under the old names in {}): {moved}", dst.display(), old_real.display()));
    }
    let dir = book.csv.parent().unwrap_or(std::path::Path::new("."));
    if let Err(e) = ensure_cohort_keys_file(dir, prefix) {
        return Some(format!(
            "copied the key rows into {} but couldn't point {} at it ({e:#}) — copy-keys still reads {}; fix the link by hand",
            dst.display(),
            book.csv.display(),
            old_real.display()
        ));
    }
    let carried = if plan.carried.is_empty() { String::new() } else { format!(" (carried along: {})", plan.carried.join(", ")) };
    println!("[keys] moved into {} (this cohort's keys file): {moved}{carried}", dst.display());
    if let Some(src) = &plan.src {
        if let Err(e) = write_secret_file(&old_real, src) {
            return Some(format!(
                "moved the key rows into {}, but couldn't take them out of {} ({e}) — harmless (copy-keys reads the new file), tidy it by hand",
                dst.display(),
                old_real.display()
            ));
        }
    }
    None
}

/// The closing lines of a rename (pure, so tested): the MACHINE_NAME note where it applies
/// ([`machine_name_note`]), the pods whose `~/.name` or hostname still has the old name (with
/// the command that fixes them), and any key-mapping warnings.
pub(crate) fn rename_report(note: Option<&str>, missed: &NameMissed, key_warnings: &[String]) -> Vec<String> {
    let mut out: Vec<String> = note.map(String::from).into_iter().collect();
    for (what, missed) in [("~/.name still has", &missed.name_file), ("the hostname is still", &missed.hostname)] {
        if missed.is_empty() {
            continue;
        }
        let names: Vec<&str> = missed.iter().map(|(n, _)| n.as_str()).collect();
        let why: Vec<String> = missed.iter().map(|(n, w)| format!("{n}: {w}")).collect();
        out.push(format!(
            "⚠ {what} the old name on {} ({}) — re-run `arena pods setup {}` once it answers",
            names.join(", "),
            why.join("; "),
            names.join(" ")
        ));
    }
    out.extend(key_warnings.iter().map(|w| format!("⚠ keys: {w}")));
    out
}

pub(crate) enum RenameRequest {
    One { old: String, new: String },
    FromPrefix(String),
}

/// What to tell the operator when a rename batch stops part-way (pure, so tested). A
/// `--from-prefix` batch must NOT suggest `proxy apply`: the pods not renamed yet still
/// carry the old prefix, which has no slot in the current MACHINE_NAME_LIST, so any proxy
/// write — `proxy apply`, another lifecycle command's sync, the `cron install --proxy`
/// tick — drops their forwards. Re-running the rename finishes the batch, then syncs.
fn rename_failure_hint(request: &RenameRequest, done: usize, total: usize) -> String {
    match request {
        RenameRequest::One { .. } => {
            format!("{done} of {total} rename(s) done; proxy NOT synced (fix and re-run the rename, or `arena proxy apply`)")
        }
        RenameRequest::FromPrefix(from) => format!(
            "{done} of {total} rename(s) done; proxy NOT synced — every forward still routes. Fix the \
             cause and re-run the rename to finish the batch (it syncs the proxy at the end). Until \
             then do NOT run `arena proxy apply` or another create/terminate/replace: the pods still \
             named {from}-* have no slot in MACHINE_NAME_LIST, so any proxy sync drops their \
             forwards — including the `cron install --proxy` tick (re-install the cron without \
             --proxy to pause it)."
        ),
    }
}

#[derive(Debug, PartialEq)]
pub(crate) struct PlannedRename {
    pub id: String,
    pub old: String,
    pub new: String,
}

/// Pure planner for `pods rename`. Every new name must be a MACHINE_NAME_LIST entry (so it
/// has a stable proxy port) and not held by another pod or another rename in the batch.
/// All problems are collected and reported together; any problem → no plan.
fn plan_renames(
    prefix: &str,
    candidates: &[String],
    pods: &[arena_core::Pod],
    request: &RenameRequest,
) -> Result<Vec<PlannedRename>> {
    use std::collections::HashSet;
    let pairs: Vec<(&arena_core::Pod, String)> = match request {
        RenameRequest::One { old, new } => {
            // The shared single-pod matcher: a typo names the closest pods, and a name two
            // pods share is refused (renaming whichever listed first could be the wrong one).
            let naming = Naming { prefix, list: candidates };
            let pod = &pods[arena_core::selector::resolve_one(&naming, pods, old)?];
            vec![(pod, arena_core::naming::canonical_name(prefix, candidates, new))]
        }
        RenameRequest::FromPrefix(from) => {
            let pre = format!("{from}-");
            pods.iter()
                .filter_map(|p| p.name.strip_prefix(&pre).map(|rest| (p, format!("{prefix}-{rest}"))))
                .filter(|(p, new)| p.name != *new)
                .collect()
        }
    };
    let valid: HashSet<String> = candidates.iter().map(|c| arena_core::naming::qualify(prefix, c)).collect();
    let renamed: HashSet<&str> = pairs.iter().map(|(p, _)| p.id.as_str()).collect();
    // Names still held after the batch: pods not being renamed keep theirs.
    let held: HashSet<&str> =
        pods.iter().filter(|p| !renamed.contains(p.id.as_str())).map(|p| p.name.as_str()).collect();
    let mut seen = HashSet::new();
    let mut problems = Vec::new();
    for (pod, new) in &pairs {
        if pod.name == *new {
            problems.push(format!("{} is already named {new}", pod.name));
        } else if !valid.contains(new) {
            problems.push(format!("{} → {new}: not in MACHINE_NAME_LIST (no stable proxy port)", pod.name));
        } else if held.contains(new.as_str()) {
            problems.push(format!("{} → {new}: name already taken by another pod", pod.name));
        } else if !seen.insert(new.clone()) {
            problems.push(format!("{} → {new}: two pods would get this name", pod.name));
        }
    }
    if !problems.is_empty() {
        anyhow::bail!("refusing to rename anything:\n  {}", problems.join("\n  "));
    }
    Ok(pairs
        .into_iter()
        .map(|(p, new)| PlannedRename { id: p.id.clone(), old: p.name.clone(), new })
        .collect())
}

/// Derive the staging + parked names for a blue-green replace of `canonical`. The
/// replacement is built under `<canonical>-new`; at the swap the source is parked at
/// `<canonical>-old` and the replacement is renamed onto `canonical` itself — so the
/// proxy port / ssh-config identity (which key off the canonical name) carry straight
/// over. Pure so the naming contract is unit-tested.
pub(crate) fn replace_stage_names(canonical: &str) -> (String, String) {
    (format!("{canonical}-new"), format!("{canonical}-old"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn rename_failure_hint_never_suggests_proxy_apply_mid_prefix_rename() {
        use super::{rename_failure_hint, RenameRequest};
        let prefix = rename_failure_hint(&RenameRequest::FromPrefix("arena7".into()), 1, 2);
        assert!(prefix.starts_with("1 of 2 rename(s) done"), "{prefix}");
        assert!(prefix.contains("re-run the rename") && prefix.contains("arena7-*"), "{prefix}");
        assert!(prefix.contains("do NOT run `arena proxy apply`") && prefix.contains("cron"), "{prefix}");
        // A single rename that failed changed nothing, so `proxy apply` is harmless there.
        let one = rename_failure_hint(&RenameRequest::One { old: "a".into(), new: "b".into() }, 0, 1);
        assert!(one.contains("or `arena proxy apply`"), "{one}");
    }

    #[test]
    fn replace_stage_names_suffix_canonical() {
        use super::replace_stage_names;
        let (new, old) = replace_stage_names("arena8-apple");
        assert_eq!(new, "arena8-apple-new");
        assert_eq!(old, "arena8-apple-old");
    }

    fn rename_pod_fixture(id: &str, name: &str) -> arena_core::Pod {
        arena_core::Pod {
            id: id.into(),
            name: name.into(),
            provider: "runpod".into(),
            status: "RUNNING".into(),
            gpu_type: None,
            cost_per_hr: None,
            ssh_ip: None,
            ssh_port: None,
            ..Default::default()
        }
    }

    fn rename_list() -> Vec<String> {
        ["apple", "bloom", "cloud", "@james-gpu"].iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn rename_from_prefix_moves_whole_cohort() {
        use super::{plan_renames, RenameRequest};
        let pods = vec![
            rename_pod_fixture("a", "arena8-apple"),
            rename_pod_fixture("b", "arena8-bloom"),
            rename_pod_fixture("j", "james-gpu"),
        ];
        let plan = plan_renames("arena9", &rename_list(), &pods, &RenameRequest::FromPrefix("arena8".into())).unwrap();
        let got: Vec<(&str, &str)> = plan.iter().map(|r| (r.old.as_str(), r.new.as_str())).collect();
        assert_eq!(got, vec![("arena8-apple", "arena9-apple"), ("arena8-bloom", "arena9-bloom")]);
    }

    #[test]
    fn rename_from_prefix_refuses_all_if_any_off_list() {
        use super::{plan_renames, RenameRequest};
        let pods = vec![rename_pod_fixture("a", "arena8-apple"), rename_pod_fixture("z", "arena8-zebra")];
        let err = plan_renames("arena9", &rename_list(), &pods, &RenameRequest::FromPrefix("arena8".into())).unwrap_err();
        assert!(err.to_string().contains("arena9-zebra: not in MACHINE_NAME_LIST"), "{err}");
    }

    #[test]
    fn rename_one_bare_new_name_and_taken_check() {
        use super::{plan_renames, RenameRequest};
        let pods = vec![rename_pod_fixture("a", "arena9-apple"), rename_pod_fixture("b", "arena9-bloom")];
        let one = |old: &str, new: &str| RenameRequest::One { old: old.into(), new: new.into() };
        let plan = plan_renames("arena9", &rename_list(), &pods, &one("apple", "cloud")).unwrap();
        assert_eq!((plan[0].id.as_str(), plan[0].new.as_str()), ("a", "arena9-cloud"));
        let err = plan_renames("arena9", &rename_list(), &pods, &one("apple", "bloom")).unwrap_err();
        assert!(err.to_string().contains("already taken"), "{err}");
        // Absolute list entries resolve without the prefix.
        let plan = plan_renames("arena9", &rename_list(), &pods, &one("b", "james-gpu")).unwrap();
        assert_eq!(plan[0].new, "james-gpu");
    }
}
