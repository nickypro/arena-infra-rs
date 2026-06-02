//! Pure, unit-tested state for the interactive dashboard: per-pod sample history
//! (for sparklines), the fleet-wide summary, and the rules governing which actions
//! exist and how an action is confirmed. None of this does any IO — `main.rs` owns
//! the terminal, the provider calls, and the SSH; this module just decides *what*
//! should be shown and *when* a confirmation is satisfied, so the risky bits (e.g.
//! "a destructive action needs the exact pod name typed back") are testable offline.

use std::collections::{HashMap, VecDeque};

use arena_core::metrics::PodMetrics;
use arena_core::Pod;

/// How many recent samples we keep per pod for the sparklines.
pub const HISTORY_LEN: usize = 60;

/// A rolling window of recent util/temp samples for one pod. Missing readings (pod
/// unreachable, no GPU) are recorded as 0 so the sparkline keeps a steady time axis
/// rather than collapsing gaps.
#[derive(Debug, Clone, Default)]
pub struct History {
    pub util: VecDeque<u64>,
    pub temp: VecDeque<u64>,
}

impl History {
    /// Append one refresh's reading, evicting the oldest beyond [`HISTORY_LEN`].
    pub fn push(&mut self, util: Option<u32>, temp: Option<u32>) {
        push_capped(&mut self.util, util.unwrap_or(0) as u64);
        push_capped(&mut self.temp, temp.unwrap_or(0) as u64);
    }

    pub fn util_data(&self) -> Vec<u64> {
        self.util.iter().copied().collect()
    }

    pub fn temp_data(&self) -> Vec<u64> {
        self.temp.iter().copied().collect()
    }
}

fn push_capped(q: &mut VecDeque<u64>, v: u64) {
    q.push_back(v);
    while q.len() > HISTORY_LEN {
        q.pop_front();
    }
}

/// At-a-glance numbers across the whole fleet, for the summary bar.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FleetSummary {
    pub pods: usize,
    /// Pods reporting at least one GPU reading.
    pub reporting: usize,
    /// Pods whose metric fetch errored (down / no nvidia-smi / unreachable).
    pub unreachable: usize,
    pub total_gpus: usize,
    /// Mean utilization across *every* GPU in the fleet (not a mean of means).
    pub mean_util: Option<u32>,
    pub mem_used_mb: u32,
    pub mem_total_mb: u32,
    /// Summed $/hr over pods that report a cost.
    pub total_cost: f64,
}

/// Compute the fleet summary from the current pods + their metrics.
pub fn summarize(pods: &[Pod], metrics: &HashMap<String, PodMetrics>) -> FleetSummary {
    let mut s = FleetSummary {
        pods: pods.len(),
        ..Default::default()
    };
    let mut util_sum = 0u64;
    let mut util_n = 0u64;
    for pod in pods {
        if let Some(c) = pod.cost_per_hr {
            s.total_cost += c;
        }
        let Some(m) = metrics.get(&pod.name) else { continue };
        if m.error.is_some() {
            s.unreachable += 1;
        }
        if !m.gpus.is_empty() {
            s.reporting += 1;
        }
        s.total_gpus += m.gpus.len();
        for g in &m.gpus {
            if let Some(u) = g.util_pct {
                util_sum += u as u64;
                util_n += 1;
            }
        }
        if let Some((u, t)) = m.mem_summary() {
            s.mem_used_mb += u;
            s.mem_total_mb += t;
        }
    }
    if util_n > 0 {
        s.mean_util = Some((util_sum / util_n) as u32);
    }
    s
}

/// The actions a user can trigger against the selected pod from the dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Restart,
    Stop,
    Terminate,
    Backup,
    Setup,
}

impl Action {
    /// The action-menu entries, in display order, with their selector keys.
    pub const MENU: &'static [(char, Action)] = &[
        ('r', Action::Restart),
        ('s', Action::Stop),
        ('t', Action::Terminate),
        ('b', Action::Backup),
        ('p', Action::Setup),
    ];

    pub fn from_key(c: char) -> Option<Action> {
        Self::MENU.iter().find(|(k, _)| *k == c).map(|(_, a)| *a)
    }

    pub fn label(&self) -> &'static str {
        match self {
            Action::Restart => "restart",
            Action::Stop => "stop",
            Action::Terminate => "terminate",
            Action::Backup => "backup",
            Action::Setup => "setup",
        }
    }

    /// Lifecycle actions change/destroy a running machine, so they require the user
    /// to type the pod's exact name back before they can be applied. Backup/setup
    /// only touch the in-pod git tree, so a single `y` confirm is enough.
    pub fn requires_typed_name(&self) -> bool {
        matches!(self, Action::Restart | Action::Stop | Action::Terminate)
    }

    /// Whether this action is irreversible (drives the warning styling).
    pub fn is_destructive(&self) -> bool {
        matches!(self, Action::Terminate)
    }
}

/// A pending confirmation for an action against a specific pod.
#[derive(Debug, Clone)]
pub struct Confirm {
    pub action: Action,
    pub pod_name: String,
    pub pod_id: String,
    /// What the user has typed so far (only used for typed-name confirmations).
    pub typed: String,
    /// The exact command(s) that will run, shown for backup/setup so the operator
    /// sees precisely what's about to execute.
    pub preview: Option<String>,
}

impl Confirm {
    pub fn new(action: Action, pod_name: String, pod_id: String, preview: Option<String>) -> Self {
        Self { action, pod_name, pod_id, typed: String::new(), preview }
    }

    /// Is the confirmation satisfied enough to apply? Typed-name actions need an exact
    /// match of the pod name; others are satisfied as soon as the user hits the
    /// confirm key (handled in `main.rs`).
    pub fn is_satisfied(&self) -> bool {
        if self.action.requires_typed_name() {
            self.typed == self.pod_name
        } else {
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena_core::metrics::{parse_nvidia_smi, PodMetrics};

    fn pod(name: &str, cost: Option<f64>) -> Pod {
        Pod {
            id: format!("id-{name}"),
            name: name.into(),
            provider: "runpod".into(),
            status: "RUNNING".into(),
            gpu_type: Some("RTX 4090".into()),
            cost_per_hr: cost,
            ssh_ip: Some("1.2.3.4".into()),
            ssh_port: Some(22001),
        }
    }

    #[test]
    fn history_caps_and_pads_missing() {
        let mut h = History::default();
        for _ in 0..(HISTORY_LEN + 5) {
            h.push(Some(50), None); // temp missing -> recorded as 0
        }
        assert_eq!(h.util.len(), HISTORY_LEN);
        assert_eq!(h.temp.len(), HISTORY_LEN);
        assert_eq!(*h.temp.back().unwrap(), 0);
        assert_eq!(*h.util.back().unwrap(), 50);
    }

    #[test]
    fn summary_aggregates_across_fleet() {
        let pods = vec![pod("arena8-apple", Some(0.40)), pod("arena8-luna", Some(0.69))];
        let mut metrics = HashMap::new();
        metrics.insert(
            "arena8-apple".to_string(),
            PodMetrics { gpus: parse_nvidia_smi("100, 8000, 16000, 60\n"), ..Default::default() },
        );
        metrics.insert(
            "arena8-luna".to_string(),
            PodMetrics { gpus: parse_nvidia_smi("0, 1000, 16000, 40\n"), ..Default::default() },
        );
        let s = summarize(&pods, &metrics);
        assert_eq!(s.pods, 2);
        assert_eq!(s.reporting, 2);
        assert_eq!(s.total_gpus, 2);
        assert_eq!(s.mean_util, Some(50)); // (100 + 0) / 2 GPUs
        assert_eq!((s.mem_used_mb, s.mem_total_mb), (9000, 32000));
        assert!((s.total_cost - 1.09).abs() < 1e-9);
    }

    #[test]
    fn summary_counts_unreachable() {
        let pods = vec![pod("arena8-apple", None)];
        let mut metrics = HashMap::new();
        metrics.insert(
            "arena8-apple".to_string(),
            PodMetrics { error: Some("connection refused".into()), ..Default::default() },
        );
        let s = summarize(&pods, &metrics);
        assert_eq!(s.unreachable, 1);
        assert_eq!(s.reporting, 0);
        assert_eq!(s.mean_util, None);
    }

    #[test]
    fn lifecycle_actions_need_typed_name() {
        let mut c = Confirm::new(Action::Terminate, "arena8-apple".into(), "id".into(), None);
        assert!(!c.is_satisfied());
        c.typed = "arena8-appl".into();
        assert!(!c.is_satisfied()); // partial doesn't count
        c.typed = "arena8-apple".into();
        assert!(c.is_satisfied());
    }

    #[test]
    fn safe_actions_are_satisfied_immediately() {
        let c = Confirm::new(Action::Backup, "arena8-apple".into(), "id".into(), Some("git ...".into()));
        assert!(c.is_satisfied());
        assert!(!Action::Backup.requires_typed_name());
        assert!(Action::Terminate.is_destructive());
        assert!(!Action::Restart.is_destructive());
    }

    #[test]
    fn menu_keys_map_to_actions() {
        assert_eq!(Action::from_key('t'), Some(Action::Terminate));
        assert_eq!(Action::from_key('z'), None);
    }
}
