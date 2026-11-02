//! The detail pane (`enter`): one pod's facts, its per-GPU table and sparklines.

use arena_core::metrics::{self, PodMetrics};
use arena_core::status::pod_status;
use arena_core::{fleet, snapshot, Pod};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Sparkline, Table, Wrap};

use crate::state::{self, count_of, mark_key, proxy_cell, proxy_detail, rtt_cell, Tone, RTT_EVERY};
use crate::table::{backup_summary, capacity_style, gpu_text, saved_style, status_style, sync_marker, sync_summary, temp_style, tone_style, truncate, util_style};
use crate::{Shared, Ui};

/// The detail view's Claude Code account for `pod`: its email (`none` when it's off), and that
/// its box may not have it yet when the last push was another account's. Nothing when it has no
/// assignment.
fn claude_account(state: Option<&arena_core::claude::State>, pod: &Pod) -> Option<(String, Tone)> {
    let a = state?.of(pod)?;
    let pushed = a.pushed.as_ref().map(|p| p.account.as_deref());
    Some(match (&a.account, pushed) {
        (Some(e), Some(p)) if p == Some(e.as_str()) => (e.clone(), Tone::Good),
        (Some(e), _) => (format!("{e} (not on it yet: `arena claude push`)"), Tone::Warn),
        (None, Some(None)) => ("none".into(), Tone::Dim),
        (None, _) => ("none (a token may still be on it: `arena claude push`)".into(), Tone::Warn),
    })
}

/// Lines in the detail pane's facts block (see `detail_pane`'s `facts`).
const FACT_LINES: u16 = 18;

/// The per-pod detail pane (shown in Detail mode): identity + endpoint, proxy forward,
/// cost, the host maintenance window + note, the last deep check (and why), a per-GPU
/// table, full progress text, and util/temp sparklines from the rolling history.
pub(crate) fn detail_pane(f: &mut Frame, shared: &Shared, ui: &Ui, area: Rect) {
    let Some(pod) = shared.pods.get(ui.selected) else { return };
    let m = shared.metrics.get(&pod.name);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(format!(" {} ", ui.shown_name(&pod.name)), Style::default().add_modifier(Modifier::BOLD)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Optional extra sections, shown only when there's data: GPU compute processes and
    // the most-recent ARENA_3.0 commits. Each costs one row for its TOP border/title.
    let n_procs = m.map(|m| m.gpu_procs.len()).unwrap_or(0).min(4);
    let n_commits = m.map(|m| m.recent_commits.len()).unwrap_or(0).min(4);
    let procs_h = if n_procs > 0 { n_procs as u16 + 1 } else { 0 };
    let commits_h = if n_commits > 0 { n_commits as u16 + 1 } else { 0 };
    // History graphs are a nice-to-have; show them only if the pane is still tall enough
    // once the facts, per-GPU table, and the extra sections have taken their space.
    // The OpenRouter key's line, under `cost:`, when a provisioning key is configured. "No
    // key" only from a listing that worked: before the first read, or after a failed one,
    // absence isn't known — and numbers kept from an older read say so.
    // In the AI$ cell's colour ([`ai_cell`]): yellow from 80% of the cap, red at it, grey when
    // paused, unknown or as last read.
    let ai = shared.ai.as_ref().map(|a| match (a.keys.get(&pod.name), a.read, a.keys_error.as_deref()) {
        (None, false, _) => ("… (reading OpenRouter's keys)".to_string(), Some(Tone::Dim)),
        (None, _, Some(e)) => (format!("? (OpenRouter's keys couldn't be read: {e})"), Some(Tone::Warn)),
        (None, _, None) => ("- (no OpenRouter key named after this pod — `arena keys gen` makes one)".to_string(), Some(Tone::Dim)),
        (Some(k), _, None) if k.paused() => (k.detail(), Some(Tone::Dim)),
        // Where to act on it — this pod's menu (`a`), its `ai …` rows — said first: the pane cuts
        // the line's end.
        (Some(k), _, None) if k.disabled && ui.ai_keys => {
            (k.detail().replacen("disabled · ", "disabled (a → ai resume) · ", 1), state::fill_tone(k.fill()))
        }
        (Some(k), _, None) if k.fill() == arena_core::openrouter::Fill::Full && ui.ai_keys => {
            (format!("at its cap (a → raise cap) · {}", k.detail()), state::fill_tone(k.fill()))
        }
        (Some(k), _, None) => (k.detail(), state::fill_tone(k.fill())),
        (Some(k), _, Some(_)) => (format!("{} (as last read; the latest read failed)", k.detail()), Some(Tone::Dim)),
    });
    // The cohort's keys at their cap, under it, while there are any: every name (the summary
    // bar has room for two) and where to look — a key can be at its cap with no pod in the
    // table (its machine terminated or renamed).
    let caps = shared.ai.as_ref().and(shared.ai_cohort.as_ref()).filter(|c| !c.capped.is_empty()).map(|c| {
        let names: Vec<String> = c.capped.iter().map(|k| if k.pod { k.name.clone() } else { format!("{} (no pod)", k.name) }).collect();
        // Where to look first: a long list is cut at the pane's edge.
        format!("⚠ {} at cap (`arena keys list` has each one's cap and spend): {}", count_of(names.len(), "key", "keys"), names.join(", "))
    });
    // Who the machine is for (`--owner`, `spend assign`), when it has an owner.
    let owner = shared.owners.get(&mark_key(pod));
    // Its AI keys as its last deep check found them (`ok, $4.97 of $5.00 left`, or the FAIL and
    // what to run): shown when that check judged them.
    let key_check = shared.snap.pods.get(ui.selected).and_then(|sp| sp.health.as_ref()).and_then(|h| state::key_check_line(&h.keys));
    let fact_lines = FACT_LINES
        + u16::from(ai.is_some())
        + u16::from(caps.is_some())
        + u16::from(owner.is_some())
        + u16::from(key_check.is_some());
    let show_graphs = inner.height >= fact_lines + 3 + procs_h + commits_h + 6;

    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(fact_lines), // header facts
        Constraint::Min(3),             // per-GPU table
    ];
    let mut idx = 2;
    let procs_idx = (procs_h > 0).then(|| {
        constraints.push(Constraint::Length(procs_h));
        let x = idx;
        idx += 1;
        x
    });
    let commits_idx = (commits_h > 0).then(|| {
        constraints.push(Constraint::Length(commits_h));
        let x = idx;
        idx += 1;
        x
    });
    if show_graphs {
        constraints.push(Constraint::Length(3)); // util sparkline
        constraints.push(Constraint::Length(3)); // temp sparkline
    }
    let rows = Layout::default().direction(Direction::Vertical).constraints(constraints).split(inner);

    // Each fact a line: its label dim, its value in the tone the table gives it (the same
    // helpers), so a glance at the colours says good/warn/bad — the words still say it all.
    let dim = Style::default().fg(Color::DarkGray);
    let plain = Style::default();
    let fact = |label: &str, spans: Vec<Span<'static>>| {
        let mut l = vec![Span::styled(format!("{label:<10}"), dim)];
        l.extend(spans);
        Line::from(l)
    };
    let one = |text: String, style: Style| vec![Span::styled(text, style)];
    // A value that's `-` (nothing to say) is dim.
    let or_dim = |text: String, style: Style| {
        let style = if text == "-" { dim } else { style };
        vec![Span::styled(text, style)]
    };
    let glyph = |b: Option<bool>| match b {
        Some(true) => Span::styled("✓", tone_style(Tone::Good)),
        Some(false) => Span::styled("✗", tone_style(Tone::Bad)),
        None => Span::styled("·", dim),
    };

    let endpoint = match (&pod.ssh_ip, pod.ssh_port) {
        (Some(ip), Some(port)) => one(format!("{ip}:{port}"), plain),
        _ => one("(no SSH endpoint yet)".into(), tone_style(Tone::Warn)),
    };
    // Where it runs (the provider's name for the place) and how far that is from here — the
    // proxy box, on prod — as the RTT column times it.
    let place = pod.location.clone().unwrap_or_else(|| "-".into());
    let (_, rtt_tone) = rtt_cell(shared.rtts.of(pod));
    let rtt = match (shared.rtts.of(pod), &pod.ssh_ip) {
        (Some(d), _) => format!("rtt {}ms from this box", d.as_millis()),
        (None, Some(_)) => format!("rtt - (a TCP connect to the SSH port, every {}s)", RTT_EVERY.as_secs()),
        (None, None) => "rtt -".into(),
    };
    let progress = match m {
        Some(m) if m.progress.is_some() => one(m.progress.clone().unwrap_or_default(), plain),
        Some(m) if m.error.is_some() => one(m.error.clone().unwrap_or_default(), tone_style(Tone::Bad)),
        _ => one("-".into(), dim),
    };
    let gpu = gpu_detail(pod, m);
    // `pods list`'s $/H label (the provider's currency; `-` unless billing), plus per day.
    let cost = match fleet::price_label(pod).as_str() {
        "-" => "-".to_string(),
        label => {
            let day = pod.cost_per_hr.map(|c| fleet::fmt_money(fleet::currency_symbol(&pod.provider), c * 24.0));
            format!("{label}/h  ({}/day)", day.unwrap_or_default())
        }
    };
    let sp = shared.snap.pods.get(ui.selected);
    // The snapshot lines are clipped to one row each (after the 10-column label), so a long
    // reason or note can't wrap and push the rest of the facts out of their block.
    let fit = |s: String| truncate(&s, (inner.width as usize).saturating_sub(10).max(8));
    let proxy = match sp {
        Some(sp) => one(fit(proxy_detail(sp)), tone_style(proxy_cell(sp).1)),
        None => one("?".into(), dim),
    };
    // The last deep check: verdict + age + worst issue as `arena snapshot` words it, and
    // the first failing/warning check's own words (the operator's view, so the raw text) —
    // both in the HLTH cell's colour.
    let record = sp.and_then(|sp| sp.health.as_ref());
    let health_style = match record {
        _ if shared.deep.is_running(pod) => tone_style(Tone::Busy),
        None => dim,
        Some(h) => tone_style(state::health_tone(h.status, shared.snap.generated_at.saturating_sub(h.checked_at))),
    };
    let health = match record {
        _ if shared.deep.is_running(pod) => "checking… (deep check running in the background)".to_string(),
        None => "- (never deep-checked — d runs one)".to_string(),
        Some(h) => format!(
            "{}  (checked {})",
            snapshot::health_label(Some(h), shared.snap.generated_at),
            snapshot::rfc3339(h.checked_at)
        ),
    };
    let why = fit(record.and_then(|h| h.reasons.first().cloned()).unwrap_or_else(|| "-".into()));
    let maint = fit(fleet::maintenance_label(pod.maintenance.as_ref()));
    let disk = match m.and_then(|m| m.disk_summary()) {
        Some((u, t)) => {
            let pct = if t > 0 { u as f64 / t as f64 * 100.0 } else { 0.0 };
            one(format!("{:.1}/{:.1}G ({pct:.0}%)", u as f64 / 1024.0, t as f64 / 1024.0), capacity_style(Some(pct as u32)))
        }
        None => one("-".into(), dim),
    };
    let origin = m.and_then(|m| m.origin.clone()).unwrap_or_else(|| "-".into());
    let origin_ok = m.and_then(|m| m.origin.as_deref().map(|o| o.contains("github.com")));
    // Host: CPU% and RAM (exact GB + %), coloured as the CPU%/RAM% columns.
    let host = {
        let cpu = m.and_then(|m| m.cpu_pct);
        let err = m.is_some_and(|m| m.error.is_some());
        let cpu_text = cpu.map(|c| format!("CPU {c}%")).unwrap_or_else(|| "CPU -".into());
        let ram = match m.and_then(|m| m.host_mem_summary()) {
            Some((u, t)) => {
                let pct = if t > 0 { u as f64 / t as f64 * 100.0 } else { 0.0 };
                Span::styled(format!("RAM {:.1}/{:.1}G ({pct:.0}%)", u as f64 / 1024.0, t as f64 / 1024.0), capacity_style(Some(pct as u32)))
            }
            None => Span::styled("RAM -", dim),
        };
        vec![Span::styled(cpu_text, if cpu.is_some() { util_style(cpu, err) } else { dim }), Span::raw("   "), ram]
    };
    // The sync line in the colour of SAVED's glyph: red unpushed/diverged, cyan behind, green in
    // sync, dim with no upstream.
    let sync = {
        let text = sync_summary(m);
        let (_, glyph_style) = sync_marker(m);
        let style = if text.starts_with('✓') {
            tone_style(Tone::Good)
        } else if text.starts_with('·') || text == "-" {
            dim
        } else {
            glyph_style
        };
        one(text, style)
    };
    let status = {
        let label = pod_status(pod, m.map(|m| m.error.is_none()));
        let mut v = vec![Span::styled(label.clone(), status_style(&label))];
        if arena_core::lock::is_locked(pod) {
            // One row (the facts block has a fixed height): `pods unlock` is in the docs.
            v.push(Span::raw(" · "));
            v.push(Span::styled("locked (stop/restart/terminate refused)", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
        }
        v
    };
    let mut facts = vec![
        fact("status:", status),
        fact("gpu:", or_dim(fit(gpu), plain)),
        fact("endpoint:", endpoint),
        // One row, as every fact (the block has a fixed height): the whole value cut to it (a
        // long place too — review ai-review codex r2), the RTT in its colour.
        fact("where:", {
            let line = fit(format!("{place}  ·  {rtt}"));
            let cut = place.chars().count() + 5;
            match line.char_indices().nth(cut) {
                Some((at, _)) => vec![Span::raw(line[..at].to_string()), Span::styled(line[at..].to_string(), tone_style(rtt_tone))],
                None => vec![Span::raw(line)],
            }
        }),
        fact("proxy:", proxy),
        fact("cost:", or_dim(cost, plain)),
    ];
    if let Some(o) = owner {
        facts.push(fact("owner:", one(fit(format!("{o}  (`arena spend` has what it cost)")), plain)));
    }
    if let Some((text, tone)) = ai {
        facts.push(fact("ai key:", one(fit(text), tone.map(tone_style).unwrap_or(plain))));
    }
    if let Some(c) = caps {
        facts.push(fact("ai caps:", one(fit(c), tone_style(Tone::Warn))));
    }
    if let Some((text, tone)) = key_check {
        facts.push(fact("ai check:", one(fit(text), tone_style(tone))));
    }
    facts.extend([
        fact("maint:", or_dim(maint, tone_style(Tone::Warn).add_modifier(Modifier::BOLD))),
        fact("health:", one(fit(health), health_style)),
        fact("reason:", or_dim(why, health_style)),
        fact("disk:", disk),
        fact("host:", host),
        fact("branch:", or_dim(m.and_then(|m| m.branch.clone()).unwrap_or_else(|| "-".into()), plain)),
        fact("backup:", or_dim(fit(backup_summary(pod, m)), saved_style(pod, m))),
        fact("sync:", sync),
        fact("origin:", vec![Span::raw(truncate(&origin, 32)), Span::raw(" "), glyph(origin_ok)]),
        fact(
            "setup:",
            vec![
                Span::raw(".name "),
                glyph(m.and_then(|m| m.has_name)),
                Span::raw("   deploy-key "),
                glyph(m.and_then(|m| m.has_key)),
                Span::raw("   origin→gh "),
                glyph(origin_ok),
                Span::raw("   api-key "),
                glyph(m.and_then(|m| m.has_api_key)),
            ],
        ),
        fact("tokens:", {
            let mut v = vec![Span::raw("HF "), glyph(m.and_then(|m| m.has_hf_token)), Span::raw("   Claude-Code "), glyph(m.and_then(|m| m.has_cc_token))];
            // Its account (`arena claude`), when it has one; `c` in the menu moves it.
            if let Some((text, tone)) = claude_account(shared.claude.as_ref(), pod) {
                v.push(Span::raw("  "));
                v.push(Span::styled(fit(text), tone_style(tone)));
            }
            v
        }),
        fact("progress:", progress),
    ]);
    f.render_widget(Paragraph::new(facts).wrap(Wrap { trim: true }), rows[0]);

    let gpu_rows: Vec<Row> = m
        .map(|m| {
            m.gpus
                .iter()
                .enumerate()
                .map(|(i, g)| {
                    let mem = match (g.mem_used_mb, g.mem_total_mb) {
                        (Some(u), Some(t)) => {
                            format!("{:.1}/{:.1}G", u as f64 / 1024.0, t as f64 / 1024.0)
                        }
                        _ => "-".into(),
                    };
                    Row::new(vec![
                        Cell::from(format!("{i}")),
                        Cell::from(g.util_pct.map(|u| format!("{u}%")).unwrap_or_else(|| "-".into()))
                            .style(util_style(g.util_pct, false)),
                        Cell::from(mem),
                        Cell::from(g.temp_c.map(|t| format!("{t}C")).unwrap_or_else(|| "-".into()))
                            .style(temp_style(g.temp_c)),
                    ])
                })
                .collect()
        })
        .unwrap_or_default();
    let gpu_table = Table::new(
        gpu_rows,
        [
            Constraint::Length(4),
            Constraint::Length(6),
            Constraint::Length(13),
            Constraint::Length(6),
        ],
    )
    .header(
        Row::new(vec!["GPU", "UTIL", "MEM", "TEMP"]).style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().borders(Borders::TOP).title(section("per-GPU")));
    f.render_widget(gpu_table, rows[1]);

    // GPU compute processes (what's actually holding the cards), under the per-GPU table.
    if let Some(pi) = procs_idx {
        let proc_rows: Vec<Row> = m
            .map(|m| {
                // Biggest VRAM first — surface the "main" jobs, not idle ~300M helpers.
                let mut procs: Vec<&metrics::GpuProc> = m.gpu_procs.iter().collect();
                procs.sort_by_key(|p| std::cmp::Reverse(p.mem_mb.unwrap_or(0)));
                procs
                    .into_iter()
                    .take(n_procs)
                    .map(|p| {
                        let mem = p.mem_mb.map(|mb| format!("{mb}M")).unwrap_or_else(|| "-".into());
                        // Show the basename; the full path is rarely useful and is long.
                        let name = p.name.rsplit('/').next().unwrap_or(&p.name).to_string();
                        Row::new(vec![
                            Cell::from(p.pid.to_string()),
                            Cell::from(mem),
                            Cell::from(name),
                        ])
                    })
                    .collect()
            })
            .unwrap_or_default();
        let proc_table = Table::new(
            proc_rows,
            [Constraint::Length(8), Constraint::Length(7), Constraint::Min(10)],
        )
        .header(
            Row::new(vec!["PID", "VRAM", "PROCESS"]).style(Style::default().add_modifier(Modifier::BOLD)),
        )
        .block(Block::default().borders(Borders::TOP).title(section("GPU procs")));
        f.render_widget(proc_table, rows[pi]);
    }

    // The last few ARENA_3.0 commits (= backup history), one compact line each.
    if let Some(ci) = commits_idx {
        let text = m
            .map(|m| m.recent_commits.iter().take(n_commits).cloned().collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();
        f.render_widget(
            Paragraph::new(text)
                .block(Block::default().borders(Borders::TOP).title(section("recent commits (ARENA repo)"))),
            rows[ci],
        );
    }

    if show_graphs {
        let hist = shared.history.get(&pod.name);
        let util_data = hist.map(|h| h.util_data()).unwrap_or_default();
        let temp_data = hist.map(|h| h.temp_data()).unwrap_or_default();
        let (util_title, util_colour) = if pod.cpu_only() { ("CPU % (history)", Color::Magenta) } else { ("util % (history)", Color::Green) };
        f.render_widget(
            Sparkline::default()
                .block(Block::default().borders(Borders::TOP).title(section(util_title)))
                .data(&util_data)
                .max(100)
                .style(Style::default().fg(util_colour)),
            rows[idx],
        );
        f.render_widget(
            Sparkline::default()
                .block(Block::default().borders(Borders::TOP).title(section("temp C (history)")))
                .data(&temp_data)
                .max(100)
                .style(Style::default().fg(Color::Yellow)),
            rows[idx + 1],
        );
    }
}

/// A detail-pane section's title: bold, in the accent colour.
fn section(title: &str) -> Span<'static> {
    Span::styled(title.to_string(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
}

/// The detail pane's `gpu:` line: the GPU column's words ([`gpu_text`], `RTX ` kept), and for a
/// MIG slice the provider's name with its memory spelled `GB` and the card it is cut from, as
/// nvidia-smi names it (`1×B300 MIG 34GB (MIG slice of B300 SXM6 AC)`). Pure.
pub(crate) fn gpu_detail(p: &Pod, m: Option<&PodMetrics>) -> String {
    match p.gpu_type.as_deref().and_then(fleet::mig_slice) {
        Some(_) => {
            let label = gpu_text(p, m, false);
            let label = label.strip_suffix('G').map(|l| format!("{l}GB")).unwrap_or(label);
            match m.and_then(|m| m.gpus.iter().find_map(|g| g.name.clone())) {
                Some(parent) => format!("{label} (MIG slice of {parent})"),
                None => label,
            }
        }
        None => gpu_text(p, m, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use arena_core::remote::FakeRemote;
    use arena_core::selector::Naming;
    use arena_core::snapshot::HealthCache;
    use ratatui::backend::TestBackend;
    use crate::actions::cli_action_args;
    use crate::state::{dashboard_snapshot, Action};
    use crate::tests::{cfg, draw, fixture_shared, pod, screen, ui, NOW};
    use crate::view::view;
    use crate::{runs, Mode};

    /// `c` (Claude Code account): runs `arena claude assign` — `spread` as its flag, anything else as
    /// the account — and the detail pane's `tokens:` line says which account the pod is on, and
    /// when its box may not have it yet.
    #[test]
    fn the_claude_action_runs_claude_assign_and_the_detail_names_the_account() {
        let args = |a: Option<&str>| cli_action_args(Action::Claude, "devtest-alpha", a).map(|(k, v)| (k, v.join(" ")));
        assert_eq!(args(Some(" 2 ")), Some((runs::Kind::Claude, "claude assign devtest-alpha 2 --yes".into())));
        assert_eq!(args(Some("Spread")), Some((runs::Kind::Claude, "claude assign devtest-alpha --spread --yes".into())));
        assert_eq!(args(Some("none")), Some((runs::Kind::Claude, "claude assign devtest-alpha none --yes".into())));
        assert_eq!(args(Some("  ")), None);
        assert!(Action::Claude.cli() && Action::Claude.needs_input() && Action::Claude.op().is_none());

        use arena_core::claude::{Assignment, How, Pushed, State};
        let alpha = pod("alpha", "runpod", "rp1", 1, Some(22001));
        let on = |account: Option<&str>, pushed: Option<Option<&str>>| State {
            machines: vec![Assignment {
                provider: "runpod".into(),
                id: "rp1".into(),
                name: alpha.name.clone(),
                account: account.map(String::from),
                how: How::Spread,
                at: NOW,
                pushed: pushed.map(|p| Pushed { account: p.map(String::from), fp: None, at: NOW }),
            }],
            ..State::default()
        };
        for (state, want) in [
            (on(Some("bob@x"), Some(Some("bob@x"))), Some(("bob@x".to_string(), Tone::Good))),
            (on(Some("bob@x"), Some(Some("alice@x"))), Some(("bob@x (not on it yet: `arena claude push`)".to_string(), Tone::Warn))),
            (on(Some("bob@x"), None), Some(("bob@x (not on it yet: `arena claude push`)".to_string(), Tone::Warn))),
            (on(None, Some(None)), Some(("none".to_string(), Tone::Dim))),
            (State::default(), None),
        ] {
            assert_eq!(claude_account(Some(&state), &alpha), want);
        }
        let snap = dashboard_snapshot(std::slice::from_ref(&alpha), &[], None, &HealthCache::new(), &Naming::from_config(&cfg("")), NOW);
        let mut shared = Shared::default();
        shared.publish(snap);
        shared.claude = Some(on(Some("bob@x"), Some(Some("bob@x"))));
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        ui.mode = Mode::Detail;
        let pane = draw(160, 40, &shared, &ui).join("\n");
        assert!(pane.contains("tokens:   HF ·   Claude-Code ·  bob@x"), "{pane}");
    }

    /// The human (2026-10-30): "the [enter] -> [overview] should have more colours". The detail
    /// pane's labels are dim, each value with a state in the table's tone for it — the words
    /// unchanged — and its sections' titles in the accent colour.
    #[test]
    fn the_detail_pane_colours_each_value_by_its_state() {
        let shared = fixture_shared();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        ui.mode = Mode::Detail;
        let colour_of = |ui: &Ui, text: &str| {
            let mut t = Terminal::new(TestBackend::new(160, 40)).unwrap();
            t.draw(|f| view(f, &shared, ui, 5)).unwrap();
            let buf = t.backend().buffer().clone();
            let lines = screen(&buf);
            let (y, line) = lines.iter().enumerate().find(|(_, l)| l.contains(text)).unwrap_or_else(|| panic!("{text:?} in\n{}", lines.join("\n")));
            let x = line[..line.find(text).unwrap()].chars().count() as u16;
            buf[(x, y as u16)].fg
        };
        ui.selected = 1; // bravo: failed its check, stale forward, locked
        for (text, colour) in [
            ("status:", Color::DarkGray),
            ("fail 2h GPU error", Color::Red),
            ("cuda: RuntimeError", Color::Red),
            (":9501 stale (`arena proxy apply`", Color::Yellow),
            ("locked (stop/restart/terminate refused)", Color::Cyan),
            ("per-GPU", Color::Cyan),
        ] {
            assert_eq!(colour_of(&ui, text), colour, "{text}");
        }
        ui.selected = 0; // alpha: passed, live forward, a maintenance window
        for (text, colour) in [("pass 12m", Color::Green), (":9500 (live)", Color::Green), ("maint 10-09 02:00", Color::Yellow)] {
            assert_eq!(colour_of(&ui, text), colour, "{text}");
        }
        ui.selected = 3; // delta: being checked
        assert_eq!(colour_of(&ui, "checking… (deep check running"), Color::Cyan);
    }

    /// The detail pane spells out what the row abbreviates: the proxy state, cost per hour
    /// and day, the maintenance window + note, the last check with its worst issue and the
    /// failing check's own words.
    #[test]
    fn detail_pane_spells_out_proxy_cost_maintenance_and_health() {
        let shared = fixture_shared();
        let mut ui = ui(cfg(""), Arc::new(FakeRemote::new()));
        ui.mode = Mode::Detail;
        let alpha = draw(160, 40, &shared, &ui).join("\n");
        for want in [
            "proxy:    :9500 (live)",
            "cost:     $0.17/h  ($4.08/day)",
            "maint:    maint 10-09 02:00→06:00 UTC · host upgrade",
            "health:   pass 12m  (checked 2026-10-08T11:48:00Z)",
        ] {
            assert!(alpha.contains(want), "{want:?} in\n{alpha}");
        }
        ui.selected = 1; // bravo
        let bravo = draw(160, 40, &shared, &ui).join("\n");
        for want in [
            "proxy:    :9501 stale (`arena proxy apply` re-points it)",
            "health:   fail 2h GPU error",
            "reason:   cuda: RuntimeError: Error 999",
            "maint:    -",
            "status:   init · locked (stop/restart/terminate refused)",
            "progress: -", // the snapshot lines don't push the rest out of the facts block
        ] {
            assert!(bravo.contains(want), "{want:?} in\n{bravo}");
        }
        assert!(!alpha.contains("locked"), "{alpha}");
        ui.selected = 3; // delta
        let delta = draw(160, 40, &shared, &ui).join("\n");
        assert!(delta.contains("health:   checking…") && delta.contains("cost:     -"), "{delta}");
    }
}
