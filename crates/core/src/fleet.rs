//! Fleet-level presentation: per-pod display labels (GPU, $/h, endpoint, maintenance) and
//! the fleet cost summary shown under `pods list`.
//!
//! Kept pure and provider-agnostic on purpose: the CLI renders it today, and the TUI
//! (Phase 3) and the public snapshot (Phase 4) are meant to reuse these exact functions so
//! a pod reads the same on every surface — no surface re-derives a label on its own.

use crate::metrics::normalize_gpu_name;
use crate::pod::{Maintenance, Pod};
use crate::status::short_status;
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
/// (Hetzner cx23 ≈ €0.006/h) doesn't round to a misleading `0.01` or `0.00`.
pub fn fmt_money(symbol: &str, amount: f64) -> String {
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

/// The $/H column: the pod's hourly price in its provider's currency, or `-` if unknown.
pub fn price_label(pod: &Pod) -> String {
    pod.cost_per_hr
        .map(|c| fmt_money(currency_symbol(&pod.provider), c))
        .unwrap_or_else(|| "-".to_string())
}

/// The ENDPOINT column: the pod's current direct SSH endpoint `ip:port`. An IP without a
/// port mapping yet (pod still booting) shows `ip:?` — not reachable, but not nothing.
pub fn endpoint_label(pod: &Pod) -> String {
    match (pod.ssh_ip.as_deref().filter(|s| !s.is_empty()), pod.ssh_port) {
        (Some(ip), Some(port)) => format!("{ip}:{port}"),
        (Some(ip), None) => format!("{ip}:?"),
        _ => "-".to_string(),
    }
}

/// Split an ISO-8601-ish timestamp (`2026-10-09T02:00:00Z`, or with a space instead of
/// `T`) into its compact `("MM-DD", "HH:MM")` parts. `None` for anything else, so an
/// unexpected format is shown raw rather than mangled.
fn iso_parts(s: &str) -> Option<(&str, &str)> {
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

/// Truncate to `max` chars with an ellipsis, so free text can't blow up a table row.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// The MAINT column: a compact host-maintenance window, e.g. `maint 10-09 02:00→06:00 UTC`
/// (the end's date is dropped when it's the same day), with the provider's note appended
/// (clipped). `-` when there's no window. Times are shown as the provider reports them;
/// unparseable strings are shown raw (clipped) instead of guessed at.
pub fn maintenance_label(m: Option<&Maintenance>) -> String {
    let Some(m) = m else { return "-".to_string() };
    let nonempty = |o: &Option<String>| o.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
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

/// What the fleet is costing right now. Only pods whose status is `RUNNING` count (that's
/// what bills GPU time; a stopped RunPod pod only bills storage, which `costPerHr` doesn't
/// describe). Hetzner's EUR is kept apart from the USD providers rather than summed with a
/// made-up exchange rate.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FleetCost {
    /// Σ $/h over RUNNING pods on USD providers (RunPod, Vast).
    pub usd_per_hr: f64,
    /// Σ €/h over RUNNING Hetzner servers.
    pub eur_per_hr: f64,
    /// RUNNING pods, priced or not.
    pub running: usize,
    /// RUNNING pods with a USD price.
    pub priced_usd: usize,
    /// RUNNING pods with a EUR price.
    pub priced_eur: usize,
    /// RUNNING pods whose provider reported no price — the totals undercount by these.
    pub unpriced: usize,
}

impl FleetCost {
    pub fn priced(&self) -> usize {
        self.priced_usd + self.priced_eur
    }
}

/// Sum the hourly cost of the RUNNING pods (see [`FleetCost`]).
pub fn fleet_cost(pods: &[Pod]) -> FleetCost {
    let mut c = FleetCost::default();
    for p in pods.iter().filter(|p| p.status.eq_ignore_ascii_case("RUNNING")) {
        c.running += 1;
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
/// `fleet: $0.51/h across 3 running pod(s) + €0.006/h hetzner (1 unpriced)`.
/// The `(N unpriced)` note makes it obvious when the total is a lower bound.
pub fn fleet_footer(c: &FleetCost) -> String {
    let mut out = format!("fleet: {}/h across {} running pod(s)", fmt_money("$", c.usd_per_hr), c.running);
    if c.priced_eur > 0 {
        out.push_str(&format!(" + {}/h hetzner", fmt_money("€", c.eur_per_hr)));
    }
    if c.unpriced > 0 {
        out.push_str(&format!(" ({} unpriced)", c.unpriced));
    }
    out
}

/// The `pods list` table: NAME PROVIDER ID STATUS GPU $/H ENDPOINT MAINT, one row per pod
/// in the given order (the caller sorts).
pub fn render_pods_table(pods: &[Pod]) -> String {
    use Align::{Left, Right};
    let rows: Vec<Vec<String>> = pods
        .iter()
        .map(|p| {
            vec![
                p.name.clone(),
                p.provider.clone(),
                p.id.clone(),
                short_status(&p.status),
                gpu_label(p),
                price_label(p),
                endpoint_label(p),
                maintenance_label(p.maintenance.as_ref()),
            ]
        })
        .collect();
    table::render(
        &["NAME", "PROVIDER", "ID", "STATUS", "GPU", "$/H", "ENDPOINT", "MAINT"],
        &[Left, Left, Left, Left, Left, Right, Left, Left],
        &rows,
    )
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
        ];
        for (m, want) in cases {
            assert_eq!(maintenance_label(m.as_ref()), *want, "{m:?}");
        }
    }

    #[test]
    fn fleet_cost_counts_running_only_and_splits_currency() {
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
        ];
        let c = fleet_cost(&pods);
        assert!((c.usd_per_hr - 0.33).abs() < 1e-9, "{c:?}");
        assert!((c.eur_per_hr - 0.0056).abs() < 1e-9);
        assert_eq!((c.running, c.priced_usd, c.priced_eur, c.unpriced), (4, 2, 1, 1));
        assert_eq!(c.priced(), 3);
        assert_eq!(fleet_footer(&c), "fleet: $0.33/h across 4 running pod(s) + €0.006/h hetzner (1 unpriced)");
    }

    #[test]
    fn fleet_footer_plain_and_empty() {
        let c = FleetCost { usd_per_hr: 2.5, running: 3, priced_usd: 3, ..Default::default() };
        assert_eq!(fleet_footer(&c), "fleet: $2.50/h across 3 running pod(s)");
        assert_eq!(fleet_footer(&fleet_cost(&[])), "fleet: $0.00/h across 0 running pod(s)");
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
devtest-bloom    runpod    def456    exit    2×GPU         $0.34  -              -
devtest-flutter  hetzner   51234567  run     cx23         €0.006  5.6.7.8:22     -
fleet: $0.17/h across 2 running pod(s) + €0.006/h hetzner
";
        assert_eq!(out, want, "\n--- got ---\n{out}");
    }
}
