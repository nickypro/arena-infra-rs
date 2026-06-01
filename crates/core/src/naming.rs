//! Machine-name allocation, mirroring the legacy logic: fully-qualified names are
//! `{prefix}-{name}` drawn in order from a fixed candidate list, skipping any that
//! already exist. Having spare names that fail to resolve is expected.

use std::collections::HashSet;

use crate::pod::Pod;

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
        .map(|n| format!("{prefix}-{n}"))
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
}
