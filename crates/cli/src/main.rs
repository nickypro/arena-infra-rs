//! `arena` — the CLI surface over arena-core.
//!
//! Safety posture: read-only commands (`pods list`) run freely. Every mutating
//! command defaults to a **dry-run** that prints exactly what it *would* do; you
//! must pass `--apply` to actually call the provider. This is deliberate — the
//! tool is developed against a live production account.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use arena_core::provider::{
    hetzner::{HetznerOpts, HetznerProvider},
    runpod::RunpodProvider,
    vast::VastProvider,
    Provider,
};
use arena_core::{Config, PodSpec};

const DEFAULT_CONFIG: &str = "/home/dev/prod-ro/config.env";

#[derive(Parser)]
#[command(name = "arena", version, about = "Streamlined ARENA infra control plane")]
struct Cli {
    /// Path to config.env (defaults to the read-only prod copy).
    #[arg(long, default_value = DEFAULT_CONFIG, global = true)]
    config: PathBuf,

    /// Compute provider to target.
    #[arg(long, default_value = "runpod", global = true)]
    provider: String,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Machine (pod) lifecycle.
    #[command(subcommand)]
    Pods(PodCmd),
    /// Plan port-forwarding/proxy wiring (read-only; prints config to apply).
    #[command(subcommand)]
    Proxy(ProxyCmd),
}

#[derive(Subcommand)]
enum ProxyCmd {
    /// Compute the proxy plan for current pods and print the nginx `stream` config
    /// (stable public port -> each pod's current SSH endpoint) to apply on the proxy
    /// host. Read-only: never connects to the proxy.
    Plan {
        /// Also write the rendered nginx config to this local path for review.
        /// (Local only — this never copies anything to the proxy host.)
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum PodCmd {
    /// List current pods (read-only).
    List,
    /// Create N pods on the next free machine names. Dry-run unless --apply.
    Create {
        #[arg(short = 'n', long)]
        count: usize,
        #[arg(long)]
        apply: bool,
    },
    /// Create N pods, then poll until they have SSH endpoints and print the proxy
    /// plan — the one-command spin-up. Dry-run unless --apply. Polling stops at the
    /// timeout; it never runs in the background or mutates the proxy.
    Up {
        #[arg(short = 'n', long)]
        count: usize,
        #[arg(long)]
        apply: bool,
        /// Don't poll after creating; just print ids (run `proxy plan` later).
        #[arg(long)]
        no_wait: bool,
        /// Give up waiting for endpoints after this many seconds.
        #[arg(long, default_value_t = 600)]
        timeout: u64,
        /// Seconds between readiness polls (one list call per poll, whole fleet).
        #[arg(long, default_value_t = 12)]
        interval: u64,
    },
    /// Stop a pod by id. Dry-run unless --apply.
    Stop {
        id: String,
        #[arg(long)]
        apply: bool,
    },
    /// Terminate (delete) a pod by id. Dry-run unless --apply.
    Terminate {
        id: String,
        #[arg(long)]
        apply: bool,
    },
}

fn build_provider(name: &str, cfg: &Config) -> Result<Box<dyn Provider>> {
    match name {
        "runpod" => {
            let key = cfg.require("RUNPOD_API_KEY")?;
            Ok(Box::new(RunpodProvider::new(key)))
        }
        "vast" => {
            let key = cfg.get("VAST_API_KEY").unwrap_or_default();
            Ok(Box::new(VastProvider::new(key)))
        }
        "hetzner" => {
            let key = cfg.require("HETZNER_API_KEY")?;
            let opts = HetznerOpts {
                server_type: cfg.get("HETZNER_SERVER_TYPE").unwrap_or("cx22").to_string(),
                image: cfg.get("HETZNER_IMAGE").unwrap_or("ubuntu-24.04").to_string(),
                location: cfg.get("HETZNER_LOCATION").map(String::from),
                ssh_keys: cfg
                    .get("HETZNER_SSH_KEY")
                    .filter(|s| !s.is_empty())
                    .map(|s| vec![s.to_string()])
                    .unwrap_or_default(),
            };
            Ok(Box::new(HetznerProvider::new(key, opts)))
        }
        other => anyhow::bail!("unknown provider `{other}` (known: runpod, vast, hetzner)"),
    }
}

fn base_spec(cfg: &Config) -> PodSpec {
    PodSpec {
        name: String::new(),
        image: cfg.get("RUNPOD_DOCKER_IMAGE").unwrap_or_default().to_string(),
        gpu_type: cfg.get("RUNPOD_GPU_TYPE").unwrap_or_default().to_string(),
        gpu_count: cfg.get_parsed("RUNPOD_NUM_GPUS").unwrap_or(1),
        cloud_type: cfg.get("RUNPOD_CLOUD_TYPE").unwrap_or("COMMUNITY").to_string(),
        disk_gb: cfg.get_parsed("RUNPOD_DISK_SPACE_IN_GB").unwrap_or(100),
        volume_gb: cfg.get_parsed("RUNPOD_VOLUME_SPACE_IN_GB").unwrap_or(0),
        ports: "8888/http,22/tcp".to_string(),
        env: Vec::new(),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load(&cli.config)
        .with_context(|| format!("loading config {}", cli.config.display()))?;
    let provider = build_provider(&cli.provider, &cfg)?;

    match cli.cmd {
        Cmd::Pods(p) => handle_pods(p, provider.as_ref(), &cfg).await,
        Cmd::Proxy(p) => handle_proxy(p, provider.as_ref(), &cfg).await,
    }
}

async fn handle_proxy(cmd: ProxyCmd, provider: &dyn Provider, cfg: &Config) -> Result<()> {
    match cmd {
        ProxyCmd::Plan { out } => {
            let pods = provider.list_pods().await.context("listing pods for proxy plan")?;
            emit_proxy_plan(&pods, cfg, out.as_deref())?;
        }
    }
    Ok(())
}

/// Render and print the proxy plan for the given pods: a summary table, the nginx
/// `stream` config (optionally also written to `out`, locally), and any skipped
/// pods. Shared by `proxy plan` and `pods up`. Never connects to the proxy.
fn emit_proxy_plan(pods: &[arena_core::Pod], cfg: &Config, out: Option<&std::path::Path>) -> Result<()> {
    let pxcfg = arena_core::proxy::ProxyConfig::from_config(cfg)?;
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    let plan = arena_core::proxy::plan_forwards(&pxcfg, prefix, &cfg.machine_names, pods);

    println!(
        "\n# proxy host: {}@{}  (nginx config path: {})",
        pxcfg.proxy_user, pxcfg.proxy_host, pxcfg.nginx_path
    );
    if plan.forwards.is_empty() {
        println!("(no forwardable pods — nothing with an SSH endpoint in the name list)");
    } else {
        println!("{:<24} {:<14} {}", "NAME", "PUBLIC", "-> POD SSH");
        for f in &plan.forwards {
            println!(
                "{:<24} {}:{:<8} {}:{}",
                f.name, pxcfg.proxy_host, f.public_port, f.target_ip, f.target_port
            );
        }
    }
    for s in &plan.skipped {
        eprintln!("warning: skipped {} — {}", s.name, s.reason);
    }

    let nginx = arena_core::proxy::render_nginx(&plan.forwards);
    println!("\n# ----- nginx config (write to {} on the proxy) -----", pxcfg.nginx_path);
    println!("{nginx}");

    if let Some(path) = out {
        std::fs::write(path, &nginx)
            .with_context(|| format!("writing nginx config to {}", path.display()))?;
        eprintln!("wrote nginx config to {} (local only — not deployed)", path.display());
    } else {
        println!(
            "# Review, then apply on the proxy: write the above to {}, then \
             `nginx -t && nginx -s reload`. (--out <file> saves it locally.)",
            pxcfg.nginx_path
        );
    }
    Ok(())
}

async fn handle_pods(cmd: PodCmd, provider: &dyn Provider, cfg: &Config) -> Result<()> {
    match cmd {
        PodCmd::List => {
            let pods = provider.list_pods().await?;
            if pods.is_empty() {
                println!("(no pods)");
                return Ok(());
            }
            println!(
                "{:<24} {:<10} {:<16} {:<16} {}",
                "NAME", "STATUS", "GPU", "IP", "PORT"
            );
            for p in &pods {
                println!(
                    "{:<24} {:<10} {:<16} {:<16} {}",
                    p.name,
                    p.status,
                    p.gpu_type.as_deref().unwrap_or("-"),
                    p.ssh_ip.as_deref().unwrap_or("-"),
                    p.ssh_port.map(|x| x.to_string()).unwrap_or_else(|| "-".into()),
                );
            }
        }

        PodCmd::Create { count, apply } => {
            let existing = provider.list_pods().await.unwrap_or_default();
            let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
            let names =
                arena_core::naming::next_free_names(prefix, &cfg.machine_names, &existing, count);
            if names.len() < count {
                eprintln!(
                    "warning: requested {count} but only {} free machine names available",
                    names.len()
                );
            }
            let base = base_spec(cfg);
            for name in names {
                let mut spec = base.clone();
                spec.name = name.clone();
                spec.env.push(("MACHINE_NAME".into(), name.clone()));
                if apply {
                    let pod = provider.create_pod(&spec).await?;
                    println!("[created] {} id={}", pod.name, pod.id);
                } else {
                    println!(
                        "[dry-run] would create {} ({} x{}, {}, disk {}GB)",
                        spec.name, spec.gpu_type, spec.gpu_count, spec.cloud_type, spec.disk_gb
                    );
                }
            }
            if !apply {
                println!("\nDry-run only — no pods created. Re-run with --apply to execute.");
            }
        }

        PodCmd::Up { count, apply, no_wait, timeout, interval } => {
            let existing = provider.list_pods().await.unwrap_or_default();
            let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
            let names =
                arena_core::naming::next_free_names(prefix, &cfg.machine_names, &existing, count);
            if names.len() < count {
                eprintln!(
                    "warning: requested {count} but only {} free machine names available",
                    names.len()
                );
            }
            if names.is_empty() {
                eprintln!("no free machine names available — nothing to do");
                return Ok(());
            }

            if !apply {
                for name in &names {
                    println!("[dry-run] would create {name}");
                }
                println!(
                    "\nDry-run only — no pods created. With --apply: create the above, \
                     poll up to {timeout}s for SSH endpoints, then print the proxy plan."
                );
                return Ok(());
            }

            // Create, tracking the names we asked for so we only wait on those.
            let base = base_spec(cfg);
            for name in &names {
                let mut spec = base.clone();
                spec.name = name.clone();
                spec.env.push(("MACHINE_NAME".into(), name.clone()));
                let pod = provider.create_pod(&spec).await?;
                println!("[created] {} id={}", pod.name, pod.id);
            }

            if no_wait {
                println!(
                    "\n--no-wait: not polling. Run `arena proxy plan` once endpoints are assigned."
                );
                return Ok(());
            }

            // Poll the whole fleet (one list call per tick) until our pods have SSH
            // endpoints or we hit the timeout. Stateless: each tick re-reads truth.
            let want: std::collections::HashSet<&str> = names.iter().map(String::as_str).collect();
            let interval = interval.max(1);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
            println!(
                "\nWaiting up to {timeout}s for SSH endpoints (polling every {interval}s)…"
            );
            let pods = loop {
                tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
                let pods = match provider.list_pods().await {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("  poll failed ({e}); retrying");
                        if std::time::Instant::now() >= deadline {
                            break Vec::new();
                        }
                        continue;
                    }
                };
                let ready = pods
                    .iter()
                    .filter(|p| {
                        want.contains(p.name.as_str()) && p.ssh_ip.is_some() && p.ssh_port.is_some()
                    })
                    .count();
                println!("  {ready}/{} ready", want.len());
                if ready == want.len() || std::time::Instant::now() >= deadline {
                    break pods;
                }
            };

            let not_ready: Vec<&str> = want
                .iter()
                .copied()
                .filter(|n| {
                    !pods.iter().any(|p| {
                        p.name == *n && p.ssh_ip.is_some() && p.ssh_port.is_some()
                    })
                })
                .collect();
            if !not_ready.is_empty() {
                eprintln!(
                    "warning: timed out waiting for: {} (still starting?). \
                     Re-run `arena proxy plan` once they're up.",
                    not_ready.join(", ")
                );
            }
            emit_proxy_plan(&pods, cfg, None)?;
        }

        PodCmd::Stop { id, apply } => {
            if apply {
                provider.stop_pod(&id).await?;
                println!("[stopped] {id}");
            } else {
                println!("[dry-run] would stop {id} (--apply to execute)");
            }
        }

        PodCmd::Terminate { id, apply } => {
            if apply {
                provider.terminate_pod(&id).await?;
                println!("[terminated] {id}");
            } else {
                println!("[dry-run] would terminate {id} (--apply to execute)");
            }
        }
    }
    Ok(())
}
