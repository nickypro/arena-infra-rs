//! `arena plan check|show`: a provisioning plan file, checked or previewed (read-only).

use std::path::PathBuf;

use anyhow::{Context, Result};
use arena_core::out;
use arena_core::out::To;
use arena_core::provider::Provider;
use arena_core::Config;
use clap::Subcommand;

use crate::proxy::{plan_proxy, proxy_action};
use crate::{expand_tilde, local_now};

#[derive(Subcommand)]
pub(crate) enum PlanCmd {
    /// Validate the plan file and print its resolved schedule
    Check {
        /// The plan file [default: arena-plan.json beside the config]
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Preview what the plan would do on a date (read-only)
    ///
    /// The GPU/provider fallback order, the fill-to-target against the current fleet and, on a
    /// replace day, what it would tear down.
    #[command(verbatim_doc_comment)]
    Show {
        /// The plan file [default: arena-plan.json beside the config]
        #[arg(long)]
        file: Option<PathBuf>,
        /// Date to preview (YYYY-MM-DD). Defaults to today (local).
        #[arg(long)]
        date: Option<String>,
    },
}

pub(crate) async fn handle_plan(cmd: PlanCmd, provider: &dyn Provider, cfg: &Config) -> Result<()> {
    use arena_core::plan::{within_window, Plan};
    use arena_core::schedule::{days_from_civil, parse_ymd, ymd_string};

    let (today_days, now_min, today_ymd, tz) = local_now()?;

    match cmd {
        PlanCmd::Check { file } => {
            let file = file.unwrap_or_else(|| crate::fleet_files::plan_file(cfg));
            let plan = Plan::load(&file)?;
            out!(Out, "plan file:  {}", file.display());
            out!(Out, "local now:  {today_ymd} {:02}:{:02} {tz}", now_min / 60, now_min % 60);
            match plan.window_minutes() {
                Some((s, e)) => out!(
                    Out,
                    "window:     {}–{} local — now is {}",
                    plan.window[0],
                    plan.window[1],
                    if within_window(now_min, s, e) { "INSIDE" } else { "outside" }
                ),
                None => out!(Out, "window:     INVALID {:?} — fix HH:MM", plan.window),
            }
            let cap = |v: Option<String>| v.unwrap_or_else(|| "unset".into());
            out!(
                Out,
                "caps:       max_total={}  max_hourly={}  max_replace={}",
                cap(plan.max_total.map(|n| n.to_string())),
                cap(plan.max_hourly.map(|n| n.to_string())),
                cap(plan.max_replace.map(|n| n.to_string()))
            );
            out!(Out, "gpus:       {}", plan.gpus.join(" > "));
            out!(Out, "providers:  {}", plan.providers.join(" > "));
            out!(Out, "days:");
            if plan.days.is_empty() {
                out!(Out, "  (none)");
            }
            for d in &plan.days {
                let date = d.effective_days(today_days).map(ymd_string).unwrap_or_else(|| "??".into());
                let today = if d.effective_days(today_days) == Some(today_days) { " (today)" } else { "" };
                out!(
                    Out,
                    "  {date}{today}: {} pods × {}gpu{}",
                    d.count,
                    d.gpus_per_pod,
                    if d.replace { "  [REPLACE]" } else { "" }
                );
            }
            out!(Detail, "\n✓ plan parses. (Scheduled apply + arming aren't wired yet — preview with `plan show`.)");
            out!(Brief, "{}", out::ok(out::bold("plan parses")));
        }

        PlanCmd::Show { file, date } => {
            let file = file.unwrap_or_else(|| crate::fleet_files::plan_file(cfg));
            let plan = Plan::load(&file)?;
            let target_days = match &date {
                Some(s) => {
                    let (y, m, d) = parse_ymd(s).context("--date must be YYYY-MM-DD")?;
                    days_from_civil(y, m, d)
                }
                None => today_days,
            };
            let target_ymd = ymd_string(target_days);
            let Some(day) = plan.day_for(today_days, target_days) else {
                out!(Out, "No plan entry for {target_ymd}.");
                return Ok(());
            };

            out!(
                Out,
                "Plan for {target_ymd}: {} pods × {} GPU/pod{}",
                day.count,
                day.gpus_per_pod,
                if day.replace { "  [REPLACE]" } else { "" }
            );

            // Existing fleet on the current provider (for the fill-to-target preview).
            let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
            let pods = provider.list_pods().await.context("listing pods")?;
            let existing = pods.iter().filter(|p| p.name.starts_with(&format!("{prefix}-"))).count();
            let remaining = day.count.saturating_sub(existing);
            out!(
                Out,
                "Current fleet on {}: {existing} pod(s) named {prefix}-*  →  would create up to {remaining} more to reach {}.",
                provider.name(),
                day.count
            );
            if day.replace {
                out!(
                    Out,
                    "REPLACE: would back up, then terminate those {existing} pod(s) (cap max_replace={}), then create {} fresh.",
                    plan.max_replace.map_or_else(|| "unset".to_string(), |n| n.to_string()),
                    day.count
                );
            }

            out!(Out, "\nFallback order (GPU-first; stop once {} filled):", day.count);
            for (i, c) in day.candidates(&plan).iter().enumerate() {
                let tier = c.cloud.as_deref().map(|t| format!(":{t}")).unwrap_or_default();
                out!(Out, "  {:>2}. {} on {}{tier}", i + 1, c.gpu, c.provider);
            }

            if let Some((s, e)) = plan.window_minutes() {
                let inside = within_window(now_min, s, e);
                out!(
                    Out,
                    "\nWindow {}–{} local — a scheduled run right now would be {}.",
                    plan.window[0],
                    plan.window[1],
                    if inside { "ELIGIBLE" } else { "SKIPPED (outside window)" }
                );
            }
            out!(Detail, "(Preview only — the executor that actually creates/replaces is the next step.)");
        }
    }
    Ok(())
}

/// `proxy plan`: show the merge against the current config (`+ ~ - =` per machine, the
/// summary, skipped pods), then the rendered nginx config (optionally also written to
/// `out`, locally). Never connects to the proxy: for a local proxy the current config is a
/// local file read; for a remote one the diff isn't shown (`proxy apply --dry-run` reads it).
///
/// Without the current config (remote proxy, or the local file unreadable) the plan starts
/// from nothing, so it lacks every forward the merge would *keep* (owner's provider
/// failed, pod listed without an endpoint). That output is labelled a listing-only preview
/// and `--out` is refused: deployed by hand, it would drop those participants' forwards.
pub(crate) fn emit_proxy_plan(listing: &arena_core::proxy::Listing, cfg: &Config, out: Option<&std::path::Path>) -> Result<()> {
    let pxcfg = arena_core::proxy::ProxyConfig::from_config(cfg)?;

    out!(
        Detail,
        "\n# proxy host: {}@{}  (nginx config path: {})",
        pxcfg.proxy_user,
        pxcfg.proxy_host,
        pxcfg.nginx_path
    );
    // `None` = the current config is unknown, so this can't be a merge.
    let prev: Option<Vec<arena_core::proxy::Forward>> = if pxcfg.local {
        let path = expand_tilde(&pxcfg.nginx_path);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let parsed = arena_core::proxy::parse_nginx_detailed(&text);
                out!(Detail, "# merge vs current config {path} ({} forward(s))", parsed.forwards.len());
                if parsed.ignored_blocks > 0 {
                    out!(
                        Err,
                        "warning: {} server block(s) in {path} aren't in a format arena can read back — \
                         they'd be dropped on apply",
                        parsed.ignored_blocks
                    );
                }
                Some(parsed.forwards)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                out!(Detail, "# no current config at {path} yet — every forward is new");
                Some(Vec::new())
            }
            Err(e) => {
                out!(Out, "# couldn't read {path} ({e}) — diff vs live config not shown");
                None
            }
        }
    } else {
        out!(Out, "# diff vs live config not shown (remote proxy) — `arena proxy apply --dry-run` reads it");
        None
    };
    if prev.is_none() && out.is_some() {
        anyhow::bail!(
            "--out needs the current proxy config to merge against, and it isn't readable here — a \
             config built from the listing alone would drop every forward the merge keeps. Use \
             `arena proxy apply` (it reads the live config; --dry-run to preview)."
        );
    }

    crate::proxy::listing_warnings(listing);
    let plan = plan_proxy(cfg, &pxcfg, prev.as_deref().unwrap_or_default(), listing);
    if let Some(why) = &plan.abort {
        anyhow::bail!("{why} — `proxy apply` would refuse to write anything");
    }
    if plan.changes.is_empty() {
        out!(Out, "(no forwardable pods — nothing with an SSH endpoint in the name list)");
    } else {
        // A line per change by default; the forwards that stay as they are with `-v`.
        let lines = plan.change_lines(&pxcfg.proxy_host, true);
        for (change, line) in plan.changes.iter().zip(lines) {
            let level = if change.kind == arena_core::proxy::ChangeKind::Unchanged { To::Detail } else { To::Out };
            out!(level, "{line}");
        }
    }
    out!(Out, "{}", plan.summary());
    for s in &plan.skipped {
        out!(ErrDetail, "warning: skipped {} — {}", s.name, s.reason);
    }
    // Pods skipped for the same reason: one line.
    let mut reasons: Vec<(&str, Vec<&str>)> = Vec::new();
    for s in &plan.skipped {
        match reasons.iter_mut().find(|(r, _)| *r == s.reason) {
            Some((_, names)) => names.push(&s.name),
            None => reasons.push((&s.reason, vec![&s.name])),
        }
    }
    for (reason, names) in &reasons {
        out!(ErrBrief, "warning: skipped {} — {reason}", crate::pods::setup::some_names(names));
    }

    let nginx = arena_core::proxy::render_nginx(&plan.forwards);
    out!(Detail, "\n{}", plan_config_header(&pxcfg.nginx_path, prev.is_some()));
    out!(Detail, "{nginx}");

    if let Some(path) = out {
        std::fs::write(path, &nginx)
            .with_context(|| format!("writing nginx config to {}", path.display()))?;
        out!(Err, "wrote nginx config to {} (local only — not deployed)", path.display());
    } else {
        out!(
            Detail,
            "# Review, then `arena proxy apply` to {}. (--out <file> saves it locally.)",
            proxy_action(&pxcfg)
        );
        // A merge that would change the file: the command that writes it.
        let c = plan.counts();
        if prev.is_some() && c.added + c.changed + c.removed > 0 {
            out!(Brief, "To write it: arena proxy apply (-v prints the nginx config)");
        }
    }
    Ok(())
}

/// The header above the config `proxy plan` prints. Only a real merge (the current config
/// was read) may say "write to … on the proxy"; a plan from the listing alone is labelled
/// as the preview it is, so nobody deploys it by hand.
fn plan_config_header(nginx_path: &str, merged: bool) -> String {
    if merged {
        format!("# ----- nginx config (write to {nginx_path} on the proxy) -----")
    } else {
        "# ----- nginx config: LISTING-ONLY PREVIEW, not a merge — it omits every forward \
         `proxy apply` would keep; do NOT deploy it by hand (`arena proxy apply --dry-run`) -----"
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    /// No pods: `plan check` never asks.
    struct NoFleet;

    #[async_trait::async_trait]
    impl arena_core::Provider for NoFleet {
        fn name(&self) -> &'static str {
            "runpod"
        }
        fn describe(&self, _spec: &arena_core::PodSpec) -> String {
            String::new()
        }
        async fn list_pods(&self) -> arena_core::Result<Vec<arena_core::Pod>> {
            Ok(Vec::new())
        }
        async fn create_pod(&self, _spec: &arena_core::PodSpec) -> arena_core::Result<arena_core::Pod> {
            unimplemented!("not exercised")
        }
        async fn stop_pod(&self, _id: &str) -> arena_core::Result<()> {
            Ok(())
        }
        async fn restart_pod(&self, _id: &str) -> arena_core::Result<()> {
            Ok(())
        }
        async fn terminate_pod(&self, _id: &str) -> arena_core::Result<()> {
            Ok(())
        }
    }

    /// `plan check` and `plan show` end with their answer by default; the notes about what isn't
    /// wired yet are `-v`'s (the dates move with the clock, so no golden here).
    #[tokio::test]
    async fn plan_check_and_show_end_with_the_answer() {
        let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../arena-plan.example.json");
        let cfg = arena_core::Config::parse("MACHINE_NAME_PREFIX=devtest\n");
        for verbose in [false, true] {
            let check = super::PlanCmd::Check { file: Some(file.clone()) };
            let text = crate::output_test::rendered(verbose, &[], super::handle_plan(check, &NoFleet, &cfg)).await;
            assert!(text.contains("gpus:       A4000 > 3090 > A5000\n"), "{text}");
            let end = if verbose {
                "\n\n✓ plan parses. (Scheduled apply + arming aren't wired yet — preview with `plan show`.)\n"
            } else {
                "gpu\n✓ plan parses\n"
            };
            assert!(text.ends_with(end), "{text}");
            let show = super::PlanCmd::Show { file: Some(file.clone()), date: Some("2026-06-10".into()) };
            let text = crate::output_test::rendered(verbose, &[], super::handle_plan(show, &NoFleet, &cfg)).await;
            assert!(text.starts_with("Plan for 2026-06-10: 8 pods × 1 GPU/pod\n"), "{text}");
            assert_eq!(text.contains("(Preview only"), verbose, "{text}");
        }
    }

    #[test]
    fn plan_config_header_only_says_write_for_a_real_merge() {
        use super::plan_config_header;
        assert_eq!(
            plan_config_header("/srv/p.conf", true),
            "# ----- nginx config (write to /srv/p.conf on the proxy) -----"
        );
        let preview = plan_config_header("/srv/p.conf", false);
        assert!(preview.contains("LISTING-ONLY PREVIEW") && preview.contains("do NOT deploy"), "{preview}");
        assert!(!preview.contains("write to"), "{preview}");
    }
}
