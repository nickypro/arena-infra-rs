//! Golden tests of `proxy plan` and `proxy apply`'s output (docs/OUTPUT.md), at default and at
//! `-v`: a write-only proxy file on this host holding one forward, a listing with a new pod, a
//! provider that fails to list and two pods outside the name list. `-v` is what they printed
//! before; the file's path is shown as `<conf>`.

use std::path::{Path, PathBuf};

use arena_core::{Config, Error, Pod, PodSpec, Provider, Result};
use async_trait::async_trait;

use super::{handle_proxy, ProxyCmd};
use crate::output_test::rendered;

/// RunPod lists apple (forwarded already), autumn (new) and two pods outside the name list;
/// Vast's listing fails.
struct Fleet;

fn pod(name: &str, ip: &str, port: u16) -> Pod {
    Pod { id: format!("id-{name}"), name: name.into(), provider: "runpod".into(), ssh_ip: Some(ip.into()), ssh_port: Some(port), ..Default::default() }
}

#[async_trait]
impl Provider for Fleet {
    fn name(&self) -> &'static str {
        "runpod"
    }
    fn describe(&self, _spec: &PodSpec) -> String {
        String::new()
    }
    async fn list_pods(&self) -> Result<Vec<Pod>> {
        unimplemented!("the proxy lists by provider")
    }
    async fn list_by_provider(&self) -> Vec<(String, Result<Vec<Pod>>)> {
        let pods = vec![
            pod("arena8-apple", "1.1.1.1", 22000),
            pod("arena8-autumn", "2.2.2.2", 22001),
            pod("arena8-zz-staff", "3.3.3.3", 22),
            pod("arena8-yy-staff", "4.4.4.4", 22),
        ];
        let body = r#"vast list HTTP 429 Too Many Requests: {"error":"rate limited, slow down"}"#;
        vec![("runpod".into(), Ok(pods)), ("vast".into(), Err(Error::provider(body)))]
    }
    async fn create_pod(&self, _spec: &PodSpec) -> Result<Pod> {
        unimplemented!("not exercised")
    }
    async fn stop_pod(&self, _id: &str) -> Result<()> {
        Ok(())
    }
    async fn restart_pod(&self, _id: &str) -> Result<()> {
        Ok(())
    }
    async fn terminate_pod(&self, _id: &str) -> Result<()> {
        Ok(())
    }
}

/// The proxy file, a fresh one per run (removed when dropped).
struct Conf(PathBuf);
impl Drop for Conf {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A write-only proxy on this host over `path`, two names on the list.
fn cfg(path: &Path) -> Config {
    Config::parse(&format!(
        "MACHINE_NAME_PREFIX=arena8\nSSH_PROXY_HOST=localhost\nSSH_PROXY_NGINX_CONFIG_PATH={}\n\
         SSH_PROXY_RELOAD_CMD=\"\"\nMACHINE_NAME_LIST=(apple autumn)\n",
        path.display()
    ))
}

/// The file with apple's forward in it (port 7000), as an apply that saw only apple wrote it.
fn with_apple(path: &Path) {
    let apple = arena_core::proxy::Forward {
        name: "arena8-apple".into(),
        public_port: 7000,
        target_ip: "1.1.1.1".into(),
        target_port: 22000,
        provider: Some("runpod".into()),
        pod_id: Some("id-arena8-apple".into()),
    };
    std::fs::write(path, arena_core::proxy::render_nginx(&[apple])).unwrap();
}

/// `cmds` in turn on a fresh file (`with_apple`) at default and at `-v`, each prompt answered
/// `y`: what each verbosity showed, the file's path as `<conf>`.
async fn both(tag: &str, cmds: impl Fn() -> Vec<ProxyCmd>) -> (String, String) {
    let mut out = Vec::new();
    for verbose in [false, true] {
        let conf = Conf(std::env::temp_dir().join(format!("arena-proxy-golden-{tag}-{verbose}-{}.conf", std::process::id())));
        with_apple(&conf.0);
        let cfg = cfg(&conf.0);
        let mut text = String::new();
        for cmd in cmds() {
            text.push_str(&rendered(verbose, &["y"], handle_proxy(cmd, &Fleet, &cfg, false)).await);
        }
        out.push(text.replace(&conf.0.display().to_string(), "<conf>"));
    }
    let loud = out.pop().unwrap();
    (out.pop().unwrap(), loud)
}

/// `proxy plan`: the change and the counts, a line per failed provider and per reason pods were
/// skipped, and the command that writes it; the forward that stays and the nginx config are
/// `-v`'s.
#[tokio::test]
async fn proxy_plan() {
    let (quiet, loud) = both("plan", || vec![ProxyCmd::Plan { out: None }]).await;
    assert_eq!(quiet, QUIET_PLAN.strip_prefix('\n').unwrap(), "default:\n{quiet}");
    assert_eq!(loud, LOUD_PLAN.strip_prefix('\n').unwrap(), "-v:\n{loud}");
}

/// `proxy apply --dry-run`, then `proxy apply` (asked), then again (the proxy cron's run: one
/// line once it's up to date).
#[tokio::test]
async fn proxy_apply() {
    let cmds = || vec![ProxyCmd::Apply { dry_run: true }, ProxyCmd::Apply { dry_run: false }, ProxyCmd::Apply { dry_run: false }];
    let (quiet, loud) = both("apply", cmds).await;
    assert_eq!(quiet, QUIET_APPLY.strip_prefix('\n').unwrap(), "default:\n{quiet}");
    assert_eq!(loud, LOUD_APPLY.strip_prefix('\n').unwrap(), "-v:\n{loud}");
}

const QUIET_PLAN: &str = r#"
‼ vast didn't list (rate limited): its forwards are kept
+ arena8-autumn            localhost:7001         -> 2.2.2.2:22001          (runpod)
+1 added, ~0 changed, -0 removed, =0 kept (stale), 1 unchanged
warning: skipped arena8-zz-staff, arena8-yy-staff — name not in MACHINE_NAME_LIST (no stable port slot)
To write it: arena proxy apply (-v prints the nginx config)
"#;

const LOUD_PLAN: &str = r#"

# proxy host: root@localhost  (nginx config path: <conf>)
# merge vs current config <conf> (1 forward(s))
warning: vast listing failed: provider error: vast list HTTP 429 Too Many Requests: {"error":"rate limited, slow down"}
  arena8-apple             localhost:7000         -> 1.1.1.1:22000          (runpod)
+ arena8-autumn            localhost:7001         -> 2.2.2.2:22001          (runpod)
+1 added, ~0 changed, -0 removed, =0 kept (stale), 1 unchanged
warning: skipped arena8-zz-staff — name not in MACHINE_NAME_LIST (no stable port slot)
warning: skipped arena8-yy-staff — name not in MACHINE_NAME_LIST (no stable port slot)

# ----- nginx config (write to <conf> on the proxy) -----
# Generated by arena-infra-rs (`arena proxy apply`). Do not edit by hand: the
# `# arena-forward` line above each block is read back on the next apply (it records
# which provider/pod owns the forward, so a provider outage never drops it).
# Bare `server` blocks: include from inside nginx's top-level `stream { }` block
# (e.g. `stream { include /etc/nginx/streams-enabled/*.conf; }`), then reload.
# arena-forward name=arena8-apple port=7000 target=1.1.1.1:22000 provider=runpod pod_id=id-arena8-apple
server {
    listen 7000;
    proxy_pass 1.1.1.1:22000;
    proxy_timeout 24h;
    proxy_connect_timeout 10s;
}
# arena-forward name=arena8-autumn port=7001 target=2.2.2.2:22001 provider=runpod pod_id=id-arena8-autumn
server {
    listen 7001;
    proxy_pass 2.2.2.2:22001;
    proxy_timeout 24h;
    proxy_connect_timeout 10s;
}

# Review, then `arena proxy apply` to write the proxy config to this host:<conf> (write-only: SSH_PROXY_RELOAD_CMD is empty, nginx is not reloaded). (--out <file> saves it locally.)
"#;

const QUIET_APPLY: &str = r#"
‼ vast didn't list (rate limited): its forwards are kept
  + arena8-autumn            localhost:7001         -> 2.2.2.2:22001          (runpod)
[dry-run] would write 2 forward(s) (+1 ~0 -0 =0) to this host:<conf> (write-only: no reload)
‼ vast didn't list (rate limited): its forwards are kept
  + arena8-autumn            localhost:7001         -> 2.2.2.2:22001          (runpod)
Write 2 forward(s) (+1 ~0 -0 =0) to this host:<conf> (write-only: no reload)? [y/N] y
[proxy] wrote 2 forward(s) to <conf> (write-only: nginx not reloaded)
‼ vast didn't list (rate limited): its forwards are kept
✓ proxy config up to date (2 forward(s), 2 pod(s) skipped)
"#;

const LOUD_APPLY: &str = r#"
warning: vast listing failed: provider error: vast list HTTP 429 Too Many Requests: {"error":"rate limited, slow down"}
  + arena8-autumn            localhost:7001         -> 2.2.2.2:22001          (runpod)
proxy: +1 added, ~0 changed, -0 removed, =0 kept (stale), 1 unchanged; 2 pod(s) skipped (`arena proxy plan` lists why)
[dry-run] would write 2 forward(s) to this host:<conf>
  write <conf> only — SSH_PROXY_RELOAD_CMD is empty, so nginx is never reloaded
(preview only — run without --dry-run to write it)
warning: vast listing failed: provider error: vast list HTTP 429 Too Many Requests: {"error":"rate limited, slow down"}
  + arena8-autumn            localhost:7001         -> 2.2.2.2:22001          (runpod)
proxy: +1 added, ~0 changed, -0 removed, =0 kept (stale), 1 unchanged; 2 pod(s) skipped (`arena proxy plan` lists why)
Will write the proxy config to this host:<conf> (write-only: SSH_PROXY_RELOAD_CMD is empty, nginx is not reloaded).
Proceed? [y/N] y
[proxy] wrote 2 forward(s) to <conf> (write-only: nginx not reloaded)
warning: vast listing failed: provider error: vast list HTTP 429 Too Many Requests: {"error":"rate limited, slow down"}
proxy: +0 added, ~0 changed, -0 removed, =0 kept (stale), 2 unchanged; 2 pod(s) skipped (`arena proxy plan` lists why)
proxy config already up to date — nothing to write.
"#;
