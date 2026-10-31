//! Capacity spares: Hetzner servers arena creates ahead of need (`arena pods spare up`), so a
//! move to a bigger type doesn't wait on capacity at the moment it's wanted (prod 2026-10-30:
//! in-place resizes to cx43 failed 22/22 with `resource_unavailable` in the boxes' own
//! locations, while new cx43s could still be made elsewhere now and then — the operator's
//! "swapper" script made spares every 3 minutes and swapped them in by hand).
//!
//! A spare is a server labelled [`SPARE_LABEL`]`=<prefix>` and named `<prefix>-spare-<n>`. The
//! fleet never lists one (`Provider::list_pods` leaves it out, like the backup archive and a
//! bake VM), so no backup, proxy forward, ssh-config block, key, reap, `pods idle` or `--all`
//! reaches it: it's nobody's machine yet. What it costs is still counted — `pods spare list`,
//! `pods list`'s footer, `arena balance`, `arena spend` and `teardown --check` read the spares.
//!
//! `pods migrate copy <name> --onto-spare` (or `--onto <spare>`, and the same on `replace`)
//! takes one into the fleet ([`SpareCloud::adopt`]): ONE `PUT /servers/{id}` renames it
//! `<name>-new` and drops its spare label, so it is never a spare under a fleet name nor a
//! fleet machine under a spare's. From there it's `-new` like one `migrate copy` created.
//!
//! Pure here (table-tested): names, the label, which spare is taken, the table. The API calls
//! are the provider's ([`SpareCloud`], implemented by the Hetzner backend).

use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::error::Result;
use crate::pod::Pod;
use crate::table::{self, Align};

pub use crate::provider::hetzner::SPARE_LABEL;

/// The word in a spare's name: `<prefix>-spare-<n>`.
pub const WORD: &str = "spare";

/// The most spares one `pods spare up` makes: a capacity grab, not a fleet (and every one bills).
pub const MAX_UP: usize = 20;

/// What the Hetzner backend does for spares (a fake in tests).
#[async_trait]
pub trait SpareCloud: Send + Sync {
    /// This cohort's spares ([`selector`]), any state.
    async fn spares(&self) -> Result<Vec<Pod>>;
    /// Take spare `id` — listed as `name` — into the fleet as `new_name`: renamed and its spare
    /// label dropped, in one request. Refused (nothing changed) when `id` isn't this cohort's
    /// spare called `name` any more (taken meanwhile). Call it under [`claim_lock`].
    async fn adopt(&self, id: &str, name: &str, new_name: &str) -> Result<()>;
}

/// The lock file, in the cohort's state dir, that every claim on a spare holds: an adoption
/// (`--onto-spare`, `--onto <spare>`) from its last look to its rename, and `spare down` from
/// its last look to its deletes. Hetzner has no conditional update, so a check-then-rename
/// (or check-then-delete) is only safe against another arena on this box when they take turns:
/// two runs never take the same spare, and a `spare down` waiting at its prompt never deletes
/// one a move took meanwhile (it looks again under the lock). Runs on another control box
/// aren't covered: one control box per cohort, as everywhere else.
pub const LOCK_FILE: &str = "spares.lock";

/// How long a claim waits for another run's: a claim is one or two requests per spare.
pub const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// Take the spare-claim lock in `dir` (created 0700 when missing; the file 0600), waiting up to
/// `wait` for another run's. Held until the returned file is dropped. `Err`: why not.
pub fn claim_lock(dir: &std::path::Path, wait: std::time::Duration) -> std::result::Result<std::fs::File, String> {
    use std::fs::{OpenOptions, TryLockError};
    crate::snapshot::create_private_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(LOCK_FILE);
    let mut opts = OpenOptions::new();
    opts.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let f = opts.open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let deadline = std::time::Instant::now() + wait;
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(f),
            Err(TryLockError::WouldBlock) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(TryLockError::WouldBlock) => {
                return Err(format!("{} is held by another arena run (taking or deleting a spare) for over {}s", path.display(), wait.as_secs()))
            }
            Err(TryLockError::Error(e)) => return Err(format!("{}: {e}", path.display())),
        }
    }
}

/// `<prefix>-spare-<n>`.
pub fn name(prefix: &str, n: u32) -> String {
    format!("{prefix}-{WORD}-{n}")
}

/// The `<n>` of one of `prefix`'s spare names (`devtest-spare-3` → 3); `None` for anything
/// else. Pure.
pub fn number(prefix: &str, name: &str) -> Option<u32> {
    let n = name.strip_prefix(prefix)?.strip_prefix('-')?.strip_prefix(WORD)?.strip_prefix('-')?;
    (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && !n.starts_with('0')).then(|| n.parse().ok()).flatten()
}

/// `count` spare names not in `taken` (every server name the project lists that could clash:
/// the spares and the fleet), lowest numbers first. Pure.
pub fn free_names(prefix: &str, taken: &[&str], count: usize) -> Vec<String> {
    let mut out = Vec::with_capacity(count);
    let mut n = 1u32;
    while out.len() < count {
        let candidate = name(prefix, n);
        if !taken.contains(&candidate.as_str()) {
            out.push(candidate);
        }
        n += 1;
    }
    out
}

/// The label value for `prefix` (Hetzner's label rules: alphanumerics, `-`, `_`, `.`, ≤ 63).
pub fn label_value(prefix: &str) -> String {
    crate::image::label_value(prefix)
}

/// The `label_selector` for `prefix`'s spares.
pub fn selector(prefix: &str) -> String {
    format!("{SPARE_LABEL}={}", label_value(prefix))
}

/// The `labels` a spare is created with (merged into the create body).
pub fn labels(prefix: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(SPARE_LABEL.into(), Value::from(label_value(prefix)));
    m
}

/// Whether spare `p` can be taken now: running, with its public IP. (Whether it answers SSH
/// is the adoption's setup to find out.)
pub fn ready(p: &Pod) -> bool {
    p.status.eq_ignore_ascii_case("RUNNING") && p.ssh_ip.is_some()
}

/// Which spare `--onto-spare` takes: the first ready one ([`ready`]) — of the first of `types`
/// that has one when types are given (`--server-type cx43,cx53`: a cx43 before any cx53),
/// else of any type — lowest spare number first. `None`: none ready (of those types). Pure.
pub fn pick<'a>(spares: &'a [Pod], prefix: &str, types: &[String]) -> Option<&'a Pod> {
    let mut ready: Vec<&Pod> = spares.iter().filter(|p| ready(p)).collect();
    ready.sort_by_key(|p| (number(prefix, &p.name).unwrap_or(u32::MAX), p.name.clone()));
    if types.is_empty() {
        return ready.first().copied();
    }
    types.iter().find_map(|t| ready.iter().copied().find(|p| p.gpu_type.as_deref().is_some_and(|g| g.eq_ignore_ascii_case(t))))
}

/// Their summed €/h (gross, where each one runs), and how many have no price listed. Pure.
pub fn cost_per_hr(spares: &[Pod]) -> (f64, usize) {
    let billing = spares.iter().filter(|p| crate::status::bills_hourly(&p.provider, &p.status));
    let (mut sum, mut unknown) = (0.0, 0);
    for p in billing {
        match p.cost_per_hr {
            Some(c) => sum += c,
            None => unknown += 1,
        }
    }
    (sum, unknown)
}

/// `2 spares (cx43 ×2), €0.0310/h — arena pods spare list`: the line under `pods list`'s table,
/// `None` without spares. Pure.
pub fn footer(spares: &[Pod]) -> Option<String> {
    if spares.is_empty() {
        return None;
    }
    let mut types: Vec<(String, usize)> = Vec::new();
    for p in spares {
        let t = p.gpu_type.clone().unwrap_or_else(|| "?".into());
        match types.iter_mut().find(|(n, _)| *n == t) {
            Some((_, c)) => *c += 1,
            None => types.push((t, 1)),
        }
    }
    let types: Vec<String> = types.into_iter().map(|(t, c)| if c == 1 { t } else { format!("{t} ×{c}") }).collect();
    let (sum, unknown) = cost_per_hr(spares);
    let unpriced = if unknown > 0 { format!(" + {unknown} unpriced") } else { String::new() };
    Some(format!(
        "{} spare{} ({}), {}/h{unpriced} — not in the table: no one's machine yet (`arena pods spare list`)",
        spares.len(),
        if spares.len() == 1 { "" } else { "s" },
        types.join(", "),
        crate::fleet::fmt_money("€", sum)
    ))
}

/// `pods spare list`'s table: name, id, type, where, status, IP, age (from `now`), €/h. Pure.
pub fn render(spares: &[Pod], now: u64) -> String {
    let mut rows: Vec<Vec<String>> = Vec::new();
    for p in spares {
        let age = p.created_at.map_or_else(|| "-".to_string(), |c| crate::snapshot::fmt_age(now.saturating_sub(c)));
        rows.push(vec![
            p.name.clone(),
            p.id.clone(),
            p.gpu_type.clone().unwrap_or_else(|| "-".into()),
            p.location.clone().unwrap_or_else(|| "-".into()),
            p.status.clone(),
            p.ssh_ip.clone().unwrap_or_else(|| "-".into()),
            age,
            p.cost_per_hr.map_or_else(|| "-".into(), |c| crate::fleet::fmt_money("€", c)),
        ]);
    }
    let header = ["NAME", "ID", "TYPE", "WHERE", "STATUS", "IP", "AGE", "€/H"];
    let align = [Align::Left, Align::Left, Align::Left, Align::Left, Align::Left, Align::Left, Align::Right, Align::Right];
    let mut out = table::render(&header, &align, &rows);
    let (sum, unknown) = cost_per_hr(spares);
    out.push_str(&format!(
        "{} spare{}, {}/h{}\n",
        spares.len(),
        if spares.len() == 1 { "" } else { "s" },
        crate::fleet::fmt_money("€", sum),
        if unknown > 0 { format!(" + {unknown} unpriced") } else { String::new() }
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spare(id: &str, name: &str, ty: &str, status: &str, ip: Option<&str>) -> Pod {
        Pod {
            id: id.into(),
            name: name.into(),
            provider: "hetzner".into(),
            status: status.into(),
            gpu_type: Some(ty.into()),
            ssh_ip: ip.map(String::from),
            cost_per_hr: Some(0.0155),
            ..Default::default()
        }
    }

    #[test]
    fn spare_names_are_numbered_and_read_back_only_for_this_prefix() {
        assert_eq!(name("arena9", 3), "arena9-spare-3");
        for (n, want) in [
            ("arena9-spare-3", Some(3)),
            ("arena9-spare-12", Some(12)),
            ("arena9-spare-03", None),
            ("arena9-spare-", None),
            ("arena9-spare-x", None),
            ("arena8-spare-3", None),
            ("arena9-apple", None),
            ("arena9-spare-3-new", None),
        ] {
            assert_eq!(number("arena9", n), want, "{n}");
        }
        // The lowest free numbers, skipping every name already taken (spare or not).
        let taken = ["arena9-spare-1", "arena9-spare-3", "arena9-apple"];
        assert_eq!(free_names("arena9", &taken, 3), ["arena9-spare-2", "arena9-spare-4", "arena9-spare-5"]);
        assert!(free_names("arena9", &taken, 0).is_empty());
    }

    /// One claim at a time: a second waits for the first (here: gives up at once), and goes
    /// through once it's dropped. (It waits a little there: a test elsewhere in this process
    /// forking a child holds a copy of the lock's descriptor until that child execs.)
    #[test]
    fn spare_claims_take_turns() {
        let dir = std::env::temp_dir().join(format!("arena-spare-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = claim_lock(&dir, std::time::Duration::ZERO).unwrap();
        let e = claim_lock(&dir, std::time::Duration::ZERO).unwrap_err();
        assert!(e.contains("held by another arena run"), "{e}");
        drop(first);
        claim_lock(&dir, std::time::Duration::from_secs(10)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_label_and_selector_name_the_cohort() {
        assert_eq!(selector("arena9"), "arena-spare=arena9");
        assert_eq!(labels("arena9").get(SPARE_LABEL), Some(&Value::from("arena9")));
        assert_eq!(selector("my cohort!"), "arena-spare=my-cohort");
    }

    /// `--onto-spare` takes a READY spare (running, with an IP), the lowest number first; with
    /// `--server-type cx43,cx53` a cx43 before any cx53, and none of another type.
    #[test]
    fn the_first_ready_spare_of_the_first_type_that_has_one_is_taken() {
        let spares = vec![
            spare("5", "arena9-spare-10", "cx43", "RUNNING", Some("1.1.1.5")),
            spare("1", "arena9-spare-1", "cx43", "INITIALIZING", None),
            spare("2", "arena9-spare-2", "cx53", "RUNNING", Some("1.1.1.2")),
            spare("3", "arena9-spare-3", "cx43", "OFF", Some("1.1.1.3")),
            spare("4", "arena9-spare-4", "cx43", "RUNNING", Some("1.1.1.4")),
        ];
        let id = |types: &[&str]| pick(&spares, "arena9", &types.iter().map(|t| t.to_string()).collect::<Vec<_>>()).map(|p| p.id.clone());
        assert_eq!(id(&[]), Some("2".into()), "any type: the lowest ready number");
        assert_eq!(id(&["cx43"]), Some("4".into()), "not 1 (no IP yet) nor 3 (off); 4 before 10");
        assert_eq!(id(&["CX53", "cx43"]), Some("2".into()));
        assert_eq!(id(&["cpx42"]), None);
        assert_eq!(pick(&[], "arena9", &[]), None);
    }

    #[test]
    fn the_footer_and_table_count_every_spare_and_what_it_costs() {
        assert_eq!(footer(&[]), None);
        let mut s = vec![
            spare("1", "arena9-spare-1", "cx43", "RUNNING", Some("1.1.1.1")),
            spare("2", "arena9-spare-2", "cx43", "RUNNING", Some("1.1.1.2")),
            spare("3", "arena9-spare-3", "cx53", "INITIALIZING", None),
        ];
        s[2].cost_per_hr = None;
        let f = footer(&s).unwrap();
        assert!(f.starts_with("3 spares (cx43 ×2, cx53), €0.031/h + 1 unpriced"), "{f}");
        assert!(f.contains("arena pods spare list"), "{f}");
        let t = render(&s, 0);
        assert!(t.contains("arena9-spare-1") && t.contains("cx53") && t.contains("INITIALIZING"), "{t}");
        assert!(t.trim_end().ends_with("3 spares, €0.031/h + 1 unpriced"), "{t}");
    }
}
