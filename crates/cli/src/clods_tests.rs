//! `arena clods install`'s lines, `config check`'s `CLODS_DIR` row and setup's plan line.

use super::*;

struct TmpDir(PathBuf);

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tmp(tag: &str) -> TmpDir {
    let d = std::env::temp_dir().join(format!("arena-cli-clods-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    TmpDir(d)
}

#[cfg(unix)]
fn tokens(dir: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let t = dir.join("tokens");
    std::fs::write(&t, "a@example.com -> sk-ant-oat01-FAKE-a\n4 b@example.com -> sk-ant-oat01-FAKE-b\n").unwrap();
    std::fs::set_permissions(&t, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// What `run` printed, as a log gets it.
fn said(run: impl FnOnce()) -> String {
    let cap = arena_core::out::capture(false);
    run();
    cap.text()
}

#[cfg(unix)]
#[test]
fn install_says_what_it_wrote_and_what_is_next() {
    let d = tmp("install");
    let dir = d.0.join(".clods");
    let p = dir.display();
    assert_eq!(
        said(|| handle_install(&dir, true).unwrap()),
        format!("Would write into {p}: setup.sh, push.sh, clods.sh, README.md, fleet.example, .gitignore new (tokens, fleet and profiles/ untouched)\n")
    );
    assert_eq!(
        said(|| handle_install(&dir, false).unwrap()),
        format!(
            "✓ {p}: setup.sh, push.sh, clods.sh, README.md, fleet.example, .gitignore new (tokens, fleet and profiles/ untouched)\n\
             Next: your accounts in {p}/tokens (one `email -> sk-ant-oat01-…` per line, chmod 600), then {p}/setup.sh\n"
        )
    );
    assert_eq!(said(|| handle_install(&dir, false).unwrap()), format!("✓ {p} is up to date\n"));
    tokens(&dir, 0o600);
    std::fs::write(dir.join("clods.sh"), "old\n").unwrap();
    assert_eq!(said(|| handle_install(&dir, false).unwrap()), format!("✓ {p}: clods.sh updated (tokens, fleet and profiles/ untouched)\n"));
}

#[cfg(unix)]
#[test]
fn the_config_check_row() {
    let d = tmp("row");
    let row = |text: &str| {
        let mut missing = Vec::new();
        let line = config_check_row(&Config::parse(text), &mut missing);
        (line, missing)
    };
    let dir = d.0.display();
    assert_eq!(row("").0, "  · CLODS_DIR                    (missing — optional: setup puts every clods account on each pod)");
    assert_eq!(row(&format!("CLODS_DIR={dir}\n")).0, format!("  ⚠ CLODS_DIR                    {dir}: no tokens file there — setup pushes nothing"));
    tokens(&d.0, 0o600);
    assert_eq!(
        row(&format!("CLODS_DIR={dir}\n")),
        (format!("  ✓ CLODS_DIR                    {dir}: 2 accounts (every pod gets them all; tokens never shown)"), vec![])
    );
    tokens(&d.0, 0o640);
    let (line, missing) = row(&format!("CLODS_DIR={dir}\n"));
    assert!(line.starts_with(&format!("  ✗ CLODS_DIR                    {dir}/tokens is mode 0640")), "{line}");
    assert_eq!(missing, ["CLODS_DIR (its tokens file can't be used)"]);
}

#[cfg(unix)]
#[test]
fn the_plan_line() {
    let d = tmp("plan");
    let cfg = Config::parse(&format!("CLODS_DIR={}\n", d.0.display()));
    assert_eq!(plan_line(&Config::parse("")), None);
    assert_eq!(plan_line(&cfg), None, "no tokens file: nothing to say");
    tokens(&d.0, 0o600);
    let _c = arena_core::out::capture(false);
    assert_eq!(
        plan_line(&cfg).unwrap(),
        format!("clods: 2 accounts from {}/tokens — every pod gets them all, a default by spread unless it has one", d.0.display())
    );
}
