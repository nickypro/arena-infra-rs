//! The AI key's rows in the action menus: raise or set a pod's OpenRouter cap (`a`), pause or
//! resume its key (`o`), rotate it (`n`, the cursor pod only) — for the cursor pod, and cap,
//! pause and resume for a marked set. Each runs the CLI's own command as a child (`runs.rs`):
//! `arena keys limit <pods> <amount> --yes`, `keys pause|resume <pods> --yes`, `keys rotate <pod>
//! --copy --yes` — so the dashboard and the CLI share one path (the plan, the refusals, the
//! workspace guard, the read-back). Once a run ends the `AI$` column is read again
//! (`Shared::ai_wanted`).
//!
//! The rows are there when a provisioning key is configured (`Ui::ai_keys`: fixed for the
//! dashboard's life, so no row moves under the cursor); whether a pod's key can be acted on is
//! checked when one is chosen ([`support`], the way `Provider::rename_support` gates rename),
//! and the menu greys the rows with why until it can.

use std::sync::{Arc, Mutex};

use arena_core::openrouter::{self as or, KeyInfo, KeyUsage};
use arena_core::{Config, Pod};

use crate::state::{Action, Confirm};
use crate::{runs, scope_pods_of, InputScope, Mode, Scope, Shared, Ui};

/// `pod`'s OpenRouter key as last read — its usage and its listing — or why the AI key's rows
/// can't act on it: no provisioning key, the keys not read yet (or unreadable, with no earlier
/// answer), or no key named after the pod.
pub(crate) fn support(shared: &Shared, pod: &Pod) -> Result<(KeyUsage, KeyInfo), String> {
    let Some(view) = shared.ai.as_ref() else { return Err("no OpenRouter provisioning key in the config".into()) };
    match (view.keys.get(&pod.name), view.infos.get(&pod.name)) {
        (Some(u), Some(k)) => Ok((u.clone(), k.clone())),
        _ if !view.read => Err("OpenRouter's keys aren't read yet — in a moment".into()),
        _ => match &view.keys_error {
            Some(e) => Err(format!("OpenRouter's keys couldn't be read ({e})")),
            None => Err(format!("no OpenRouter key is named after it — `arena keys gen {}` mints one", pod.name)),
        },
    }
}

/// The cursor pod's menu ([`Action::pod_menu`]) — its `o` row pause or resume as the key is — and,
/// when the AI key's rows can't act on it, why (the menu greys them with it).
pub(crate) fn pod_rows(shared: &Shared, ui: &Ui, pod: Option<&Pod>) -> (Vec<(char, Action)>, Option<String>) {
    let support = pod.map(|p| support(shared, p));
    let off = matches!(&support, Some(Ok((u, _))) if u.disabled);
    let rows = Action::pod_menu(ui.ai_keys).into_iter().map(|(k, a)| (k, a.for_key(off))).collect();
    let why = match support {
        _ if !ui.ai_keys => None,
        Some(Ok(_)) => None,
        Some(Err(e)) => Some(e),
        None => Some("no pod".into()),
    };
    (rows, why)
}

/// Why a set's AI key rows can't act: none of its pods has a key that can be ([`support`]).
pub(crate) fn set_why(shared: &Shared, pods: &[&Pod]) -> Option<String> {
    let keyed = pods.iter().filter(|p| support(shared, p).is_ok()).count();
    (keyed == 0).then(|| match pods.first() {
        Some(p) if pods.len() == 1 => support(shared, p).err().unwrap_or_default(),
        _ => "none of them has an OpenRouter key that was read".into(),
    })
}

/// What the raise-cap box opens with: `+` the cap a new key gets (`OPENROUTER_KEY_LIMIT`, else 5:
/// `+5`) — one more key's worth.
pub(crate) fn prefill(cfg: &Config) -> String {
    let x = cfg.get_parsed::<f64>("OPENROUTER_KEY_LIMIT").filter(|x| x.is_finite() && *x > 0.0).unwrap_or(5.0);
    let s = format!("{x:.2}");
    format!("+{}", s.trim_end_matches('0').trim_end_matches('.'))
}

/// The `arena` arguments for the AI key's `action` on `names` (`arg`: the amount for a cap), and
/// the run's kind. The dashboard has confirmed, so `--yes`, and the keys it showed go along as
/// they were (`--expect HASH=STATE` each, `keys`): the command refuses one that has been replaced
/// or has moved since (review codex r2 #3). Rotate copies the new key onto the pod (`--copy`).
/// `None`: not one of them, or a cap with no amount. Pure.
pub(crate) fn args(action: Action, names: &[String], arg: Option<&str>, keys: &[KeyInfo]) -> Option<(runs::Kind, Vec<String>)> {
    let (kind, verb) = match action {
        Action::KeyLimit => (runs::Kind::KeyLimit, "limit"),
        Action::KeyPause => (runs::Kind::KeyPause, "pause"),
        Action::KeyResume => (runs::Kind::KeyResume, "resume"),
        Action::KeyRotate => (runs::Kind::KeyRotate, "rotate"),
        _ => return None,
    };
    let mut out = vec!["keys".to_string(), verb.to_string()];
    out.extend(names.iter().cloned());
    match action {
        Action::KeyLimit => out.push(arg.map(str::trim).filter(|a| !a.is_empty())?.to_string()),
        Action::KeyRotate => out.push("--copy".into()),
        _ => {}
    }
    // Rotate deletes whatever keys the name has: nothing to bind (it has no `--expect`).
    if action != Action::KeyRotate {
        for k in keys {
            out.push("--expect".into());
            out.push(or::Expected::of(k).token());
        }
    }
    out.push("--yes".into());
    Some((kind, out))
}

/// What the raise-cap box shows for `value` on `pods` (as last read): for one pod `spent $0.42
/// of $5.00 this month` and `new cap: $10.00`; for several a line each (`alpha: cap $5.00 →
/// $10.00 · spent …`, a pod without a key left out). `Err`: why Enter won't run it — an amount
/// that isn't one, or a cap the CLI would refuse ([`or::plan_cap`], never forced from here).
/// Pure.
pub(crate) fn limit_preview(shared: &Shared, pods: &[Pod], value: &str) -> Result<Vec<String>, Vec<String>> {
    let money = |x: f64| arena_core::fleet::fmt_money("$", x);
    let change = or::parse_cap(value).map_err(|e| vec![e])?;
    let (mut lines, mut refused) = (Vec::new(), Vec::new());
    for pod in pods {
        let Ok((_, info)) = support(shared, pod) else {
            lines.push(format!("{}: no AI key — left out", pod.name));
            continue;
        };
        match or::plan_cap(&info, Some(change), None, false) {
            Err(why) if pods.len() == 1 => refused.push(why),
            Err(why) => refused.push(format!("{}: {why}", pod.name)),
            Ok(p) if pods.len() == 1 => {
                let cap = |x: Option<f64>| x.map(money).unwrap_or_else(|| "no cap".into());
                let spent = p.spent.map(money).unwrap_or_else(|| "?".into());
                lines.push(format!("spent {spent} of {} {}", cap(p.before), or::period_words(p.reset_after)));
                let mut after = format!("new cap: {}", cap(p.after));
                if !p.changes() {
                    after.push_str(" (unchanged)");
                } else if p.still_at_cap() {
                    after.push_str(" — still at its cap");
                }
                if info.disabled {
                    after.push_str(" — the key is paused: `o` resumes it");
                }
                lines.push(after);
            }
            Ok(p) => lines.push(format!("{}: {}", pod.name, p.words())),
        }
    }
    if refused.iter().any(|r| r.contains("--force")) {
        refused.push("(the dashboard never forces it: `arena keys limit … --force`)".into());
    }
    if refused.is_empty() {
        Ok(lines)
    } else {
        Err(refused)
    }
}

/// A pick of the AI key's `action` for the cursor `pod` (shown as `shown`): refused with why when
/// its key can't be acted on; a cap opens the amount box, prefilled ([`prefill`]); pause, resume
/// (whichever the key's state calls for) and rotate a `y` confirm that says the key's state now
/// and (`v`) the command.
pub(crate) fn choose(shared: &Arc<Mutex<Shared>>, ui: &mut Ui, pod: Pod, shown: String, action: Action) {
    let found = support(&shared.lock().unwrap(), &pod);
    let (usage, info) = match found {
        Ok(k) => k,
        Err(why) => {
            ui.mode = Mode::Result(format!("✗ can't {} {} — {why}", action.label(), pod.name));
            return;
        }
    };
    let action = action.for_key(usage.disabled);
    if action == Action::KeyLimit {
        ui.mode = Mode::Input { action, scope: InputScope::Pod { name: shown, id: pod.id }, value: prefill(&ui.cfg) };
        return;
    }
    let preview = args(action, std::slice::from_ref(&pod.name), None, std::slice::from_ref(&info)).map(|(_, a)| format!("arena {}", a.join(" ")));
    let mut note = format!("ai key now: {}", usage.detail());
    if action == Action::KeyRotate {
        let cap = ui.cfg.get_parsed::<f64>("OPENROUTER_KEY_LIMIT").unwrap_or(5.0);
        let at = std::env::current_dir().map(|d| d.join("keys").display().to_string()).unwrap_or_else(|_| "./keys".into());
        note.push_str(&format!("\nthe new key: cap {} (OPENROUTER_KEY_LIMIT), saved in {at}", arena_core::fleet::fmt_money("$", cap)));
    }
    let mut c = Confirm::new(action, shown, pod.id, preview);
    c.note = Some(note);
    c.ai = vec![(pod.name, info)];
    ui.mode = Mode::Confirm(c);
}

/// A pick of the AI key's `action` for a marked set: refused when none of its pods has a key that
/// can be acted on; a cap opens the amount box; pause and resume the set's typed-count confirm,
/// noting the pods left out.
pub(crate) fn choose_set(shared: &Arc<Mutex<Shared>>, ui: &mut Ui, action: Action, scope: Scope) {
    let (why, left_out, keyed) = {
        let s = shared.lock().unwrap();
        let pods = scope_pods_of(&s, ui, scope);
        let out: Vec<String> = pods.iter().filter(|p| support(&s, p).is_err()).map(|p| ui.shown_name(&p.name)).collect();
        let owned: Vec<Pod> = pods.iter().map(|p| (*p).clone()).collect();
        (set_why(&s, &pods), out, snapshot(&s, &owned))
    };
    // What the confirm is opened with is what `ALL`/the count runs (review codex r3 #1).
    ui.ai_set = keyed;
    if let Some(why) = why {
        ui.mode = Mode::Result(format!("✗ can't {} — {why}", action.label()));
        return;
    }
    let note = if left_out.is_empty() { String::new() } else { format!("Left out (no AI key that was read): {}", left_out.join(", ")) };
    ui.mode = match action {
        Action::KeyLimit => Mode::Input { action, scope: InputScope::Set(scope), value: prefill(&ui.cfg) },
        _ => Mode::FleetConfirm { action, typed: String::new(), scope, note },
    };
}

/// Those of `pods` with a key, and the key as last read ([`support`]) — what a run is bound to.
pub(crate) fn snapshot(shared: &Shared, pods: &[Pod]) -> Vec<(String, KeyInfo)> {
    pods.iter().filter_map(|p| support(shared, p).ok().map(|(_, k)| (p.name.clone(), k))).collect()
}

/// Run the AI key's `action` on `keyed` (pod names and their keys as the operator was shown them,
/// [`snapshot`]) as one CLI run (`arg`: a cap's amount), each key bound as shown ([`args`]), and
/// open the runs panel on it — or say why it can't run. `provider`: one pod's own (the CLI finds
/// a pod fleet-wide whatever `--provider` says, but its messages then name the right one).
pub(crate) fn start(ui: &mut Ui, shared: &Arc<Mutex<Shared>>, action: Action, keyed: &[(String, KeyInfo)], provider: Option<&str>, arg: Option<&str>) {
    if keyed.is_empty() {
        ui.mode = Mode::Result(format!("✗ can't {} — none of these pods has an OpenRouter key that was read", action.label()));
        return;
    }
    let (names, keys): (Vec<String>, Vec<KeyInfo>) = keyed.iter().cloned().unzip();
    let Some((kind, argv)) = args(action, &names, arg, &keys) else { return };
    let title = match keyed {
        [(one, _)] => format!("{} {}", action.command(), ui.shown_name(one)),
        _ => format!("{} {}", action.command(), crate::state::count_of(names.len(), "pod", "pods")),
    };
    let provider = provider.filter(|p| !p.is_empty()).map_or_else(|| ui.provider_name.clone(), String::from);
    ui.mode = match runs::start(ui, shared, kind, &provider, argv, title) {
        Ok(id) => Mode::Runs { id, scroll: None, stop: false },
        Err(e) => Mode::Result(format!("✗ {e}")),
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena_core::openrouter::UsageView;

    fn pod(name: &str) -> Pod {
        Pod { id: format!("id-{name}"), name: name.into(), provider: "hetzner".into(), status: "running".into(), ..Default::default() }
    }

    /// `devtest-alpha` $0.42 of $5 this month, `devtest-bravo` paused at $4.90 of $5 in all; no
    /// key for `devtest-cloud`.
    fn shared_with_keys() -> Shared {
        let key = |name: &str, limit: f64, spent: f64, reset: Option<&str>, off: bool| KeyInfo {
            hash: format!("h-{name}"),
            name: Some(name.into()),
            limit: Some(limit),
            limit_remaining: Some(limit - spent),
            limit_reset: reset.map(String::from),
            usage: Some(spent),
            usage_monthly: Some(spent),
            disabled: off,
            ..Default::default()
        };
        let view = UsageView::from_keys(&[key("devtest-alpha", 5.0, 0.42, Some("monthly"), false), key("devtest-bravo", 5.0, 4.9, None, true)], None);
        Shared { ai: Some(view), ..Default::default() }
    }

    #[test]
    fn support_says_why_a_pods_key_cant_be_acted_on() {
        let s = shared_with_keys();
        assert_eq!(support(&s, &pod("devtest-alpha")).unwrap().1.hash, "h-devtest-alpha");
        assert!(support(&s, &pod("devtest-cloud")).unwrap_err().contains("`arena keys gen devtest-cloud` mints one"));
        assert!(support(&Shared::default(), &pod("devtest-alpha")).unwrap_err().contains("no OpenRouter provisioning key"));
        let unread = Shared { ai: Some(UsageView::default()), ..Default::default() };
        assert!(support(&unread, &pod("devtest-alpha")).unwrap_err().contains("aren't read yet"));
        let failed = Shared { ai: Some(UsageView { read: true, keys_error: Some("HTTP 500".into()), ..Default::default() }), ..Default::default() };
        assert!(support(&failed, &pod("devtest-alpha")).unwrap_err().contains("couldn't be read (HTTP 500)"));
    }

    /// The command lines the rows run — pinned on the CLI's side too, by
    /// `key_limits::tests::the_dashboards_key_command_lines_parse_as_it_means_them`.
    #[test]
    fn args_build_the_clis_own_commands() {
        let one = vec!["devtest-alpha".to_string()];
        let two = vec!["devtest-alpha".to_string(), "devtest-bravo".into()];
        let line = |a: Action, names: &[String], arg: Option<&str>| args(a, names, arg, &[]).map(|(k, v)| (k, v.join(" ")));
        assert_eq!(line(Action::KeyLimit, &one, Some(" +5 ")), Some((runs::Kind::KeyLimit, "keys limit devtest-alpha +5 --yes".into())));
        assert_eq!(line(Action::KeyLimit, &two, Some("10")), Some((runs::Kind::KeyLimit, "keys limit devtest-alpha devtest-bravo 10 --yes".into())));
        assert_eq!(line(Action::KeyPause, &two, None), Some((runs::Kind::KeyPause, "keys pause devtest-alpha devtest-bravo --yes".into())));
        assert_eq!(line(Action::KeyResume, &one, None), Some((runs::Kind::KeyResume, "keys resume devtest-alpha --yes".into())));
        assert_eq!(line(Action::KeyRotate, &one, None), Some((runs::Kind::KeyRotate, "keys rotate devtest-alpha --copy --yes".into())));
        assert_eq!(line(Action::KeyLimit, &one, Some("  ")), None, "no amount, no run");
        assert_eq!(line(Action::Backup, &one, None), None);
        // The keys as shown go along; rotate takes none.
        let shown = [KeyInfo { hash: "ab12".into(), limit: Some(5.0), ..Default::default() }, KeyInfo { hash: "cd34".into(), disabled: true, ..Default::default() }];
        let bound = |a: Action, arg: Option<&str>| args(a, &two, arg, &shown).map(|(_, v)| v.join(" "));
        assert_eq!(bound(Action::KeyLimit, Some("+5")).as_deref(), Some("keys limit devtest-alpha devtest-bravo +5 --expect ab12=5/never/on --expect cd34=none/never/off --yes"));
        assert_eq!(bound(Action::KeyRotate, None).as_deref(), Some("keys rotate devtest-alpha devtest-bravo --copy --yes"));
    }

    #[test]
    fn the_box_opens_with_one_more_keys_worth() {
        assert_eq!(prefill(&Config::parse("")), "+5");
        assert_eq!(prefill(&Config::parse("OPENROUTER_KEY_LIMIT=2.5\n")), "+2.5");
        assert_eq!(prefill(&Config::parse("OPENROUTER_KEY_LIMIT=0.10\n")), "+0.1");
        assert_eq!(prefill(&Config::parse("OPENROUTER_KEY_LIMIT=junk\n")), "+5");
    }

    /// The raise-cap box: what the key spent of its cap and the new cap as typed; a set a line a
    /// pod; an amount that isn't one, or a cap under the spend, keeps Enter from running it.
    #[test]
    fn limit_preview_shows_spent_and_the_new_cap() {
        let s = shared_with_keys();
        let alpha = [pod("devtest-alpha")];
        assert_eq!(limit_preview(&s, &alpha, "+5").unwrap(), ["spent $0.42 of $5.00 this month", "new cap: $10.00"]);
        assert_eq!(limit_preview(&s, &alpha, "5").unwrap()[1], "new cap: $5.00 (unchanged)");
        assert_eq!(limit_preview(&s, &[pod("devtest-bravo")], "+0.05").unwrap()[1], "new cap: $5.05 — the key is paused: `o` resumes it");
        assert_eq!(limit_preview(&s, &[pod("devtest-bravo")], "+0.05").unwrap()[0], "spent $4.90 of $5.00 in all");
        let e = limit_preview(&s, &[pod("devtest-bravo")], "4").unwrap_err();
        assert!(e[0].starts_with("a $4.00 cap is under what it has spent in all ($4.90)") && e[1].contains("never forces"), "{e:?}");
        assert!(limit_preview(&s, &alpha, "abc").unwrap_err()[0].contains("isn't an amount"));
        assert!(limit_preview(&s, &alpha, "none").unwrap_err()[0].contains("--no-cap"));
        let set = [pod("devtest-alpha"), pod("devtest-bravo"), pod("devtest-cloud")];
        assert_eq!(
            limit_preview(&s, &set, "+1").unwrap(),
            [
                "devtest-alpha: cap $5.00 → $6.00 (monthly) · spent $0.42 this month",
                "devtest-bravo: cap $5.00 → $6.00 · spent $4.90 in all",
                "devtest-cloud: no AI key — left out",
            ]
        );
        assert!(limit_preview(&s, &set, "4.5").unwrap_err()[0].starts_with("devtest-bravo: a $4.50 cap"));
    }
}
