//! Where a `replace` / `migrate copy`'s new pod comes from when it isn't created now: `--onto
//! <pod>` (a machine arena already manages) or `--onto-spare` (a capacity spare, Hetzner:
//! `pods spare up`) — and the "is anyone on it right now?" probe the moves ask first (`replace`,
//! `migrate cutover`, `copy-home`, and `--onto` of someone's machine).
//!
//! Prod 2026-10-30: the operator moved cx33 boxes to cx43 with a script of their own, onto
//! spares made ahead of time, because `migrate` "only copies a pod onto a fresh copy of itself".
//! Taking a machine is a rename: it becomes `<name>-new` (a spare also loses its spare mark, in
//! the same request), is set up like a created one, and from there the migration goes as ever
//! — the cutover gives it the name, a revert takes it back.

use anyhow::{Context, Result};
use arena_core::idle::{self, Presence, Probe};
use arena_core::provider::Provider;
use arena_core::remote::Remote;
use arena_core::selector::Naming;
use arena_core::spare::SpareCloud;
use arena_core::ssh::SshTarget;
use arena_core::{Config, Pod};

use crate::{money, reap, spend};

/// Where the new pod comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Source {
    /// Created now (the default).
    Create,
    /// `--onto <pod>`: that machine (a name, short name or id; a spare's too).
    Onto(String),
    /// `--onto-spare`: the first ready capacity spare.
    Spare,
}

impl Source {
    /// The flag as the operator typed it, for messages.
    pub(crate) fn flag(&self) -> String {
        match self {
            Source::Create => String::new(),
            Source::Onto(p) => format!("--onto {p}"),
            Source::Spare => "--onto-spare".into(),
        }
    }
}

/// A machine to take as `<name>-new`.
#[derive(Debug, Clone)]
pub(crate) struct Onto {
    pub pod: Pod,
    /// A capacity spare: nobody's, image + nothing yet — its copy starts a fresh baseline, and
    /// it's taken by [`SpareCloud::adopt`] (its mark goes with the rename).
    pub spare: bool,
}

/// The capacity spares of `owner`'s backend, when it has any (Hetzner): the real API.
pub(crate) fn spares_for(owner: &dyn Provider, cfg: &Config) -> Option<Box<dyn SpareCloud>> {
    if owner.name() != "hetzner" {
        return None;
    }
    crate::image::hetzner(cfg).ok().map(|h| Box::new(h) as Box<dyn SpareCloud>)
}

/// Read who's on `pod` now: the `pods idle` probe over SSH, within [`money::IDLE_TIMEOUT`].
/// Read-only. A stopped pod isn't probed (its listed endpoint is stale).
pub(crate) async fn probe(remote: &dyn Remote, cfg: &Config, pod: &Pod) -> Probe {
    if arena_core::status::is_stopped(&pod.status) {
        return Probe::NotBilling;
    }
    let Ok(target) = SshTarget::from_pod(pod, cfg) else { return Probe::NoEndpoint };
    let cmd = idle::probe_command(&arena_core::backup::repo_path(cfg));
    let call = remote.exec(&target, &cmd, Some(money::IDLE_TIMEOUT)).await.map_err(|e| arena_core::remote::describe_error(&e));
    money::read_probe(call)
}

/// `someone is on it (1 SSH session, a VS Code server running)` / `couldn't tell whether anyone
/// is on it (…)`; `None` when no one is. Pure.
pub(crate) fn presence_words(p: &Presence) -> Option<String> {
    match p {
        Presence::Away => None,
        Presence::Here(_) => Some(format!("someone is on it ({})", p.reasons())),
        Presence::Unknown(_) => Some(format!("couldn't tell whether anyone is on it ({})", p.reasons())),
    }
}

/// What's on a machine, for the refusal that names it someone's: who's on it now, how fresh
/// its newest file is, how long it's been up. Pure.
pub(crate) fn whats_on(probe: &Probe) -> String {
    let Probe::Read(r) = probe else {
        return presence_words(&idle::presence(probe)).unwrap_or_default();
    };
    let mut out = vec![presence_words(&idle::presence(probe)).unwrap_or_else(|| "no one on it right now".into())];
    match r.newest_file {
        Some(t) => out.push(format!("its newest file changed {} ago", arena_core::snapshot::fmt_age(r.now.saturating_sub(t)))),
        None if r.files_complete => out.push("no file in its home".into()),
        None => {}
    }
    if let Some(up) = r.uptime_secs {
        out.push(format!("up {}", arena_core::snapshot::fmt_age(up)));
    }
    out.join("; ")
}

/// Whether `name` is a machine of `MACHINE_NAME_LIST` — a participant's (or, an `@` entry, a
/// staff box): someone's, whatever is on it at this instant.
pub(crate) fn listed_machine(cfg: &Config, name: &str) -> bool {
    let naming = Naming::from_config(cfg);
    naming.reapable(name) || naming.is_staff(name)
}

/// The machine `source` names for `canonical`'s migration, on `canonical`'s own backend: a
/// fleet machine from `pods` (`owner`'s listing) by name, short name or id, else one of
/// `spares` by name or id; `--onto-spare` the first ready spare ([`arena_core::spare::pick`],
/// of the first of `types` that has one). Read-only. The refusals that need no probe are
/// [`onto_refusal`]'s.
pub(crate) async fn find(
    owner: &dyn Provider,
    spares: Option<&dyn SpareCloud>,
    cfg: &Config,
    canonical: &str,
    pods: &[Pod],
    source: &Source,
    types: &[String],
) -> Result<Onto> {
    let prefix = cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena");
    let provider = owner.name();
    match source {
        Source::Create => anyhow::bail!("nothing to take: no --onto / --onto-spare"),
        Source::Spare => {
            let Some(spares) = spares else {
                anyhow::bail!(
                    "--onto-spare: capacity spares are Hetzner's (`arena pods spare up`), and {canonical} is on {provider} — \
                     drop the flag to create the new pod, or pass --onto <pod> for a machine on {provider}"
                );
            };
            let all = spares.spares().await.context("listing the capacity spares")?;
            arena_core::spare::pick(&all, prefix, types).cloned().map(|pod| Onto { pod, spare: true }).ok_or_else(|| {
                let of = if types.is_empty() { String::new() } else { format!(" of type {}", types.join(" or ")) };
                anyhow::anyhow!(
                    "--onto-spare: no ready spare{of} ({} spare{} in all: `arena pods spare list`) — make one with \
                     `arena pods spare up 1{} --keep-trying`, or drop the flag to create the new pod",
                    all.len(),
                    if all.len() == 1 { "" } else { "s" },
                    types.first().map(|t| format!(" --server-type {t}")).unwrap_or_default()
                )
            })
        }
        Source::Onto(token) => {
            if !types.is_empty() {
                anyhow::bail!(
                    "--server-type shapes a new pod (or picks a spare with --onto-spare); --onto {token} takes that machine as \
                     it is — drop one of them"
                );
            }
            let naming = Naming::from_config(cfg);
            let missed = match arena_core::selector::resolve_one(&naming, pods, token) {
                Ok(i) => return Ok(Onto { pod: pods[i].clone(), spare: false }),
                Err(e) => e,
            };
            if let Some(spares) = spares {
                let full = arena_core::naming::qualify(prefix, token);
                let all = spares.spares().await.context("listing the capacity spares")?;
                if let Some(p) = all.into_iter().find(|p| p.id == token.trim() || p.name == token.trim() || p.name == full) {
                    return Ok(Onto { pod: p, spare: true });
                }
            }
            anyhow::bail!(
                "--onto {token}: {missed} — only a machine on {provider} (where {canonical} is) can be taken: a migration \
                 renames it, and a name can't move between providers"
            )
        }
    }
}

/// Why `onto` can't be taken for `src`'s migration, needing no probe (pure): it's `src`
/// itself, a migration's own `-new`/`-old`, or stopped (its endpoint is stale; a setup there
/// would fail). `None`: it may be — a listed machine is [`someones_refusal`]'s.
pub(crate) fn onto_refusal(src: &Pod, onto: &Onto) -> Option<String> {
    let p = &onto.pod;
    if p.id == src.id {
        return Some(format!("--onto {}: that's {} itself — name another machine", p.name, src.name));
    }
    if p.name.ends_with("-new") || p.name.ends_with("-old") {
        return Some(format!(
            "--onto {}: that's a migration's own pod (`-new`/`-old`) — finish or revert that migration first, or terminate it",
            p.name
        ));
    }
    if arena_core::status::is_stopped(&p.status) {
        return Some(format!("--onto {}: it is stopped ({}) — `arena pods start {}` first", p.name, p.status, p.name));
    }
    None
}

/// The refusal to take a machine of `MACHINE_NAME_LIST` without `--force` — someone's — saying
/// what's on it (`probe`) and what taking it does. `None` when it isn't a listed machine. Pure.
pub(crate) fn someones_refusal(cfg: &Config, canonical: &str, onto: &Onto, probe: &Probe) -> Option<String> {
    let name = &onto.pod.name;
    if onto.spare || !listed_machine(cfg, name) {
        return None;
    }
    Some(format!(
        "--onto {name}: it's a cohort machine (in MACHINE_NAME_LIST) — someone's: {}. Taking it renames it {canonical}-new \
         (its proxy port and name go to nobody until it's renamed back) and syncs {canonical}'s home over its own (what \
         that replaces is moved aside to ~/{}/ there, never deleted). Pass --force to take it anyway, or use a spare \
         (`arena pods spare up`) — to move a person onto another machine under its own name, `arena pods copy-home \
         {canonical} {name}`.",
        whats_on(probe),
        arena_core::replica::KEPT_DIR
    ))
}

/// The plan line for taking `onto` as `new_name`.
pub(crate) fn take_line(onto: &Onto, new_name: &str) -> String {
    let p = &onto.pod;
    let what = [p.gpu_type.clone(), p.location.clone()].into_iter().flatten().collect::<Vec<_>>().join(", ");
    let what = if what.is_empty() { String::new() } else { format!(", {what}") };
    if onto.spare {
        format!("take spare {} (id={}{what}) as {new_name}: renamed, its spare mark dropped — one request", p.name, p.id)
    } else {
        format!("take {} (id={}{what}) as {new_name}: renamed", p.name, p.id)
    }
}

/// The spare-claim lock ([`arena_core::spare::claim_lock`]) in this fleet's state dir, held
/// until dropped — in this crate's tests without `ARENA_STATE_DIR`, a temp dir of the process's
/// (never the real state dir).
pub(crate) fn claim_spares(cfg: &Config) -> Result<std::fs::File> {
    let dir = if cfg!(test) && cfg.get("ARENA_STATE_DIR").is_none() {
        std::env::temp_dir().join(format!("arena-spare-claims-{}", std::process::id()))
    } else {
        arena_core::snapshot::state_dir_for(cfg).map_err(anyhow::Error::msg)?
    };
    arena_core::spare::claim_lock(&dir, arena_core::spare::LOCK_WAIT).map_err(|e| anyhow::anyhow!("not taking or deleting a spare: {e}"))
}

/// The line once `onto` is taken as `new_name`: and for a machine that had a name of its own
/// (not a spare), how to give it back if the move stops before the swap. Pure.
pub(crate) fn taken_line(onto: &Onto, new_name: &str) -> String {
    let p = &onto.pod;
    if onto.spare {
        format!("{} -> {new_name} (id={})", p.name, p.id)
    } else {
        format!(
            "{} -> {new_name} (id={}); if this stops before the swap, `arena pods rename {new_name} {}` gives it its name back",
            p.name, p.id, p.name
        )
    }
}

/// Take `onto` as `new_name`: a spare by [`SpareCloud::adopt`] (renamed + unmarked in one
/// request), anything else by a rename. The reaper's record and the spend ledger follow
/// (the row goes on under the canonical machine's owner: [`arena_core::spend::Event::Adopted`]).
/// Nothing else is touched: setup and the copy come after, as for a created pod.
pub(crate) async fn take(owner: &dyn Provider, spares: Option<&dyn SpareCloud>, cfg: &Config, onto: &Onto, new_name: &str) -> Result<()> {
    let policy = arena_core::retry::RetryPolicy::default();
    let id = onto.pod.id.as_str();
    let done = if onto.spare {
        let spares = spares.context("a spare can only be taken through its provider's spare API (Hetzner)")?;
        // Under the claim lock: no other run takes or deletes it between the look and the rename.
        let _claim = claim_spares(cfg)?;
        // A retry may find its own earlier PUT done (its answer lost); only a retry may.
        let attempts = std::sync::atomic::AtomicU32::new(0);
        arena_core::retry::retrying(&policy, || {
            let retry = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0;
            spares.adopt(id, &onto.pod.name, new_name, retry)
        })
        .await
    } else {
        arena_core::retry::retrying(&policy, || owner.rename_pod(id, new_name)).await
    };
    done.with_context(|| format!("taking {} as {new_name} — nothing was renamed", onto.pod.name))?;
    reap::renamed(cfg, &onto.pod.provider, id, new_name);
    let taken = Pod { name: new_name.to_string(), ..onto.pod.clone() };
    spend::SpendCtx::from_config(cfg).event(&arena_core::spend::Event::Adopted { pod: &taken, owner: &arena_core::spend::OwnerRule::Inherit });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena_core::idle::fixture::Reply;

    fn pod(id: &str, name: &str, status: &str) -> Pod {
        Pod { id: id.into(), name: name.into(), provider: "hetzner".into(), status: status.into(), ..Default::default() }
    }

    #[test]
    fn a_machine_that_cant_be_taken_is_refused_before_any_probe() {
        let src = pod("1", "devtest-alpha", "RUNNING");
        let take = |p: Pod| onto_refusal(&src, &Onto { pod: p, spare: false });
        assert!(take(pod("1", "devtest-alpha", "RUNNING")).unwrap().contains("itself"));
        assert!(take(pod("2", "devtest-bravo-new", "RUNNING")).unwrap().contains("a migration's own pod"));
        assert!(take(pod("3", "devtest-bravo-old", "RUNNING")).unwrap().contains("a migration's own pod"));
        assert!(take(pod("4", "devtest-bravo", "OFF")).unwrap().contains("arena pods start devtest-bravo"));
        assert_eq!(take(pod("5", "devtest-bravo", "RUNNING")), None);
    }

    /// A cohort machine (or a staff box) is someone's: refused without --force, saying what's on
    /// it now and what taking it does; a spare or a machine off the list isn't.
    #[test]
    fn a_cohort_machine_is_someones_and_the_refusal_says_whats_on_it() {
        let cfg = Config::parse("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(alpha bravo @solo-gpu)\n");
        let busy = Probe::Read(idle::parse_reply(&Reply { sessions: vec![[10, 0, 0, 7]], newest_age: Some(7200), ..Reply::default() }.render()).unwrap());
        let r = someones_refusal(&cfg, "devtest-alpha", &Onto { pod: pod("2", "devtest-bravo", "RUNNING"), spare: false }, &busy).unwrap();
        assert!(r.contains("someone's: someone is on it (1 SSH session); its newest file changed 2h ago; up 3d"), "{r}");
        assert!(r.contains("renames it devtest-alpha-new") && r.contains("--force") && r.contains("copy-home devtest-alpha devtest-bravo"), "{r}");
        assert!(someones_refusal(&cfg, "devtest-alpha", &Onto { pod: pod("3", "solo-gpu", "RUNNING"), spare: false }, &busy).is_some());
        for (name, spare) in [("devtest-spare-1", true), ("devtest-extra", false), ("devtest-bravo", true)] {
            assert_eq!(someones_refusal(&cfg, "devtest-alpha", &Onto { pod: pod("4", name, "RUNNING"), spare }, &busy), None, "{name}");
        }
        assert!(whats_on(&Probe::Failed("exit 255: refused".into())).contains("couldn't tell"));
        // Taken: a machine with a name of its own is told how to get it back; a spare isn't.
        let bravo = Onto { pod: pod("2", "devtest-bravo", "RUNNING"), spare: false };
        assert!(taken_line(&bravo, "devtest-alpha-new").ends_with("`arena pods rename devtest-alpha-new devtest-bravo` gives it its name back"));
        assert_eq!(taken_line(&Onto { pod: pod("9", "devtest-spare-1", "RUNNING"), spare: true }, "devtest-alpha-new"), "devtest-spare-1 -> devtest-alpha-new (id=9)");
        let quiet = Probe::Read(idle::parse_reply(&Reply::default().render()).unwrap());
        assert!(whats_on(&quiet).starts_with("no one on it right now; its newest file changed 9h ago"), "{}", whats_on(&quiet));
    }
}
