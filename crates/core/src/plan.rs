//! Scheduled provisioning plan: a day-by-day desired fleet with GPU/provider fallback.
//!
//! A plan (JSON, so it needs no extra deps and stays hand-editable) describes, per day,
//! how many pods of what shape to bring up, plus ordered **fallback chains** for GPU
//! type and provider/tier. When a day is applied, the executor walks the candidates
//! GPU-first — exhaust every provider/tier for the preferred GPU before downgrading —
//! filling to the target count and stepping to the next candidate when one is out of
//! capacity.
//!
//! This module is pure: parsing, candidate ordering, the night-window check, and
//! date resolution are all unit-tested. The impure parts (reading the wall clock,
//! actually creating pods, scheduling cron) live in the caller, behind dry-run/arming.

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::schedule::{days_from_civil, parse_ymd};

/// A whole provisioning plan: global fallback chains + caps + per-day entries.
#[derive(Debug, Clone, Deserialize)]
pub struct Plan {
    /// Provider/tier fallback order, e.g. `["runpod:community","runpod:secure","vast"]`.
    #[serde(default = "default_providers")]
    pub providers: Vec<String>,
    /// GPU fallback order, e.g. `["A4000","3090","A5000"]`.
    #[serde(default = "default_gpus")]
    pub gpus: Vec<String>,
    /// Hard cap: never run more than this many pods total (a typo can't overspend).
    pub max_total: Option<usize>,
    /// Hard cap: refuse if projected fleet $/hr would exceed this.
    pub max_hourly: Option<f64>,
    /// Hard cap on how many pods a single `replace` day may terminate.
    pub max_replace: Option<usize>,
    /// Local-time window `[start,end]` (e.g. `["00:00","06:00"]`) the scheduled apply
    /// is allowed to run in. Outside it, a scheduled run is a no-op.
    #[serde(default = "default_window")]
    pub window: [String; 2],
    #[serde(default)]
    pub days: Vec<DayPlan>,
}

/// One day's desired fleet.
#[derive(Debug, Clone, Deserialize)]
pub struct DayPlan {
    /// `YYYY-MM-DD`. If absent, `offset` days from "today" is used.
    pub date: Option<String>,
    /// Days from today (0 = today, 1 = tomorrow). Ignored if `date` is set.
    pub offset: Option<i64>,
    /// Target number of pods.
    pub count: usize,
    /// GPUs per pod.
    #[serde(default = "one")]
    pub gpus_per_pod: u32,
    /// If true, terminate the existing fleet (after a backup) before creating.
    #[serde(default)]
    pub replace: bool,
    /// Per-day GPU fallback override (else the plan-level `gpus`).
    pub gpus: Option<Vec<String>>,
    /// Per-day provider fallback override (else the plan-level `providers`).
    pub providers: Option<Vec<String>>,
}

/// One concrete thing to try creating: a GPU on a provider/tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub gpu: String,
    pub provider: String,
    /// Cloud tier for providers that have one (RunPod COMMUNITY/SECURE); None otherwise.
    pub cloud: Option<String>,
}

fn default_providers() -> Vec<String> {
    vec!["runpod:community".into(), "runpod:secure".into(), "vast".into()]
}
fn default_gpus() -> Vec<String> {
    vec!["A4000".into()]
}
fn default_window() -> [String; 2] {
    ["00:00".into(), "06:00".into()]
}
fn one() -> u32 {
    1
}

/// Split a `"provider:tier"` token into `(provider, Some(TIER))`, or `(provider, None)`.
pub fn parse_tier(s: &str) -> (String, Option<String>) {
    match s.split_once(':') {
        Some((p, c)) => (p.trim().to_string(), Some(c.trim().to_uppercase())),
        None => (s.trim().to_string(), None),
    }
}

/// `"HH:MM"` to minutes-since-midnight, validated.
pub fn parse_hm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    (h <= 23 && m <= 59).then_some(h * 60 + m)
}

/// Is `now` (minutes since local midnight) inside `[start,end)`? Handles a window that
/// crosses midnight (start > end).
pub fn within_window(now: u32, start: u32, end: u32) -> bool {
    if start <= end {
        now >= start && now < end
    } else {
        now >= start || now < end
    }
}

impl Plan {
    pub fn parse(json: &str) -> Result<Self> {
        serde_json::from_str(json).map_err(|e| Error::Config(format!("parsing plan: {e}")))
    }

    pub fn load(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("reading plan {}: {e}", path.display())))?;
        Self::parse(&text)
    }

    /// The window as `(start, end)` minutes, if both parse.
    pub fn window_minutes(&self) -> Option<(u32, u32)> {
        Some((parse_hm(&self.window[0])?, parse_hm(&self.window[1])?))
    }

    /// The day entry effective for `target_days`, with `offset`s resolved against the
    /// *real* `today_days` (so `offset:1` always means tomorrow-from-now, regardless of
    /// which date we're previewing). For a live apply, `target_days == today_days`.
    pub fn day_for(&self, today_days: i64, target_days: i64) -> Option<&DayPlan> {
        self.days.iter().find(|d| d.effective_days(today_days) == Some(target_days))
    }
}

impl DayPlan {
    /// The day's effective days-from-epoch: from `date`, else `today_days + offset`.
    pub fn effective_days(&self, today_days: i64) -> Option<i64> {
        match &self.date {
            Some(d) => {
                let (y, m, dd) = parse_ymd(d)?;
                Some(days_from_civil(y, m, dd))
            }
            None => Some(today_days + self.offset.unwrap_or(0)),
        }
    }

    pub fn gpus<'a>(&'a self, plan: &'a Plan) -> &'a [String] {
        self.gpus.as_deref().unwrap_or(&plan.gpus)
    }

    pub fn providers<'a>(&'a self, plan: &'a Plan) -> &'a [String] {
        self.providers.as_deref().unwrap_or(&plan.providers)
    }

    /// The ordered create candidates, **GPU-first**: for each GPU in preference order,
    /// every provider/tier in order, before moving to the next GPU.
    pub fn candidates(&self, plan: &Plan) -> Vec<Candidate> {
        let mut out = Vec::new();
        for gpu in self.gpus(plan) {
            for p in self.providers(plan) {
                let (provider, cloud) = parse_tier(p);
                out.push(Candidate { gpu: gpu.clone(), provider, cloud });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "providers": ["runpod:community", "runpod:secure", "vast"],
        "gpus": ["A4000", "3090"],
        "max_total": 20,
        "window": ["00:00", "06:00"],
        "days": [
            { "date": "2026-06-03", "count": 15, "gpus_per_pod": 1 },
            { "offset": 1, "count": 15, "gpus_per_pod": 2, "replace": true }
        ]
    }"#;

    #[test]
    fn parses_and_resolves_days() {
        let plan = Plan::parse(SAMPLE).unwrap();
        assert_eq!(plan.max_total, Some(20));
        let today = days_from_civil(2026, 6, 3);
        let d = plan.day_for(today, today).expect("today's entry");
        assert_eq!(d.count, 15);
        assert_eq!(d.gpus_per_pod, 1);
        assert!(!d.replace);
        // The offset:1 entry (resolved against real today) is for tomorrow.
        let tomorrow = plan.day_for(today, today + 1).expect("tomorrow's entry");
        assert!(tomorrow.replace);
        assert_eq!(tomorrow.gpus_per_pod, 2);
        // A date with no entry -> none.
        assert!(plan.day_for(today, today + 5).is_none());
    }

    #[test]
    fn candidates_are_gpu_first() {
        let plan = Plan::parse(SAMPLE).unwrap();
        let today = days_from_civil(2026, 6, 3);
        let c = plan.day_for(today, today).unwrap().candidates(&plan);
        // A4000 across all tiers, THEN 3090 across all tiers.
        assert_eq!(c[0], Candidate { gpu: "A4000".into(), provider: "runpod".into(), cloud: Some("COMMUNITY".into()) });
        assert_eq!(c[1].cloud, Some("SECURE".into()));
        assert_eq!(c[2], Candidate { gpu: "A4000".into(), provider: "vast".into(), cloud: None });
        assert_eq!(c[3], Candidate { gpu: "3090".into(), provider: "runpod".into(), cloud: Some("COMMUNITY".into()) });
        assert_eq!(c.len(), 6);
    }

    #[test]
    fn tier_parsing() {
        assert_eq!(parse_tier("runpod:secure"), ("runpod".into(), Some("SECURE".into())));
        assert_eq!(parse_tier("vast"), ("vast".into(), None));
    }

    #[test]
    fn window_minutes_and_membership() {
        assert_eq!(parse_hm("00:00"), Some(0));
        assert_eq!(parse_hm("06:30"), Some(390));
        assert_eq!(parse_hm("24:00"), None);
        // 00:00–06:00 night window
        assert!(within_window(0, 0, 360));
        assert!(within_window(359, 0, 360));
        assert!(!within_window(360, 0, 360)); // end exclusive
        assert!(!within_window(720, 0, 360)); // noon -> never
        // crossing midnight (22:00–02:00)
        assert!(within_window(23 * 60, 22 * 60, 2 * 60));
        assert!(within_window(60, 22 * 60, 2 * 60));
        assert!(!within_window(12 * 60, 22 * 60, 2 * 60));
    }

    #[test]
    fn defaults_when_minimal() {
        let plan = Plan::parse(r#"{ "days": [] }"#).unwrap();
        assert_eq!(plan.providers, default_providers());
        assert_eq!(plan.gpus, default_gpus());
        assert_eq!(plan.window, default_window());
    }
}
