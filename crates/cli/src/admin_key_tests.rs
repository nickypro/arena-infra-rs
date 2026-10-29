//! The admin key (`ADMIN_SSH_KEY_PATH`): a key whose private half only the proxy box holds, which
//! every machine authorizes. `config check` says its state; setup, copy-keys and the create
//! spec authorize its public half everywhere — staff boxes too — and never send its private half
//! anywhere; `ssh-config --admin [--install]` gives staff on the proxy box every machine with it.
use super::*;
use arena_core::remote::{FakeRemote, RemoteCall};
use arena_core::Pod;

/// A temp dir of fake keys: each `<name>` private file holds a marker, its `<name>.pub` a public
/// key line. Removed on drop.
struct Keys(PathBuf);

impl Drop for Keys {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The admin key's private marker: never in anything sent to a machine.
const ADMIN_PRIVATE: &str = "ADMIN-PRIVATE-KEY-MATERIAL";
const ADMIN_PUB: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAdminAdminAdminAdminAdminAdminAdminAdmin staff@proxy";
const COHORT_PUB: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICohortCohortCohortCohortCohortCohortCohor devtest";
const DEPLOY_PUB: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDeployDeployDeployDeployDeployDeployDeplo deploy";

impl Keys {
    fn new(tag: &str) -> Keys {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("arena-admin-key-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, private, public) in [
            ("devtest_key", "COHORT-PRIVATE", COHORT_PUB),
            ("devtest_deploy_key", "DEPLOY-PRIVATE", DEPLOY_PUB),
            ("arena_admin_key", ADMIN_PRIVATE, ADMIN_PUB),
        ] {
            let p = dir.join(name);
            std::fs::write(&p, format!("-----BEGIN OPENSSH PRIVATE KEY-----\n{private}\n-----END OPENSSH PRIVATE KEY-----\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
            std::fs::write(dir.join(format!("{name}.pub")), format!("{public}\n")).unwrap();
        }
        Keys(dir)
    }

    fn path(&self, name: &str) -> String {
        self.0.join(name).display().to_string()
    }

    /// The three keys, as a config's lines.
    fn lines(&self) -> String {
        format!(
            "SHARED_SSH_KEY_PATH={}\nGIT_SSH_KEY_LOCAL={}\nADMIN_SSH_KEY_PATH={}\n",
            self.path("devtest_key"),
            self.path("devtest_deploy_key"),
            self.path("arena_admin_key")
        )
    }
}

fn pod(name: &str, provider: &str, port: u16) -> Pod {
    Pod {
        id: format!("id-{name}"),
        name: name.into(),
        provider: provider.into(),
        status: "RUNNING".into(),
        ssh_ip: Some("10.0.0.1".into()),
        ssh_port: Some(port),
        ..Default::default()
    }
}

/// One machine of each kind: a pod on every provider (two bare VMs among them) and a staff box.
fn machines() -> Vec<Pod> {
    vec![
        pod("devtest-apple", "runpod", 22001),
        pod("devtest-bloom", "vast", 22002),
        pod("devtest-cloud", "hetzner", 22003),
        pod("devtest-dawn", "shadeform", 22004),
        pod("james-gpu", "runpod", 22005),
    ]
}

const NAMES: &str = "MACHINE_NAME_PREFIX=devtest\nMACHINE_NAME_LIST=(apple bloom cloud dawn @james-gpu)\n";

/// Everything one machine was sent, checked: no call carries the admin key's private half or
/// names its file — not a copied file, not a command, not the ssh config written onto it — and
/// some command authorized its public half.
fn sent_nothing_private_but_authorized(calls: &[RemoteCall], keys: &Keys, who: &str) {
    let admin = keys.path("arena_admin_key");
    assert!(!calls.is_empty(), "{who}: nothing was sent");
    let mut authorized = false;
    for c in calls {
        match c {
            RemoteCall::Copy { local, .. } => {
                assert!(!local.contains("arena_admin_key"), "{who}: copied {local}");
                let sent = std::fs::read_to_string(local).unwrap_or_default();
                assert!(!sent.contains(ADMIN_PRIVATE), "{who}: {local} holds the admin key's private half");
            }
            RemoteCall::Exec { cmd, .. } => {
                assert!(!cmd.contains(ADMIN_PRIVATE), "{who}: a command carries the admin key's private half");
                assert!(!cmd.contains(&admin) && !cmd.contains("arena_admin_key"), "{who}: a command names the admin key file: {cmd}");
                authorized |= cmd.contains(&format!("'{ADMIN_PUB}'")) && cmd.contains("authorized_keys");
            }
        }
    }
    assert!(authorized, "{who}: the admin key's public half was never authorized: {calls:#?}");
}

/// Task: never distributed. Setup (image pods, both bare VMs, a staff box) and copy-keys (with no
/// key at all — the fleet's ssh alone — and with a token) authorize the admin key's public half
/// on every machine and send nothing of its private half: no file copied, no command, no line of
/// the fleet ssh config written onto the pods (its identity is the pod's own deploy key).
#[tokio::test(start_paused = true)]
async fn no_setup_or_copy_keys_path_sends_the_admin_private_key_anywhere() {
    let keys = Keys::new("never");
    let cfg = Config::parse(&format!(
        "{NAMES}ARENA_REPO_OWNER=o\nARENA_REPO_NAME=r\nGIT_SSH_KEY_REMOTE=/root/.ssh/id_ed25519\n{}",
        keys.lines()
    ));
    let sel = Selected::all(machines());
    let no_csv = keys.path("no-keys-dir");

    let fake = Arc::new(FakeRemote::new());
    let timeouts = arena_core::setup::SetupTimeouts::from_config(&cfg, None).unwrap();
    handle_setup(fake.clone(), &cfg, true, false, None, None, false, false, timeouts, &sel, &no_csv, None).await.unwrap();
    for p in &sel.pods {
        let host = format!("10.0.0.1:{}", p.ssh_port.unwrap());
        sent_nothing_private_but_authorized(&fake.calls_to(&host), &keys, &format!("setup {} ({})", p.name, p.provider));
    }

    for token in [None, Some("hf_x".to_string())] {
        let fake = Arc::new(FakeRemote::new());
        handle_copy_keys(fake.clone(), &cfg, &no_csv, token.clone(), None, &sel, false, true).await.unwrap();
        for p in &sel.pods {
            let calls = fake.calls_to(&format!("10.0.0.1:{}", p.ssh_port.unwrap()));
            sent_nothing_private_but_authorized(&calls, &keys, &format!("copy-keys {} ({token:?})", p.name));
            let RemoteCall::Exec { cmd, .. } = &calls[0] else { panic!("{calls:?}") };
            let fleet = &cmd[cmd.find("# BEGIN arena-infra fleet").unwrap()..];
            assert!(fleet.contains("IdentityFile /root/.ssh/id_ed25519") && !fleet.contains("arena_admin"), "{fleet}");
        }
    }

    // The create spec every provider builds from (Hetzner's cloud-init included): public halves.
    let spec = PodSpec::from_config(&cfg);
    let env = format!("{:?}", spec.env);
    assert!(env.contains(ADMIN_PUB) && !env.contains(ADMIN_PRIVATE) && !env.contains("arena_admin_key"), "{env}");
    let (fleet_ssh, n) = fleet_ssh_for(&cfg, &sel.pods);
    assert_eq!(n, 3, "shared, deploy, admin");
    assert!(fleet_ssh.contains(ADMIN_PUB) && !fleet_ssh.contains("arena_admin_key"));
}

/// `config check`'s admin key rows: `·` unset (optional); `✓` readable with a public half; `⚠`
/// with why — unreadable, no public half, the shared or the deploy key under another name (by
/// file or by key), other users able to get at it. Advice only: never a missing key.
#[cfg(unix)]
#[test]
fn config_check_says_what_state_the_admin_key_is_in() {
    use std::os::unix::fs::PermissionsExt;
    let keys = Keys::new("check");
    let base = format!("SHARED_SSH_KEY_PATH={}\nGIT_SSH_KEY_LOCAL={}\n", keys.path("devtest_key"), keys.path("devtest_deploy_key"));
    let rows = |extra: &str| admin_key_rows(&Config::parse(&format!("{base}{extra}"))).join("\n");
    let admin = keys.path("arena_admin_key");

    let unset = rows("");
    assert!(unset.starts_with("  · admin SSH key              (unset — optional: ADMIN_SSH_KEY_PATH"), "{unset}");
    assert_eq!(rows(&format!("ADMIN_SSH_KEY_PATH={admin}\n")), format!("  ✓ admin SSH key              {admin}"));

    // The shared key's file, or a copy of the deploy key: pods or participants hold it.
    let shared = rows(&format!("ADMIN_SSH_KEY_PATH={}\n", keys.path("devtest_key")));
    assert!(shared.starts_with("  ⚠ admin SSH key") && shared.contains("└ it is SHARED_SSH_KEY_PATH (the shared key"), "{shared}");
    let copy = keys.path("deploy_copy");
    std::fs::copy(keys.path("devtest_deploy_key"), &copy).unwrap();
    std::fs::copy(keys.path("devtest_deploy_key.pub"), format!("{copy}.pub")).unwrap();
    let deploy = rows(&format!("ADMIN_SSH_KEY_PATH={copy}\n"));
    assert!(deploy.contains("└ it is GIT_SSH_KEY_LOCAL (the deploy key: every pod holds it)"), "{deploy}");
    assert!(!deploy.contains("SHARED_SSH_KEY_PATH"), "{deploy}");

    // Others can read it.
    std::fs::set_permissions(&admin, std::fs::Permissions::from_mode(0o644)).unwrap();
    let open = rows(&format!("ADMIN_SSH_KEY_PATH={admin}\n"));
    assert!(open.starts_with("  ⚠ admin SSH key") && open.contains(&format!("└ mode 0644: other users of this box can get at it (and ssh refuses such a key) — chmod 600 {admin}")), "{open}");
    std::fs::set_permissions(&admin, std::fs::Permissions::from_mode(0o600)).unwrap();

    // No public half: no .pub and not a key ssh-keygen can read.
    let bare = keys.path("no_pub_key");
    std::fs::write(&bare, "not a key").unwrap();
    std::fs::set_permissions(&bare, std::fs::Permissions::from_mode(0o600)).unwrap();
    let no_pub = rows(&format!("ADMIN_SSH_KEY_PATH={bare}\n"));
    assert!(no_pub.contains("└ its public half can't be derived"), "{no_pub}");

    // Not readable by this user (as root, every file is).
    let missing = rows(&format!("ADMIN_SSH_KEY_PATH={}\n", keys.path("absent_key")));
    assert!(missing.starts_with("  ⚠ admin SSH key") && missing.contains("└ not readable by this user"), "{missing}");

    // In the report: under setup readiness, beside the other keys, and never a missing key.
    let (report, missing_keys) = config_report(&Config::parse(&format!("{base}ADMIN_SSH_KEY_PATH={}\n", keys.path("devtest_key"))), "runpod");
    let at = report.find("admin SSH key").unwrap();
    assert!(report.find("git deploy key .pub").unwrap() < at, "{report}");
    assert!(!missing_keys.iter().any(|m| m.contains("ADMIN")), "{missing_keys:?}");
}

/// A provider that lists a fixed fleet, or fails to.
struct Listed(Vec<Pod>, bool);

#[async_trait::async_trait]
impl Provider for Listed {
    fn name(&self) -> &'static str {
        "runpod"
    }
    fn describe(&self, _spec: &PodSpec) -> String {
        String::new()
    }
    async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
        if self.1 {
            Err(arena_core::Error::provider("HTTP 500"))
        } else {
            Ok(self.0.clone())
        }
    }
    async fn create_pod(&self, _spec: &PodSpec) -> arena_core::Result<Pod> {
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

/// A temp home with an empty `~/.ssh`.
fn home(tag: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!("arena-admin-home-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    home
}

/// `ssh-config --admin --install`: refused without the admin key or one this user can't read; else
/// staff's file (`~/.ssh/arena-admin.conf`, its own first line, every machine at its endpoint with
/// the admin key) and its `Include` first in `~/.ssh/config`, the `config set` hint for
/// SSH_ADMIN_CONFIG_FILE; never the participants' file (by their key's path, or by its first
/// line), and never beside it in one ~/.ssh/config (either way round); `--uninstall` takes its
/// Include out. `config check` then shows the file's row.
#[cfg(unix)]
#[tokio::test]
async fn ssh_config_admin_install_writes_staff_s_own_file() {
    let keys = Keys::new("install");
    let h = home("install");
    let hs = h.to_str().unwrap();
    let fleet = Listed(machines(), false);
    let refused = |r: anyhow::Result<()>, why: &str| {
        let e = r.unwrap_err().to_string();
        assert!(e.contains(why), "{why}: {e}");
    };
    let no_key = Config::parse(NAMES);
    refused(handle_ssh_config_install(&fleet, &no_key, Audience::Admin, None, true, false, Some(hs)).await, "ADMIN_SSH_KEY_PATH isn't set");
    let unreadable = Config::parse(&format!("{NAMES}ADMIN_SSH_KEY_PATH={}\n", keys.path("absent")));
    refused(handle_ssh_config_install(&fleet, &unreadable, Audience::Admin, None, true, false, Some(hs)).await, "isn't readable by this user");
    let cfg = Config::parse(&format!("{NAMES}{}", keys.lines()));
    refused(handle_ssh_config_install(&Listed(machines(), true), &cfg, Audience::Admin, None, true, false, Some(hs)).await, "failed to list");
    assert!(!h.join(".ssh/arena-admin.conf").exists() && !h.join(".ssh/config").exists(), "nothing written");

    handle_ssh_config_install(&fleet, &cfg, Audience::Admin, None, true, false, Some(hs)).await.unwrap();
    let text = std::fs::read_to_string(h.join(".ssh/arena-admin.conf")).unwrap();
    let want = Audience::Admin.text(&arena_core::sshconfig::render_admin_for(&cfg, &machines()).unwrap());
    assert_eq!(text, want);
    assert!(text.contains(&format!("Host james-gpu\n    HostName 10.0.0.1\n    Port 22005\n    User root\n    IdentityFile {}\n", keys.path("arena_admin_key"))));
    assert_eq!(std::fs::read_to_string(h.join(".ssh/config")).unwrap(), "Include ~/.ssh/arena-admin.conf\n");

    // `config check`: its row, once the key is set (SSH_ADMIN_CONFIG_FILE given here).
    let kept = Config::parse(&format!("{NAMES}{}SSH_ADMIN_CONFIG_FILE=~/.ssh/arena-admin.conf\n", keys.lines()));
    let (row, bad) = ssh_config_check_row_as(Audience::Admin, &kept, Some(hs));
    assert_eq!(row, "  ✓ SSH_ADMIN_CONFIG_FILE        ~/.ssh/arena-admin.conf (arena's), included by ~/.ssh/config");
    assert_eq!(bad, None);
    let clash = Config::parse(&format!(
        "{NAMES}{}SSH_ADMIN_CONFIG_FILE=~/.ssh/arena.conf\nSSH_CONFIG_FILE=~/.ssh/arena.conf\n",
        keys.lines()
    ));
    let (row, bad) = ssh_config_check_row_as(Audience::Admin, &clash, Some(hs));
    assert!(row.starts_with("  ✗ SSH_ADMIN_CONFIG_FILE") && bad.unwrap().contains("SSH_CONFIG_FILE's file too"), "{row}");
    let (row, _) = ssh_config_check_row_as(Audience::Admin, &cfg, Some(hs));
    assert!(row.starts_with("  · SSH_ADMIN_CONFIG_FILE        (unset) — on the proxy box"), "{row}");

    // The participants' file is never taken: by their key's path, or by its first line.
    refused(handle_ssh_config_install(&fleet, &clash, Audience::Admin, None, true, false, Some(hs)).await, "is SSH_CONFIG_FILE's file");
    std::fs::write(h.join(".ssh/arena.conf"), arena_core::sshconfig::managed_text("Host devtest-*\n")).unwrap();
    refused(
        handle_ssh_config_install(&fleet, &cfg, Audience::Admin, Some("~/.ssh/arena.conf"), true, false, Some(hs)).await,
        "is arena's other file (`arena ssh-config --install`)",
    );
    refused(
        handle_ssh_config_install(&fleet, &cfg, Audience::Participants, Some("~/.ssh/arena-admin.conf"), true, false, Some(hs)).await,
        "is arena's other file (`arena ssh-config --admin --install`)",
    );
    assert_eq!(std::fs::read_to_string(h.join(".ssh/arena-admin.conf")).unwrap(), want);
    // Never both included by one ~/.ssh/config: the same Host names, and ssh takes each host's
    // address from whichever Include comes first.
    refused(handle_ssh_config_install(&fleet, &cfg, Audience::Participants, None, true, false, Some(hs)).await, "already includes");

    handle_ssh_config_uninstall(&cfg, Audience::Admin, None, true, false, Some(hs)).unwrap();
    assert_eq!(std::fs::read_to_string(h.join(".ssh/config")).unwrap(), "");
    assert!(h.join(".ssh/arena-admin.conf").exists(), "the file stays");
    handle_ssh_config_install(&fleet, &cfg, Audience::Participants, None, true, false, Some(hs)).await.unwrap();
    assert_eq!(std::fs::read_to_string(h.join(".ssh/config")).unwrap(), "Include ~/.ssh/arena.conf\n");
    refused(handle_ssh_config_install(&fleet, &cfg, Audience::Admin, None, true, false, Some(hs)).await, "already includes");
    let _ = std::fs::remove_dir_all(&h);
}

/// The one-file-per-config guard reads Includes as ssh does: a relative one (under ~/.ssh), a
/// glob, a nested one — in both directions; and an Include it can't follow refuses rather than
/// guesses.
#[cfg(unix)]
#[tokio::test]
async fn either_install_sees_the_other_file_however_it_is_included() {
    let keys = Keys::new("guard");
    let cfg = Config::parse(&format!("{NAMES}{}", keys.lines()));
    let fleet = Listed(machines(), false);
    let theirs = arena_core::sshconfig::managed_text("Host devtest-*\n");
    let staff = Audience::Admin.text("Host devtest-apple\n");
    for (tag, files, config, audience, why) in [
        ("rel", vec![("arena.conf", theirs.as_str())], "Include arena.conf\n", Audience::Admin, "already includes"),
        ("glob", vec![("arena-admin.conf", staff.as_str())], "Include ~/.ssh/arena-*.conf\n", Audience::Participants, "already includes"),
        (
            "nested",
            vec![("arena.conf", theirs.as_str()), ("mine.conf", "Include \"arena.conf\"\n")],
            "Host x\n    Include mine.conf\n",
            Audience::Admin,
            "already includes",
        ),
        ("env", vec![], "Include ${HOME}/.ssh/arena.conf\n", Audience::Admin, "has an Include arena can't follow (${HOME}/.ssh/arena.conf)"),
    ] {
        let h = home(&format!("guard-{tag}"));
        for (name, text) in files {
            std::fs::write(h.join(".ssh").join(name), text).unwrap();
        }
        std::fs::write(h.join(".ssh/config"), config).unwrap();
        let e = handle_ssh_config_install(&fleet, &cfg, audience, None, true, false, h.to_str()).await.unwrap_err().to_string();
        assert!(e.contains(why), "{tag}: {e}");
        assert_eq!(std::fs::read_to_string(h.join(".ssh/config")).unwrap(), config, "{tag}: untouched");
        let _ = std::fs::remove_dir_all(&h);
    }
}

/// Every pod change keeps staff's file current beside the participants' (one line each), and
/// lists the fleet for it even when the participants' file takes the proxy's ports.
#[cfg(unix)]
#[tokio::test]
async fn a_pod_change_rewrites_staff_s_file_too() {
    let keys = Keys::new("after");
    let dir = home("after");
    let (theirs, staff) = (dir.join(".ssh/arena.conf"), dir.join(".ssh/arena-admin.conf"));
    let cfg = Config::parse(&format!(
        "{NAMES}{}SSH_PROXY_HOST=px.example\nSSH_PROXY_STARTING_PORT=9500\nSSH_CONFIG_FILE={}\nSSH_ADMIN_CONFIG_FILE={}\n",
        keys.lines(),
        theirs.display(),
        staff.display()
    ));
    let lines = ssh_config_after(&cfg, &Listed(machines(), false), "create").await;
    assert_eq!(
        lines,
        [format!("[ssh-config] after create: rewrote {}", theirs.display()), format!("[ssh-config] after create: rewrote {}", staff.display())]
    );
    assert!(std::fs::read_to_string(&staff).unwrap().contains("Host devtest-dawn\n    HostName 10.0.0.1\n    Port 22004\n"));
    assert!(ssh_config_after(&cfg, &Listed(machines(), false), "up").await.is_empty(), "nothing moved");
    let failed = ssh_config_after(&cfg, &Listed(machines(), true), "restart").await;
    assert!(failed.len() == 1 && failed[0].contains("not rewritten — runpod failed to list"), "{failed:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ssh_config_admin_flags_parse() {
    let parse = |args: &[&str]| Cli::try_parse_from(["arena", "ssh-config"].iter().chain(args)).map(|c| c.cmd);
    assert!(matches!(parse(&["--admin"]).unwrap(), Cmd::SshConfig { admin: true, install: None, .. }));
    assert!(matches!(parse(&["--admin", "--install"]).unwrap(), Cmd::SshConfig { admin: true, install: Some(None), .. }));
    assert!(matches!(parse(&["--admin", "--uninstall", "--dry-run"]).unwrap(), Cmd::SshConfig { admin: true, uninstall: Some(None), .. }));
    assert!(matches!(parse(&["--admin", "--out", "x.conf"]).unwrap(), Cmd::SshConfig { admin: true, out: Some(_), .. }));
    assert!(parse(&["--admin", "--proxy"]).is_err(), "staff's config is never the proxy's ports");
}
