//! `arena` — the CLI surface over arena-core.
//!
//! Safety posture: read-only commands (`pods list`) run freely. Every mutating
//! command defaults to a **dry-run** that prints exactly what it *would* do; you
//! must pass `--apply` to actually call the provider. This is deliberate — the
//! tool is developed against a live production account.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use arena_core::provider::{runpod::RunpodProvider, vast::VastProvider, Provider};
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
    /// Compute the proxy plan for current pods and print the nginx config +
    /// SSH-tunnel commands to apply on the proxy host. Never connects anywhere.
    Plan {
        /// Service port inside each pod to expose (default: Jupyter 8888).
        #[arg(long, default_value_t = arena_core::proxy::DEFAULT_TARGET_PORT)]
        port: u16,
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
        other => anyhow::bail!("unknown provider `{other}` (known: runpod, vast)"),
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
        ProxyCmd::Plan { port, out } => {
            let pxcfg = arena_core::proxy::ProxyConfig::from_config(cfg)?;
            let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
            let pods = provider.list_pods().await.context("listing pods for proxy plan")?;
            let plan = arena_core::proxy::plan_forwards(
                &pxcfg,
                prefix,
                &cfg.machine_names,
                &pods,
                port,
            );

            println!(
                "# proxy host: {}@{}  (nginx config path: {})\n",
                pxcfg.proxy_user, pxcfg.proxy_host, pxcfg.nginx_path
            );
            if plan.forwards.is_empty() {
                println!("(no forwardable pods — nothing with an SSH endpoint in the name list)");
            } else {
                println!("{:<24} {:<8} {:<8} {}", "NAME", "PUBLIC", "LOCAL", "POD SSH");
                for f in &plan.forwards {
                    println!(
                        "{:<24} {:<8} {:<8} {}@{}:{}",
                        f.name, f.public_port, f.local_port, f.pod_ssh_user, f.pod_ip, f.pod_ssh_port
                    );
                }
            }
            for s in &plan.skipped {
                eprintln!("warning: skipped {} — {}", s.name, s.reason);
            }

            let nginx = arena_core::proxy::render_nginx(&plan.forwards);
            println!("\n# ----- nginx config (write to {} on the proxy) -----", pxcfg.nginx_path);
            println!("{nginx}");
            println!("# ----- SSH tunnels (run on the proxy host {}) -----", pxcfg.proxy_host);
            for line in arena_core::proxy::render_tunnels(&plan.forwards) {
                println!("{line}");
            }

            if let Some(path) = out {
                std::fs::write(&path, &nginx)
                    .with_context(|| format!("writing nginx config to {}", path.display()))?;
                eprintln!("\nwrote nginx config to {} (local only — not deployed)", path.display());
            } else {
                println!(
                    "\n# Review the above, then apply manually. \
                     Re-run with --out <file> to save the nginx config locally."
                );
            }
        }
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
