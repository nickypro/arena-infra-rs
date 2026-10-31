//! Stamp the binary with its version, commit and platform, exposed to `main.rs` via
//! `env!(…)`:
//! - `ARENA_VERSION`: `2.9.<YYMMDD>.<HHMM>`. The `2.9` is the base major.minor ([`BASE`]); the
//!   date/time identifies the build, so it's obvious which one is installed. A release build sets
//!   `ARENA_BUILD_STAMP` (`YYMMDD.HHMM`, the commit's time in UTC) so one commit is always
//!   one version; any other build takes the time it was built.
//! - `ARENA_COMMIT`: `ARENA_GIT_SHA` if set, else `git rev-parse --short=10 HEAD`, with
//!   `-dirty` for uncommitted changes; `unknown` outside a checkout.
//! - `ARENA_TARGET`: the target triple, naming the release asset `arena update` downloads.
//!
//! `arena --version` prints `arena <version> (<commit>)`; `arena update` compares versions.
//!
//! We emit no `cargo:rerun-if-*` lines on purpose: with none present, Cargo re-runs this
//! script whenever any file in the crate changes — so a rebuild after an edit restamps a
//! fresh timestamp, while an unchanged tree keeps the version from its last build. (Even a
//! `rerun-if-env-changed` line would switch that off.)

use std::process::Command;

/// The major.minor (release.yml reads it from this line; the policy for bumping it is in
/// CHANGELOG.md's header).
const BASE: &str = "2.9";

/// `cmd`'s trimmed stdout, if it ran and succeeded with some.
fn output(cmd: &mut Command) -> Option<String> {
    cmd.output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `YYMMDD.HHMM`, all digits.
fn is_stamp(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 11 && b[6] == b'.' && b.iter().enumerate().all(|(i, c)| i == 6 || c.is_ascii_digit())
}

fn main() {
    // `date +%y%m%d.%H%M` → e.g. `260625.1438`. Shelling out avoids a chrono build-dep.
    let stamp = std::env::var("ARENA_BUILD_STAMP")
        .ok()
        .filter(|s| is_stamp(s))
        .or_else(|| output(Command::new("date").args(["+%y%m%d.%H%M"])))
        .unwrap_or_else(|| "000000.0000".to_string());
    println!("cargo:rustc-env=ARENA_VERSION={BASE}.{stamp}");

    let commit = std::env::var("ARENA_GIT_SHA").ok().filter(|s| !s.trim().is_empty()).or_else(|| {
        let sha = output(Command::new("git").args(["rev-parse", "--short=10", "HEAD"]))?;
        let dirty = output(Command::new("git").args(["status", "--porcelain", "--untracked-files=no"])).is_some();
        Some(if dirty { format!("{sha}-dirty") } else { sha })
    });
    println!("cargo:rustc-env=ARENA_COMMIT={}", commit.as_deref().unwrap_or("unknown"));

    println!("cargo:rustc-env=ARENA_TARGET={}", std::env::var("TARGET").unwrap_or_default());
}
