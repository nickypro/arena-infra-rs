/// The one line a sync of `SSH_CONFIG_FILE` prints: a rewrite, or why not (a warning);
/// nothing when it was current or is off.
#[test]
fn ssh_config_line_says_a_rewrite_or_why_not_and_nothing_else() {
    use arena_core::sshconfig::Synced;
    let path = std::path::PathBuf::from("/home/u/.ssh/arena.conf");
    assert_eq!(super::ssh_config_line("create", None), None);
    assert_eq!(super::ssh_config_line("create", Some(Ok(Synced { path: path.clone(), written: false }))), None);
    assert_eq!(
        super::ssh_config_line("create", Some(Ok(Synced { path, written: true }))).as_deref(),
        Some("[ssh-config] after create: rewrote /home/u/.ssh/arena.conf")
    );
    assert_eq!(
        super::ssh_config_line("pods list", Some(Err("x".into()))).as_deref(),
        Some("warning: [ssh-config] after pods list: not rewritten — x")
    );
}

/// A provider listing `pods` (or failing to, `fail`) — for the ssh-config install.
struct Listed(Vec<arena_core::Pod>, bool);

#[async_trait::async_trait]
impl arena_core::Provider for Listed {
    fn name(&self) -> &'static str {
        "runpod"
    }
    fn describe(&self, _spec: &arena_core::PodSpec) -> String {
        String::new()
    }
    async fn list_pods(&self) -> arena_core::Result<Vec<arena_core::Pod>> {
        if self.1 {
            Err(arena_core::Error::provider("HTTP 500"))
        } else {
            Ok(self.0.clone())
        }
    }
    async fn create_pod(&self, _spec: &arena_core::PodSpec) -> arena_core::Result<arena_core::Pod> {
        unimplemented!("not exercised")
    }
    async fn stop_pod(&self, _id: &str) -> arena_core::Result<()> {
        Ok(())
    }
    async fn restart_pod(&self, _id: &str) -> arena_core::Result<()> {
        Ok(())
    }
    async fn terminate_pod(&self, _id: &str) -> arena_core::Result<()> {
        Ok(())
    }
}

/// A temp home with a `.ssh` folder; `config` (if any) as `~/.ssh/config`, mode 0644.
#[cfg(unix)]
fn ssh_home_with(tag: &str, config: Option<&str>) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let home = std::env::temp_dir().join(format!("arena-sshinst-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    if let Some(c) = config {
        let p = home.join(".ssh/config");
        std::fs::write(&p, c).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    home
}

/// The names in `~/.ssh` that are copies arena kept (`*.arena-bak-*`), sorted.
#[cfg(unix)]
fn kept_copies(home: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(home.join(".ssh"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".arena-bak-"))
        .collect();
    v.sort();
    v
}

/// `ssh-config --install` / `--uninstall` against a temp home (spec ssh-config-include): the
/// file written as arena's, 0600; `Include` made ~/.ssh/config's first line, its mode kept,
/// a copy of the old config kept first; a second run changes nothing; a dry run writes
/// nothing; a partial listing writes nothing; a file that isn't arena's is replaced only with
/// a copy kept; an Include inside a Host block is left alone; a missing ~/.ssh/config is
/// created 0600; `--uninstall` takes out exactly the line (a copy kept, never over the
/// install's), leaves the file, and then has nothing to take out.
#[cfg(unix)]
#[tokio::test]
async fn ssh_config_install_and_uninstall_against_a_temp_home() {
    use super::{handle_ssh_config_install as install, handle_ssh_config_uninstall as uninstall};
    use arena_core::sshconfig::Audience;
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let pods = vec![arena_core::Pod {
        id: "id-a".into(),
        name: "devtest-apple".into(),
        provider: "runpod".into(),
        status: "RUNNING".into(),
        ssh_ip: Some("1.1.1.1".into()),
        ssh_port: Some(22000),
        ..Default::default()
    }];
    let cfg = arena_core::Config::parse("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple)\nSHARED_SSH_KEY_PATH=~/.ssh/k\n");
    let want = arena_core::sshconfig::managed_text(&arena_core::sshconfig::render_manual("devtest", "root", "~/.ssh/k", &pods));
    let mine = "Host box\n    HostName 9.9.9.9\n";
    let home = ssh_home_with("main", Some(mine));
    let h = home.to_str().unwrap();
    let (conf, config) = (home.join(".ssh/arena.conf"), home.join(".ssh/config"));
    // A provider that failed to list: refused, nothing written.
    let e = install(&Listed(pods.clone(), true), &cfg, Audience::Participants, None, true, false, Some(h)).await.unwrap_err().to_string();
    assert!(e.starts_with("not installed: a provider failed to list"), "{e}");
    // A dry run: nothing written either.
    install(&Listed(pods.clone(), false), &cfg, Audience::Participants, None, true, true, Some(h)).await.unwrap();
    assert!(!conf.exists() && kept_copies(&home).is_empty());
    assert_eq!(std::fs::read_to_string(&config).unwrap(), mine);
    // The install.
    install(&Listed(pods.clone(), false), &cfg, Audience::Participants, None, true, false, Some(h)).await.unwrap();
    assert_eq!(std::fs::read_to_string(&conf).unwrap(), want);
    assert_eq!(mode(&conf), 0o600);
    assert_eq!(std::fs::read_to_string(&config).unwrap(), format!("Include ~/.ssh/arena.conf\n{mine}"));
    assert_eq!(mode(&config), 0o644, "its own mode kept");
    let copies = kept_copies(&home);
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert!(copies[0].starts_with("config.arena-bak-"));
    assert_eq!(std::fs::read_to_string(home.join(".ssh").join(&copies[0])).unwrap(), mine);
    // Again: nothing changes, no second copy.
    install(&Listed(pods.clone(), false), &cfg, Audience::Participants, None, true, false, Some(h)).await.unwrap();
    assert_eq!(kept_copies(&home).len(), 1);
    assert_eq!(std::fs::read_to_string(&config).unwrap(), format!("Include ~/.ssh/arena.conf\n{mine}"));
    // A file at the path that isn't arena's: replaced, a copy kept.
    std::fs::write(&conf, "Host handmade\n").unwrap();
    install(&Listed(pods.clone(), false), &cfg, Audience::Participants, None, true, false, Some(h)).await.unwrap();
    assert_eq!(std::fs::read_to_string(&conf).unwrap(), want);
    let copy = kept_copies(&home).into_iter().find(|n| n.starts_with("arena.conf.arena-bak-")).unwrap();
    assert_eq!(std::fs::read_to_string(home.join(".ssh").join(copy)).unwrap(), "Host handmade\n");
    // Uninstall: exactly the line, a copy kept (never over the install's), the file stays.
    uninstall(&cfg, Audience::Participants, None, true, false, Some(h)).unwrap();
    assert_eq!(std::fs::read_to_string(&config).unwrap(), mine);
    assert!(conf.exists());
    let configs: Vec<String> = kept_copies(&home).into_iter().filter(|n| n.starts_with("config.")).collect();
    assert_eq!(configs.len(), 2, "{configs:?}");
    assert_eq!(std::fs::read_to_string(home.join(".ssh").join(&configs[0])).unwrap(), mine, "the install's copy intact");
    // And again: nothing to take out, no new copy.
    uninstall(&cfg, Audience::Participants, None, true, false, Some(h)).unwrap();
    assert_eq!(kept_copies(&home).len(), 3);
    // An Include inside a Host block is the user's to move: left alone.
    let in_block = "Host box\n    Include ~/.ssh/arena.conf\n";
    let home2 = ssh_home_with("inblock", Some(in_block));
    install(&Listed(pods.clone(), false), &cfg, Audience::Participants, None, true, false, home2.to_str()).await.unwrap();
    assert_eq!(std::fs::read_to_string(home2.join(".ssh/config")).unwrap(), in_block);
    // No ~/.ssh/config yet: created, owner-only, with just the line; a PATH given is used.
    let home3 = ssh_home_with("fresh", None);
    install(&Listed(pods.clone(), false), &cfg, Audience::Participants, Some("~/.ssh/fleet.conf"), true, false, home3.to_str()).await.unwrap();
    assert_eq!(std::fs::read_to_string(home3.join(".ssh/config")).unwrap(), "Include ~/.ssh/fleet.conf\n");
    assert_eq!(mode(&home3.join(".ssh/config")), 0o600);
    assert_eq!(std::fs::read_to_string(home3.join(".ssh/fleet.conf")).unwrap(), want);
    // No home to work in: refused.
    assert!(install(&Listed(pods, false), &cfg, Audience::Participants, None, true, false, None).await.unwrap_err().to_string().contains("HOME"));
    for d in [home, home2, home3] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// What `--install`/`--uninstall` must never edit, refused before anything is written: a
/// symlinked `~/.ssh/config` or PATH (a dotfiles link would be cut), PATH being
/// `~/.ssh/config` itself (directly or through a linked folder: it would Include itself), a
/// path ssh's `Include` can't take as one argument, a home owned by someone else (sudo
/// keeping HOME). An `Include` only inside a Host/Match block is the user's: `--uninstall`
/// leaves it.
#[cfg(unix)]
#[tokio::test]
async fn ssh_config_install_refuses_what_it_must_not_edit() {
    use super::{handle_ssh_config_install as install, handle_ssh_config_uninstall as uninstall};
    use arena_core::sshconfig::Audience;
    let pods = vec![arena_core::Pod {
        id: "id-a".into(),
        name: "devtest-apple".into(),
        provider: "runpod".into(),
        status: "RUNNING".into(),
        ssh_ip: Some("1.1.1.1".into()),
        ssh_port: Some(22000),
        ..Default::default()
    }];
    let cfg = arena_core::Config::parse("MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple)\n");
    let fleet = Listed(pods, false);
    let refused = |r: anyhow::Result<()>, why: &str| {
        let e = r.unwrap_err().to_string();
        assert!(e.contains(why), "{why}: {e}");
    };
    // A symlinked ~/.ssh/config: refused, the link and its file as they were, nothing written.
    let home = ssh_home_with("links", None);
    let h = home.to_str().unwrap();
    std::fs::write(home.join("dotfiles-config"), "Host box\n").unwrap();
    std::os::unix::fs::symlink(home.join("dotfiles-config"), home.join(".ssh/config")).unwrap();
    refused(install(&fleet, &cfg, Audience::Participants, None, true, false, Some(h)).await, "~/.ssh/config is a symlink");
    refused(uninstall(&cfg, Audience::Participants, None, true, false, Some(h)), "~/.ssh/config is a symlink");
    assert!(std::fs::symlink_metadata(home.join(".ssh/config")).unwrap().file_type().is_symlink());
    assert_eq!(std::fs::read_to_string(home.join("dotfiles-config")).unwrap(), "Host box\n");
    assert!(!home.join(".ssh/arena.conf").exists() && kept_copies(&home).is_empty());
    // A symlinked PATH: refused too.
    std::fs::remove_file(home.join(".ssh/config")).unwrap();
    std::os::unix::fs::symlink(home.join("dotfiles-config"), home.join(".ssh/arena.conf")).unwrap();
    refused(install(&fleet, &cfg, Audience::Participants, None, true, false, Some(h)).await, "~/.ssh/arena.conf is a symlink");
    assert!(!home.join(".ssh/config").exists());
    // PATH = ~/.ssh/config, by name or through a linked folder.
    std::fs::write(home.join(".ssh/config"), "Host box\n").unwrap();
    refused(install(&fleet, &cfg, Audience::Participants, Some("~/.ssh/config"), true, false, Some(h)).await, "is ~/.ssh/config itself");
    std::os::unix::fs::symlink(home.join(".ssh"), home.join("alias")).unwrap();
    refused(install(&fleet, &cfg, Audience::Participants, Some("~/alias/config"), true, false, Some(h)).await, "is ~/.ssh/config itself");
    // A path ssh would split or expand.
    refused(install(&fleet, &cfg, Audience::Participants, Some("~/.ssh/my arena.conf"), true, false, Some(h)).await, "can't take as one plain path");
    assert_eq!(std::fs::read_to_string(home.join(".ssh/config")).unwrap(), "Host box\n");
    assert!(kept_copies(&home).is_empty());
    // An Include only inside a block: --uninstall leaves it, and keeps no copy.
    let block = "Host box\n    Include ~/.ssh/arena.conf\n";
    std::fs::write(home.join(".ssh/config"), block).unwrap();
    uninstall(&cfg, Audience::Participants, None, true, false, Some(h)).unwrap();
    assert_eq!(std::fs::read_to_string(home.join(".ssh/config")).unwrap(), block);
    assert!(kept_copies(&home).is_empty());
    // Someone else's home (here `/`, root's): refused — unless this test runs as `/`'s owner.
    if !arena_core::sshconfig::owned_by_me(&std::fs::metadata("/").unwrap()) {
        refused(install(&fleet, &cfg, Audience::Participants, Some("/nonexistent-arena/x.conf"), true, false, Some("/")).await, "isn't owned by this user");
    }
    let _ = std::fs::remove_dir_all(&home);
}

/// `config check`'s SSH_CONFIG_FILE row: off, a bad value (counted), not written yet, not
/// arena's, and arena's — included by ~/.ssh/config before any block, inside one, or not.
#[cfg(unix)]
#[test]
fn ssh_config_check_row_says_what_state_the_file_is_in() {
    use super::ssh_config_check_row as row;
    let home = ssh_home_with("check", None);
    let h = home.to_str().unwrap();
    let cfg = |v: &str| arena_core::Config::parse(&format!("SSH_CONFIG_FILE={v}\n"));
    let (r, bad) = row(&arena_core::Config::parse(""), Some(h));
    assert!(r.starts_with("  · SSH_CONFIG_FILE") && r.contains("(unset)") && bad.is_none(), "{r}");
    let (r, bad) = row(&cfg("arena.conf"), Some(h));
    assert!(r.starts_with("  ✗ SSH_CONFIG_FILE") && bad.unwrap().starts_with("SSH_CONFIG_FILE must be"), "{r}");
    let c = cfg("~/.ssh/arena.conf");
    let (r, _) = row(&c, Some(h));
    assert!(r.starts_with("  ⚠ SSH_CONFIG_FILE") && r.contains("~/.ssh/arena.conf — not written yet"), "{r}");
    std::fs::write(home.join(".ssh/arena.conf"), "Host mine\n").unwrap();
    assert!(row(&c, Some(h)).0.contains("isn't arena's"));
    std::fs::write(home.join(".ssh/arena.conf"), arena_core::sshconfig::managed_text("Host a\n")).unwrap();
    assert!(row(&c, Some(h)).0.contains("(arena's), not included by ~/.ssh/config"));
    std::fs::write(home.join(".ssh/config"), "Host box\n  Include ~/.ssh/arena.conf\n").unwrap();
    assert!(row(&c, Some(h)).0.contains("inside a Host/Match block (line 2)"));
    std::fs::write(home.join(".ssh/config"), format!("Include {h}/.ssh/arena.conf\nHost box\n")).unwrap();
    let (r, bad) = row(&c, Some(h));
    assert!(r.starts_with("  ✓ SSH_CONFIG_FILE") && r.ends_with("(arena's), included by ~/.ssh/config") && bad.is_none(), "{r}");
    let _ = std::fs::remove_dir_all(&home);
}

/// The copies `--install`/`--uninstall` keep never land on an earlier one — the same second's
/// stamp gets `-2`, `-3` — and each holds the file as it was then, owner-only.
#[cfg(unix)]
#[test]
fn kept_copies_never_overwrite_an_earlier_copy() {
    use std::os::unix::fs::PermissionsExt;
    let home = ssh_home_with("copies", Some("v1\n"));
    let config = home.join(".ssh/config");
    assert_eq!(super::keep_copy(&config, "20261022T120000Z", "~/.ssh/config").unwrap(), "~/.ssh/config.arena-bak-20261022T120000Z");
    std::fs::write(&config, "v2\n").unwrap();
    assert_eq!(super::keep_copy(&config, "20261022T120000Z", "~/.ssh/config").unwrap(), "~/.ssh/config.arena-bak-20261022T120000Z-2");
    let read = |n: &str| std::fs::read_to_string(home.join(".ssh").join(n)).unwrap();
    assert_eq!((read("config.arena-bak-20261022T120000Z"), read("config.arena-bak-20261022T120000Z-2")), ("v1\n".into(), "v2\n".into()));
    let m = std::fs::metadata(home.join(".ssh/config.arena-bak-20261022T120000Z-2")).unwrap().permissions().mode() & 0o777;
    assert_eq!(m, 0o600);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn ssh_config_flags_parse() {
    use crate::cli::{Cli, Cmd};
    use clap::Parser;
    let parse = |args: &[&str]| Cli::try_parse_from(["arena", "ssh-config"].iter().chain(args).copied()).map(|c| c.cmd);
    assert!(matches!(parse(&[]).unwrap(), Cmd::SshConfig { install: None, uninstall: None, proxy: false, .. }));
    assert!(matches!(parse(&["--install"]).unwrap(), Cmd::SshConfig { install: Some(None), .. }));
    assert!(matches!(parse(&["--install", "~/x.conf"]).unwrap(), Cmd::SshConfig { install: Some(Some(p)), .. } if p == "~/x.conf"));
    assert!(matches!(parse(&["--uninstall", "--dry-run"]).unwrap(), Cmd::SshConfig { uninstall: Some(None), dry_run: true, .. }));
    for bad in [
        &["--install", "--uninstall"][..],
        &["--install", "--proxy"],
        &["--uninstall", "--out", "/x"],
        // A dry run of what isn't a setup would be ignored, and `--out` write anyway.
        &["--dry-run"],
        &["--out", "/x", "--dry-run"],
        &["--proxy", "--dry-run"],
    ] {
        assert!(parse(bad).is_err(), "{bad:?}");
    }
}
