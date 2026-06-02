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
    /// GPU memory usage as a percentage of total.
    pub mem: VecDeque<u64>,
}

impl History {
    /// Append one refresh's reading, evicting the oldest beyond [`HISTORY_LEN`].
    pub fn push(&mut self, util: Option<u32>, temp: Option<u32>, mem_pct: Option<u32>) {
        push_capped(&mut self.util, util.unwrap_or(0) as u64);
        push_capped(&mut self.temp, temp.unwrap_or(0) as u64);
        push_capped(&mut self.mem, mem_pct.unwrap_or(0) as u64);
    }

    pub fn util_data(&self) -> Vec<u64> {
        self.util.iter().copied().collect()
    }

    pub fn temp_data(&self) -> Vec<u64> {
        self.temp.iter().copied().collect()
    }

    pub fn mem_data(&self) -> Vec<u64> {
        self.mem.iter().copied().collect()
    }
}

fn push_capped(q: &mut VecDeque<u64>, v: u64) {
    q.push_back(v);
    while q.len() > HISTORY_LEN {
        q.pop_front();
    }
}

/// Render the last `width` values of `data` (each a 0–100 percentage) as a one-line
/// block-eighths sparkline (`▁▂▃▄▅▆▇█`), left-padded with spaces if there's less
/// history than `width`. Empty history renders all spaces.
pub fn spark(data: &[u64], width: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let start = data.len().saturating_sub(width);
    let slice = &data[start..];
    let mut s = String::with_capacity(width);
    for _ in 0..width.saturating_sub(slice.len()) {
        s.push(' ');
    }
    for &v in slice {
        // Map 0..=100 to one of 8 bar heights (rounded).
        let level = ((v.min(100) * 7 + 50) / 100) as usize;
        s.push(BARS[level.min(7)]);
    }
    s
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

/// How a pod name is shown: full (`arena8-apple`) or short (`apple`). Stripping the
/// `{prefix}-` is purely cosmetic — the canonical name is still used for provider calls.
pub fn display_name(full: &str, prefix: &str, short: bool) -> String {
    if short {
        full.strip_prefix(&format!("{prefix}-")).unwrap_or(full).to_string()
    } else {
        full.to_string()
    }
}

/// Shorten an autocommit branch to just its iteration label: an
/// `autocommit-{prefix}-w1d2-apple` becomes `w1d2` (the machine name is already in the
/// NAME column). Any other branch (`main`, a feature branch, …) is returned unchanged.
pub fn short_branch(branch: &str, prefix: &str) -> String {
    match branch.strip_prefix(&format!("autocommit-{prefix}-")) {
        // rest is "w1d2-apple" -> take the part before the trailing "-<machine>".
        Some(rest) => rest.rsplit_once('-').map(|(label, _)| label.to_string()).unwrap_or_else(|| rest.to_string()),
        None => branch.to_string(),
    }
}

/// One provider choice in the add-pod form, with whether its API key is configured.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderOpt {
    pub name: String,
    pub available: bool,
}

/// The editable fields of the add-pod form, in display order. Which ones apply depends
/// on the chosen provider (no cloud type except RunPod; no GPU fields on Hetzner).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NpField {
    Provider,
    CloudType,
    GpuType,
    GpuCount,
    Disk,
    Volume,
    Pods,
}

/// State of the interactive add-pod form: `↑↓` moves between [`NpField`]s, `←→` changes
/// the selected field's value. All pure — `main` owns the provider build / listing.
#[derive(Debug, Clone)]
pub struct NewPodForm {
    pub providers: Vec<ProviderOpt>,
    pub provider_idx: usize,
    pub cloud_types: Vec<String>,
    pub cloud_idx: usize,
    pub gpu_types: Vec<String>,
    pub gpu_idx: usize,
    pub gpu_count: u32,
    /// Container disk size in GB (adjusted in 50GB steps) — the main storage knob.
    pub disk_gb: u32,
    /// Persistent volume size in GB (0 = none, the default); 100GB steps.
    pub volume_gb: u32,
    pub count: usize,
    /// Free machine names available on the selected provider (for the count cap).
    pub free: Vec<String>,
    /// Index into [`Self::fields`] of the currently-selected field.
    pub field: usize,
}

fn wrap(idx: usize, delta: i32, len: usize) -> usize {
    if len == 0 {
        0
    } else {
        (idx as i32 + delta).rem_euclid(len as i32) as usize
    }
}

impl NewPodForm {
    pub fn provider_name(&self) -> &str {
        self.providers.get(self.provider_idx).map(|p| p.name.as_str()).unwrap_or("runpod")
    }

    fn is_runpod(&self) -> bool {
        self.provider_name() == "runpod"
    }

    /// GPU providers take GPU type/count; Hetzner is CPU-only.
    fn is_gpu(&self) -> bool {
        self.provider_name() != "hetzner"
    }

    /// The fields that apply to the current provider, in display order.
    pub fn fields(&self) -> Vec<NpField> {
        let mut f = vec![NpField::Provider];
        if self.is_runpod() {
            f.push(NpField::CloudType);
        }
        if self.is_gpu() {
            f.push(NpField::GpuType);
            f.push(NpField::GpuCount);
            f.push(NpField::Disk);
            f.push(NpField::Volume);
        }
        f.push(NpField::Pods);
        f
    }

    pub fn selected(&self) -> NpField {
        let f = self.fields();
        f[self.field.min(f.len() - 1)]
    }

    pub fn move_field(&mut self, delta: i32) {
        let n = self.fields().len();
        self.field = wrap(self.field, delta, n);
    }

    /// Change the selected field's value by `delta` (−1 left / +1 right). Returns true
    /// if the *provider* changed, since the caller must then re-list free names.
    pub fn change(&mut self, delta: i32) -> bool {
        match self.selected() {
            NpField::Provider => {
                self.cycle_provider(delta);
                // The field set may have shrunk (e.g. → Hetzner); keep `field` valid.
                let n = self.fields().len();
                self.field = self.field.min(n - 1);
                return true;
            }
            NpField::CloudType => self.cloud_idx = wrap(self.cloud_idx, delta, self.cloud_types.len()),
            NpField::GpuType => self.gpu_idx = wrap(self.gpu_idx, delta, self.gpu_types.len()),
            NpField::GpuCount => self.gpu_count = (self.gpu_count as i32 + delta).clamp(1, 8) as u32,
            // Container disk in 50GB steps, 20–2000GB.
            NpField::Disk => self.disk_gb = (self.disk_gb as i32 + delta * 50).clamp(20, 2000) as u32,
            // Volume in 100GB steps, 0 (none) up to 2000GB.
            NpField::Volume => self.volume_gb = (self.volume_gb as i32 + delta * 100).clamp(0, 2000) as u32,
            NpField::Pods => {
                let max = self.free.len().max(1) as i32;
                self.count = (self.count as i32 + delta).clamp(1, max) as usize;
            }
        }
        false
    }

    /// Step to the next available provider in `delta`'s direction, skipping any whose
    /// API key isn't configured (so you can't select an unusable provider).
    fn cycle_provider(&mut self, delta: i32) {
        let n = self.providers.len() as i32;
        if n == 0 {
            return;
        }
        let step = if delta < 0 { -1 } else { 1 };
        let mut idx = self.provider_idx as i32;
        for _ in 0..n {
            idx = (idx + step).rem_euclid(n);
            if self.providers[idx as usize].available {
                self.provider_idx = idx as usize;
                return;
            }
        }
    }

    /// The selected cloud type, only meaningful for RunPod.
    pub fn cloud_type(&self) -> Option<&str> {
        if self.is_runpod() {
            self.cloud_types.get(self.cloud_idx).map(String::as_str)
        } else {
            None
        }
    }

    pub fn gpu_type(&self) -> &str {
        self.gpu_types.get(self.gpu_idx).map(String::as_str).unwrap_or("")
    }

    /// The names that would be created for the current count.
    pub fn planned_names(&self) -> Vec<String> {
        self.free.iter().take(self.count).cloned().collect()
    }
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
            h.push(Some(50), None, Some(10)); // temp missing -> recorded as 0
        }
        assert_eq!(h.util.len(), HISTORY_LEN);
        assert_eq!(h.temp.len(), HISTORY_LEN);
        assert_eq!(h.mem.len(), HISTORY_LEN);
        assert_eq!(*h.temp.back().unwrap(), 0);
        assert_eq!(*h.util.back().unwrap(), 50);
        assert_eq!(*h.mem.back().unwrap(), 10);
    }

    #[test]
    fn spark_pads_and_scales() {
        // Empty -> all spaces.
        assert_eq!(spark(&[], 4), "    ");
        // Left-padded when shorter than width.
        let s = spark(&[100], 3);
        assert_eq!(s.chars().count(), 3);
        assert!(s.starts_with("  "));
        assert!(s.ends_with('█')); // 100 -> full bar
        // 0 -> lowest bar.
        assert_eq!(spark(&[0], 1), "▁");
    }

    #[test]
    fn summary_aggregates_across_fleet() {
        let pods = vec![pod("arena8-apple", Some(0.40)), pod("arena8-luna", Some(0.69))];
        let mut metrics = HashMap::new();
        metrics.insert(
            "arena8-apple".to_string(),
            PodMetrics { gpus: parse_nvidia_smi("RTX 3090, 100, 8000, 16000, 60\n"), ..Default::default() },
        );
        metrics.insert(
            "arena8-luna".to_string(),
            PodMetrics { gpus: parse_nvidia_smi("RTX 3090, 0, 1000, 16000, 40\n"), ..Default::default() },
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

    #[test]
    fn display_name_strips_prefix_only_when_short() {
        assert_eq!(display_name("arena8-apple", "arena8", true), "apple");
        assert_eq!(display_name("arena8-apple", "arena8", false), "arena8-apple");
        // a name without the prefix is left as-is
        assert_eq!(display_name("apple", "arena8", true), "apple");
    }

    fn form() -> NewPodForm {
        NewPodForm {
            providers: vec![
                ProviderOpt { name: "runpod".into(), available: true },
                ProviderOpt { name: "vast".into(), available: false },
                ProviderOpt { name: "hetzner".into(), available: true },
            ],
            provider_idx: 0,
            cloud_types: vec!["COMMUNITY".into(), "SECURE".into()],
            cloud_idx: 0,
            gpu_types: vec!["RTX 3090".into(), "RTX 4090".into()],
            gpu_idx: 0,
            gpu_count: 1,
            disk_gb: 100,
            volume_gb: 0,
            count: 1,
            free: vec!["arena8-apple".into(), "arena8-autumn".into()],
            field: 0,
        }
    }

    #[test]
    fn fields_depend_on_provider() {
        let f = form();
        assert_eq!(
            f.fields(),
            vec![
                NpField::Provider,
                NpField::CloudType,
                NpField::GpuType,
                NpField::GpuCount,
                NpField::Disk,
                NpField::Volume,
                NpField::Pods
            ]
        );
    }

    #[test]
    fn provider_cycle_skips_unavailable_and_reshapes_fields() {
        let mut f = form();
        // runpod -> (skip vast, no key) -> hetzner
        assert!(f.change(1)); // provider changed
        assert_eq!(f.provider_name(), "hetzner");
        // hetzner is CPU-only: no cloud/gpu fields
        assert_eq!(f.fields(), vec![NpField::Provider, NpField::Pods]);
        assert_eq!(f.cloud_type(), None);
    }

    #[test]
    fn change_clamps_gpu_count_and_pods() {
        let mut f = form();
        f.field = 3; // GpuCount
        for _ in 0..20 {
            f.change(1);
        }
        assert_eq!(f.gpu_count, 8); // capped
        f.field = 4; // Disk (50GB steps)
        f.change(1);
        assert_eq!(f.disk_gb, 150);
        f.field = 5; // Volume (100GB steps)
        f.change(1);
        f.change(1);
        assert_eq!(f.volume_gb, 200);
        f.field = 6; // Pods
        for _ in 0..20 {
            f.change(1);
        }
        assert_eq!(f.count, 2); // capped at free.len()
    }

    #[test]
    fn short_branch_extracts_iteration_label() {
        assert_eq!(short_branch("autocommit-arena8-w1d2-apple", "arena8"), "w1d2");
        assert_eq!(short_branch("autocommit-arena8-w0d1-autumn", "arena8"), "w0d1");
        // non-autocommit branches are untouched
        assert_eq!(short_branch("main", "arena8"), "main");
        assert_eq!(short_branch("feature/foo", "arena8"), "feature/foo");
    }
}
