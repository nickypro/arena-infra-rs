//! `pods up` as one independent pipeline per pod (PLAN 2.B) — the pure half.
//!
//! After the create, each pod runs its own pipeline: wait for *its* SSH endpoint → proxy
//! sync → setup → (`--check`) deep check → API keys → `[name] READY` or `[name] FAILED
//! <stage>`, printed the moment that pod finishes. That's the ops playbook's "bring a whole
//! fleet up as independent pipelines, never as a batch": one slow pod never gates the
//! others, and the per-pod line is the honest progress report.
//!
//! With `--check` a pod that FAILs the deep check on its host ([`crate::health::FailureKind`])
//! is on a bad machine ("it will not fix itself"): it is terminated and its name recreated
//! from the confirmed placement options —
//! the ones that haven't failed first ([`replacement_order`]) — and a replacement that lands
//! on a host that already failed is rejected ([`on_failed_host`]: "same IP → same machine,
//! the rebuild fixed nothing"). Bounded by `--check-attempts` placements per name.
//!
//! With or without `--check`, a pod up without a usable SSH endpoint for `--endpoint-timeout`
//! ([`endpoint_timeout_mins`], [`crate::status::up_without_endpoint`]: on RunPod, its container
//! running behind RunPod's SSH proxy only) is a host failure too, replaced within the same bound
//! ([`AttemptEnd::NoEndpoint`]).
//!
//! The executor is I/O and lives in the CLI (it owns the proxy writer, setup and key
//! distribution); here are the decisions it takes and the report it ends with — the types
//! ([`Verdict`], [`Attempt`], [`UpRow`]) and [`render_up_summary`] — so they're table-tested
//! and the TUI can show a run the same way.

use std::time::Duration;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::health::Status;
use crate::table::{self, Align};

/// The config key for `pods up --endpoint-timeout`: whole minutes.
pub const ENDPOINT_TIMEOUT_KEY: &str = "ENDPOINT_TIMEOUT_MINS";

/// `--endpoint-timeout` when neither the flag nor [`ENDPOINT_TIMEOUT_KEY`] says: the grace the
/// proxy-only check has had since wave 6 (a healthy pod has its endpoint within seconds of its
/// container starting; one behind the proxy only stayed so 15+ minutes, live).
pub const DEFAULT_ENDPOINT_TIMEOUT_MINS: u64 = 5;

/// The most `--endpoint-timeout` / [`ENDPOINT_TIMEOUT_KEY`] take: past two hours it's a typo,
/// and `--timeout` has ended every wait long before.
pub const MAX_ENDPOINT_TIMEOUT_MINS: u64 = 120;

/// How long `pods up` lets a pod be up without a usable SSH endpoint before it's a host failure
/// and replaced: `flag` (`--endpoint-timeout`), else [`ENDPOINT_TIMEOUT_KEY`], else
/// [`DEFAULT_ENDPOINT_TIMEOUT_MINS`]; whole minutes, 1..=[`MAX_ENDPOINT_TIMEOUT_MINS`] (unset or
/// empty = the default). Read before anything is created, so a bad value fails the run then.
/// Pure.
pub fn endpoint_timeout_mins(cfg: &Config, flag: Option<u64>) -> Result<u64> {
    let range = 1..=MAX_ENDPOINT_TIMEOUT_MINS;
    if let Some(m) = flag {
        return if range.contains(&m) {
            Ok(m)
        } else {
            Err(Error::Config(format!("--endpoint-timeout must be whole minutes from 1 to {MAX_ENDPOINT_TIMEOUT_MINS}, not {m}")))
        };
    }
    let Some(raw) = cfg.get(ENDPOINT_TIMEOUT_KEY).map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(DEFAULT_ENDPOINT_TIMEOUT_MINS);
    };
    match raw.parse::<u64>() {
        Ok(m) if range.contains(&m) => Ok(m),
        _ => Err(Error::Config(format!(
            "{ENDPOINT_TIMEOUT_KEY} must be whole minutes from 1 to {MAX_ENDPOINT_TIMEOUT_MINS}, not {raw:?}"
        ))),
    }
}

/// Where a pod's pipeline is (and so where it can fail or be stopped). The proxy sync isn't
/// a stage a pod can fail at: a sync never fails a lifecycle command (the pod is fine, and
/// the final sync — or `arena proxy apply` — retries), so it's reported, not judged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// The name never got a pod: the create phase (capacity, its retry window, Ctrl+C)
    /// ended without one. Not a pipeline stage — it's how a requested name that no pipeline
    /// ran for still gets a row (and a non-zero exit) instead of vanishing from the report.
    Create,
    /// Waiting for the provider to report the pod's SSH endpoint, and for sshd to answer on it.
    Endpoint,
    /// Provisioning over SSH (deploy key, repo, tokens — `pods setup`).
    Setup,
    /// The deep check (`pods test --deep`), including replacing a pod that failed it.
    Check,
    /// Distributing the per-host API keys (`pods copy-keys`).
    Keys,
    /// `up --lock`: locking the pod, the last step (after any check — a pod `--check` must
    /// replace has to stay terminable).
    Lock,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Create => "create",
            Stage::Endpoint => "endpoint",
            Stage::Setup => "setup",
            Stage::Check => "check",
            Stage::Keys => "keys",
            Stage::Lock => "lock",
        }
    }
}

/// How a name's pipeline ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Every stage passed (a deep-check WARN counts as ready; the HEALTH column shows it).
    Ready,
    /// Gave up at `stage`; `reason` says why and what was left behind.
    Failed { stage: Stage, reason: String },
    /// Ctrl+C while at `stage`. Nothing is terminated on an interrupt: the pod (if any) is
    /// left exactly as it was.
    Stopped { stage: Stage },
}

impl Verdict {
    pub fn is_ready(&self) -> bool {
        matches!(self, Verdict::Ready)
    }

    /// The STATUS column: `READY`, `FAILED setup`, `STOPPED endpoint`.
    pub fn status_label(&self) -> String {
        match self {
            Verdict::Ready => "READY".to_string(),
            Verdict::Failed { stage, .. } => format!("FAILED {}", stage.label()),
            Verdict::Stopped { stage } => format!("STOPPED {}", stage.label()),
        }
    }
}

/// How one placement of a name ended, when it didn't simply stay the name's pod.
#[derive(Debug, Clone, PartialEq)]
pub enum AttemptEnd {
    /// Still the name's pod when the run ended — its fate is the row's [`Verdict`].
    Kept,
    /// FAILed the deep check (its notes).
    CheckFailed(String),
    /// A replacement that landed on a machine IP that had already failed the deep check:
    /// rejected before it was set up or checked.
    SameHost,
    /// Landed on a machine remembered as bad — one an `up --check` failed on its host, in an
    /// earlier run (within `snapshot::BAD_HOST_TTL_SECS`) or this one: rejected before it was
    /// set up or checked, on any attempt. Carries when and why it was found bad.
    KnownBad(String),
    /// Up without a usable SSH endpoint for `--endpoint-timeout` — RunPod's proxy-only host, with
    /// no public IP. A host failure, like a failed check, replaced with or without `--check`.
    /// Carries what it was, for the attempt line (`no public SSH endpoint (RunPod ssh proxy only)`).
    NoEndpoint(String),
    /// Never came up: listed `ERROR` (Shadeform: `error: <status details>`), or not up by its
    /// backend's ready deadline — a failed placement, terminated.
    Dead(String),
}

/// One placement of a name — its first create or a replacement.
#[derive(Debug, Clone, PartialEq)]
pub struct Attempt {
    /// The option it was created on, e.g. `1×RTX A4000 COMMUNITY`.
    pub option: String,
    pub pod_id: String,
    /// The machine ([`crate::health::host_key`]: its IP, or the provider's machine id) once
    /// its endpoint was known.
    pub host: Option<String>,
    pub end: AttemptEnd,
    /// We terminated it (only ever to make room for its replacement).
    pub terminated: bool,
}

/// The order a replacement tries the confirmed options in: every option that hasn't failed
/// a check for this name, in the confirmed order, then the failed ones. Failing on one GPU
/// type moves the name to the next (the playbook: "switching GPU type is the reliable way
/// to draw a different host"), while the order the operator agreed to is otherwise kept. The
/// failed ones stay at the back rather than being dropped — a different machine of the same
/// type can be fine, and [`on_failed_host`] catches a re-draw of the same machine. Pure.
pub fn replacement_order(options: usize, failed: &[usize]) -> Vec<usize> {
    let (fresh, tried): (Vec<usize>, Vec<usize>) = (0..options).partition(|k| !failed.contains(k));
    fresh.into_iter().chain(tried).collect()
}

/// Whether a pod on `host` sits on a machine that already failed the deep check this run —
/// any name's, since a bad host breaks every pod on it. An unknown host (no endpoint yet, or
/// nothing that names the machine — a proxy hostname, a Vast IP without Vast's machine id;
/// see [`crate::health::host_key`]) is never rejected: a false "same host" would throw away
/// a good pod. Pure.
pub fn on_failed_host(host: Option<&str>, failed_hosts: &[String]) -> bool {
    host.is_some_and(|h| !h.is_empty() && failed_hosts.iter().any(|f| f == h))
}

/// The confirmed names that got no pod in the create phase — each gets a row of its own in
/// the report (a retry round only ever retries confirmed names, so one dropped as taken
/// meanwhile, or past a met `-n` target, is listed too). At most as many as the create fell
/// short by: should a name be made in another's place, that is no shortfall. Pure.
pub fn not_created(confirmed: &[String], created: &[String]) -> Vec<String> {
    let short = confirmed.len().saturating_sub(created.len());
    confirmed.iter().filter(|n| !created.contains(n)).take(short).cloned().collect()
}

/// A compact elapsed time for the READY lines and column: `45s`, `4m10s`, `1h02m`.
pub fn fmt_elapsed(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// One name's line in the end-of-run table.
#[derive(Debug, Clone, PartialEq)]
pub struct UpRow {
    pub name: String,
    /// `1×RTX A4000` (the deep check's nvidia-smi view when there is one), `-` if none.
    pub gpu: String,
    /// `$0.17`, `~$0.17` (preset estimate), or `-`.
    pub price: String,
    /// The stable proxy port routed to this pod, if any.
    pub proxy_port: Option<u16>,
    /// The deep check's verdict on the name's final pod; `None` = not checked.
    pub health: Option<Status>,
    /// The check's failures/warnings (`check: detail; …`), for the lines under the table.
    pub health_notes: String,
    pub verdict: Verdict,
    /// From the name's first create to confirmed-ready — replacements included: boot time
    /// and rebuild churn are the operator's cost, so participants' clocks start at READY.
    pub ready_after: Option<Duration>,
    pub attempts: Vec<Attempt>,
}

/// `#2 1×RTX 3090 COMMUNITY (id2, 10.0.0.2): check FAIL — cuda: …; terminated`.
fn attempt_line(n: usize, a: &Attempt, verdict: &Verdict) -> String {
    let at = match &a.host {
        Some(h) => format!("{}, {h}", a.pod_id),
        None => a.pod_id.clone(),
    };
    let fate = if a.terminated { "; terminated" } else { "; left running" };
    let what = match &a.end {
        AttemptEnd::CheckFailed(notes) => format!("check FAIL — {notes}{fate}"),
        AttemptEnd::SameHost => format!("same host as a failed check — rejected{fate}"),
        AttemptEnd::KnownBad(when) => format!("on a machine that failed a deep check {when} — rejected{fate}"),
        AttemptEnd::NoEndpoint(what) => format!("{what}{fate}"),
        AttemptEnd::Dead(why) => format!("never came up — {why}{fate}"),
        AttemptEnd::Kept => match verdict {
            Verdict::Ready => "ready".to_string(),
            Verdict::Failed { stage, .. } => format!("FAILED {}", stage.label()),
            Verdict::Stopped { stage } => format!("stopped at {}", stage.label()),
        },
    };
    format!("#{n} {} ({at}): {what}", a.option)
}

/// The end-of-run report: `NAME GPU $/H PROXY PORT HEALTH STATUS READY AFTER`, then a line
/// per name with something to explain — why it failed, every placement it took when there
/// was more than one (or one was rejected), a deep-check warning — then the tally. Pure.
pub fn render_up_summary(rows: &[UpRow]) -> String {
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            vec![
                r.name.clone(),
                r.gpu.clone(),
                r.price.clone(),
                r.proxy_port.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
                r.health.map(|s| s.label().to_string()).unwrap_or_else(|| "-".into()),
                r.verdict.status_label(),
                r.ready_after.map(fmt_elapsed).unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    let mut out = table::render(
        &["NAME", "GPU", "$/H", "PROXY PORT", "HEALTH", "STATUS", "READY AFTER"],
        &[Align::Left, Align::Left, Align::Right, Align::Right, Align::Left, Align::Left, Align::Right],
        &cells,
    );
    let mut notes = Vec::new();
    for r in rows {
        let eventful = r.attempts.len() > 1 || r.attempts.iter().any(|a| a.end != AttemptEnd::Kept);
        if eventful {
            let chain: Vec<String> =
                r.attempts.iter().enumerate().map(|(i, a)| attempt_line(i + 1, a, &r.verdict)).collect();
            notes.push(format!("{}: {}", r.name, chain.join(" → ")));
        }
        match &r.verdict {
            Verdict::Failed { stage, reason } => notes.push(format!("{}: FAILED {} — {reason}", r.name, stage.label())),
            Verdict::Stopped { stage } => {
                notes.push(format!("{}: stopped at {} (Ctrl+C) — left as it was", r.name, stage.label()))
            }
            Verdict::Ready if r.health == Some(Status::Warn) => notes.push(format!("{}: warn — {}", r.name, r.health_notes)),
            Verdict::Ready => {}
        }
    }
    if !notes.is_empty() {
        out.push('\n');
        for n in notes {
            out.push_str(&n);
            out.push('\n');
        }
    }
    let count = |f: fn(&Verdict) -> bool| rows.iter().filter(|r| f(&r.verdict)).count();
    let ready = count(|v| matches!(v, Verdict::Ready));
    let failed = count(|v| matches!(v, Verdict::Failed { .. }));
    let stopped = count(|v| matches!(v, Verdict::Stopped { .. }));
    let mut tally = format!("\n{ready} ready, {failed} failed");
    if stopped > 0 {
        tally.push_str(&format!(", {stopped} stopped"));
    }
    tally.push_str(&format!(" (of {})", rows.len()));
    if ready > 0 {
        tally.push_str(" — READY AFTER runs from the name's first create, replacements included");
    }
    out.push_str(&tally);
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_order_puts_failed_options_last_and_keeps_the_confirmed_order() {
        // (options, failed so far, order to try)
        let cases: &[(usize, &[usize], &[usize])] = &[
            (3, &[0], &[1, 2, 0]),       // failed on the first → the next one
            (3, &[1], &[0, 2, 1]),       // failed on the second → the rest in order, it last
            (3, &[0, 1], &[2, 0, 1]),    // two failed → the one left, then them
            (3, &[1, 0], &[2, 0, 1]),    // (failed order doesn't matter, the confirmed one does)
            (3, &[0, 1, 2], &[0, 1, 2]), // all failed → as confirmed (a new host may still be fine)
            (1, &[0], &[0]),             // the single configured option: it again
            (2, &[], &[0, 1]),
            (2, &[0, 0], &[1, 0]), // a repeat failure is still one option
            (0, &[], &[]),
        ];
        for (n, failed, want) in cases {
            assert_eq!(replacement_order(*n, failed), *want, "{n} options, failed {failed:?}");
        }
    }

    #[test]
    fn only_a_known_failed_machine_ip_is_rejected() {
        let failed = vec!["10.0.0.1".to_string(), "10.0.0.7".to_string()];
        assert!(on_failed_host(Some("10.0.0.1"), &failed));
        assert!(on_failed_host(Some("10.0.0.7"), &failed));
        assert!(!on_failed_host(Some("10.0.0.2"), &failed));
        // Unknown (no endpoint, or nothing naming the machine — `host_key` gives None) never matches.
        assert!(!on_failed_host(None, &failed));
        assert!(!on_failed_host(Some(""), &["".to_string()]));
        assert!(!on_failed_host(Some("10.0.0.1"), &[]));
    }

    #[test]
    fn names_without_a_pod_are_the_shortfall_only() {
        let v = |s: &[&str]| s.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        // (confirmed, created, rows for)
        let cases: &[(&[&str], &[&str], &[&str])] = &[
            (&["a", "b"], &["a", "b"], &[]),
            (&["a", "b"], &["a"], &["b"]),           // explicit names: the one missing
            (&["a", "b", "c"], &[], &["a", "b", "c"]),
            (&["a", "b"], &["a", "c"], &[]),         // c made in b's place: no shortfall
            (&["a", "b", "c"], &["c", "d"], &["a"]), // short by one: the first one missing
            (&[], &["a"], &[]),
        ];
        for (confirmed, created, want) in cases {
            assert_eq!(not_created(&v(confirmed), &v(created)), v(want), "{confirmed:?} / {created:?}");
        }
    }

    #[test]
    fn elapsed_is_compact() {
        let s = Duration::from_secs;
        for (d, want) in [
            (s(0), "0s"),
            (s(45), "45s"),
            (s(60), "1m00s"),
            (s(250), "4m10s"),
            (s(3599), "59m59s"),
            (s(3600), "1h00m"),
            (s(3720), "1h02m"),
            (Duration::from_millis(59_900), "59s"),
        ] {
            assert_eq!(fmt_elapsed(d), want, "{d:?}");
        }
    }

    /// `--endpoint-timeout` over `ENDPOINT_TIMEOUT_MINS` over the default (5); whole minutes
    /// 1..=120 from either, anything else refused before a create.
    #[test]
    fn endpoint_timeout_comes_from_the_flag_then_the_config_then_the_default() {
        let cfg = |text: &str| Config::parse(text);
        for (text, flag, want) in [
            ("", None, Ok(5)),
            ("ENDPOINT_TIMEOUT_MINS=\n", None, Ok(5)),
            ("ENDPOINT_TIMEOUT_MINS= 8 \n", None, Ok(8)),
            ("ENDPOINT_TIMEOUT_MINS=120\n", None, Ok(120)),
            ("ENDPOINT_TIMEOUT_MINS=8\n", Some(3), Ok(3)),
            ("ENDPOINT_TIMEOUT_MINS=junk\n", Some(3), Ok(3)),
            ("", Some(1), Ok(1)),
            ("ENDPOINT_TIMEOUT_MINS=0\n", None, Err("ENDPOINT_TIMEOUT_MINS must be whole minutes from 1 to 120, not \"0\"")),
            ("ENDPOINT_TIMEOUT_MINS=121\n", None, Err("ENDPOINT_TIMEOUT_MINS must be whole minutes from 1 to 120, not \"121\"")),
            ("ENDPOINT_TIMEOUT_MINS=4.5\n", None, Err("ENDPOINT_TIMEOUT_MINS must be whole minutes from 1 to 120, not \"4.5\"")),
            ("ENDPOINT_TIMEOUT_MINS=-1\n", None, Err("ENDPOINT_TIMEOUT_MINS must be whole minutes from 1 to 120, not \"-1\"")),
            ("", Some(0), Err("--endpoint-timeout must be whole minutes from 1 to 120, not 0")),
            ("", Some(500), Err("--endpoint-timeout must be whole minutes from 1 to 120, not 500")),
        ] {
            let got = endpoint_timeout_mins(&cfg(text), flag).map_err(|e| e.to_string());
            assert_eq!(got, want.map_err(|e| format!("config error: {e}")), "{text:?} {flag:?}");
        }
    }

    fn attempt(option: &str, id: &str, host: Option<&str>, end: AttemptEnd, terminated: bool) -> Attempt {
        Attempt { option: option.into(), pod_id: id.into(), host: host.map(String::from), end, terminated }
    }

    fn row(name: &str, verdict: Verdict, attempts: Vec<Attempt>) -> UpRow {
        UpRow {
            name: name.into(),
            gpu: "1×RTX A4000".into(),
            price: "$0.17".into(),
            proxy_port: None,
            health: None,
            health_notes: String::new(),
            verdict,
            ready_after: None,
            attempts,
        }
    }

    #[test]
    fn summary_snapshot_explains_every_name_that_needs_it() {
        let kept = |id: &str, host: &str| attempt("1×RTX A4000 COMMUNITY", id, Some(host), AttemptEnd::Kept, false);
        let rows = vec![
            // Plain READY: a row, no note.
            UpRow {
                proxy_port: Some(9500),
                health: Some(Status::Pass),
                ready_after: Some(Duration::from_secs(250)),
                ..row("devtest-apple", Verdict::Ready, vec![kept("id1", "10.0.0.1")])
            },
            // Replaced once, then ready with a warning.
            UpRow {
                gpu: "1×RTX 3090".into(),
                price: "~$0.22".into(),
                proxy_port: Some(9501),
                health: Some(Status::Warn),
                health_notes: "network: 1.2 MB/s from huggingface.co".into(),
                ready_after: Some(Duration::from_secs(720)),
                ..row(
                    "devtest-bloom",
                    Verdict::Ready,
                    vec![
                        attempt("1×RTX A4000 COMMUNITY", "id2", Some("10.0.0.2"), AttemptEnd::CheckFailed("cuda: Error 999".into()), true),
                        attempt("1×RTX 3090 COMMUNITY", "id4", Some("10.0.0.4"), AttemptEnd::Kept, false),
                    ],
                )
            },
            // Every attempt used: the last FAIL is left running for a look.
            UpRow {
                health: Some(Status::Fail),
                ..row(
                    "devtest-cloud",
                    Verdict::Failed { stage: Stage::Check, reason: "no attempts left (--check-attempts 2); left running".into() },
                    vec![
                        attempt("1×RTX A4000 COMMUNITY", "id3", Some("10.0.0.3"), AttemptEnd::CheckFailed("cuda: Error 999".into()), true),
                        attempt("1×RTX 3090 COMMUNITY", "id5", Some("10.0.0.3"), AttemptEnd::SameHost, false),
                    ],
                )
            },
            row(
                "devtest-dune",
                Verdict::Failed { stage: Stage::Setup, reason: "timed out at repo + keys config after 300s; left running".into() },
                vec![kept("id6", "10.0.0.6")],
            ),
            UpRow { gpu: "-".into(), price: "-".into(), ..row("devtest-echo", Verdict::Stopped { stage: Stage::Endpoint }, vec![attempt("1×RTX A4000 COMMUNITY", "id7", None, AttemptEnd::Kept, false)]) },
        ];
        let want = "\
NAME           GPU             $/H  PROXY PORT  HEALTH  STATUS            READY AFTER
devtest-apple  1×RTX A4000   $0.17        9500  pass    READY                   4m10s
devtest-bloom  1×RTX 3090   ~$0.22        9501  warn    READY                  12m00s
devtest-cloud  1×RTX A4000   $0.17           -  fail    FAILED check                -
devtest-dune   1×RTX A4000   $0.17           -  -       FAILED setup                -
devtest-echo   -                 -           -  -       STOPPED endpoint            -

devtest-bloom: #1 1×RTX A4000 COMMUNITY (id2, 10.0.0.2): check FAIL — cuda: Error 999; terminated → #2 1×RTX 3090 COMMUNITY (id4, 10.0.0.4): ready
devtest-bloom: warn — network: 1.2 MB/s from huggingface.co
devtest-cloud: #1 1×RTX A4000 COMMUNITY (id3, 10.0.0.3): check FAIL — cuda: Error 999; terminated → #2 1×RTX 3090 COMMUNITY (id5, 10.0.0.3): same host as a failed check — rejected; left running
devtest-cloud: FAILED check — no attempts left (--check-attempts 2); left running
devtest-dune: FAILED setup — timed out at repo + keys config after 300s; left running
devtest-echo: stopped at endpoint (Ctrl+C) — left as it was

2 ready, 2 failed, 1 stopped (of 5) — READY AFTER runs from the name's first create, replacements included
";
        let got = render_up_summary(&rows);
        assert_eq!(got, want, "\n--- got ---\n{got}");
    }

    /// Item 13: a first attempt turned away for a machine remembered as bad is explained in
    /// the summary — when and why it was found bad — like any rejected placement.
    #[test]
    fn a_machine_remembered_as_bad_is_explained_in_the_summary() {
        let when = "3h ago (2026-10-11T20:13:05Z, devtest-bravo: cuda: CUDA unknown error)";
        let rows = vec![row(
            "devtest-apple",
            Verdict::Ready,
            vec![
                attempt("1×RTX 3070 COMMUNITY", "id1", Some("runpod machine ue6mmk7dp8qi"), AttemptEnd::KnownBad(when.into()), true),
                attempt("1×RTX A4000 COMMUNITY", "id2", Some("runpod machine m8x2k"), AttemptEnd::Kept, false),
            ],
        )];
        let got = render_up_summary(&rows);
        assert!(
            got.contains(&format!(
                "devtest-apple: #1 1×RTX 3070 COMMUNITY (id1, runpod machine ue6mmk7dp8qi): on a machine that failed a deep \
                 check {when} — rejected; terminated → #2 1×RTX A4000 COMMUNITY (id2, runpod machine m8x2k): ready\n"
            )),
            "{got}"
        );
    }

    #[test]
    fn status_labels_and_an_all_failed_tally() {
        assert_eq!(Verdict::Ready.status_label(), "READY");
        assert_eq!(Verdict::Failed { stage: Stage::Keys, reason: String::new() }.status_label(), "FAILED keys");
        assert_eq!(Verdict::Stopped { stage: Stage::Check }.status_label(), "STOPPED check");
        assert_eq!(Verdict::Failed { stage: Stage::Lock, reason: String::new() }.status_label(), "FAILED lock");
        assert_eq!(Verdict::Failed { stage: Stage::Create, reason: String::new() }.status_label(), "FAILED create");
        assert!(Verdict::Ready.is_ready() && !Verdict::Stopped { stage: Stage::Setup }.is_ready());
        let rows = vec![row("devtest-apple", Verdict::Failed { stage: Stage::Endpoint, reason: "no SSH endpoint after 600s".into() }, vec![])];
        let got = render_up_summary(&rows);
        assert!(got.ends_with("\n0 ready, 1 failed (of 1)\n"), "{got}");
        assert!(got.contains("devtest-apple: FAILED endpoint — no SSH endpoint after 600s\n"), "{got}");
    }
}
