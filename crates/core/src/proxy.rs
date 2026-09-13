//! Port-forwarding / proxy planning.
//!
//! Pods are accessed over SSH (the team uses VS Code Remote-SSH, not Jupyter). The
//! provider assigns each pod's SSH endpoint (IP + port) and *reassigns* it whenever
//! the pod restarts — so a raw provider endpoint is a moving target to put in a VS
//! Code config. The proxy host (`SSH_PROXY_HOST`) solves that by giving each machine
//! a **stable public address** — `cute.sus.cat:7000`, `:7001`, … — that nginx's
//! `stream` module forwards straight to the pod's *current* SSH endpoint as raw TCP.
//! No tunnel process: nginx is the whole mechanism (declarative, graceful reload).
//!
//! This module only *plans* — it renders the nginx config; deploying it to the proxy
//! is a deliberate manual step, because the proxy is shared production infrastructure.
//!
//! ## Why the ports are stable
//!
//! A pod's public port is anchored to its machine name's index in the fixed
//! `MACHINE_NAME_LIST`, not to its position among the currently-running pods. So
//! `arena8-apple` is always `starting_port + 0`. Tearing down one pod never renumbers
//! the others, and when a machine restarts (or is recreated with the same name) it
//! reclaims its old port — only the `proxy_pass` target behind it changes.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::pod::Pod;

/// Proxy-host settings, read from the `SSH_PROXY_*` keys in `config.env`.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// User to SSH into the proxy host as (for manual deploy of the rendered config).
    pub proxy_user: String,
    /// The public proxy host (domain or IP) that holds the stable ports.
    pub proxy_host: String,
    /// Where the generated nginx config should live on the proxy host.
    pub nginx_path: String,
    /// First public port; machine index 0 gets this, index 1 gets +1, etc.
    pub starting_port: u16,
    /// Deploy nginx on THIS machine (write the config + reload locally) instead of over
    /// SSH to `proxy_host`. Defaults on: the control plane usually *is* the proxy host
    /// (e.g. `proxy_host` resolves back to this box), where SSHing to it is a needless
    /// hairpin. Set `PROXY_LOCAL=false` for a genuinely remote proxy.
    pub local: bool,
}

impl ProxyConfig {
    pub fn from_config(cfg: &Config) -> Result<Self> {
        let proxy_host = cfg
            .get("SSH_PROXY_HOST")
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Config("missing SSH_PROXY_HOST".into()))?
            .to_string();
        // Local by default; only a literal false/0/no opts back into remote SSH deploy.
        // A loopback host name also forces local regardless.
        let opted_remote = matches!(
            cfg.get("PROXY_LOCAL").map(|s| s.trim().to_lowercase()).as_deref(),
            Some("false") | Some("0") | Some("no") | Some("off")
        );
        let loopback = matches!(proxy_host.as_str(), "localhost" | "127.0.0.1" | "::1");
        Ok(Self {
            proxy_user: cfg.get("SSH_PROXY_USER").unwrap_or("root").to_string(),
            proxy_host,
            nginx_path: cfg
                .get("SSH_PROXY_NGINX_CONFIG_PATH")
                .unwrap_or("~/proxy.conf")
                .to_string(),
            starting_port: cfg.get_parsed("SSH_PROXY_STARTING_PORT").unwrap_or(7000),
            local: loopback || !opted_remote,
        })
    }
}

/// One pod's forwarding: a stable public port on the proxy that streams to the pod's
/// current SSH endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forward {
    pub name: String,
    /// Stable public port on the proxy host (what a VS Code SSH config targets).
    pub public_port: u16,
    /// The pod's current SSH endpoint that nginx streams to.
    pub target_ip: String,
    pub target_port: u16,
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
/// candidate list. A pod is skipped if its name isn't in the candidate list (no
/// stable slot for it) or if it has no SSH endpoint yet (still starting).
pub fn plan_forwards(
    cfg: &ProxyConfig,
    prefix: &str,
    candidates: &[String],
    pods: &[Pod],
) -> ProxyPlan {
    let mut plan = ProxyPlan::default();
    for pod in pods {
        let idx = candidates
            .iter()
            .position(|c| crate::naming::qualify(prefix, c) == pod.name);
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
        // Reject empty/garbage SSH hosts and ports that would run off the u16 ceiling,
        // rather than emitting a broken nginx block or silently colliding two pods on a
        // saturated port. Compute the port in u32 so the overflow is detectable.
        if ip.trim().is_empty() {
            plan.skipped.push(Skipped {
                name: pod.name.clone(),
                reason: "empty SSH host from provider".into(),
            });
            continue;
        }
        let port = cfg.starting_port as u32 + idx as u32;
        let Ok(public_port) = u16::try_from(port) else {
            plan.skipped.push(Skipped {
                name: pod.name.clone(),
                reason: format!("public port {port} exceeds 65535 (starting_port + index too high)"),
            });
            continue;
        };
        plan.forwards.push(Forward {
            name: pod.name.clone(),
            public_port,
            target_ip: ip,
            target_port: ssh_port,
        });
    }
    plan.forwards.sort_by_key(|f| f.public_port);
    plan
}

/// Render an nginx `stream {}` block: each stable public port is proxied straight to
/// a pod's current SSH endpoint. `proxy_timeout 24h` keeps long-lived VS Code SSH
/// sessions from being dropped. Suitable to drop at `ProxyConfig::nginx_path` and
/// load with nginx's `stream` module, then `nginx -t && nginx -s reload`.
pub fn render_nginx(forwards: &[Forward]) -> String {
    // Bare `server` blocks (NOT wrapped in `stream { }`): this file is meant to be included
    // from inside nginx's top-level `stream { }` block — the standard drop-in pattern
    // (`stream { include /etc/nginx/streams-enabled/*.conf; }`). A `stream {}` here would
    // nest and fail with "stream directive is not allowed here".
    let mut out = String::from(
        "# Generated by arena-infra-rs `arena proxy plan`. Do not edit by hand.\n\
         # Bare `server` blocks: include from inside nginx's top-level `stream { }` block\n\
         # (e.g. `stream { include /etc/nginx/streams-enabled/*.conf; }`), then reload.\n",
    );
    for f in forwards {
        out.push_str(&format!(
            "# {name}\nserver {{\n    listen {pub_p};\n    proxy_pass {ip}:{tport};\n    \
             proxy_timeout 24h;\n    proxy_connect_timeout 10s;\n}}\n",
            name = f.name,
            pub_p = f.public_port,
            ip = f.target_ip,
            tport = f.target_port,
        ));
    }
    out
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
            local: false,
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
        // Only the *second* candidate is up; it must still get index-1's port (7001),
        // proving the allocation doesn't depend on which other pods exist.
        let pods = vec![pod("arena8-autumn", Some("1.2.3.4"), Some(22001))];
        let plan = plan_forwards(&cfg(), "arena8", &candidates(), &pods);
        assert_eq!(plan.forwards.len(), 1);
        let f = &plan.forwards[0];
        assert_eq!(f.public_port, 7001);
        assert_eq!(f.target_ip, "1.2.3.4");
        assert_eq!(f.target_port, 22001);
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn skips_unknown_names_and_pods_without_ssh() {
        let pods = vec![
            pod("arena8-apple", Some("1.1.1.1"), Some(22000)),
            pod("arena8-ghost", Some("9.9.9.9"), Some(22099)), // not in candidates
            pod("arena8-bloom", None, None),                   // no ssh endpoint yet
        ];
        let plan = plan_forwards(&cfg(), "arena8", &candidates(), &pods);
        assert_eq!(plan.forwards.len(), 1);
        assert_eq!(plan.forwards[0].name, "arena8-apple");
        assert_eq!(plan.forwards[0].public_port, 7000);
        assert_eq!(plan.skipped.len(), 2);
        assert!(plan.skipped.iter().any(|s| s.name == "arena8-ghost"));
        assert!(plan.skipped.iter().any(|s| s.name == "arena8-bloom"));
    }

    #[test]
    fn skips_port_overflow_and_empty_host_instead_of_silently_colliding() {
        let mut c = cfg();
        c.starting_port = 65535;
        // index 0 -> 65535 (ok); index 1 -> 65536 (overflow -> skipped, not saturated).
        let pods = vec![
            pod("arena8-apple", Some("1.1.1.1"), Some(22000)),
            pod("arena8-autumn", Some("2.2.2.2"), Some(22001)),
            pod("arena8-bloom", Some("   "), Some(22002)), // empty/garbage host -> skipped
        ];
        let plan = plan_forwards(&c, "arena8", &candidates(), &pods);
        assert_eq!(plan.forwards.len(), 1);
        assert_eq!(plan.forwards[0].name, "arena8-apple");
        assert_eq!(plan.forwards[0].public_port, 65535);
        assert!(plan.skipped.iter().any(|s| s.name == "arena8-autumn" && s.reason.contains("65535")));
        assert!(plan.skipped.iter().any(|s| s.name == "arena8-bloom" && s.reason.contains("empty")));
    }

    #[test]
    fn absolute_name_forwards_to_bare_pod_at_its_index_port() {
        // An `@`-marked candidate forwards to the bare pod name and still gets its
        // index-anchored port (index 2 -> 7002), exactly like a prefixed machine.
        let candidates =
            ["apple".to_string(), "autumn".into(), "@james-gpu".into()].to_vec();
        let pods = vec![pod("james-gpu", Some("5.6.7.8"), Some(22055))];
        let plan = plan_forwards(&cfg(), "arena8", &candidates, &pods);
        assert_eq!(plan.forwards.len(), 1, "skipped: {:?}", plan.skipped);
        let f = &plan.forwards[0];
        assert_eq!(f.name, "james-gpu");
        assert_eq!(f.public_port, 7002);
        assert_eq!(f.target_ip, "5.6.7.8");
        assert_eq!(f.target_port, 22055);
    }

    #[test]
    fn renders_nginx_stream_to_pod_ssh_endpoint() {
        let pods = vec![pod("arena8-apple", Some("1.1.1.1"), Some(22000))];
        let plan = plan_forwards(&cfg(), "arena8", &candidates(), &pods);
        let nginx = render_nginx(&plan.forwards);
        // Bare server blocks — no actual `stream {` *directive* (it's only in a comment
        // example); the file is included inside nginx's own top-level `stream { }`.
        assert!(!nginx.lines().any(|l| l.trim_start().starts_with("stream {")));
        assert!(nginx.contains("server {"));
        assert!(nginx.contains("listen 7000;"));
        assert!(nginx.contains("proxy_pass 1.1.1.1:22000;"));
        assert!(nginx.contains("proxy_timeout 24h;"));
    }
}
