//! Pod locks: what a lock protects, how a refusal reads, and what `pods lock`/`unlock`
//! would do. Pure, shared by the CLI and the TUI.
//!
//! RunPod REST v2 can lock a pod (`PATCH /v2/pods/{id} {"locked": true}`). A locked pod
//! refuses stop, restart and terminate from ANY client — this tool, a script, RunPod's own
//! web console: each answers `400 "Pod is locked"` and the container is untouched
//! (live-verified 2026-10-08). That makes it the way to protect a running cohort's pods from
//! an accident, wherever the accident comes from. Its spec words it as "Locked pods cannot
//! be stopped or reset", so a reimage (which resets the container) is assumed refused too,
//! and a rename (metadata only, restart-free) assumed allowed — neither is live-verified,
//! which is why the commands below don't lean on the API for them (see [`refusal`]).
//!
//! Lifting a lock is always an explicit step: `arena pods unlock`, or `pods terminate
//! --unlock` (which says so in its confirm text). Nothing here ever unlocks on its own.
//!
//! Two layers, so the operator always reads the same sentence ([`unlock_hint`]):
//! - **Up front**: a command refuses the pods the listing reports locked
//!   (`Pod::locked == Some(true)`) before it touches anything — so a batch never half-runs,
//!   and a reimage is never left to an API behaviour nobody has verified.
//! - **From the API**: a refusal the listing didn't predict (a pod locked since, or on a
//!   backend that doesn't report the flag) is a [`crate::ProviderErrorKind::Locked`] error,
//!   and [`explain`] turns it into the same sentence.

use crate::error::Error;
use crate::pod::Pod;

/// Whether the listing reports `pod` locked. `None` (a backend that doesn't say) is not
/// locked — the API's own refusal still catches it ([`explain`]).
pub fn is_locked(pod: &Pod) -> bool {
    pod.locked == Some(true)
}

/// The pods of `pods` the listing reports locked, in order.
pub fn locked<'a>(pods: impl IntoIterator<Item = &'a Pod>) -> Vec<&'a Pod> {
    pods.into_iter().filter(|p| is_locked(p)).collect()
}

/// The one sentence every refusal of a locked pod reads as: `devtest-x is locked — `arena
/// pods unlock devtest-x` first` (several: `a, b are locked — `arena pods unlock a b`
/// first`). The command is pasteable — names are what every selector accepts.
pub fn unlock_hint(names: &[&str]) -> String {
    match names {
        [one] => format!("{one} is locked — `arena pods unlock {one}` first"),
        many => format!("{} are locked — `arena pods unlock {}` first", many.join(", "), many.join(" ")),
    }
}

/// The refusal for running `verb` on `pods`, or `None` when none of them is locked. `extra`
/// is appended (e.g. terminate's "or pass --unlock …").
pub fn refusal(verb: &str, pods: &[&Pod], extra: &str) -> Option<String> {
    let names: Vec<&str> = locked(pods.iter().copied()).iter().map(|p| p.name.as_str()).collect();
    if names.is_empty() {
        return None;
    }
    Some(format!("refusing to {verb}: {}{extra}", unlock_hint(&names)))
}

/// A mutating call's error on pod `name`, as the operator should read it: the provider
/// refusing a locked pod says how to unlock it; anything else is the error itself.
pub fn explain(name: &str, e: &Error) -> String {
    if e.is_locked() {
        unlock_hint(&[name])
    } else {
        e.to_string()
    }
}

/// What `pods lock` (`lock = true`) or `pods unlock` would do with the selected pods.
#[derive(Debug, PartialEq)]
pub struct LockPlan<'a> {
    /// Pods whose lock would be set (or lifted).
    pub act: Vec<&'a Pod>,
    /// Pods already in the wanted state (reported, not touched).
    pub already: Vec<&'a Pod>,
    /// Pods whose backend can't lock, with why — the command fails for these.
    pub unsupported: Vec<(&'a Pod, String)>,
}

/// Split the selection: `support` says, per pod, whether its backend can (un)lock it
/// (`Err(why)` = it can't — see `Provider::lock_support`). A pod already in the wanted state
/// is left alone; one whose state is unknown on a backend that can lock is acted on.
pub fn plan<'a>(pods: &'a [Pod], lock: bool, support: impl Fn(&Pod) -> Result<(), String>) -> LockPlan<'a> {
    let mut out = LockPlan { act: Vec::new(), already: Vec::new(), unsupported: Vec::new() };
    for p in pods {
        match support(p) {
            Err(why) => out.unsupported.push((p, why)),
            Ok(()) if p.locked == Some(lock) => out.already.push(p),
            Ok(()) => out.act.push(p),
        }
    }
    out
}

/// What a lock or unlock does, for the confirm text and the dry run.
pub fn consequence(lock: bool) -> &'static str {
    if lock {
        "a locked pod refuses stop, restart and terminate — from any client, RunPod's console included — \
         until `arena pods unlock` (setup, backups, run, SSH: all unaffected)"
    } else {
        "an unlocked pod can be stopped, restarted and terminated again — from any client"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ProviderErrorKind;

    fn pod(name: &str, provider: &str, locked: Option<bool>) -> Pod {
        Pod { id: format!("id-{name}"), name: name.into(), provider: provider.into(), locked, ..Default::default() }
    }

    #[test]
    fn hint_names_the_pods_and_the_unlock_command() {
        assert_eq!(unlock_hint(&["devtest-x"]), "devtest-x is locked — `arena pods unlock devtest-x` first");
        assert_eq!(
            unlock_hint(&["devtest-a", "devtest-b"]),
            "devtest-a, devtest-b are locked — `arena pods unlock devtest-a devtest-b` first"
        );
    }

    /// Only `Some(true)` is locked: an unknown state is left to the API's own refusal.
    #[test]
    fn refusal_lists_only_the_locked_pods() {
        let (a, b, c) = (pod("devtest-a", "runpod", Some(true)), pod("devtest-b", "runpod", Some(false)), pod("devtest-c", "vast", None));
        assert_eq!(refusal("stop", &[&b, &c], ""), None);
        let r = refusal("stop", &[&a, &b, &c], " — or pass --unlock").unwrap();
        assert_eq!(r, "refusing to stop: devtest-a is locked — `arena pods unlock devtest-a` first — or pass --unlock");
        assert_eq!(locked([&a, &b, &c]).len(), 1);
        assert!(is_locked(&a) && !is_locked(&b) && !is_locked(&c));
    }

    /// The API's "Pod is locked" (tagged by the backend) reads as the hint; anything else —
    /// even an untagged error that merely says so — reads as itself.
    #[test]
    fn explain_maps_only_the_tagged_locked_error() {
        let locked = Error::Provider { kind: ProviderErrorKind::Locked, message: "stop pod HTTP 400: Pod is locked".into() };
        assert_eq!(explain("devtest-x", &locked), "devtest-x is locked — `arena pods unlock devtest-x` first");
        let other = Error::provider("stop pod HTTP 400: Pod is locked");
        assert_eq!(explain("devtest-x", &other), "provider error: stop pod HTTP 400: Pod is locked");
        let cap = Error::capacity("no instances");
        assert_eq!(explain("devtest-x", &cap), "provider error: no instances");
    }

    /// (case, pods, lock?, act, already, unsupported)
    #[test]
    fn plan_table() {
        let support = |p: &Pod| if p.provider == "runpod" { Ok(()) } else { Err(format!("not supported on {}", p.provider)) };
        let pods = vec![
            pod("devtest-a", "runpod", Some(false)),
            pod("devtest-b", "runpod", Some(true)),
            pod("devtest-c", "runpod", None),
            pod("devtest-d", "vast", None),
            pod("devtest-e", "hetzner", Some(true)),
        ];
        let names = |v: &[&Pod]| v.iter().map(|p| p.name.clone()).collect::<Vec<_>>();
        let cases: [(bool, &[&str], &[&str]); 2] = [
            (true, &["devtest-a", "devtest-c"], &["devtest-b"]),
            (false, &["devtest-b", "devtest-c"], &["devtest-a"]),
        ];
        for (lock, act, already) in cases {
            let p = plan(&pods, lock, support);
            assert_eq!(names(&p.act), act, "lock={lock}");
            assert_eq!(names(&p.already), already, "lock={lock}");
            let unsupported: Vec<(String, String)> = p.unsupported.iter().map(|(p, w)| (p.name.clone(), w.clone())).collect();
            assert_eq!(
                unsupported,
                [("devtest-d".to_string(), "not supported on vast".to_string()), ("devtest-e".into(), "not supported on hetzner".into())],
                "lock={lock}: a backend that can't lock is reported whatever its pod claims"
            );
        }
        assert!(consequence(true).contains("refuses stop, restart and terminate"));
        assert!(consequence(false).contains("can be stopped"));
    }
}
