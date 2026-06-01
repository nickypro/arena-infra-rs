//! Port-forwarding / proxy planning.
//!
//! The legacy setup exposes each pod's Jupyter (port 8888) to the world through a
//! single proxy host (`SSH_PROXY_HOST`) running nginx: the proxy holds an SSH tunnel
//! to each pod and nginx streams a public port to that tunnel's local end.
//!
//! This module *plans* that wiring — it computes a stable port map and renders the
//! nginx config and the SSH-tunnel commands — but it never opens a connection or
//! mutates the proxy itself. Applying the plan is a deliberate, manual step, because
//! the proxy is shared production infrastructure.
//!
//! ## Why the ports are stable
//!
//! A pod's public port is anchored to its machine name's index in the fixed
//! `MACHINE_NAME_LIST`, not to its position among the currently-running pods. So
//! `arena8-apple` is always `starting_port + 0`, whether or not its neighbours are
//! up. Tearing down one pod never renumbers the others, and a freed port is reused
//! only when that exact machine name comes back.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::pod::Pod;

/// Default service port inside a pod that we expose (Jupyter).
pub const DEFAULT_TARGET_PORT: u16 = 8888;

/// Proxy-host settings, read from the `SSH_PROXY_*` keys in `config.env`.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// User to SSH into the proxy host as.
    pub proxy_user: String,
    /// The public proxy host (domain or IP).
    pub proxy_host: String,
    /// Where the generated nginx config should live on the proxy host.
    pub nginx_path: String,
    /// First public port; machine index 0 gets this, index 1 gets +1, etc.
    pub starting_port: u16,
    /// First localhost port the tunnels bind on the proxy (nginx forwards to these).
    /// Kept distinct from the public range so nginx and the tunnels don't collide.
    pub local_base: u16,
    /// User to SSH into each *pod* as (from `SSH_USER`).
    pub pod_ssh_user: String,
}

impl ProxyConfig {
    pub fn from_config(cfg: &Config) -> Result<Self> {
        let starting_port: u16 = cfg.get_parsed("SSH_PROXY_STARTING_PORT").unwrap_or(7000);
        // Default the local bind range 1000 above the public range, or read an
        // explicit override. 1000 comfortably clears any plausible machine count.
        let local_base: u16 = cfg
            .get_parsed("SSH_PROXY_LOCAL_BASE")
            .unwrap_or(starting_port.saturating_add(1000));
        Ok(Self {
            proxy_user: cfg.get("SSH_PROXY_USER").unwrap_or("root").to_string(),
            proxy_host: cfg
                .get("SSH_PROXY_HOST")
                .filter(|s| !s.is_empty())
                .ok_or_else(|| Error::Config("missing SSH_PROXY_HOST".into()))?
                .to_string(),
            nginx_path: cfg
                .get("SSH_PROXY_NGINX_CONFIG_PATH")
                .unwrap_or("~/proxy.conf")
                .to_string(),
            starting_port,
            local_base,
            pod_ssh_user: cfg.get("SSH_USER").unwrap_or("root").to_string(),
        })
    }
}

/// One pod's full forwarding wiring: public port on the proxy -> local tunnel port
/// on the proxy -> the pod's service port over SSH.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forward {
    pub name: String,
    /// Public port users hit on the proxy host.
    pub public_port: u16,
    /// Localhost port on the proxy that the SSH tunnel binds and nginx forwards to.
    pub local_port: u16,
    /// Service port inside the pod (e.g. 8888 for Jupyter).
    pub target_port: u16,
    pub pod_ip: String,
    pub pod_ssh_port: u16,
    pub pod_ssh_user: String,
}

/// A pod that can't be wired up, with the reason — surfaced so the operator never
/// thinks coverage is complete when it isn't.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub name: String,
    pub reason: String,
}

/// The result of planning: forwards we can build, plus pods we had to skip.
#[derive(Debug, Clone, Default)]
pub struct ProxyPlan {
    pub forwards: Vec<Forward>,
    pub skipped: Vec<Skipped>,
}

/// Build the forwarding plan. Pure (no I/O) so it's unit-tested without a network:
/// the caller passes the current `pods` (from a provider list) and the fixed machine
/// candidate list. A pod is skipped if its name isn't in the candidate list (we have
/// no stable slot for it) or if it has no SSH endpoint yet.
pub fn plan_forwards(
    cfg: &ProxyConfig,
    prefix: &str,
    candidates: &[String],
    pods: &[Pod],
    target_port: u16,
) -> ProxyPlan {
    let mut plan = ProxyPlan::default();
    for pod in pods {
        let idx = candidates
            .iter()
            .position(|c| format!("{prefix}-{c}") == pod.name);
        let Some(idx) = idx else {
            plan.skipped.push(Skipped {
                name: pod.name.clone(),
                reason: "name not in MACHINE_NAME_LIST (no stable port slot)".into(),
            });
            continue;
        };
        let (Some(ip), Some(ssh_port)) = (pod.ssh_ip.clone(), pod.ssh_port) else {
            plan.skipped.push(Skipped {
                name: pod.name.clone(),
                reason: "no SSH endpoint yet (pod still starting?)".into(),
            });
            continue;
        };
        let idx = idx as u16;
        plan.forwards.push(Forward {
            name: pod.name.clone(),
            public_port: cfg.starting_port.saturating_add(idx),
            local_port: cfg.local_base.saturating_add(idx),
            target_port,
            pod_ip: ip,
            pod_ssh_port: ssh_port,
            pod_ssh_user: cfg.pod_ssh_user.clone(),
        });
    }
    plan.forwards.sort_by_key(|f| f.public_port);
    plan
}

/// Render an nginx `stream {}` block: each public port is proxied to the localhost
/// port its SSH tunnel binds. Suitable to drop at `ProxyConfig::nginx_path`.
pub fn render_nginx(forwards: &[Forward]) -> String {
    let mut out = String::from(
        "# Generated by arena-infra-rs `arena proxy plan`. Do not edit by hand.\n\
         # Each public port streams to a localhost port held open by an SSH tunnel\n\
         # (see the companion tunnel commands). Load with nginx's `stream` module.\n\
         stream {\n",
    );
    for f in forwards {
        out.push_str(&format!(
            "    # {name} -> pod {ip}:{tport}\n    \
             server {{\n        listen {pub_p};\n        proxy_pass 127.0.0.1:{loc};\n    }}\n",
            name = f.name,
            ip = f.pod_ip,
            tport = f.target_port,
            pub_p = f.public_port,
            loc = f.local_port,
        ));
    }
    out.push_str("}\n");
    out
}

/// Render the SSH-tunnel commands to run **on the proxy host**. Each binds a
/// localhost port on the proxy and forwards it, over SSH, to the pod's service port.
/// `-N` = no shell, `ExitOnForwardFailure` = fail loudly instead of a silent half-up
/// tunnel, keepalives so a dropped pod doesn't wedge the forward.
pub fn render_tunnels(forwards: &[Forward]) -> Vec<String> {
    forwards
        .iter()
        .map(|f| {
            format!(
                "ssh -fN -o ExitOnForwardFailure=yes -o ServerAliveInterval=30 \
                 -L 127.0.0.1:{loc}:localhost:{tport} -p {sshp} {user}@{ip}  \
                 # {name} -> public :{pub_p}",
                loc = f.local_port,
                tport = f.target_port,
                sshp = f.pod_ssh_port,
                user = f.pod_ssh_user,
                ip = f.pod_ip,
                name = f.name,
                pub_p = f.public_port,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ProxyConfig {
        ProxyConfig {
            proxy_user: "root".into(),
            proxy_host: "cute.sus.cat".into(),
            nginx_path: "~/proxy.conf".into(),
            starting_port: 7000,
            local_base: 8000,
            pod_ssh_user: "root".into(),
        }
    }

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

    fn candidates() -> Vec<String> {
        ["apple", "autumn", "bloom"].iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn port_is_anchored_to_candidate_index_not_pod_order() {
        // Only the *second* candidate is up; it must still get index-1 ports (7001),
        // proving the allocation doesn't depend on which other pods exist.
        let pods = vec![pod("arena8-autumn", Some("1.2.3.4"), Some(22001))];
        let plan = plan_forwards(&cfg(), "arena8", &candidates(), &pods, DEFAULT_TARGET_PORT);
        assert_eq!(plan.forwards.len(), 1);
        let f = &plan.forwards[0];
        assert_eq!(f.public_port, 7001);
        assert_eq!(f.local_port, 8001);
        assert_eq!(f.target_port, 8888);
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn skips_unknown_names_and_pods_without_ssh() {
        let pods = vec![
            pod("arena8-apple", Some("1.1.1.1"), Some(22000)),
            pod("arena8-ghost", Some("9.9.9.9"), Some(22099)), // not in candidates
            pod("arena8-bloom", None, None),                   // no ssh endpoint yet
        ];
        let plan = plan_forwards(&cfg(), "arena8", &candidates(), &pods, DEFAULT_TARGET_PORT);
        assert_eq!(plan.forwards.len(), 1);
        assert_eq!(plan.forwards[0].name, "arena8-apple");
        assert_eq!(plan.forwards[0].public_port, 7000);
        assert_eq!(plan.skipped.len(), 2);
        assert!(plan.skipped.iter().any(|s| s.name == "arena8-ghost"));
        assert!(plan.skipped.iter().any(|s| s.name == "arena8-bloom"));
    }

    #[test]
    fn renders_nginx_and_tunnels_consistently() {
        let pods = vec![pod("arena8-apple", Some("1.1.1.1"), Some(22000))];
        let plan = plan_forwards(&cfg(), "arena8", &candidates(), &pods, DEFAULT_TARGET_PORT);
        let nginx = render_nginx(&plan.forwards);
        assert!(nginx.contains("listen 7000;"));
        assert!(nginx.contains("proxy_pass 127.0.0.1:8000;"));
        let tunnels = render_tunnels(&plan.forwards);
        assert_eq!(tunnels.len(), 1);
        assert!(tunnels[0].contains("-L 127.0.0.1:8000:localhost:8888"));
        assert!(tunnels[0].contains("-p 22000 root@1.1.1.1"));
    }
}
