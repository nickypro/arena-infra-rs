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

#[cfg(test)]
mod tests {
    use super::*;

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
