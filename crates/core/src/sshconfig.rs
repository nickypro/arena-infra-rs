//! Generating the participant-facing `~/.ssh/config` (legacy `ssh_config_manual.py` /
//! `ssh_config_proxy.py`).
//!
//! Participants reach pods as `ssh <prefix>-<name>`. Two ways to wire that up:
//!   * **manual**: each `Host` block points straight at the pod's *current* provider
//!     endpoint (IP + port). Simple, but the config must be re-shared whenever a pod
//!     restarts (the endpoint moves).
//!   * **proxy**: each `Host` points at a *stable* port on the proxy host
//!     (`starting_port + machine-index`), which nginx forwards to the live endpoint. The
//!     participant config never changes; only the proxy's nginx config does.
//!
//! Both renderers are pure (just string building) so they're unit-tested. A shared
//! wildcard block (`Host <prefix>-*`) carries the common options; per-machine blocks add
//! only what differs.

use crate::pod::Pod;

/// The wildcard `Host <prefix>-*` header shared by every machine: the SSH user, the
/// shared identity file, and lax host-key checking (pod host keys churn as pods are
/// recreated, so pinning them just causes spurious failures for participants).
/// `host_name` is `Some(proxy_host)` for the proxy layout (all machines share it) and
/// `None` for the manual layout (each machine sets its own `HostName`).
fn wildcard_block(prefix: &str, user: &str, identity_file: &str, host_name: Option<&str>) -> String {
    let mut s = format!("Host {prefix}-*\n    User {user}\n");
    if let Some(h) = host_name {
        s.push_str(&format!("    HostName {h}\n"));
    }
    s.push_str(&format!(
        "    IdentityFile {identity_file}\n    UserKnownHostsFile=/dev/null\n    StrictHostKeyChecking=no\n"
    ));
    s
}

/// Render the **manual** ssh config: a per-pod `Host` block with the pod's current IP +
/// port, for pods that actually have an endpoint. Pods without one are skipped (they're
/// still starting). Output is sorted by name for a stable diff.
pub fn render_manual(prefix: &str, user: &str, identity_file: &str, pods: &[Pod]) -> String {
    let mut out = wildcard_block(prefix, user, identity_file, None);
    let mut rows: Vec<(&str, &str, u16)> = pods
        .iter()
        .filter_map(|p| match (p.ssh_ip.as_deref(), p.ssh_port) {
            (Some(ip), Some(port)) if !ip.trim().is_empty() => Some((p.name.as_str(), ip, port)),
            _ => None,
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(b.0));
    for (name, ip, port) in rows {
        out.push_str(&format!("\nHost {name}\n    HostName {ip}\n    Port {port}\n"));
    }
    out
}

/// Render the **proxy** ssh config: each machine in `candidates` gets a `Host` block
/// whose `Port` is `starting_port + index`, all sharing the proxy host from the wildcard
/// block. This is index-based (not endpoint-based), so it covers every machine in the
/// list whether or not it's currently running — matching the stable-port proxy scheme.
pub fn render_proxy(
    prefix: &str,
    user: &str,
    identity_file: &str,
    proxy_host: &str,
    starting_port: u16,
    candidates: &[String],
) -> String {
    let mut out = wildcard_block(prefix, user, identity_file, Some(proxy_host));
    for (idx, entry) in candidates.iter().enumerate() {
        let port = starting_port as u32 + idx as u32;
        let Ok(port) = u16::try_from(port) else { continue };
        let name = crate::naming::qualify(prefix, entry);
        if crate::naming::is_absolute(entry) {
            // A bare name doesn't match the `Host {prefix}-*` wildcard, so it must carry the
            // shared options itself (same User/HostName/IdentityFile the wildcard supplies).
            out.push_str(&format!(
                "\nHost {name}\n    User {user}\n    HostName {proxy_host}\n    \
                 IdentityFile {identity_file}\n    UserKnownHostsFile=/dev/null\n    \
                 StrictHostKeyChecking=no\n    Port {port}\n"
            ));
        } else {
            out.push_str(&format!("\nHost {name}\n    Port {port}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(name: &str, ip: Option<&str>, port: Option<u16>) -> Pod {
        Pod {
            id: "x".into(),
            name: name.into(),
            provider: "runpod".into(),
            status: "RUNNING".into(),
            gpu_type: None,
            cost_per_hr: None,
            ssh_ip: ip.map(String::from),
            ssh_port: port,
        }
    }

    #[test]
    fn manual_uses_live_endpoints_and_skips_unready() {
        let pods = vec![
            pod("arena8-bloom", Some("2.2.2.2"), Some(22002)),
            pod("arena8-apple", Some("1.1.1.1"), Some(22000)),
            pod("arena8-cloud", None, None), // no endpoint -> skipped
        ];
        let cfg = render_manual("arena8", "root", "~/.ssh/arena8_key", &pods);
        // shared block
        assert!(cfg.contains("Host arena8-*"));
        assert!(cfg.contains("IdentityFile ~/.ssh/arena8_key"));
        assert!(cfg.contains("StrictHostKeyChecking=no"));
        // per-pod, sorted (apple before bloom)
        let a = cfg.find("Host arena8-apple").unwrap();
        let b = cfg.find("Host arena8-bloom").unwrap();
        assert!(a < b);
        assert!(cfg.contains("HostName 1.1.1.1\n    Port 22000"));
        // unready pod absent
        assert!(!cfg.contains("arena8-cloud"));
    }

    #[test]
    fn proxy_is_index_based_and_covers_all_candidates() {
        let candidates = ["apple", "autumn", "bloom"].map(String::from).to_vec();
        let cfg = render_proxy("arena8", "root", "~/.ssh/arena8_key", "cute.sus.cat", 7000, &candidates);
        // proxy host on the wildcard block; per-machine blocks carry only the port
        assert!(cfg.contains("Host arena8-*\n    User root\n    HostName cute.sus.cat"));
        assert!(cfg.contains("Host arena8-apple\n    Port 7000"));
        assert!(cfg.contains("Host arena8-autumn\n    Port 7001"));
        assert!(cfg.contains("Host arena8-bloom\n    Port 7002"));
    }

    #[test]
    fn proxy_absolute_name_gets_bare_host_with_standalone_options() {
        let candidates = ["apple".to_string(), "@james-gpu".into()].to_vec();
        let cfg = render_proxy("arena8", "root", "~/.ssh/arena8_key", "cute.sus.cat", 7000, &candidates);
        // prefixed machine relies on the wildcard: just Host + Port
        assert!(cfg.contains("Host arena8-apple\n    Port 7000"));
        // absolute machine: bare Host, carries its own options (won't match `arena8-*`),
        // still on the proxy host at its index port (7001).
        assert!(cfg.contains(
            "Host james-gpu\n    User root\n    HostName cute.sus.cat\n    \
             IdentityFile ~/.ssh/arena8_key\n    UserKnownHostsFile=/dev/null\n    \
             StrictHostKeyChecking=no\n    Port 7001"
        ));
        // it must NOT be emitted as a prefixed host
        assert!(!cfg.contains("arena8-james-gpu"));
    }
}
