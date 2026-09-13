//! Machine-name allocation, mirroring the legacy logic: fully-qualified names are
//! `{prefix}-{name}` drawn in order from a fixed candidate list, skipping any that
//! already exist. Having spare names that fail to resolve is expected.
//!
//! ## Absolute (no-prefix) entries
//!
//! A `MACHINE_NAME_LIST` entry may start with [`ABSOLUTE_MARKER`] (`@`) to opt out of the
//! fleet prefix: `@james-gpu` resolves to the bare pod name `james-gpu`, not
//! `arena8-james-gpu`. Such an entry still occupies a slot in the list, so it keeps the
//! same index-anchored proxy port as any other — it just doesn't wear the prefix. This
//! lets a few personal/dev boxes share the list (and the proxy) without joining the
//! prefixed cohort. [`qualify`] is the one place the rule lives; route every
//! list-entry → pod-name composition through it.

use std::collections::HashSet;

use crate::pod::Pod;

/// Leading marker on a `MACHINE_NAME_LIST` entry that means "use this name verbatim, with
/// no `{prefix}-` glued on" (e.g. `@james-gpu` → `james-gpu`).
pub const ABSOLUTE_MARKER: char = '@';

/// Whether a raw list entry is an absolute (no-prefix) name.
pub fn is_absolute(entry: &str) -> bool {
    entry.starts_with(ABSOLUTE_MARKER)
}

/// The fully-qualified pod name for a raw list entry: `{prefix}-{entry}`, unless the entry
/// is marked absolute (`@name` → `name`).
pub fn qualify(prefix: &str, entry: &str) -> String {
    match entry.strip_prefix(ABSOLUTE_MARKER) {
        Some(bare) => bare.to_string(),
        None => format!("{prefix}-{entry}"),
    }
}

/// Canonicalize a *user-typed* machine token to a full pod name, honoring absolute list
/// entries. If the token (with or without a leading `@`) matches an absolute entry in
/// `candidates`, it resolves to the bare name; otherwise it's `{prefix}-{token}` unless it
/// already carries the prefix. So `arena … james-gpu` targets the right pod even though the
/// list stores it as `@james-gpu`.
pub fn canonical_name(prefix: &str, candidates: &[String], token: &str) -> String {
    let t = token.trim();
    let bare = t.strip_prefix(ABSOLUTE_MARKER).unwrap_or(t);
    if candidates.iter().any(|c| is_absolute(c) && c[ABSOLUTE_MARKER.len_utf8()..] == *bare) {
        return bare.to_string();
    }
    let pre = format!("{prefix}-");
    if t.starts_with(&pre) { t.to_string() } else { format!("{pre}{t}") }
}

/// Return up to `count` fully-qualified names not already taken by `existing`.
pub fn next_free_names(
    prefix: &str,
    candidates: &[String],
    existing: &[Pod],
    count: usize,
) -> Vec<String> {
    let taken: HashSet<&str> = existing.iter().map(|p| p.name.as_str()).collect();
    candidates
        .iter()
        .map(|n| qualify(prefix, n))
        .filter(|fq| !taken.contains(fq.as_str()))
        .take(count)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(name: &str) -> Pod {
        Pod {
            id: "x".into(),
            name: name.into(),
            provider: "runpod".into(),
            status: "RUNNING".into(),
            gpu_type: None,
            cost_per_hr: None,
            ssh_ip: None,
            ssh_port: None,
        }
    }

    #[test]
    fn skips_existing_and_respects_count() {
        let candidates = vec!["apple".to_string(), "autumn".into(), "bloom".into()];
        let existing = vec![pod("arena8-apple")];
        let got = next_free_names("arena8", &candidates, &existing, 2);
        assert_eq!(got, vec!["arena8-autumn", "arena8-bloom"]);
    }

    #[test]
    fn qualify_honors_absolute_marker() {
        assert_eq!(qualify("arena8", "apple"), "arena8-apple");
        assert_eq!(qualify("arena8", "@james-gpu"), "james-gpu");
        assert!(!is_absolute("apple"));
        assert!(is_absolute("@james-gpu"));
    }

    #[test]
    fn next_free_names_emits_bare_absolute_names() {
        let candidates = vec!["apple".to_string(), "@james-gpu".into(), "bloom".into()];
        // apple taken; the absolute entry must come out bare (no `arena8-`), not skipped.
        let existing = vec![pod("arena8-apple")];
        let got = next_free_names("arena8", &candidates, &existing, 2);
        assert_eq!(got, vec!["james-gpu", "arena8-bloom"]);
    }

    #[test]
    fn canonical_name_resolves_user_tokens() {
        let candidates = vec!["apple".to_string(), "@james-gpu".into()];
        // bare pony name -> prefixed; already-prefixed left alone
        assert_eq!(canonical_name("arena8", &candidates, "apple"), "arena8-apple");
        assert_eq!(canonical_name("arena8", &candidates, "arena8-apple"), "arena8-apple");
        // absolute: typed with or without '@', and never gets the prefix
        assert_eq!(canonical_name("arena8", &candidates, "james-gpu"), "james-gpu");
        assert_eq!(canonical_name("arena8", &candidates, "@james-gpu"), "james-gpu");
        // a name not in the list falls back to the prefixed form
        assert_eq!(canonical_name("arena8", &candidates, "ghost"), "arena8-ghost");
    }
}
