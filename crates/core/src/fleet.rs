//! Fleet-level presentation: per-pod display labels (GPU, $/h, endpoint, maintenance) and
//! the fleet cost summary shown under `pods list`.
//!
//! Kept pure and provider-agnostic on purpose: the CLI renders it today, and the TUI
//! (Phase 3) and the public snapshot (Phase 4) are meant to reuse these exact functions so
//! a pod reads the same on every surface — no surface re-derives a label on its own.

use serde::Serialize;

use crate::metrics::normalize_gpu_name;
use crate::pod::{Maintenance, Pod};
use crate::status::bills_hourly;
use crate::table::{self, Align};

/// The billing currency symbol for a provider. Hetzner bills in EUR; RunPod and Vast in
/// USD. Kept explicit so a € price is never silently summed into a $ total.
pub fn currency_symbol(provider: &str) -> &'static str {
    if provider.eq_ignore_ascii_case("hetzner") {
        "€"
    } else {
        "$"
    }
}

/// Format an hourly price. Two decimals normally; three below 0.10 so a cheap CPU VM
/// (Hetzner cx23 ≈ €0.006/h) doesn't round to a misleading `0.01` or `0.00`. Never a signed
/// zero: an empty `f64` sum is `-0.0` and printed `$-0.00` (live, under `balance -v`) — a
/// value that would print as zero is zero.
pub fn fmt_money(symbol: &str, amount: f64) -> String {
    let amount = if amount <= 0.0 && amount > -0.005 { 0.0 } else { amount };
    if amount > 0.0 && amount < 0.10 {
        format!("{symbol}{amount:.3}")
    } else {
        format!("{symbol}{amount:.2}")
    }
}

/// `true` when a GPU string already carries a count prefix like `2×RTX A4000` — which is
/// what the SSH (`nvidia-smi`) probe writes into `gpu_type`. Such a label is the ground
/// truth from the machine itself, so it's shown as-is rather than re-prefixed.
fn has_count_prefix(s: &str) -> bool {
    match s.split_once('×') {
        Some((n, _)) => !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

/// The GPU column: `{count}×{type}` (e.g. `1×RTX A4000`), falling back to just the type
/// (Hetzner reuses `gpu_type` for its CPU server type, e.g. `cx23`), `{count}×GPU` when only
/// the count is known, or `-`. A zero count (CPU pod) is treated as unknown.
pub fn gpu_label(pod: &Pod) -> String {
    let ty = pod.gpu_type.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let count = pod.gpu_count.filter(|&n| n > 0);
    match (ty, count) {
        (Some(t), _) if has_count_prefix(t) => t.to_string(),
        (Some(t), Some(n)) => format!("{n}×{}", normalize_gpu_name(t)),
        (Some(t), None) => normalize_gpu_name(t),
        (None, Some(n)) => format!("{n}×GPU"),
        (None, None) => "-".to_string(),
    }
}

/// The $/H column: the pod's hourly price in its provider's currency — `-` if unknown, or
/// if the pod isn't billing ([`bills_hourly`]): a stopped RunPod pod still reports its
/// GPU's rate (live: an EXITED v2 pod showed `$0.13`), which read as if it were costing
/// that. `pods list --json` keeps the raw `cost_per_hr`.
pub fn price_label(pod: &Pod) -> String {
    match pod.cost_per_hr {
        Some(c) if bills_hourly(&pod.provider, &pod.status) => fmt_money(currency_symbol(&pod.provider), c),
        _ => "-".to_string(),
    }
}

/// The ENDPOINT column: the pod's current direct SSH endpoint `ip:port`. An IP without a
/// port mapping yet (pod still booting) shows `ip:?` — not reachable, but not nothing.
pub fn endpoint_label(pod: &Pod) -> String {
    match (pod.ssh_ip.as_deref().filter(|s| !s.is_empty()), pod.ssh_port) {
        (Some(ip), Some(port)) => format!("{ip}:{port}"),
        (Some(ip), None) => format!("{ip}:?"),
        _ if pod.proxy_only => PROXY_ONLY.to_string(),
        _ => "-".to_string(),
    }
}

/// How a pod up without a direct SSH endpoint ([`Pod::proxy_only`]) reads, wherever it shows.
pub const PROXY_ONLY: &str = "no public endpoint (RunPod ssh proxy only)";

/// Why `pod` can't be reached over SSH, when there's more to say than "no endpoint yet": it
/// is stopped (live: `run`/`test` said "no SSH endpoint yet" for an EXITED pod, as if it
/// were still coming up), or it is up with no public endpoint ([`PROXY_ONLY`]). `None` =
/// the plain case (still coming up) — callers keep their own wording. Pure.
pub fn no_endpoint_reason(pod: &Pod) -> Option<String> {
    if crate::status::endpoint_overdue(pod, crate::status::now_secs()) {
        return Some("RUNNING for at least 10 minutes without a direct SSH endpoint — check the pod's networking".into());
    }
    if pod.proxy_only {
        return Some(format!("{PROXY_ONLY} — its host has no public IP; terminate it and create it again"));
    }
    let st = pod.status.trim().to_ascii_uppercase();
    matches!(st.as_str(), "EXITED" | "STOPPED" | "STOPPING" | "OFF" | "TERMINATED" | "DELETING")
        .then(|| format!("stopped ({}) — no SSH while it's stopped", pod.status.trim()))
}

/// Split an ISO-8601-ish timestamp (`2026-10-09T02:00:00Z`, or with a space instead of
/// `T`) into its compact `("MM-DD", "HH:MM")` parts. `None` for anything else, so an
/// unexpected format is shown raw rather than mangled. (Also the public snapshot's test of
/// "is this a timestamp at all".)
pub(crate) fn iso_parts(s: &str) -> Option<(&str, &str)> {
    let b = s.as_bytes();
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    let ok = b.len() >= 16
        && digits(0..4)
        && b[4] == b'-'
        && digits(5..7)
        && b[7] == b'-'
        && digits(8..10)
        && (b[10] == b'T' || b[10] == b' ')
        && digits(11..13)
        && b[13] == b':'
        && digits(14..16);
    // Every byte checked above is ASCII, so these slices sit on char boundaries.
    ok.then(|| (&s[5..10], &s[11..16]))
}

/// Whether a timestamp is explicitly UTC (`Z` / `+00:00`), so the label can say so.
fn is_utc(s: &str) -> bool {
    s.ends_with('Z') || s.ends_with("+00:00")
}

/// Provider free text flattened onto one line: every run of whitespace / control
/// characters becomes a single space (and the ends are trimmed). A host-written
/// maintenance note is arbitrary text — a `\n` in it would split the pod's row in two
/// (and break the column layout), and an ESC could smuggle terminal escape sequences.
fn one_line(s: &str) -> String {
    s.split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Truncate to `max` chars with an ellipsis, so free text can't blow up a table row.
/// (Also clips `pods test --deep`'s NOTES column.)
pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Shorten to at most `max` chars by cutting the **middle** (` … `), keeping the start and the
/// end. For a message whose last words are the instruction — setup's warnings end with what to
/// do (`… then re-run setup`) — where a plain [`clip`] cut exactly that (live).
pub(crate) fn clip_middle(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    const GAP: &str = " … ";
    let room = max.saturating_sub(GAP.chars().count());
    // The end carries the instruction: give it the larger share.
    let tail = room - room * 2 / 5;
    let head = room - tail;
    let start: String = s.chars().take(head).collect();
    let end: String = s.chars().skip(n - tail).collect();
    format!("{}{GAP}{}", start.trim_end(), end.trim_start())
}

/// The MAINT column: a compact host-maintenance window, e.g. `maint 10-09 02:00→06:00 UTC`
/// (the end's date is dropped when it's the same day), with the provider's note appended
/// (clipped). `-` when there's no window. Times are shown as the provider reports them;
/// unparseable strings are shown raw (clipped, flattened onto one line) instead of
/// guessed at.
pub fn maintenance_label(m: Option<&Maintenance>) -> String {
    let Some(m) = m else { return "-".to_string() };
    let nonempty = |o: &Option<String>| o.as_deref().map(one_line).filter(|s| !s.is_empty());
    let (start, end, note) = (nonempty(&m.start), nonempty(&m.end), nonempty(&m.note));
    if start.is_none() && end.is_none() && note.is_none() {
        return "-".to_string();
    }
    let compact = |s: &str| match iso_parts(s) {
        Some((d, t)) => format!("{d} {t}"),
        None => clip(s, 20),
    };
    let window = match (start.as_deref(), end.as_deref()) {
        (Some(s), Some(e)) => {
            let end = match (iso_parts(s), iso_parts(e)) {
                (Some((sd, _)), Some((ed, et))) if sd == ed => et.to_string(),
                _ => compact(e),
            };
            Some(format!("{}→{end}", compact(s)))
        }
        (Some(s), None) => Some(format!("{}→?", compact(s))),
        (None, Some(e)) => Some(format!("?→{}", compact(e))),
        (None, None) => None,
    };
    // Claim "UTC" only when every timestamp shown is a parsed ISO time marked UTC.
    let stamps: Vec<&str> = [start.as_deref(), end.as_deref()].into_iter().flatten().collect();
    let utc = !stamps.is_empty() && stamps.iter().all(|s| iso_parts(s).is_some() && is_utc(s));
    let mut out = String::from("maint");
    if let Some(w) = window {
        out.push(' ');
        out.push_str(&w);
        if utc {
            out.push_str(" UTC");
        }
    }
    if let Some(n) = note {
        out.push_str(" · ");
        out.push_str(&clip(&n, 40));
    }
    out
}

/// What the fleet is costing right now. Only billing pods count ([`bills_hourly`]: up or
/// coming up — not just `RUNNING`, since RunPod v2 bills `PROVISIONING`/`STARTING` pods too
/// — plus every existing Hetzner server); a stopped RunPod pod only bills storage, which
/// `costPerHr` doesn't describe. Hetzner's EUR is kept apart from the USD providers rather
/// than summed with a made-up exchange rate.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct FleetCost {
    /// Σ $/h over billing pods on USD providers (RunPod, Vast).
    pub usd_per_hr: f64,
    /// Σ €/h over billing Hetzner servers.
    pub eur_per_hr: f64,
    /// Billing pods, priced or not.
    pub billing: usize,
    /// Billing pods with a USD price.
    pub priced_usd: usize,
    /// Billing pods with a EUR price.
    pub priced_eur: usize,
    /// Billing pods whose provider reported no price — the totals undercount by these.
    pub unpriced: usize,
}

impl FleetCost {
    pub fn priced(&self) -> usize {
        self.priced_usd + self.priced_eur
    }
}

/// Sum the hourly cost of the billing pods (see [`FleetCost`]).
pub fn fleet_cost(pods: &[Pod]) -> FleetCost {
    let mut c = FleetCost::default();
    for p in pods.iter().filter(|p| bills_hourly(&p.provider, &p.status)) {
        c.billing += 1;
        match p.cost_per_hr {
            Some(v) if currency_symbol(&p.provider) == "€" => {
                c.eur_per_hr += v;
                c.priced_eur += 1;
            }
            Some(v) => {
                c.usd_per_hr += v;
                c.priced_usd += 1;
            }
            None => c.unpriced += 1,
        }
    }
    c
}

/// The one-line footer under `pods list`, e.g.
/// `fleet: $0.51/h across 3 billing pod(s) + €0.006/h hetzner (1 unpriced)`.
/// The `(N unpriced)` note makes it obvious when the total is a lower bound.
pub fn fleet_footer(c: &FleetCost) -> String {
    let mut out = format!("fleet: {}/h across {} billing pod(s)", fmt_money("$", c.usd_per_hr), c.billing);
    if c.priced_eur > 0 {
        out.push_str(&format!(" + {}/h hetzner", fmt_money("€", c.eur_per_hr)));
    }
    if c.unpriced > 0 {
        out.push_str(&format!(" ({} unpriced)", c.unpriced));
    }
    out
}

/// `pods list`'s footer: [`fleet_footer`], marked as a lower bound when some providers
/// didn't answer (`failed`) — their pods aren't in the table, so neither is their burn. Live
/// (finding #9), a bogus `VAST_API_KEY` showed `fleet: $0.61/h across 3 billing pod(s)` for a
/// fleet of 4 at $0.71/h, with nothing on that line saying a provider was missing. Pure.
pub fn listing_footer(c: &FleetCost, failed: &[String]) -> String {
    let line = fleet_footer(c);
    if failed.is_empty() {
        return line;
    }
    format!("{line} — PARTIAL: {} didn't answer, so its pods and their cost are missing", failed.join(", "))
}

/// What `pods list` prints instead of a table when nothing was listed — "(no pods)" only
/// when every provider answered; otherwise the fleet is unknown, not empty. Pure.
pub fn empty_listing_line(failed: &[String]) -> String {
    if failed.is_empty() {
        "(no pods)".to_string()
    } else {
        format!("(no pods listed — {} didn't answer, so this is NOT known to be empty)", failed.join(", "))
    }
}

/// The `pods list` columns, in order (MAINT last: it's the long, free-text one).
pub const POD_HEADERS: [&str; 8] = ["NAME", "PROVIDER", "ID", "STATUS", "GPU", "$/H", "ENDPOINT", "MAINT"];

/// How each of [`POD_HEADERS`] is aligned.
pub const POD_ALIGN: [Align; 8] =
    [Align::Left, Align::Left, Align::Left, Align::Left, Align::Left, Align::Right, Align::Left, Align::Left];

/// The STATUS column: the short status (`run`, `exit`…), plus ` locked` when the provider
/// reports the pod locked ([`crate::lock`]) — a glance at `pods list` says which pods refuse
/// stop, restart and terminate. A backend that doesn't report a lock adds nothing.
pub fn status_label(p: &Pod) -> String {
    let s = if crate::status::endpoint_overdue(p, crate::status::now_secs()) { "no-ssh".into() } else { crate::status::short_status(&p.status) };
    if crate::lock::is_locked(p) {
        format!("{s} locked")
    } else {
        s
    }
}

/// One pod's cells under [`POD_HEADERS`] — shared with `arena snapshot`'s table, which adds
/// its own columns, so a pod's labels read the same in both.
pub fn pod_cells(p: &Pod) -> Vec<String> {
    vec![
        p.name.clone(),
        p.provider.clone(),
        p.id.clone(),
        status_label(p),
        gpu_label(p),
        price_label(p),
        endpoint_label(p),
        maintenance_label(p.maintenance.as_ref()),
    ]
}

/// The `pods list` table: NAME PROVIDER ID STATUS GPU $/H ENDPOINT MAINT, one row per pod
/// in the given order (the caller sorts).
pub fn render_pods_table(pods: &[Pod]) -> String {
    render_pods_table_with(pods, None)
}

/// [`render_pods_table`] with the auto-reaper's `ENDS` column (`ends[i]` for `pods[i]`, from
/// `crate::reap::render::ends_label`) before MAINT — `None` = no column (the reaper keeps no
/// state here), so the table is unchanged for everyone not using it.
pub fn render_pods_table_with(pods: &[Pod], ends: Option<&[String]>) -> String {
    let Some(ends) = ends.filter(|e| e.len() == pods.len()) else {
        let rows: Vec<Vec<String>> = pods.iter().map(pod_cells).collect();
        return table::render(&POD_HEADERS, &POD_ALIGN, &rows);
    };
    let mut headers: Vec<&str> = POD_HEADERS.to_vec();
    headers.insert(headers.len() - 1, "ENDS");
    let mut align: Vec<Align> = POD_ALIGN.to_vec();
    align.insert(align.len() - 1, Align::Left);
    let rows: Vec<Vec<String>> = pods
        .iter()
        .zip(ends)
        .map(|(p, e)| {
            let mut c = pod_cells(p);
            c.insert(c.len() - 1, e.clone());
            c
        })
        .collect();
    table::render(&headers, &align, &rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(name: &str, provider: &str, status: &str) -> Pod {
        Pod {
            id: format!("id-{name}"),
            name: name.into(),
            provider: provider.into(),
            status: status.into(),
            ..Default::default()
        }
    }

    fn maint(start: Option<&str>, end: Option<&str>, note: Option<&str>) -> Maintenance {
        Maintenance {
            start: start.map(String::from),
            end: end.map(String::from),
            note: note.map(String::from),
        }
    }

    /// Never a signed zero (live: `$-0.00` from an empty sum); small amounts keep their
    /// third decimal.
    #[test]
    fn fmt_money_table() {
        let cases = [
            (0.0, "$0.00"),
            (-0.0, "$0.00"),
            (-0.004, "$0.00"),
            (0.006, "$0.006"),
            (0.0999, "$0.100"),
            (0.1, "$0.10"),
            (1.234, "$1.23"),
            (-0.5, "$-0.50"),
        ];
        for (x, want) in cases {
            assert_eq!(fmt_money("$", x), want, "{x}");
        }
        assert_eq!(fmt_money("€", -0.0), "€0.00");
        assert_eq!(fmt_money("$", [0.0f64; 0].iter().sum()), "$0.00", "an empty sum");
    }

    #[test]
    fn gpu_label_table() {
        let mk = |ty: Option<&str>, n: Option<u32>| Pod {
            gpu_type: ty.map(String::from),
            gpu_count: n,
            ..Default::default()
        };
        let cases: &[(Option<&str>, Option<u32>, &str)] = &[
            (Some("RTX A4000"), Some(1), "1×RTX A4000"),
            (Some("NVIDIA GeForce RTX 3090"), Some(2), "2×RTX 3090"), // normalized
            (Some("2×RTX A4000"), Some(1), "2×RTX A4000"),           // SSH probe wins as-is
            (Some("cx23"), None, "cx23"),                             // hetzner server type
            (Some("RTX A4000"), Some(0), "RTX A4000"),               // 0 = unknown count
            (None, Some(4), "4×GPU"),
            (Some("  "), None, "-"),
            (None, None, "-"),
        ];
        for (ty, n, want) in cases {
            assert_eq!(gpu_label(&mk(*ty, *n)), *want, "{ty:?} {n:?}");
        }
    }

    /// The ENDS column sits before MAINT, and only when given (one label per pod).
    #[test]
    fn ends_column_only_when_given() {
        let pods = vec![pod("devtest-a", "runpod", "RUNNING"), pod("devtest-b", "hetzner", "RUNNING")];
        let plain = render_pods_table(&pods);
        assert!(!plain.contains("ENDS"));
        let ends = vec!["18:00 (in 5h)".to_string(), "-".to_string()];
        let with = render_pods_table_with(&pods, Some(&ends));
        let header = with.lines().next().unwrap();
        assert!(header.find("ENDS").unwrap() < header.find("MAINT").unwrap(), "{header}");
        assert!(with.lines().nth(1).unwrap().contains("18:00 (in 5h)"), "{with}");
        assert_eq!(render_pods_table_with(&pods, Some(&ends[..1])), plain, "a mismatched column is left out");
    }

    #[test]
    fn status_label_says_locked_only_when_the_provider_does() {
        let mut p = pod("a", "runpod", "RUNNING");
        assert_eq!(status_label(&p), "run");
        p.locked = Some(false);
        assert_eq!(status_label(&p), "run");
        p.locked = Some(true);
        assert_eq!(status_label(&p), "run locked");
        p.status = "EXITED".into();
        assert_eq!(status_label(&p), "exit locked");
        assert_eq!(pod_cells(&p)[3], "exit locked");
    }

    #[test]
    fn price_and_endpoint_labels() {
        let mut p = pod("a", "runpod", "RUNNING");
        assert_eq!(price_label(&p), "-");
        p.cost_per_hr = Some(0.17);
        assert_eq!(price_label(&p), "$0.17");
        p.cost_per_hr = Some(1.0);
        assert_eq!(price_label(&p), "$1.00");
        let mut h = pod("h", "hetzner", "RUNNING");
        h.cost_per_hr = Some(0.0056);
        assert_eq!(price_label(&h), "€0.006"); // 3 decimals below 0.10, EUR symbol

        assert_eq!(endpoint_label(&p), "-");
        p.ssh_ip = Some("1.2.3.4".into());
        assert_eq!(endpoint_label(&p), "1.2.3.4:?");
        p.ssh_port = Some(10022);
        assert_eq!(endpoint_label(&p), "1.2.3.4:10022");
    }

    #[test]
    fn maintenance_label_table() {
        let cases: &[(Option<Maintenance>, &str)] = &[
            (None, "-"),
            (Some(maint(None, None, None)), "-"),
            (Some(maint(Some(""), Some("  "), None)), "-"),
            (
                Some(maint(Some("2026-10-09T02:00:00Z"), Some("2026-10-09T06:00:00Z"), None)),
                "maint 10-09 02:00→06:00 UTC",
            ),
            (
                Some(maint(Some("2026-10-09T22:00:00.000Z"), Some("2026-10-10T02:30:00.000Z"), None)),
                "maint 10-09 22:00→10-10 02:30 UTC",
            ),
            (
                // Non-UTC offset: shown as reported, no "UTC" claim.
                Some(maint(Some("2026-10-09 02:00:00+02:00"), None, None)),
                "maint 10-09 02:00→?",
            ),
            (Some(maint(None, Some("2026-10-09T06:00:00Z"), None)), "maint ?→10-09 06:00 UTC"),
            (
                // Mixed UTC / offset: no blanket "UTC" claim.
                Some(maint(Some("2026-10-09T02:00:00Z"), Some("2026-10-09T08:00:00+02:00"), None)),
                "maint 10-09 02:00→08:00",
            ),
            (
                Some(maint(Some("soon"), None, Some("host upgrade"))),
                "maint soon→? · host upgrade",
            ),
            (Some(maint(None, None, Some("GPU swap"))), "maint · GPU swap"),
            (
                Some(maint(None, None, Some("a very long maintenance note that keeps going and going"))),
                "maint · a very long maintenance note that keeps…",
            ),
            (
                // Host-written free text: newlines/tabs/control chars (incl. an ESC) are
                // flattened to single spaces so the row stays one table line.
                Some(maint(None, None, Some("Host reboot.\nPlease\tmigrate\r\n\x1b[31mnow"))),
                "maint · Host reboot. Please migrate [31mnow",
            ),
            (Some(maint(Some("\n\t"), None, Some(" \u{7}\n "))), "-"),
            (Some(maint(Some("next\ntuesday"), None, None)), "maint next tuesday→?"),
        ];
        for (m, want) in cases {
            assert_eq!(maintenance_label(m.as_ref()), *want, "{m:?}");
        }
    }

    #[test]
    fn fleet_cost_counts_billing_pods_only_and_splits_currency() {
        let priced = |mut p: Pod, c: f64| {
            p.cost_per_hr = Some(c);
            p
        };
        let pods = vec![
            priced(pod("a", "runpod", "RUNNING"), 0.17),
            priced(pod("b", "vast", "running"), 0.16), // case-insensitive status
            priced(pod("c", "runpod", "EXITED"), 0.50), // stopped: not billed per hour
            priced(pod("d", "hetzner", "RUNNING"), 0.0056),
            pod("e", "runpod", "RUNNING"), // no price reported
            priced(pod("f", "runpod", "STARTING"), 0.13), // v2: booting already bills
            priced(pod("g", "runpod", "PROVISIONING"), 0.20),
            priced(pod("h", "hetzner", "OFF"), 0.0056), // hetzner bills a powered-off server
            priced(pod("i", "vast", "STOPPED"), 0.40),
        ];
        let c = fleet_cost(&pods);
        assert!((c.usd_per_hr - 0.66).abs() < 1e-9, "{c:?}");
        assert!((c.eur_per_hr - 0.0112).abs() < 1e-9);
        assert_eq!((c.billing, c.priced_usd, c.priced_eur, c.unpriced), (7, 4, 2, 1));
        assert_eq!(c.priced(), 6);
        assert_eq!(fleet_footer(&c), "fleet: $0.66/h across 7 billing pod(s) + €0.011/h hetzner (1 unpriced)");
    }

    /// Live finding: an EXITED v2 pod showed `$0.13` in the $/H column (the total was
    /// right). A non-billing pod's price reads `-`; the raw cost stays on the pod (JSON).
    #[test]
    fn price_label_is_a_dash_for_non_billing_pods() {
        let priced = |provider: &str, status: &str| {
            let mut p = pod("x", provider, status);
            p.cost_per_hr = Some(0.13);
            p
        };
        for (provider, status, want) in [
            ("runpod", "RUNNING", "$0.13"),
            ("runpod", "STARTING", "$0.13"),
            ("runpod", "PROVISIONING", "$0.13"),
            ("runpod", "EXITED", "-"),
            ("runpod", "TERMINATED", "-"),
            ("vast", "STOPPED", "-"),
            ("vast", "LOADING", "$0.13"),
            ("hetzner", "OFF", "€0.13"),
            ("hetzner", "INITIALIZING", "€0.13"),
        ] {
            let p = priced(provider, status);
            assert_eq!(price_label(&p), want, "{provider} {status}");
            assert_eq!(p.cost_per_hr, Some(0.13));
        }
    }

    /// A pod up with no direct endpoint says so in the ENDPOINT column, and a stopped or
    /// proxy-only pod gets a reason of its own instead of "no SSH endpoint yet".
    #[test]
    fn endpoint_label_and_reason_name_proxy_only_and_stopped_pods() {
        let mut p = pod("devtest-alpha", "runpod", "RUNNING");
        assert_eq!((endpoint_label(&p), no_endpoint_reason(&p)), ("-".to_string(), None));
        p.proxy_only = true;
        assert_eq!(endpoint_label(&p), "no public endpoint (RunPod ssh proxy only)");
        assert!(no_endpoint_reason(&p).unwrap().contains("no public IP"));
        assert!(render_pods_table(std::slice::from_ref(&p)).contains("no public endpoint (RunPod ssh proxy only)"));
        let stopped = pod("devtest-bravo", "runpod", "EXITED");
        assert_eq!(no_endpoint_reason(&stopped).as_deref(), Some("stopped (EXITED) — no SSH while it's stopped"));
        assert!(no_endpoint_reason(&pod("devtest-h", "hetzner", "off")).unwrap().starts_with("stopped (off)"));
    }

    /// Finding #9: a listing some provider didn't answer says so on the total, and an empty
    /// one isn't "no pods".
    #[test]
    fn a_partial_listing_marks_its_footer_and_its_empty_line() {
        let c = FleetCost { usd_per_hr: 0.61, billing: 3, priced_usd: 3, ..Default::default() };
        assert_eq!(listing_footer(&c, &[]), "fleet: $0.61/h across 3 billing pod(s)");
        assert_eq!(
            listing_footer(&c, &["vast".to_string()]),
            "fleet: $0.61/h across 3 billing pod(s) — PARTIAL: vast didn't answer, so its pods and their cost are missing"
        );
        assert_eq!(empty_listing_line(&[]), "(no pods)");
        assert!(empty_listing_line(&["vast".into(), "hetzner".into()]).contains("vast, hetzner didn't answer"));
    }

    #[test]
    fn fleet_footer_plain_and_empty() {
        let c = FleetCost { usd_per_hr: 2.5, billing: 3, priced_usd: 3, ..Default::default() };
        assert_eq!(fleet_footer(&c), "fleet: $2.50/h across 3 billing pod(s)");
        assert_eq!(fleet_footer(&fleet_cost(&[])), "fleet: $0.00/h across 0 billing pod(s)");
    }

    /// Snapshot of the whole `pods list` table + footer for a mixed fleet, so a column
    /// reorder or label change is a deliberate, reviewed diff.
    #[test]
    fn pods_table_snapshot() {
        let pods = vec![
            Pod {
                id: "abc123".into(),
                name: "devtest-apple".into(),
                provider: "runpod".into(),
                status: "RUNNING".into(),
                gpu_type: Some("RTX A4000".into()),
                gpu_count: Some(1),
                cost_per_hr: Some(0.17),
                ssh_ip: Some("1.2.3.4".into()),
                ssh_port: Some(10022),
                maintenance: Some(maint(
                    Some("2026-10-09T02:00:00Z"),
                    Some("2026-10-09T06:00:00Z"),
                    None,
                )),
                machine_id: None,
                locked: None,
                proxy_only: false,
                disk_gb: None,
                volume_gb: None,
                delete_protected: None,
                created_at: None,
                started_at: None,
                identity: None,
                status_detail: None,
            },
            Pod {
                id: "def456".into(),
                name: "devtest-bloom".into(),
                provider: "runpod".into(),
                status: "EXITED".into(),
                gpu_count: Some(2),
                cost_per_hr: Some(0.34),
                ..Default::default()
            },
            Pod {
                id: "51234567".into(),
                name: "devtest-flutter".into(),
                provider: "hetzner".into(),
                status: "RUNNING".into(),
                gpu_type: Some("cx23".into()),
                cost_per_hr: Some(0.0056),
                ssh_ip: Some("5.6.7.8".into()),
                ssh_port: Some(22),
                ..Default::default()
            },
        ];
        let out = format!("{}{}\n", render_pods_table(&pods), fleet_footer(&fleet_cost(&pods)));
        let want = "\
NAME             PROVIDER  ID        STATUS  GPU             $/H  ENDPOINT       MAINT
devtest-apple    runpod    abc123    run     1×RTX A4000   $0.17  1.2.3.4:10022  maint 10-09 02:00→06:00 UTC
devtest-bloom    runpod    def456    exit    2×GPU             -  -              -
devtest-flutter  hetzner   51234567  run     cx23         €0.006  5.6.7.8:22     -
fleet: $0.17/h across 2 billing pod(s) + €0.006/h hetzner
";
        assert_eq!(out, want, "\n--- got ---\n{out}");
    }

    #[test]
    fn multiline_maintenance_note_keeps_the_table_one_line_per_pod() {
        let mut p = pod("devtest-apple", "runpod", "RUNNING");
        p.maintenance = Some(maint(None, None, Some("Host reboot.\nPlease migrate")));
        let out = render_pods_table(&[p]);
        assert_eq!(out.lines().count(), 2, "header + one row:\n{out}");
        assert!(out.ends_with("maint · Host reboot. Please migrate\n"), "{out}");
    }
}
