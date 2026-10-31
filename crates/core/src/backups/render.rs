//! The backups' status in words: `arena backups`' text, the cells the dashboard's screen shares
//! with it, and the docs' inventory table. Pure.

use super::{ago, until, Cell, Health, JobStatus, Setup, Status, INVENTORY, NOT_COVERED};
use crate::table::{render as table, Align};

/// `2026-10-30 21:40 UTC`.
pub fn clock(t: u64) -> String {
    let rem = t % 86_400;
    format!("{} {:02}:{:02} UTC", crate::schedule::ymd_string((t / 86_400) as i64), rem / 3600, rem % 3600 / 60)
}

/// An age as a table cell: `now`, `12m`, `3h`, `2d`.
pub fn short_age(secs: u64) -> String {
    match secs {
        s if s < 60 => "now".into(),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 2 * 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// A pod's cell in a few characters: `✓ pushed 12m`, `✗ failed 14m (ok 2d)`, `⚠ on main 12m`,
/// `? commit 2h`, `-`.
pub fn cell(c: &Cell, now: u64) -> String {
    if c.health == Health::Off {
        return "-".into();
    }
    let age = c.tried_at.map(|t| format!(" {}", short_age(now.saturating_sub(t)))).unwrap_or_default();
    let ok = match (c.health, c.ok_at) {
        // The last success, when it's another time than the try's (a minute apart, at least).
        (Health::Fail | Health::Warn, Some(t)) if c.tried_at.is_none_or(|tr| tr.abs_diff(t) >= 60) => format!(" (ok {})", short_age(now.saturating_sub(t))),
        (Health::Fail, None) if c.tried_at.is_some() => " (never ok)".into(),
        _ => String::new(),
    };
    format!("{} {}{age}{ok}", c.health.mark(), c.text)
}

/// How often a job runs, or that it isn't set up.
pub fn how_often(j: &JobStatus) -> String {
    match &j.setup {
        Setup::Cron { every, .. } => every.clone(),
        Setup::Missing { .. } => "not set up".into(),
        Setup::Unknown { .. } => "?".into(),
    }
}

/// A job's last run in words: `12 min ago: 1 pushed, 41 on main` / `none recorded yet`.
pub fn last_run(j: &JobStatus, now: u64) -> String {
    match (&j.last, &j.last_unknown) {
        (Some(l), _) => {
            let mark = if l.ok { "✓" } else { "✗" };
            format!("{mark} {}: {}", ago(now, l.finished), l.summary)
        }
        (None, Some(why)) => format!("? {why}"),
        (None, None) => "-".into(),
    }
}

/// When a job's next run is due: `in 3 min`, `running now (since 2 min ago)`, `-`.
pub fn next_run(j: &JobStatus, now: u64) -> String {
    match (j.running_since, j.next) {
        (Some(0), _) => "running now".into(),
        (Some(t), _) => format!("running now (since {})", ago(now, t)),
        (None, Some(t)) => until(now, t),
        (None, None) => "-".into(),
    }
}

/// `arena backups`' whole text.
pub fn text(s: &Status) -> String {
    let now = s.generated_at;
    let mut out = String::new();
    out.push_str(&format!("Backups — {} · {}\n\n", s.prefix, clock(now)));
    let rows: Vec<Vec<String>> = s
        .jobs
        .iter()
        .map(|j| vec![j.health.mark().to_string(), j.name.to_string(), how_often(j), last_run(j, now), next_run(j, now)])
        .collect();
    let lines = table(&["", "JOB", "HOW OFTEN", "LAST RUN", "NEXT"], &[], &rows);
    for (line, j) in lines.lines().zip(std::iter::once(None).chain(s.jobs.iter().map(Some))) {
        out.push_str(line);
        out.push('\n');
        if let Some(why) = j.and_then(|j| j.why.as_ref()) {
            out.push_str(&format!("    └ {why}\n"));
        }
    }
    out.push('\n');
    match (&s.running_unknown, s.running.is_empty()) {
        (Some(why), _) => out.push_str(&format!("Running now: unknown ({why})\n")),
        (None, true) => out.push_str("Running now: nothing\n"),
        (None, false) => {
            let parts: Vec<String> = s.running.iter().map(|r| r.since.map_or_else(|| r.what.clone(), |t| format!("{} since {}", r.what, ago(now, t)))).collect();
            out.push_str(&format!("Running now: {}\n", parts.join(" · ")));
        }
    }
    out.push('\n');
    if s.pods.is_empty() {
        out.push_str("Pods: none (no MACHINE_NAME_LIST, and nothing recorded)\n");
    } else {
        out.push_str("Pods (failing first, then the longest since a good backup):\n");
        let rows: Vec<Vec<String>> = s
            .pods
            .iter()
            .map(|p| {
                let name = if p.listed { p.name.clone() } else { format!("{} (not listed)", p.name) };
                let busy = if p.running { "  (backing up now)" } else { "" };
                vec![p.health.mark().to_string(), name, cell(&p.git, now), cell(&p.copy, now), format!("{}{busy}", cell(&p.archive, now))]
            })
            .collect();
        let lines = table(&["", "POD", "GIT", "HOME COPY", "ARCHIVE"], &[Align::Left], &rows);
        for (line, p) in lines.lines().zip(std::iter::once(None).chain(s.pods.iter().map(Some))) {
            out.push_str(line);
            out.push('\n');
            if let Some(why) = p.filter(|p| matches!(p.health, Health::Fail | Health::Warn | Health::Unknown)).and_then(|p| p.why.as_ref()) {
                out.push_str(&format!("    └ {why}\n"));
            }
        }
    }
    out.push_str("\nWhere the copies live:\n");
    let rows: Vec<Vec<String>> = s.storage.iter().map(|st| vec![st.health.mark().to_string(), st.what.clone(), st.place.clone(), st.text.clone()]).collect();
    for line in table(&["", "WHAT", "WHERE", ""], &[], &rows).lines().skip(1) {
        out.push_str(line);
        out.push('\n');
    }
    if !s.skips.is_empty() {
        out.push_str("\nCopies skipped for room in the last day:\n");
        for l in &s.skips {
            out.push_str(&format!("  {l}\n"));
        }
    }
    for n in &s.notes {
        out.push_str(&format!("\nnote: {n}\n"));
    }
    out.push_str("\nNot covered: ");
    out.push_str(&NOT_COVERED.join("; "));
    out.push_str(".\nRestore: `arena pods restore <pod>` (newest snapshot; `--from big|<label>`); the archive: `sftp archive-");
    out.push_str(&s.prefix);
    out.push_str("`.\n");
    out
}

/// The docs' table of every backup kind ([`INVENTORY`]): Markdown, one row each.
pub fn inventory_table() -> String {
    let mut out = String::from("| Backup | Started by | Where the copy lands | What it records |\n|---|---|---|---|\n");
    for (kind, trigger, lands, record) in INVENTORY {
        out.push_str(&format!("| {kind} | {trigger} | {lands} | {record} |\n"));
    }
    out
}
