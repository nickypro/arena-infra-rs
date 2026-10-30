//! Pod-status display helpers, shared by the CLI and TUI so a pod reads the same on
//! both surfaces (e.g. `run` / `init` / `exit`, not raw `RUNNING`).

/// A compact status label so columns stay narrow: `RUNNING` -> `run`, `EXITED` ->
/// `exit`, etc. Unknown statuses fall back to a lowercased 4-char prefix.
pub fn short_status(s: &str) -> String {
    match s.to_ascii_uppercase().as_str() {
        "RUNNING" => "run".into(),
        "EXITED" => "exit".into(),
        "STOPPED" => "stop".into(),
        "TERMINATED" => "term".into(),
        "CREATED" => "new".into(),
        "PENDING" | "PROVISIONING" => "prov".into(),
        "RESTARTING" => "rstr".into(),
        _ => s.chars().take(4).collect::<String>().to_lowercase(),
    }
}

/// The status to show, combining the provider's reported status with whether we can
/// actually reach the pod over SSH. Providers call a pod `RUNNING` the moment it's
/// requested (`desiredStatus`), well before it's booted/serving SSH — so a `RUNNING`
/// pod we can't reach yet shows `init` ("coming up"), not `run`. `reachable`:
/// `Some(true)` = probe succeeded, `Some(false)` = probe errored, `None` = not probed.
pub fn display_status(status: &str, reachable: Option<bool>) -> String {
    if status.eq_ignore_ascii_case("RUNNING") && reachable != Some(true) {
        return "init".to_string();
    }
    short_status(status)
}

/// Unix time for endpoint age checks. The comparison itself is pure for tests.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// A running pod that still has no direct SSH after ten minutes needs attention. An
/// unknown start time stays unknown; creation time would mislabel a recent restart.
pub fn endpoint_overdue(pod: &crate::pod::Pod, now: u64) -> bool {
    pod.status.eq_ignore_ascii_case("RUNNING")
        && (pod.ssh_ip.as_deref().is_none_or(|s| s.trim().is_empty()) || pod.ssh_port.is_none_or(|p| p == 0))
        && pod.started_at.is_some_and(|t| now.saturating_sub(t) >= 600)
}

/// Whether `pod` is up — its machine or container running — with no direct SSH endpoint: what
/// `pods up` replaces once it stays so for `--endpoint-timeout`. On RunPod only a pod reachable
/// through RunPod's SSH proxy alone counts ([`crate::pod::Pod::proxy_only`]: a live runtime, no
/// public IP/port): RunPod lists a pod `RUNNING` from its create, while its image is still
/// being pulled — live 2026-10-30, a COMMUNITY 3070 was `RUNNING` with `runtime: null` on v2
/// (`desiredStatus: RUNNING`, `publicIp: ""`, `portMappings: null` on v1) for 6½ minutes before
/// its container started — so its `RUNNING` alone says nothing about the host. Elsewhere a
/// running pod is a booted one (Hetzner `running`, Vast's observed `running`, Shadeform's
/// `active`). Pure.
pub fn up_without_endpoint(pod: &crate::pod::Pod) -> bool {
    let reachable = pod.ssh_ip.as_deref().is_some_and(|s| !s.trim().is_empty()) && pod.ssh_port.is_some_and(|p| p != 0);
    if reachable {
        return false;
    }
    pod.proxy_only || (!pod.provider.eq_ignore_ascii_case("runpod") && pod.status.trim().eq_ignore_ascii_case("RUNNING"))
}

/// How long after a pod is first listed up its backend may still hold its endpoint back on
/// purpose — time `pods up` doesn't count towards `--endpoint-timeout`, so a healthy pod is never
/// condemned for it. Shadeform: an `active` instance's IP is hidden for
/// [`crate::provider::shadeform::BOOT_GRACE`] after its `active_at` (the bootstrap installs root's
/// keys then), so it lists "active, no IP" by design — and `--endpoint-timeout 1` would otherwise
/// end exactly when its IP appears (later still on a clock behind Shadeform's). Others: none. Pure.
pub fn endpoint_held_back(pod: &crate::pod::Pod) -> std::time::Duration {
    if pod.provider.eq_ignore_ascii_case(crate::provider::shadeform::NAME) {
        std::time::Duration::from_secs(crate::provider::shadeform::BOOT_GRACE)
    } else {
        std::time::Duration::ZERO
    }
}

/// The pod-aware status shared by the list and dashboard.
pub fn pod_status(pod: &crate::pod::Pod, reachable: Option<bool>) -> String {
    if endpoint_overdue(pod, now_secs()) { "no-ssh".into() }
    else { display_status(&pod.status, reachable) }
}

/// Whether a pod in this state holds — and is billed for — its compute: up, on its way up,
/// or stuck while still allocated. The one definition behind the fleet cost total, the
/// `$/H` column and `pods stop --all`'s selection, so they can't disagree.
///
/// It's more than `RUNNING` because the providers don't all speak v1's `desiredStatus`:
/// RunPod v2 reports real lifecycle states, and a `PROVISIONING`/`STARTING` pod already
/// bills (as does `ERROR`: v2's `cost` is 0 only for `EXITED`/`TERMINATED`); Hetzner
/// reports `initializing`/`starting`, Vast `loading`/`created`, and our own Vast create
/// says `CREATING`. An allow-list on purpose: `pods stop --all` acts on it, and a state we
/// don't know must not get a pod stopped (on RunPod a stop resets the container disk).
/// Stopped/gone states (`EXITED`, `STOPPED`, `TERMINATED`, Hetzner `off`, …) are false.
pub fn is_billing(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_uppercase().as_str(),
        "RUNNING"
            | "STARTING"
            | "PROVISIONING"
            | "PENDING"
            | "CREATING"
            | "CREATED"
            | "INITIALIZING"
            | "LOADING"
            | "RESTARTING"
            | "REBUILDING"
            | "MIGRATING"
            | "ERROR"
    )
}

/// Whether a pod in this state is **stopped** — there, but powered off until someone starts
/// it (`pods start`): RunPod/Vast `EXITED`, `STOPPED`, Hetzner `off`. An allow-list, like
/// [`is_billing`]: a state we don't know (or one on its way somewhere, `STOPPING`) is not
/// "stopped", so it's never told to start. A stopped pod has no live SSH endpoint — a
/// listing may still show its last one, which belongs to nothing now — so the SSH commands
/// skip it saying so, and `restart` (which needs a running pod) points at `pods start`.
pub fn is_stopped(status: &str) -> bool {
    matches!(status.trim().to_ascii_uppercase().as_str(), "EXITED" | "STOPPED" | "OFF")
}

/// Whether a pod on `provider` is costing its hourly rate right now — what the fleet total
/// and the `$/H` column count. [`is_billing`], except that Hetzner charges for a server for
/// as long as it exists, powered off included (its resources stay reserved): an `off`
/// Hetzner server still costs its full €/h, so leaving it out would understate the bill.
pub fn bills_hourly(provider: &str, status: &str) -> bool {
    if provider.eq_ignore_ascii_case("hetzner") {
        return !matches!(status.trim().to_ascii_uppercase().as_str(), "DELETING" | "TERMINATED");
    }
    is_billing(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn billing_statuses_table() {
        // (status as the providers spell it, is_billing)
        let cases = [
            ("RUNNING", true),    // v1 desiredStatus / v2 / everyone
            ("running", true),    // vast/hetzner lowercase
            ("PROVISIONING", true), // v2: being allocated — already billed
            ("STARTING", true),   // v2 / hetzner `starting`
            ("ERROR", true),      // v2: still allocated, `cost` > 0
            ("CREATING", true),   // our synthesized vast create status
            ("CREATED", true),
            ("INITIALIZING", true), // hetzner right after create
            ("LOADING", true),    // vast pulling the image
            ("RESTARTING", true),
            ("PENDING", true),
            (" Running ", true),
            ("EXITED", false),    // v1/v2 stopped
            ("exited", false),
            ("STOPPED", false),
            ("TERMINATED", false),
            ("OFF", false),       // hetzner powered off (see bills_hourly)
            ("STOPPING", false),
            ("DELETING", false),
            ("UNKNOWN", false),   // unparseable: never a stop target
            ("", false),
        ];
        for (status, want) in cases {
            assert_eq!(is_billing(status), want, "{status:?}");
        }
    }

    #[test]
    fn stopped_statuses_table() {
        for (status, stopped) in [
            ("EXITED", true),
            ("exited", true),
            ("STOPPED", true),
            ("off", true),
            (" Off ", true),
            ("RUNNING", false),
            ("STOPPING", false), // on its way: not startable yet
            ("TERMINATED", false),
            ("PROVISIONING", false),
            ("UNKNOWN", false),
            ("", false),
        ] {
            assert_eq!(is_stopped(status), stopped, "{status:?}");
            assert!(!(is_stopped(status) && is_billing(status)), "{status:?}: never both");
        }
    }

    #[test]
    fn hetzner_bills_while_the_server_exists() {
        assert!(bills_hourly("hetzner", "OFF"));
        assert!(bills_hourly("hetzner", "RUNNING"));
        assert!(bills_hourly("hetzner", "STOPPING"));
        assert!(!bills_hourly("hetzner", "DELETING"));
        // Everyone else follows the status.
        assert!(!bills_hourly("runpod", "EXITED"));
        assert!(bills_hourly("runpod", "STARTING"));
        assert!(!bills_hourly("vast", "STOPPED"));
        assert!(!bills_hourly("vast", "OFF"));
    }

    #[test]
    fn short_status_abbreviates_known_and_falls_back() {
        assert_eq!(short_status("RUNNING"), "run");
        assert_eq!(short_status("exited"), "exit");
        assert_eq!(short_status("TERMINATED"), "term");
        assert_eq!(short_status("WEIRDSTATE"), "weir");
    }

    #[test]
    fn display_status_reflects_reachability() {
        assert_eq!(display_status("RUNNING", Some(true)), "run");
        assert_eq!(display_status("RUNNING", Some(false)), "init");
        assert_eq!(display_status("RUNNING", None), "init");
        assert_eq!(display_status("EXITED", Some(false)), "exit");
        assert_eq!(display_status("pending", None), "prov");
    }
}

#[cfg(test)]
mod endpoint_tests {
    use super::*;
    #[test]
    fn running_without_ssh_expires_from_last_start_not_creation() {
        let mut p = crate::pod::Pod { status:"RUNNING".into(), created_at:Some(1), started_at:Some(1000), ..Default::default() };
        assert!(!endpoint_overdue(&p, 1599)); assert!(endpoint_overdue(&p, 1600));
        p.started_at = None; assert!(!endpoint_overdue(&p, 9999));
        p.started_at = Some(20000); assert!(!endpoint_overdue(&p, 9999));
        p.started_at = Some(1000); p.status = "EXITED".into(); assert!(!endpoint_overdue(&p, 9999));
        p.status = "RUNNING".into(); p.ssh_ip = Some("203.0.113.1".into()); p.ssh_port = Some(22);
        assert!(!endpoint_overdue(&p, 9999));
        p.ssh_port = None; assert_eq!(pod_status(&p, None), "no-ssh");
    }

    /// `pods up`'s "up without an endpoint" (`--endpoint-timeout`): RunPod's `RUNNING` alone is
    /// its desired status, the image maybe still pulling, so there only the proxy-only state
    /// counts; elsewhere a running pod is a booted one. Never one with an endpoint.
    #[test]
    fn up_without_an_endpoint_table() {
        let pod = |provider: &str, status: &str, endpoint: Option<(&str, u16)>, proxy_only: bool| crate::pod::Pod {
            provider: provider.into(),
            status: status.into(),
            ssh_ip: endpoint.map(|(ip, _)| ip.to_string()),
            ssh_port: endpoint.map(|(_, port)| port),
            proxy_only,
            ..Default::default()
        };
        let cases = [
            // (case, pod, up without an endpoint)
            ("runpod RUNNING, image pulling", pod("runpod", "RUNNING", None, false), false),
            ("runpod proxy only", pod("runpod", "RUNNING", None, true), true),
            ("runpod with an endpoint", pod("runpod", "RUNNING", Some(("203.0.113.1", 22)), false), false),
            ("runpod provisioning", pod("runpod", "PROVISIONING", None, false), false),
            ("vast running, no endpoint", pod("vast", "RUNNING", None, false), true),
            ("vast loading", pod("vast", "LOADING", None, false), false),
            ("vast running, reachable", pod("vast", "RUNNING", Some(("ssh4.vast.ai", 1234)), false), false),
            ("hetzner running, no IP", pod("hetzner", "running", None, false), true),
            ("hetzner initializing", pod("hetzner", "initializing", None, false), false),
            ("shadeform active (RUNNING), no IP yet", pod("shadeform", "RUNNING", None, false), true),
            ("shadeform pending", pod("shadeform", "PENDING", None, false), false),
            ("an empty IP is none", pod("vast", "RUNNING", Some(("  ", 22)), false), true),
            ("port 0 is none", pod("vast", "RUNNING", Some(("203.0.113.1", 0)), false), true),
            ("stopped", pod("vast", "EXITED", None, false), false),
        ];
        for (case, p, want) in cases {
            assert_eq!(up_without_endpoint(&p), want, "{case}");
        }
    }

    /// Only Shadeform holds an endpoint back on purpose (its `BOOT_GRACE` after `active_at`):
    /// that long isn't counted towards `--endpoint-timeout`.
    #[test]
    fn only_shadeform_holds_its_endpoint_back() {
        use std::time::Duration;
        let pod = |provider: &str| crate::pod::Pod { provider: provider.into(), status: "RUNNING".into(), ..Default::default() };
        assert_eq!(endpoint_held_back(&pod("shadeform")), Duration::from_secs(crate::provider::shadeform::BOOT_GRACE));
        assert_eq!(crate::provider::shadeform::BOOT_GRACE, 60);
        for other in ["runpod", "vast", "hetzner", ""] {
            assert_eq!(endpoint_held_back(&pod(other)), Duration::ZERO, "{other}");
        }
    }
}
