//! The keys CSV checked against OpenRouter's listing before a copy puts its keys on a machine
//! (`up`'s keys stage, `copy-keys` and everything that runs it: `setup`, `keys gen --copy`,
//! `replace`), and `pods up`'s own key for a name that has none.
//!
//! **Why a check.** The CSV is arena's copy of each machine's secret (OpenRouter shows a secret
//! once). A key deleted elsewhere — on openrouter.ai, from another control box, by a rotate whose
//! CSV was another folder's — leaves its row behind, and a copy handed that dead key on: the
//! machine's AI calls failed with 401 and nothing said why (the rehearsal, 2026-10-31). Now a row
//! whose name has no key on OpenRouter, or whose secret isn't any of that name's keys (as far as
//! the listing's redacted label can say), isn't copied, and the copy says what to run. A listing
//! that can't be read changes nothing: the CSV is copied as before, and the copy says so.
//!
//! **Why `up` mints.** `pods up apple --owner alice` was the organiser's one command for a
//! machine, but a name with no key got none until `keys gen apple --copy`. With
//! `OPENROUTER_PROVISIONING_KEY` set, `up` mints one for each cohort name that has no key on
//! OpenRouter (capped at `OPENROUTER_KEY_LIMIT`, as `keys gen`), after the prompt and before any
//! create, so the keys stage copies it on. A name whose key belongs to someone else is a
//! handover (rotated, `crate::spend::hand_over`), never a second key; a staff `@` box is left
//! alone (its key, if any, is the organisers' choice). A mint that fails never stops the `up`.
//!
//! The decisions are pure ([`stale`], [`plan_mints`], [`check_csv_keys`]); the listing and the
//! mint are the I/O.

use std::time::Duration;

use arena_core::openrouter::{exact_money, key_name, KeyApi, KeyInfo};
use arena_core::selector::Naming;

use super::{lock_keys_dir, machine_key_names, write_openrouter_key_held, KeySources, OPENROUTER_KEYS_LINK};
use crate::spend::SpendCtx;

/// The variable a machine's OpenRouter key is exported as (`apikeys::PROVIDERS`).
pub(crate) const OPENROUTER_ENV: &str = "OPENROUTER_API_KEY";

/// How long the check's listing may take before the copy goes ahead unchecked.
const LIST_TIMEOUT: Duration = Duration::from_secs(20);

/// What OpenRouter's redacted `label` (`sk-or-v1-0e6...1c96`: the secret's start and end) says
/// of `secret`: `Some(true)` it may be that key, `Some(false)` it isn't, `None` the label can't
/// tell (absent, or not of that form — a check never guesses a live key dead). Pure.
pub(crate) fn label_says(label: Option<&str>, secret: &str) -> Option<bool> {
    let (head, tail) = label?.split_once("...")?;
    if head.len() < 4 || tail.is_empty() {
        return None;
    }
    Some(secret.starts_with(head) && secret.ends_with(tail))
}

/// Why a CSV row's key isn't a live one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stale {
    /// No key on OpenRouter goes by the machine's name, and none's label is the row's secret.
    Gone,
    /// Keys go by its name, but the row's secret is none of them (each label says so): it was
    /// rotated or re-minted elsewhere.
    Replaced,
}

/// Whether the keys CSV's `secret` for machine `host` is a key `listed` (OpenRouter's whole
/// listing, disabled keys included: a paused key still exists) still has. `None`: it is, or the
/// labels can't tell. A key renamed on OpenRouter (a name is only a label there) is still found by
/// its label. Pure.
pub(crate) fn stale(naming: &Naming, host: &str, secret: &str, listed: &[KeyInfo]) -> Option<Stale> {
    // Listed under any name (renamed on OpenRouter, or another name's key now): live.
    if listed_by_label(secret, listed) {
        return None;
    }
    let theirs = named_for(naming, host, listed);
    if theirs.is_empty() {
        return Some(Stale::Gone);
    }
    theirs.iter().all(|k| label_says(k.label.as_deref(), secret) == Some(false)).then_some(Stale::Replaced)
}

/// Whether some listed key's label says it is `secret` — whatever it's named now.
fn listed_by_label(secret: &str, listed: &[KeyInfo]) -> bool {
    listed.iter().any(|k| label_says(k.label.as_deref(), secret) == Some(true))
}

/// The listed keys that go by machine `host`'s name (as `keys gen` names it, or the pod's own).
fn named_for<'k>(naming: &Naming, host: &str, listed: &'k [KeyInfo]) -> Vec<&'k KeyInfo> {
    let names = machine_key_names(naming, host);
    listed.iter().filter(|k| k.name.as_deref().is_some_and(|n| names.iter().any(|w| w == n))).collect()
}

/// Whether machine `host` has a key on OpenRouter: one that goes by its name, or — renamed there —
/// the one its keys CSV row (`secret`) holds, by its label. What `up` decides a mint by, before the
/// prompt and again under the keys folder's lock. Pure.
pub(crate) fn has_key(naming: &Naming, host: &str, secret: Option<&str>, listed: &[KeyInfo]) -> bool {
    !named_for(naming, host, listed).is_empty() || secret.is_some_and(|s| listed_by_label(s, listed))
}

/// A machine's name without the fleet prefix, as commands take it (`apple`).
fn short<'a>(naming: &Naming, host: &'a str) -> &'a str {
    host.strip_prefix(naming.prefix).and_then(|s| s.strip_prefix('-')).filter(|s| !s.is_empty()).unwrap_or(host)
}

/// OpenRouter's listing for a check, read within [`LIST_TIMEOUT`]: `None` without a
/// provisioning key (nothing to check against), `Some(Err)` when it couldn't be read.
pub(crate) async fn list_for_check(api: Option<&dyn KeyApi>) -> Option<Result<Vec<KeyInfo>, String>> {
    let api = api?;
    Some(match arena_core::ratelimit::bounded(LIST_TIMEOUT, api.list_keys()).await {
        Ok(Ok(k)) => Ok(k),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!("no answer within {}s", LIST_TIMEOUT.as_secs())),
    })
}

/// Before a copy: leave out of `keys` each of `hosts`' OpenRouter rows that [`stale`] says is
/// dead, and say so — one line per row, with what to run. `listed` `None` (no provisioning key):
/// nothing checked, nothing said; `Some(Err)`: copied as it is, said once (only when there's a
/// row to copy). Pure.
pub(crate) fn check_csv_keys(keys: &mut KeySources, hosts: &[String], listed: Option<&Result<Vec<KeyInfo>, String>>, naming: &Naming) -> Vec<String> {
    let rows: Vec<(String, String)> =
        hosts.iter().filter_map(|h| keys.openrouter_secret(h).map(|s| (h.clone(), s.to_string()))).collect();
    let listed = match listed {
        None => return Vec::new(),
        Some(_) if rows.is_empty() => return Vec::new(),
        Some(Err(e)) => {
            return vec![format!(
                "⚠ OpenRouter's keys couldn't be listed ({e}): the keys CSV's OpenRouter keys are copied as they are, unchecked"
            )]
        }
        Some(Ok(k)) => k,
    };
    let mut out = Vec::new();
    for (host, secret) in rows {
        let Some(why) = stale(naming, &host, &secret, listed) else { continue };
        keys.leave_out_openrouter(&host);
        let s = short(naming, &host);
        out.push(match why {
            Stale::Gone => format!(
                "⚠ {host}: the keys CSV's OpenRouter key is gone from OpenRouter (revoked, or deleted elsewhere) — not \
                 copied. `arena keys gen {s} --copy` mints a fresh one"
            ),
            Stale::Replaced => format!(
                "⚠ {host}: the keys CSV's OpenRouter key isn't the one OpenRouter has for it (rotated elsewhere) — not \
                 copied. `arena keys rotate {s} --copy` gives it a fresh one this folder holds"
            ),
        });
    }
    out
}

/// What `pods up` will do about OpenRouter keys ([`plan_mints`]).
#[derive(Debug, Default)]
pub(crate) struct MintPlan {
    /// The names it mints a key for (none on OpenRouter, a cohort name, no handover).
    pub mint: Vec<String>,
    /// Their cap (`OPENROUTER_KEY_LIMIT`, else $5 — as `keys gen`).
    pub limit: f64,
    /// What to say before the prompt (or in the dry run).
    pub lines: Vec<String>,
    /// The listing it was planned from, for the copy's check ([`check_csv_keys`]).
    pub listed: Option<Result<Vec<KeyInfo>, String>>,
}

/// `pods up`'s keys for `names`: each cohort name with no key on OpenRouter gets one minted
/// (`handing`: names a handover gives a fresh key anyway), a staff box is only told about. A name
/// whose key in the keys CSV (`csv`) OpenRouter still has under another name (renamed there) has a
/// key. No listing (no provisioning key): nothing. A listing that failed: nothing minted, said.
/// Pure.
pub(crate) fn plan_mints(
    naming: &Naming,
    names: &[String],
    handing: &[String],
    csv: Option<&KeySources>,
    listed: Option<Result<Vec<KeyInfo>, String>>,
    limit: f64,
    dry_run: bool,
) -> MintPlan {
    let mut plan = MintPlan { limit, ..MintPlan::default() };
    match &listed {
        None => {}
        Some(Err(e)) => plan.lines.push(format!(
            "⚠ OpenRouter's keys couldn't be listed ({e}): no key is minted for a name that has none — \
             `arena keys gen <name> --copy` once it answers"
        )),
        Some(Ok(keys)) => {
            let has_key = |n: &str| has_key(naming, n, csv.and_then(|c| c.openrouter_secret(n)), keys);
            for n in names.iter().filter(|n| !handing.contains(n) && !has_key(n)) {
                if naming.is_cohort(n) {
                    plan.mint.push(n.clone());
                } else {
                    plan.lines.push(format!(
                        "{n}: no OpenRouter key, and none is minted for a staff box — `arena keys gen {} --copy` if it should have one",
                        short(naming, n)
                    ));
                }
            }
        }
    }
    if !plan.mint.is_empty() {
        let tag = if dry_run { "[dry-run] would mint" } else { "will mint" };
        plan.lines.insert(
            0,
            format!(
                "{tag} an OpenRouter key (cap {}, OPENROUTER_KEY_LIMIT) for {}: none on OpenRouter — each is copied on once its machine is set up",
                exact_money(limit),
                plan.mint.join(", ")
            ),
        );
    }
    plan.listed = listed;
    plan
}

/// One machine name's keys, held: the keys folder locked ([`lock_keys_dir`]), OpenRouter's whole
/// listing read under the lock, and the keys CSV's row for the name as it is now. Every mint and
/// delete for a name decides by a hold and acts before dropping it, so two arenas (an `up` that
/// waited at its prompt, a `keys gen`, a handover) never both mint for one name, and none acts on a
/// listing older than another's change. One name at a time: a hold lasts one listing and one
/// name's requests, never a whole batch's.
pub(crate) struct NameHold {
    _held: Option<std::fs::File>,
    pub listed: Vec<KeyInfo>,
    pub listed_at: u64,
    /// The keys CSV's OpenRouter key for the name, as the file has it now.
    pub secret: Option<String>,
}

/// Take a [`NameHold`] for `host` over the keys folder `dir`: `Err` says why (the folder stayed
/// locked, the listing failed or took too long).
pub(crate) async fn hold_name(api: &dyn KeyApi, dir: &std::path::Path, host: &str) -> Result<NameHold, String> {
    let _ = std::fs::create_dir_all(dir);
    let held = lock_keys_dir(dir).map_err(|e| format!("the keys folder {} stayed locked: {e}", dir.display()))?;
    let listed_at = arena_core::snapshot::unix_now();
    let listed = match arena_core::ratelimit::bounded(LIST_TIMEOUT, api.list_keys()).await {
        Ok(Ok(k)) => k,
        Ok(Err(e)) => return Err(format!("listing OpenRouter keys failed: {e}")),
        Err(_) => return Err(format!("OpenRouter's keys didn't list within {}s", LIST_TIMEOUT.as_secs())),
    };
    let secret = std::fs::read_to_string(dir.join(OPENROUTER_KEYS_LINK))
        .ok()
        .and_then(|t| arena_core::apikeys::parse_csv(&t).into_iter().find(|(h, _)| h == host).map(|(_, k)| k));
    Ok(NameHold { _held: held, listed, listed_at, secret })
}

/// What [`mint_missing`] did.
#[derive(Debug, Default)]
pub(crate) struct Minted {
    /// `✓`/`=`/`✗` lines, one per name.
    pub lines: Vec<String>,
    /// The names that got a key (in the CSV now).
    pub names: Vec<String>,
    /// Names whose key someone else's command made while this one waited, for another owner: not
    /// to be copied onto this owner's machine.
    pub left_out: Vec<String>,
}

/// Mint `plan`'s keys, one name at a time under a [`NameHold`]: each named after its machine,
/// capped at the plan's limit, saved to the keys CSV in `dir` and recorded for `arena spend` as
/// `owner`'s. A name that got a key while this `up` waited at its prompt (`keys gen`, another `up`;
/// renamed since, too) keeps it — never two keys for a name — unless that key is another owner's:
/// then it's left out of this machine's copy, with the `keys rotate` that gives this owner one. A
/// failure is a `✗` line naming what to run, never an error: the machine comes up without a key,
/// as it did before.
pub(crate) async fn mint_missing(api: &dyn KeyApi, ctx: &SpendCtx, naming: &Naming<'_>, dir: &std::path::Path, plan: &MintPlan, owner: Option<&str>) -> Minted {
    let mut done = Minted::default();
    let whose = owner.map(|o| format!(", {o}'s")).unwrap_or_default();
    for host in &plan.mint {
        let s = short(naming, host);
        let hold = match hold_name(api, dir, host).await {
            Ok(h) => h,
            Err(why) => {
                done.lines.push(format!("✗ {host}: no OpenRouter key minted ({why}) — `arena keys gen {s} --copy` once it's up"));
                continue;
            }
        };
        if has_key(naming, host, hold.secret.as_deref(), &hold.listed) {
            // Made while this one waited at its prompt. Another owner's isn't this owner's to use.
            match owner.and_then(|o| arena_core::spend::handover_needed(&ctx.state(), host, o).map(|t| (o, t.to_string()))) {
                Some((o, theirs)) => {
                    done.lines.push(format!(
                        "✗ {host}: an OpenRouter key was made for it meanwhile, {theirs}'s — not copied onto {o}'s machine. \
                         `arena keys rotate {s} --copy` gives {o} a fresh one ({theirs} keeps what it spent)"
                    ));
                    done.left_out.push(host.clone());
                }
                None => done.lines.push(format!("= {host}: has an OpenRouter key now (made while this waited) — kept, none minted")),
            }
            continue;
        }
        let kn = key_name(naming.prefix, naming.list, host);
        let ck = match api.create_key(&kn, Some(plan.limit)).await {
            Ok(ck) => ck,
            Err(e) => {
                done.lines.push(format!("✗ {host}: no OpenRouter key minted ({e}) — `arena keys gen {s} --copy` once it's up"));
                continue;
            }
        };
        ctx.key_minted(&ck.hash, host, owner);
        let short_hash = &ck.hash[..ck.hash.len().min(12)];
        match write_openrouter_key_held(dir, host, &ck.secret, naming.prefix) {
            Ok(()) => {
                done.lines.push(format!("✓ {host}: OpenRouter key minted (a fresh key {short_hash}, cap {}{whose})", exact_money(plan.limit)));
                done.names.push(host.clone());
            }
            Err(e) => {
                // Its secret is lost (OpenRouter shows it once): revoke it rather than leave a
                // live key nobody holds.
                let fresh = KeyInfo { hash: ck.hash.clone(), name: Some(kn.clone()), usage: Some(0.0), ..KeyInfo::default() };
                let then = if api.delete_key(&ck.hash).await.is_ok() {
                    ctx.keys_deleted(&[&fresh], std::slice::from_ref(&ck.hash));
                    "revoked it".to_string()
                } else {
                    format!("revoke it: `arena keys revoke {s}`")
                };
                done.lines.push(format!("✗ {host}: a fresh OpenRouter key {short_hash} couldn't be saved ({e:#}) — {then}"));
            }
        }
        drop(hold);
    }
    done
}

/// After `up`'s prompt, one consistent look under the keys folder's lock ([`lock_keys_dir`]): the
/// keys stage's sources as the keys CSV has them now (`load`) and OpenRouter's listing for the
/// copy's check — never the snapshot from before the prompt, which a `keys rotate` meanwhile could
/// have made stale.
pub(crate) async fn copy_snapshot<T>(api: Option<&dyn KeyApi>, dir: &std::path::Path, load: impl FnOnce() -> T) -> (T, Option<Result<Vec<KeyInfo>, String>>) {
    let held = api.and_then(|_| lock_keys_dir(dir).ok());
    let sources = load();
    let listed = list_for_check(api).await;
    drop(held);
    (sources, listed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena_core::openrouter::CreatedKey;
    use std::sync::Mutex;

    fn naming() -> Naming<'static> {
        static LIST: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
        Naming { prefix: "devtest", list: LIST.get_or_init(|| vec!["apple".into(), "bloom".into(), "@staff-gpu".into()]) }
    }

    fn key(name: &str, label: Option<&str>) -> KeyInfo {
        KeyInfo { hash: format!("h-{name}"), name: Some(name.into()), label: label.map(String::from), ..KeyInfo::default() }
    }

    const SECRET: &str = "sk-or-v1-0e6aaaaaaaaaaaaaaaaaaaa1c96";

    #[test]
    fn a_redacted_label_says_whether_it_is_the_secret_only_when_it_can() {
        assert_eq!(label_says(Some("sk-or-v1-0e6...1c96"), SECRET), Some(true));
        assert_eq!(label_says(Some("sk-or-v1-0e7...1c96"), SECRET), Some(false));
        assert_eq!(label_says(Some("sk-or-v1-0e6...1c97"), SECRET), Some(false));
        for unsure in [None, Some("my key"), Some("sk-or-v1-customkey"), Some("...1c96"), Some("sk-or-v1-0e6...")] {
            assert_eq!(label_says(unsure, SECRET), None, "{unsure:?}");
        }
    }

    /// A row is stale when no key goes by its machine's name, or when every key that does says
    /// (by its label) it isn't the row's; a label that can't tell keeps the row.
    #[test]
    fn a_row_is_stale_only_when_the_listing_says_so() {
        let n = naming();
        let cases: &[(&[KeyInfo], Option<Stale>)] = &[
            (&[], Some(Stale::Gone)),
            (&[key("devtest-bloom", Some("sk-or-v1-111...2222"))], Some(Stale::Gone)),
            (&[key("devtest-apple", Some("sk-or-v1-0e6...1c96"))], None),
            (&[key("devtest-apple", Some("sk-or-v1-999...0000"))], Some(Stale::Replaced)),
            (&[key("devtest-apple", None)], None),
            (&[key("devtest-apple", Some("sk-or-v1-999...0000")), key("devtest-apple", Some("sk-or-v1-0e6...1c96"))], None),
            (&[key("devtest-apple", Some("sk-or-v1-999...0000")), key("devtest-apple", Some("odd"))], None),
            // The row's key renamed on OpenRouter, a new one under apple's name: the row is live.
            (&[key("devtest-apple", Some("sk-or-v1-999...0000")), key("apple (old)", Some("sk-or-v1-0e6...1c96"))], None),
        ];
        for (listed, want) in cases {
            assert_eq!(stale(&n, "devtest-apple", SECRET, listed), *want, "{listed:?}");
        }
        // A paused (disabled) key still exists.
        let paused = KeyInfo { disabled: true, ..key("devtest-apple", None) };
        assert_eq!(stale(&n, "devtest-apple", SECRET, &[paused]), None);
        // Renamed on OpenRouter (`apple — alice's`): still the row's key, found by its label.
        assert_eq!(stale(&n, "devtest-apple", SECRET, &[key("apple — alice's", Some("sk-or-v1-0e6...1c96"))]), None);
        assert_eq!(stale(&n, "devtest-apple", SECRET, &[key("apple — alice's", Some("sk-or-v1-999...0000"))]), Some(Stale::Gone));
    }

    struct Dir(std::path::PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn dir(tag: &str) -> Dir {
        let d = std::env::temp_dir().join(format!("arena-keys-checked-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Dir(d)
    }

    fn sources(d: &Dir, rows: &[(&str, &str)]) -> KeySources {
        let mut text = String::new();
        for (h, k) in rows {
            text.push_str(&format!("{h},{k}\n"));
        }
        std::fs::write(d.0.join("openrouter_api_keys.csv"), text).unwrap();
        std::fs::write(d.0.join("openai_api_keys.csv"), "devtest-apple,sk-openai\n").unwrap();
        KeySources::load(&arena_core::Config::parse(""), &d.0.display().to_string(), None)
    }

    /// The copy leaves a dead row out (and says what to run), keeps a live one and the machine's
    /// other keys; an unreadable listing copies everything and says so once; no provisioning key
    /// says nothing.
    #[test]
    fn the_copy_leaves_out_dead_rows_and_says_what_to_run() {
        let d = dir("check");
        let n = naming();
        let hosts: Vec<String> = ["devtest-apple", "devtest-bloom", "devtest-cedar"].map(String::from).to_vec();
        let mut keys = sources(&d, &[("devtest-apple", SECRET), ("devtest-bloom", "sk-or-v1-old"), ("devtest-cedar", "sk-or-v1-c")]);
        assert_eq!(keys.csv, ["OpenAI (1 host)", "OpenRouter (3 hosts)"]);
        let listed = Ok(vec![key("devtest-apple", Some("sk-or-v1-0e6...1c96")), key("devtest-cedar", Some("sk-or-v1-zzz...9999"))]);
        let lines = check_csv_keys(&mut keys, &hosts, Some(&listed), &n);
        assert_eq!(lines.len(), 2, "{lines:#?}");
        assert!(lines[0].starts_with("⚠ devtest-bloom: the keys CSV's OpenRouter key is gone from OpenRouter") && lines[0].ends_with("`arena keys gen bloom --copy` mints a fresh one"), "{}", lines[0]);
        assert!(lines[1].starts_with("⚠ devtest-cedar: the keys CSV's OpenRouter key isn't the one") && lines[1].contains("`arena keys rotate cedar --copy`"), "{}", lines[1]);
        assert_eq!(keys.openrouter_secret("devtest-apple"), Some(SECRET));
        assert_eq!((keys.openrouter_secret("devtest-bloom"), keys.openrouter_secret("devtest-cedar")), (None, None));
        assert!(keys.vars_for("devtest-apple").0.iter().any(|(k, _)| k == "OPENAI_API_KEY"));
        assert!(!keys.vars_for("devtest-bloom").1, "nothing left for bloom");
        assert_eq!(keys.csv, ["OpenAI (1 host)", "OpenRouter (1 host)"]);
        // Only the hosts asked about are checked.
        let mut keys = sources(&d, &[("devtest-bloom", "sk-or-v1-old")]);
        assert!(check_csv_keys(&mut keys, &["devtest-apple".into()], Some(&listed), &n).is_empty());
        assert!(keys.openrouter_secret("devtest-bloom").is_some());
        // The listing failed: copied as it is, said once.
        let mut keys = sources(&d, &[("devtest-bloom", "sk-or-v1-old")]);
        let lines = check_csv_keys(&mut keys, &hosts, Some(&Err("HTTP 502".into())), &n);
        assert_eq!(lines, ["⚠ OpenRouter's keys couldn't be listed (HTTP 502): the keys CSV's OpenRouter keys are copied as they are, unchecked"]);
        assert!(keys.openrouter_secret("devtest-bloom").is_some());
        // No provisioning key: as before, silently.
        assert!(check_csv_keys(&mut keys, &hosts, None, &n).is_empty());
        // Every row dead: the OpenRouter source goes from the line.
        let mut keys = sources(&d, &[("devtest-bloom", "sk-or-v1-old")]);
        check_csv_keys(&mut keys, &hosts, Some(&Ok(vec![])), &n);
        assert_eq!(keys.csv, ["OpenAI (1 host)"]);
    }

    /// `up` mints for a cohort name with no key on OpenRouter — not for one that has a key (even
    /// paused), not for a handover's (it gets a fresh one anyway), not for a staff box (told);
    /// a failed listing mints nothing and says so; no provisioning key, nothing at all.
    #[test]
    fn up_plans_a_key_for_each_cohort_name_without_one() {
        let n = naming();
        let names: Vec<String> = ["devtest-apple", "devtest-bloom", "devtest-cedar", "staff-gpu"].map(String::from).to_vec();
        let listed = vec![KeyInfo { disabled: true, ..key("devtest-bloom", None) }];
        let plan = plan_mints(&n, &names, &["devtest-cedar".into()], None, Some(Ok(listed)), 0.05, false);
        assert_eq!(plan.mint, ["devtest-apple"]);
        assert_eq!(plan.lines[0], "will mint an OpenRouter key (cap $0.05, OPENROUTER_KEY_LIMIT) for devtest-apple: none on OpenRouter — each is copied on once its machine is set up");
        assert_eq!(plan.lines[1], "staff-gpu: no OpenRouter key, and none is minted for a staff box — `arena keys gen staff-gpu --copy` if it should have one");
        assert!(matches!(plan.listed, Some(Ok(ref k)) if k.len() == 1), "kept for the copy's check");
        let dry = plan_mints(&n, &names[..1], &[], None, Some(Ok(vec![])), 5.0, true);
        assert!(dry.lines[0].starts_with("[dry-run] would mint an OpenRouter key (cap $5.00"), "{:?}", dry.lines);
        let failed = plan_mints(&n, &names, &[], None, Some(Err("HTTP 401".into())), 5.0, false);
        assert!(failed.mint.is_empty());
        assert_eq!(failed.lines, ["⚠ OpenRouter's keys couldn't be listed (HTTP 401): no key is minted for a name that has none — `arena keys gen <name> --copy` once it answers"]);
        let none = plan_mints(&n, &names, &[], None, None, 5.0, false);
        assert!(none.mint.is_empty() && none.lines.is_empty());
        // apple's key in the CSV, renamed on OpenRouter: it has a key — none minted.
        let d = dir("plan-renamed");
        let csv = sources(&d, &[("devtest-apple", SECRET)]);
        let renamed = vec![key("apple — alice's", Some("sk-or-v1-0e6...1c96"))];
        assert!(plan_mints(&n, &names[..1], &[], Some(&csv), Some(Ok(renamed)), 5.0, false).mint.is_empty());
        let other = vec![key("apple — alice's", Some("sk-or-v1-999...0000"))];
        assert_eq!(plan_mints(&n, &names[..1], &[], Some(&csv), Some(Ok(other)), 5.0, false).mint, ["devtest-apple"]);
    }

    #[derive(Default)]
    struct FakeKeys {
        created: Mutex<Vec<(String, Option<f64>)>>,
        refuse: Vec<String>,
        /// What the listing has (besides what this fake created).
        listed: Vec<KeyInfo>,
    }

    #[async_trait::async_trait]
    impl KeyApi for FakeKeys {
        async fn list_keys(&self) -> arena_core::Result<Vec<KeyInfo>> {
            Ok(self.listed.clone())
        }
        async fn create_key(&self, name: &str, limit: Option<f64>) -> arena_core::Result<CreatedKey> {
            if self.refuse.iter().any(|r| r == name) {
                return Err(arena_core::Error::provider("openrouter create key: HTTP 503"));
            }
            let mut c = self.created.lock().unwrap();
            c.push((name.into(), limit));
            Ok(CreatedKey { secret: format!("sk-or-v1-{name}"), hash: format!("hash{}abcdefghijk", c.len()), name: name.into() })
        }
        async fn delete_key(&self, _hash: &str) -> arena_core::Result<()> {
            Ok(())
        }
        async fn rename_key(&self, _hash: &str, _new_name: &str) -> arena_core::Result<()> {
            Ok(())
        }
    }

    /// The mint: each key named after its machine at the plan's cap, in the CSV and the spend
    /// ledger as the owner's; a refused create is a `✗` line naming what to run, the rest go on.
    #[tokio::test]
    async fn mint_saves_each_key_and_records_its_owner() {
        let d = dir("mint");
        let state = dir("mint-state");
        let cfg = arena_core::Config::parse(&format!("MACHINE_NAME_PREFIX=devtest\nARENA_STATE_DIR={}\n", state.0.display()));
        let ctx = SpendCtx::from_config(&cfg);
        let n = naming();
        let api = FakeKeys { refuse: vec!["devtest-bloom".into()], ..FakeKeys::default() };
        let plan = MintPlan { mint: vec!["devtest-apple".into(), "devtest-bloom".into()], limit: 0.1, ..MintPlan::default() };
        let done = mint_missing(&api, &ctx, &n, &d.0, &plan, Some("alice")).await;
        assert_eq!(done.names, ["devtest-apple"]);
        assert_eq!(done.lines[0], "✓ devtest-apple: OpenRouter key minted (a fresh key hash1abcdefg, cap $0.10, alice's)");
        assert_eq!(done.lines[1], "✗ devtest-bloom: no OpenRouter key minted (provider error: openrouter create key: HTTP 503) — `arena keys gen bloom --copy` once it's up");
        assert_eq!(*api.created.lock().unwrap(), [("devtest-apple".to_string(), Some(0.1))]);
        let csv = std::fs::read_to_string(d.0.join("openrouter_api_keys.csv")).unwrap();
        assert!(csv.contains("devtest-apple,sk-or-v1-devtest-apple") && !csv.contains("bloom"), "{csv}");
        assert_eq!(arena_core::spend::key_owner(&ctx.state(), "devtest-apple"), Some("alice"));
        // A key made for a planned name while `up` waited at its prompt (`keys gen` elsewhere, no
        // owner): seen in the listing read under the hold — kept, no second key.
        let api = FakeKeys { listed: vec![key("devtest-cedar", None)], ..FakeKeys::default() };
        let plan = MintPlan { mint: vec!["devtest-cedar".into()], limit: 0.1, ..MintPlan::default() };
        let done = mint_missing(&api, &ctx, &n, &d.0, &plan, Some("alice")).await;
        assert_eq!(done.lines, ["= devtest-cedar: has an OpenRouter key now (made while this waited) — kept, none minted"]);
        assert!(done.names.is_empty() && done.left_out.is_empty() && api.created.lock().unwrap().is_empty());
        // Renamed on OpenRouter since, its secret in the CSV: found by its label — still kept.
        std::fs::write(d.0.join("devtest_openrouter_keys.csv"), format!("devtest-dune,{SECRET}\n")).unwrap();
        let api = FakeKeys { listed: vec![key("dune — renamed", Some("sk-or-v1-0e6...1c96"))], ..FakeKeys::default() };
        let plan = MintPlan { mint: vec!["devtest-dune".into()], limit: 0.1, ..MintPlan::default() };
        let done = mint_missing(&api, &ctx, &n, &d.0, &plan, Some("alice")).await;
        assert!(done.lines[0].starts_with("= devtest-dune: has an OpenRouter key now"), "{:?}", done.lines);
        assert!(api.created.lock().unwrap().is_empty());
        // Another owner's (bob's `up` minted it meanwhile): left out of alice's machine, with the fix.
        ctx.key_minted("hash-bob", "devtest-echo", Some("bob"));
        let api = FakeKeys { listed: vec![key("devtest-echo", None)], ..FakeKeys::default() };
        let plan = MintPlan { mint: vec!["devtest-echo".into()], limit: 0.1, ..MintPlan::default() };
        let done = mint_missing(&api, &ctx, &n, &d.0, &plan, Some("alice")).await;
        assert_eq!(done.left_out, ["devtest-echo"]);
        assert!(done.lines[0].starts_with("✗ devtest-echo: an OpenRouter key was made for it meanwhile, bob's — not copied onto alice's machine"), "{:?}", done.lines);
        assert!(api.created.lock().unwrap().is_empty());
    }
}
