//! Stamp the binary with a build-time version string of the form `2.0.<YYMMDD>.<HHMM>`
//! (date+time of the build), exposed to `main.rs` via `env!("ARENA_VERSION")` and shown by
//! `arena --version`. The `2.0` is the base major.minor; the date/time identifies the exact
//! build so it's obvious which one is installed.
//!
//! We emit no `cargo:rerun-if-changed` lines on purpose: with none present, Cargo re-runs
//! this script whenever any file in the crate changes — so a rebuild after an edit (e.g.
//! `cargo install --path crates/cli --force`) restamps a fresh timestamp, while an unchanged
//! tree keeps the version from its last build.

use std::process::Command;

const BASE: &str = "2.0";

fn main() {
    // `date +%y%m%d.%H%M` → e.g. `260625.1438`. Shelling out avoids a chrono build-dep.
    let stamp = Command::new("date")
        .args(["+%y%m%d.%H%M"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "000000.0000".to_string());
    println!("cargo:rustc-env=ARENA_VERSION={BASE}.{stamp}");
}
