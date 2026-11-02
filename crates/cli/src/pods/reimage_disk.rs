//! `pods reimage --disk GB / --volume GB`: a reimage that resizes the pod in the same request
//! (project week, 2026-11-02: "remake trouble with 500G of disk, i think it should be possible to
//! use api and not make a new pod?"). The pod keeps its id and host, and as with any reimage its
//! container disk is wiped: the `--wipe-ok` gate and the busy check stay as they are.
//!
//! Only RunPod can do it: its reimage `PATCH` takes the sizes as well (the capability table's
//! `pods reimage --disk` / `--volume`). Anywhere else it's refused before anything is asked,
//! with the command that does work. Each pod's sizes are read before the prompt, so a volume
//! that would shrink (RunPod only grows one) and a pod without a volume of its own are refused
//! up front too. A host without the room refuses the request and leaves the pod as it was; that
//! is said in one line pointing at `pods replace NAME --disk GB`, which makes a new pod.

use anyhow::Result;
use arena_core::capability::Op;
use arena_core::provider::{Provider, Resize};
use arena_core::{out, Pod};

/// The resize refused up front where a pod's backend can't do it (the capability table, all
/// pods or none): one refusal per flag given, with what to do instead. `<GB>` reads as the size
/// asked for, and `<name>` as the pod's when there's one. `None`: every pod can. Pure.
pub(crate) fn refusal(provider: &dyn Provider, resize: Resize, pods: &[&Pod]) -> Option<String> {
    let mut lines = Vec::new();
    for (gb, op, verb) in [(resize.disk_gb, Op::ReimageDisk, "resize the disk of"), (resize.volume_gb, Op::ReimageVolume, "grow the volume of")] {
        let Some(gb) = gb else { continue };
        if let Some(r) = crate::pods::capable::refusal(provider, op, verb, pods) {
            let r = r.replace("<GB>", &gb.to_string());
            lines.push(match pods {
                [one] => r.replace("<name>", &one.name),
                _ => r,
            });
        }
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// One pod's sizes, GB, now and after: what `-v`'s plan says.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Sizes {
    /// The container disk `(now, after)`, when it's resized.
    pub disk: Option<(u32, u32)>,
    /// The volume `(now, after)`, when it's grown.
    pub volume: Option<(u32, u32)>,
}

/// Each pod's sizes now (its spec, read once each) against `resize`, in `pods`' order; nothing
/// is read when nothing is resized. Refused before anything is asked: a pod whose sizes can't
/// be read, and a volume that can't be grown ([`volume_refusal`]).
pub(crate) async fn sizes(provider: &dyn Provider, resize: Resize, pods: &[Pod]) -> Result<Vec<Sizes>> {
    if !resize.any() {
        return Ok(vec![Sizes::default(); pods.len()]);
    }
    let mut out = Vec::with_capacity(pods.len());
    let mut refused = Vec::new();
    for p in pods {
        let spec = match provider.pod_spec(&p.id).await {
            Ok(spec) => spec,
            Err(e) => {
                out!(ErrDetail, "reading {}'s spec: {e}", p.name);
                anyhow::bail!("couldn't read {}'s disk sizes ({}): nothing was reimaged", p.name, out::reason(&e));
            }
        };
        if let Some(r) = volume_refusal(&p.name, spec.volume_gb, resize) {
            refused.push(r);
        }
        out.push(Sizes { disk: resize.disk_gb.map(|gb| (spec.disk_gb, gb)), volume: resize.volume_gb.map(|gb| (spec.volume_gb, gb)) });
    }
    if !refused.is_empty() {
        anyhow::bail!("{}", refused.join("\n"));
    }
    Ok(out)
}

/// Why `name`'s volume, `now_gb` GB, can't become `resize`'s: RunPod grows a volume but never
/// shrinks one, and adds none to a pod made without (v2 answers 400). `None`: it can, or no
/// volume is asked for. A smaller container disk is fine: it's wiped anyway. Pure.
pub(crate) fn volume_refusal(name: &str, now_gb: u32, resize: Resize) -> Option<String> {
    let want = resize.volume_gb?;
    let instead = format!("`arena pods replace {name} --volume {want}` (a new pod, files copied)");
    if now_gb == 0 {
        return Some(format!("refusing to grow {name}'s volume: it has none of its own, and RunPod adds none in place — {instead}"));
    }
    (want < now_gb).then(|| {
        format!("refusing to shrink {name}'s volume from {now_gb} GB to {want} GB: RunPod only grows one — pass --volume {now_gb} or more, or {instead}")
    })
}

/// What the prompt and the dry run add after the names: ` with a 500 GB disk`, ` with 500 GB
/// disks and 100 GB volumes`; empty when nothing is resized. Pure.
pub(crate) fn words(resize: Resize, pods: usize) -> String {
    let part = |gb: u32, what: &str| if pods == 1 { format!("{} {gb} GB {what}", article(gb)) } else { format!("{gb} GB {what}s") };
    let parts: Vec<String> = [resize.disk_gb.map(|gb| part(gb, "disk")), resize.volume_gb.map(|gb| part(gb, "volume"))].into_iter().flatten().collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!(" with {}", parts.join(" and "))
    }
}

/// `a` or `an` before `n` as it's read: an 8, 11, 18, 80–89, 800–899, 8000, 11000 … GB disk. Pure.
fn article(n: u32) -> &'static str {
    let s = n.to_string();
    let eleven_or_eighteen = s.len() % 3 == 2 && (s.starts_with("11") || s.starts_with("18"));
    if s.starts_with('8') || eleven_or_eighteen {
        "an"
    } else {
        "a"
    }
}

/// `-v`'s words for one pod's sizes, after its plan: `; disk 60 → 500 GB, volume 20 → 50 GB`;
/// empty when nothing is resized. Pure.
pub(crate) fn plan_words(s: Sizes) -> String {
    let parts: Vec<String> = [("disk", s.disk), ("volume", s.volume)]
        .into_iter()
        .filter_map(|(what, change)| change.map(|(now, after)| format!("{what} {now} → {after} GB")))
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("; {}", parts.join(", "))
    }
}

/// The flags that resize, as a retry repeats them: ` --disk 500 --volume 80`. Pure.
pub(crate) fn flags(resize: Resize) -> String {
    [("--disk", resize.disk_gb), ("--volume", resize.volume_gb)].into_iter().filter_map(|(f, gb)| gb.map(|gb| format!(" {f} {gb}"))).collect()
}

/// The one line for a resize the host hadn't the room for — the pod is left as it was — with
/// the command that makes a new pod instead (`pods replace`, which takes the same sizes); `None`
/// for any other failure. Pure (but for the styles).
pub(crate) fn no_room(name: &str, resize: Resize, e: &arena_core::Error) -> Option<String> {
    if !resize.any() || !host_lacks_room(&e.to_string()) {
        return None;
    }
    let what = words(resize, 1);
    Some(format!(
        "[{}] {}: its host hasn't the room for it (instead: arena pods replace {name}{})",
        out::bold(name),
        out::red(format!("not reimaged{what}")),
        flags(resize)
    ))
}

/// Whether a refused resize's error says the host hasn't the disk for it: RunPod's `400 This
/// machine does not have enough disk available, please try to allocate less container or volume
/// disk.` (live 2026-11-02, a 9000 GB disk asked of a pod in EUR-IS-1), or words like it. Pure.
pub(crate) fn host_lacks_room(message: &str) -> bool {
    let m = message.to_lowercase();
    ["enough disk", "not enough", "insufficient", "no space", "disk space", "out of space"].iter().any(|w| m.contains(w))
}
