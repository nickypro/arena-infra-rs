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
