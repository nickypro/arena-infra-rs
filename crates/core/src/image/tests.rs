use std::path::Path;
use std::process::Command;

use serde_json::json;

use super::*;
use crate::provider::hetzner::ServerType;

fn scope() -> Scope {
    Scope::new("devtest", "nickypro", "arena-sandbox-materials")
}

fn rid() -> String {
    repo_id("nickypro", "arena-sandbox-materials")
}

/// A snapshot as `GET /images` lists one, with arena's labels unless `labels` is null.
fn snap(id: u64, built: u64, labels: serde_json::Value) -> Snapshot {
    parse_snapshot(&json!({
        "id": id, "description": format!("devtest-image-{id}"), "status": "available", "image_size": 5.85,
        "disk_size": 40, "architecture": "x86", "created": "2026-10-26T11:43:43Z", "labels": labels,
        "type": "snapshot", "name": null
    }))
    .map(|mut s| {
        if built > 0 {
            s.built = Some(built);
        }
        s
    })
    .unwrap()
}

fn ours(id: u64, built: u64) -> Snapshot {
    snap(id, built, json!({"arena-image": "1", "arena-prefix": "devtest", "repo": "nickypro.arena-sandbox-materials", "repo-id": rid(), "commit": "abc", "req": "r1"}))
}

fn fit(ty: &str, arch: &str, disk: u32) -> Fit {
    Fit { server_type: ty.into(), arch: Some(arch.into()), disk_gb: Some(disk) }
}

const DAY: u64 = 86_400;
const NOW: u64 = 1_793_000_000;

/// The Rust hash and the shell function in both scripts agree, byte for byte, with both files,
/// one, or none — what makes the image's `req` label comparable to a VM's checkout.
#[test]
fn the_requirements_hash_is_the_same_in_rust_and_in_the_scripts() {
    let dir = std::env::temp_dir().join(format!("arena-req-{}", std::process::id()));
    type Files<'a> = (Option<&'a [u8]>, Option<&'a [u8]>, Option<&'a [u8]>);
    let cases: [Files; 5] = [
        (Some(b"#!/bin/bash\nuv venv\n"), Some(b"torch==2.14\ntransformer_lens\n"), None),
        (None, Some(b"numpy\n"), Some(b"[project]\nrequires-python = \">=3.14\"\n")),
        (Some(b"x"), None, None),
        (None, None, Some(b"y")),
        (None, None, None),
    ];
    for (install, reqs, pyproject) in cases {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(b) = install {
            std::fs::write(dir.join("install_uv.sh"), b).unwrap();
        }
        if let Some(b) = reqs {
            std::fs::write(dir.join("requirements.txt"), b).unwrap();
        }
        if let Some(b) = pyproject {
            std::fs::write(dir.join("pyproject.toml"), b).unwrap();
        }
        let out = Command::new("bash").arg("-c").arg(format!("{REQ_HASH_FN}; arena_req_hash \"$1\"")).arg("x").arg(&dir).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), req_hash(install, reqs, pyproject), "{install:?} {reqs:?} {pyproject:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(scrub_script().contains(REQ_HASH_FN));
    assert_ne!(req_hash(Some(b"a"), None, None), req_hash(None, Some(b"a"), None), "which file it is counts");
    assert_ne!(req_hash(None, Some(b"a"), None), req_hash(None, Some(b"a"), Some(b"p")), "a new requires-python counts");
}

#[test]
fn label_values_are_what_hetzner_accepts() {
    for (raw, want) in [
        ("nickypro.arena-sandbox-materials", "nickypro.arena-sandbox-materials"),
        ("Owner/Name", "Owner-Name"),
        ("-x_", "x"),
        ("a b", "a-b"),
        ("", ""),
    ] {
        assert_eq!(label_value(raw), want, "{raw}");
    }
    let long = label_value(&format!("{}-", "a".repeat(70)));
    assert_eq!(long.len(), 63);
    let edge = label_value(&format!("{}-b", "a".repeat(62)));
    assert!(edge.len() <= 63 && edge.ends_with(|c: char| c.is_ascii_alphanumeric()), "{edge}");
}

#[test]
fn the_setting_is_the_keyword_an_id_or_a_name() {
    for (raw, want) in [
        ("arena", Setting::Arena),
        (" Arena ", Setting::Arena),
        ("448331072", Setting::Id(448331072)),
        ("ubuntu-24.04", Setting::Name("ubuntu-24.04".into())),
        ("", Setting::Name(FALLBACK.into())),
    ] {
        assert_eq!(Setting::parse(raw), want, "{raw:?}");
    }
}

/// The sandbox's hand-made snapshot (2026-10-26) as the API listed it: no `arena-image` label, so
/// it's never this cohort's image — not resolved, pruned or counted as ours — though it carries
/// the prefix label.
#[test]
fn a_snapshot_is_read_from_the_api_and_a_hand_made_one_is_not_ours() {
    let v = json!({
        "architecture": "x86", "bound_to": null, "created": "2026-10-26T11:43:43Z",
        "description": "devtest-arena-env-py313-cpu-20261026", "disk_size": 40, "id": 448331072,
        "image_size": 5.8452584111328125, "labels": {"arena-prefix": "devtest", "arena-sandbox": "provision-parity"},
        "status": "available", "type": "snapshot"
    });
    let s = parse_snapshot(&v).unwrap();
    assert_eq!((s.id, s.disk_gb, s.arch.as_deref(), s.arena), (448331072, Some(40), Some("x86"), false));
    assert_eq!(s.built, s.created, "no built label: its creation time");
    assert!(!s.ours(&scope()));
    assert!(ours(1, NOW).ours(&scope()));
    assert!(!ours(1, NOW).ours(&Scope { prefix: "arena9".into(), ..scope() }), "another cohort's");
    assert_eq!(parse_snapshot(&json!({"description": "no id"})), None);
}

/// `arena` picks the newest of this cohort's available images of this repo that the server type
/// can boot, and says why when none fits.
#[test]
fn arena_resolves_to_the_newest_image_that_fits() {
    let cx23 = fit("cx23", "x86", 40);
    let mut creating = ours(5, NOW);
    creating.status = "creating".into();
    let mut arm = ours(6, NOW);
    arm.arch = Some("arm".into());
    let mut big = ours(7, NOW);
    big.disk_gb = Some(80);
    let other_repo = snap(8, NOW, json!({"arena-image": "1", "arena-prefix": "devtest", "repo": "o.other", "repo-id": repo_id("o", "other")}));
    // Two long repos whose readable labels are the same 63 characters: told apart by their ids.
    let long = |n: &str| Scope::new("devtest", &"o".repeat(40), &format!("{}{n}", "n".repeat(30)));
    assert_eq!(long("a").repo, long("b").repo);
    assert_ne!(long("a").repo_id, long("b").repo_id);
    let twin = snap(12, NOW, json!({"arena-image": "1", "arena-prefix": "devtest", "repo": long("a").repo, "repo-id": long("a").repo_id}));
    assert!(newest_fitting(std::slice::from_ref(&twin), &long("b"), &fit("cx23", "x86", 40)).is_err());
    assert_eq!(newest_fitting(std::slice::from_ref(&twin), &long("a"), &fit("cx23", "x86", 40)).map(|s| s.id), Ok(12));
    let hand_made = snap(9, NOW, json!({"arena-prefix": "devtest"}));
    type Case<'a> = (&'a str, Vec<Snapshot>, Fit, Result<u64, &'a str>);
    let cases: Vec<Case> = vec![
        ("none", vec![], cx23.clone(), Err("no arena-built snapshot in this Hetzner project")),
        ("hand-made only", vec![hand_made.clone()], cx23.clone(), Err("no arena-built snapshot")),
        ("another repo's", vec![other_repo.clone()], cx23.clone(), Err("is of nickypro/arena-sandbox-materials")),
        ("still being made", vec![creating], cx23.clone(), Err("available yet")),
        ("arm only", vec![arm], cx23.clone(), Err("cx23's x86 architecture")),
        ("too big a disk", vec![big.clone()], cx23.clone(), Err("none fits cx23's 40 GB disk (the smallest is 80 GB)")),
        ("big fits a big type", vec![big], fit("cx43", "x86", 160), Ok(7)),
        ("newest wins", vec![ours(1, NOW - 3 * DAY), ours(2, NOW - DAY), hand_made, other_repo], cx23.clone(), Ok(2)),
        ("same time: the higher id", vec![ours(3, NOW), ours(4, NOW)], cx23, Ok(4)),
    ];
    for (what, snaps, fit, want) in cases {
        let got = newest_fitting(&snaps, &scope(), &fit).map(|s| s.id);
        match (got, want) {
            (Ok(id), Ok(w)) => assert_eq!(id, w, "{what}"),
            (Err(e), Err(w)) => assert!(e.contains(w), "{what}: {e}"),
            (got, want) => panic!("{what}: {got:?}, want {want:?}"),
        }
    }
}

#[test]
fn a_resolution_says_what_a_create_sends_and_why() {
    let s = ours(42, NOW - 2 * DAY);
    let r = resolve(&Setting::Arena, std::slice::from_ref(&s), &scope(), &fit("cx33", "x86", 80));
    assert_eq!(r.image(), json!(42));
    assert_eq!(r.as_setting(), "42");
    assert_eq!(r.describe(NOW), format!("image arena → snapshot 42 (devtest-image-42, built {}, 2d old, commit abc)", &crate::snapshot::rfc3339(NOW - 2 * DAY)[..10]));
    let none = resolve(&Setting::Arena, &[], &scope(), &fit("cx33", "x86", 80));
    assert_eq!(none.image(), json!("ubuntu-24.04"));
    assert!(none.describe(NOW).starts_with("image arena → ubuntu-24.04 (no arena-built snapshot in this Hetzner project; setup builds the env"), "{}", none.describe(NOW));
    // An id or a name goes as given, whatever the project holds.
    assert_eq!(resolve(&Setting::Id(7), std::slice::from_ref(&s), &scope(), &fit("cx33", "x86", 80)).image(), json!(7));
    assert_eq!(resolve(&Setting::Name("debian-12".into()), &[s], &scope(), &fit("cx33", "x86", 80)).image(), json!("debian-12"));
}

#[test]
fn an_old_image_or_changed_requirements_get_a_note() {
    let fresh = ours(1, NOW - DAY);
    assert!(notes(&fresh, NOW, Some("r1")).is_empty());
    assert!(notes(&fresh, NOW, None).is_empty(), "unknown requirements: nothing claimed");
    let old = ours(1, NOW - 15 * DAY);
    let n = notes(&old, NOW, Some("r2"));
    assert_eq!(n.len(), 2, "{n:?}");
    assert!(n[0].starts_with("built 15 days ago (over 14)"), "{n:?}");
    assert!(n[1].contains("requirements") && n[1].contains("reinstalls"), "{n:?}");
    assert!(notes(&ours(1, NOW - 14 * DAY), NOW, Some("r1")).is_empty(), "14 days is not over 14");
    assert_eq!(ReqMatch::of(Some("a"), Some("a")), ReqMatch::Same);
    assert_eq!(ReqMatch::of(Some("a"), Some("b")), ReqMatch::Differ);
    assert_eq!(ReqMatch::of(None, Some("b")), ReqMatch::Unknown);
}

fn ty(name: &str, arch: &str, mem: f64, disk: u32, eur: f64) -> ServerType {
    ServerType { name: name.into(), architecture: Some(arch.into()), memory_gb: Some(mem), disk_gb: Some(disk), eur_per_hour: Some(eur), ..Default::default() }
}

/// The bake runs on the smallest disk of the configured type's architecture — with memory
/// enough to build the env (live 2026-10-29: Hetzner sells cpx02, 1 GB / 20 GB).
#[test]
fn the_bake_type_is_the_smallest_disk_with_room_to_build() {
    let types = vec![
        ty("cpx02", "x86", 1.0, 20, 0.012),
        ty("cpx11", "x86", 2.0, 40, 0.0096),
        ty("cx23", "x86", 4.0, 40, 0.0104),
        ty("cpx12", "x86", 2.0, 40, 0.0216),
        ty("cx33", "x86", 8.0, 80, 0.0168),
        ty("cpx42", "x86", 16.0, 320, 0.05),
        ty("cax11", "arm", 4.0, 40, 0.0112),
        ty("cax21", "arm", 8.0, 80, 0.02),
    ];
    for (configured, want) in [("cx33", Ok("cx23")), ("cx23", Ok("cx23")), ("cpx42", Ok("cx23")), ("cax21", Ok("cax11")), ("nope", Err("no server type `nope`"))] {
        match (bake_type(&types, configured), want) {
            (Ok(t), Ok(w)) => assert_eq!(t, w, "{configured}"),
            (Err(e), Err(w)) => assert!(e.contains(w), "{e}"),
            (got, want) => panic!("{configured}: {got:?}, want {want:?}"),
        }
    }
    // Only small-memory types of that arch: none fits, say so.
    let e = bake_type(&[ty("cpx02", "x86", 1.0, 20, 0.01)], "cpx02").unwrap_err();
    assert!(e.contains("pass --server-type"), "{e}");
}

#[test]
fn the_build_vm_and_snapshot_are_named_and_labelled_as_ours() {
    // 2026-10-29 15:42 UTC.
    let t = 1_793_288_520;
    assert_eq!(&crate::snapshot::rfc3339(t)[..16], "2026-10-29T15:42");
    assert_eq!(build_name("devtest", t), "devtest-image-261029-1542");
    let l = labels(&scope(), &Baked { build_id: "1793288520000000000-42".into(), built: t, commit: "3eed962b9bd8".into(), req: "a40bc7471764".into(), arch: "x86".into(), disk_gb: 40 });
    let want = json!({
        "arena-image": "1", "arena-prefix": "devtest", "built": t.to_string(), "repo": "nickypro.arena-sandbox-materials",
        "repo-id": rid(), "arena-image-build-id": "1793288520000000000-42",
        "commit": "3eed962b9bd8", "req": "a40bc7471764", "arch": "x86", "disk": "40"
    });
    assert_eq!(rid().len(), 16);
    assert_eq!(Value::Object(l.clone()), want);
    for v in l.values() {
        let v = v.as_str().unwrap();
        assert_eq!(label_value(v), v, "a label value Hetzner refuses: {v}");
    }
    let vm = build_vm_labels(&scope(), "7-1");
    assert_eq!(Value::Object(vm), json!({"arena-image-build": "devtest", "arena-prefix": "devtest", "arena-image-build-id": "7-1"}));
    let (a, b) = (new_build_id(), new_build_id());
    assert!(a != b && label_value(&a) == a, "{a} {b}");
    assert_eq!(scope().selector(LABEL), "arena-image=1,arena-prefix=devtest");
    assert_eq!(scope().selector(BUILD_LABEL), "arena-image-build=devtest");
}

#[test]
fn the_scrub_report_is_read_from_its_lines() {
    let r = parse_scrub("noise\narena-image-commit: abc\r\narena-image-req: r1\narena-scan: /root/x\narena-scan: /etc/y\narena-scrub: done (2 hit(s))\n");
    assert_eq!(r, ScrubReport { commit: Some("abc".into()), req: Some("r1".into()), hits: vec!["/root/x".into(), "/etc/y".into()], done: true });
    assert_eq!(parse_scrub(""), ScrubReport::default());
}

#[test]
fn the_snapshot_price_is_read_with_its_currency() {
    let body = json!({"pricing": {"currency": "USD", "image": {"price_per_gb_month": {"gross": "0.0199000000000000", "net": "0.0199000000"}}}});
    let p = parse_image_price(&body).unwrap();
    assert_eq!((p.per_gb_month, p.currency.as_str(), p.money(5.85 * p.per_gb_month)), (0.0199, "USD", "$0.12".to_string()));
    let eur = ImagePrice { per_gb_month: 0.0119, currency: "EUR".into() };
    assert_eq!(eur.money(1.0), "€1.00");
    assert_eq!(parse_image_price(&json!({"pricing": {}})), None);
}

#[test]
fn list_marks_what_arena_picks_and_prune_keeps_it() {
    let snaps = vec![ours(1, NOW - 20 * DAY), ours(2, NOW - 2 * DAY), ours(3, NOW - DAY), snap(9, 0, json!({"arena-prefix": "devtest"}))];
    let price = ImagePrice { per_gb_month: 0.0199, currency: "USD".into() };
    let t = list_table(&snaps, &scope(), NOW, Some(&price), Some("r2"), Some(2));
    let lines: Vec<&str> = t.lines().collect();
    assert_eq!(lines.len(), 4, "the header and our three, not the hand-made one:\n{t}");
    assert!(lines[0].starts_with("ID") && lines[0].contains("$/MONTH") && lines[0].contains("REQ"), "{t}");
    assert!(lines[1].starts_with("3 ") && lines[2].starts_with("2 ") && lines[3].starts_with("1 "), "newest first:\n{t}");
    assert!(lines[2].ends_with("differ  ← arena"), "{t}");
    assert!(lines[1].contains("5.8 GB") && lines[1].contains("$0.12") && lines[1].contains("40 GB"), "{t}");
    // Keep the newest one — and the one arena resolves to (2) and one HETZNER_IMAGE names (1),
    // though older.
    let ids = |v: Vec<&Snapshot>| v.iter().map(|s| s.id).collect::<Vec<_>>();
    let (kept, gone) = prune_plan(&snaps, &scope(), 1, &[2]);
    assert_eq!((ids(kept), ids(gone)), (vec![3, 2], vec![1]));
    let (kept, gone) = prune_plan(&snaps, &scope(), 1, &[2, 1]);
    assert_eq!((ids(kept), ids(gone)), (vec![3, 2, 1], vec![]));
    // --keep 0: the programme's end — every one of ours goes, protected or not; never the hand-made one.
    let (kept, gone) = prune_plan(&snaps, &scope(), 0, &[2, 1]);
    assert_eq!((ids(kept), ids(gone)), (vec![], vec![3, 2, 1]));
}

#[test]
fn a_build_vm_is_left_over_after_three_hours() {
    let vm = |id: &str, age: Option<u64>| crate::pod::Pod { id: id.into(), created_at: age.map(|a| NOW - a), ..Default::default() };
    let servers = vec![vm("young", Some(3600)), vm("old", Some(4 * 3600)), vm("unknown", None)];
    let left: Vec<&str> = leftover_build_vms(&servers, NOW).iter().map(|p| p.id.as_str()).collect();
    assert_eq!(left, ["old", "unknown"]);
}

// ---------------------------------------------------------------------------------------------
// The scrub script, run for real under bash on a stand-in root
// ---------------------------------------------------------------------------------------------

/// VS Code's machine settings as setup leaves them on a Hetzner VM (interpreter + memory-saving).
const VSCODE_SETTINGS: &str = "{\n    \"python.defaultInterpreterPath\": \"/root/ARENA_materials/.venv/bin/python\",\n    \
                               \"python.analysis.indexing\": false,\n    \"files.watcherExclude\": {\n        \"**/.venv/**\": true\n    }\n}\n";

/// A fake root as the bake VM has it after setup (and, as a belt, after a copy-keys).
struct Root {
    dir: std::path::PathBuf,
}

impl Root {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("arena-scrub-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let r = Root { dir };
        let w = |p: &str, body: &str| r.write(p, body);
        w("root/.ssh/id_ed25519", &format!("{}\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----\n", pem_head()));
        w("root/.ssh/id_ed25519.pub", "ssh-ed25519 AAAADEPLOY deploy\n");
        w("root/.ssh/authorized_keys", "ssh-ed25519 AAAACOHORT cohort\n");
        w("root/.ssh/config", "# BEGIN arena-infra github.com\nHost github.com\n  IdentityFile /root/.ssh/id_ed25519\n# END arena-infra github.com\n");
        w("root/.ssh/known_hosts", "github.com ssh-ed25519 AAAAGITHUB\n");
        w("root/.bashrc", &format!("alias ll='ls -l'\nexport HF_TOKEN=\"{}\"\nexport PATH=$PATH:/x\n  export OPENROUTER_API_KEY=\"{}\"\n", hf(), or_key()));
        w("root/.zshrc", &format!("source ~/.p10k.zsh\nexport CLAUDE_CODE_OAUTH_TOKEN=\"{}\"\n", cc()));
        w("root/.zshrc.pre-arena-20261029T1200", &format!("export HF_TOKEN=\"{}\"\nalias x=y\n", hf()));
        w("root/.claude.json", "{\"hasCompletedOnboarding\":true}\n");
        w("root/.codex/auth.json", "{}\n");
        w("root/.codex/config.toml", "model = \"x\"\n");
        w("root/.cache/huggingface/token", &hf());
        w("root/.cache/uv/wheel.whl", "x");
        w("root/.bash_history", "cat secret\n");
        w("root/.zsh_history", "ls\n");
        w("root/.name", "export MACHINE_NAME=devtest-image-261029-1542\n");
        w("root/.arena/keep", "");
        w("root/hetzner_setup.sh", "#!/bin/bash\n");
        w("home/ubuntu/.ssh/authorized_keys", "ssh-ed25519 AAAAOTHER other\n");
        w("home/ubuntu/.bash_history", "x\n");
        w("swapfile", "");
        w("etc/fstab", "UUID=abc / ext4 defaults 0 1\n/swapfile none swap sw 0 0\n");
        w("etc/sysctl.d/99-arena-swap.conf", "vm.swappiness = 10\n");
        w("etc/machine-id", "0123456789abcdef0123456789abcdef\n");
        w("var/lib/dbus/machine-id", "0123456789abcdef0123456789abcdef\n");
        w("etc/ssh/ssh_host_ed25519_key", &format!("{}\nAAAA\n-----END OPENSSH PRIVATE KEY-----\n", pem_head()));
        w("etc/ssh/ssh_host_ed25519_key.pub", "ssh-ed25519 AAAAHOST\n");
        w("etc/ssh/sshd_config", "PermitRootLogin prohibit-password\n");
        w("etc/update-motd.d/20-arena", "echo 'You are on ARENA machine devtest-image-261029-1542'\n");
        w("tmp/.arena-setup.x/f", "x");
        w("tmp/uv-lock", "");
        w("var/tmp/x", "x");
        w("var/log/syslog", "devtest-image-261029-1542 sshd: Accepted publickey\n");
        w("var/log/syslog.1", "old\n");
        w("var/log/auth.log.2.gz", "old\n");
        // The checkout and its env: a venv package's docs may hold an example key (AWS's own).
        w("root/ARENA_materials/requirements.txt", "torch\n");
        w("root/ARENA_materials/install_uv.sh", "#!/bin/bash\n");
        w("root/ARENA_materials/.venv/lib/botocore/docs.txt", &format!("aws_access_key_id = AKIA{}\n", "IOSFODNN7EXAMPLE"));
        // Third-party code and docs that quote a PEM header inside a line (live 2026-10-29: VS
        // Code server's ssh2 module, oh-my-zsh's dotenv README): not key files.
        let pem = pem_head().replace("OPENSSH ", "");
        w("root/.vscode-server/node_modules/ssh2/lib/protocol/constants.js", &format!("const key =\n  '{pem}\\r\\nMC4CAQAwBQYDK2VwBCIEIHKj'\n"));
        w("root/.vscode-server/node_modules/ssh2/lib/keyParser.js", &format!("const regexp = /^{}(?:\\r\\n|\\n)([\\s\\S]+)/;\n", pem_head()));
        w("root/.oh-my-zsh/plugins/dotenv/README.md", &format!("PRIVATE_KEY=\"{}\nMIIEowIBAAKCAQEA...\n", pem.replace("BEGIN ", "BEGIN RSA ")));
        let git = |args: &[&str]| {
            let ok = Command::new("git").current_dir(r.path("root/ARENA_materials")).args(args).output().unwrap().status.success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&["-c", "user.email=t@t", "-c", "user.name=t", "add", "requirements.txt", "install_uv.sh"]);
        git(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);
        // What setup gives every machine, which VMs from the image keep: the checkout's git
        // identity and VS Code's memory-saving machine settings.
        git(&["config", "user.name", crate::setup::GIT_IDENTITY_NAME]);
        git(&["config", "user.email", crate::setup::GIT_IDENTITY_EMAIL]);
        w("root/.vscode-server/data/Machine/settings.json", VSCODE_SETTINGS);
        // The stand-ins for what the script runs as root: swapoff, logged (exit $SWAPOFF_EXIT).
        // (A swapoff that works takes the swap off the stand-in /proc/swaps, as the kernel does.)
        w("bin/swapoff", "#!/bin/sh\necho \"swapoff $*\" >> \"$(dirname \"$0\")/../calls.log\"\n[ \"${SWAPOFF_EXIT:-0}\" = 0 ] || exit \"$SWAPOFF_EXIT\"\n[ -n \"${SWAPOFF_KEEPS:-}\" ] || sed -i '2,$d' \"$ARENA_PROC_SWAPS\"\n");
        Command::new("chmod").arg("+x").arg(r.path("bin/swapoff")).status().unwrap();
        r
    }

    fn path(&self, p: &str) -> std::path::PathBuf {
        self.dir.join(p)
    }

    fn write(&self, p: &str, body: &str) {
        let f = self.path(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, body).unwrap();
    }

    fn read(&self, p: &str) -> String {
        std::fs::read_to_string(self.path(p)).unwrap_or_else(|e| panic!("{p}: {e}"))
    }

    fn exists(&self, p: &str) -> bool {
        self.path(p).exists()
    }

    /// Run the rendered scrub script on this root: (exit code, stdout, stderr).
    fn scrub(&self) -> (i32, String, String) {
        self.scrub_with(&[])
    }

    /// [`Self::scrub`] with `env` extra (`/proc/swaps` stood in for by `proc-swaps` here, no swap
    /// on unless a test writes one).
    fn scrub_with(&self, env: &[(&str, &str)]) -> (i32, String, String) {
        if !self.exists("proc-swaps") {
            self.write("proc-swaps", "Filename\tType\tSize\tUsed\tPriority\n");
        }
        let script = self.path("scrub.sh");
        std::fs::write(&script, scrub_script()).unwrap();
        let path = format!("{}:{}", self.path("bin").display(), std::env::var("PATH").unwrap_or_default());
        let out = Command::new("bash")
            .arg(&script)
            .env("ARENA_SCRUB_ROOT", &self.dir)
            .env("PATH", path)
            .env("REPO_DIR", "/root/ARENA_materials")
            .env("KEY_REMOTE", "/root/.ssh/id_ed25519")
            .env("NAME", "devtest-image-261029-1542")
            .env("BUILT", "1793288520")
            .env("ARENA_PROC_SWAPS", self.path("proc-swaps"))
            .envs(env.iter().copied())
            .output()
            .unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Test secrets, made at run time so no key-shaped literal is in the source.
fn hf() -> String {
    format!("hf_{}", "abcdefghijklmnopqrstuvwxyzABCDEFGH")
}
fn or_key() -> String {
    format!("sk-or-v1-{}", "0123456789abcdef".repeat(4))
}
fn cc() -> String {
    format!("sk-ant-oat01-{}", "A".repeat(90))
}
fn pem_head() -> String {
    format!("-----BEGIN OPENSSH {} KEY-----", "PRIVATE")
}

/// Everything that belongs to the bake VM goes; what the image needs stays; the scan finds
/// nothing; the marker says what the image is. The scrub's own output never carries a secret.
#[test]
fn the_scrub_leaves_nothing_of_the_bake_vm_and_its_scan_is_clean() {
    let r = Root::new("clean");
    let (code, out, err) = r.scrub();
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    let rep = parse_scrub(&out);
    let head = String::from_utf8_lossy(&Command::new("git").current_dir(r.path("root/ARENA_materials")).args(["rev-parse", "--short=12", "HEAD"]).output().unwrap().stdout).trim().to_string();
    assert_eq!(rep, ScrubReport { commit: Some(head.clone()), req: Some(req_hash(Some(b"#!/bin/bash\n"), Some(b"torch\n"), None)), hits: vec![], done: true }, "{out}");
    for gone in [
        "root/.ssh/id_ed25519",
        "root/.ssh/id_ed25519.pub",
        "root/.ssh/authorized_keys",
        "root/.ssh/config",
        "home/ubuntu/.ssh/authorized_keys",
        "root/.claude.json",
        "root/.codex/auth.json",
        "root/.cache/huggingface",
        "root/.cache/uv",
        "root/.bash_history",
        "root/.zsh_history",
        "home/ubuntu/.bash_history",
        "root/.name",
        "root/.arena",
        "root/hetzner_setup.sh",
        "swapfile",
        "etc/sysctl.d/99-arena-swap.conf",
        "var/lib/dbus/machine-id",
        "etc/ssh/ssh_host_ed25519_key",
        "etc/ssh/ssh_host_ed25519_key.pub",
        "etc/update-motd.d/20-arena",
        "tmp/.arena-setup.x",
        "tmp/uv-lock",
        "var/tmp/x",
        "var/log/syslog.1",
        "var/log/auth.log.2.gz",
    ] {
        assert!(!r.exists(gone), "{gone} is still there");
    }
    for kept in ["root/.ssh/known_hosts", "root/.codex/config.toml", "etc/ssh/sshd_config", "root/ARENA_materials/.venv/lib/botocore/docs.txt", "var/log/syslog", "root/.oh-my-zsh/plugins/dotenv/README.md"] {
        assert!(r.exists(kept), "{kept} went");
    }
    assert_eq!(r.read("root/.vscode-server/data/Machine/settings.json"), VSCODE_SETTINGS, "VS Code's settings stay");
    let ident = |key: &str| {
        let out = Command::new("git").current_dir(r.path("root/ARENA_materials")).args(["config", "--local", key]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    assert_eq!(
        (ident("user.name"), ident("user.email")),
        (crate::setup::GIT_IDENTITY_NAME.to_string(), crate::setup::GIT_IDENTITY_EMAIL.to_string()),
        "the checkout's git identity stays"
    );
    assert_eq!(r.read("root/.bashrc"), "alias ll='ls -l'\nexport PATH=$PATH:/x\n", "only the token exports go");
    assert_eq!(r.read("root/.zshrc"), "source ~/.p10k.zsh\n");
    assert_eq!(r.read("root/.zshrc.pre-arena-20261029T1200"), "alias x=y\n", "setup's saved copy too");
    assert_eq!(r.read("etc/fstab"), "UUID=abc / ext4 defaults 0 1\n", "only the swap line goes");
    assert_eq!(r.read("etc/machine-id"), "", "emptied, not deleted");
    assert_eq!(r.read("var/log/syslog"), "", "logs emptied");
    assert!(!r.exists("calls.log"), "no swap was on: nothing turned off");
    assert_eq!(
        r.read("etc/arena-image"),
        format!("name=devtest-image-261029-1542\nbuilt=1793288520\ncommit={head}\nreq={}\n", rep.req.unwrap())
    );
    for secret in [hf(), or_key(), cc(), pem_head()] {
        assert!(!out.contains(&secret) && !err.contains(&secret), "the output carries a secret");
    }
}

/// Anything the scrub doesn't know to remove — a token pasted into a file, a private key in
/// /etc, a key file left in ~/.ssh — is found: the file is named, never what matched, and the
/// script exits 3 so the bake refuses to snapshot.
#[test]
fn the_scan_names_every_file_with_a_secret_and_never_the_secret() {
    let r = Root::new("hits");
    r.write("root/notes.txt", &format!("my token {}\n", hf()));
    r.write("root/ARENA_materials/day1.ipynb", &format!("{{\"source\": [\"key = '{}'\\n\"]}}\n", or_key()));
    r.write("etc/ssl/private/thing.key", &format!("{}\nAAAA\n", pem_head().replace("OPENSSH ", "")));
    r.write("root/win.pem", &format!("{}\r\nAAAA\r\n", pem_head()));
    r.write("home/ubuntu/.config/x.env", &format!("ANTHROPIC={}\n", cc()));
    let (code, out, err) = r.scrub();
    assert_eq!(code, 3, "stdout:\n{out}\nstderr:\n{err}");
    let mut hits = parse_scrub(&out).hits;
    hits.sort();
    assert_eq!(hits, ["/etc/ssl/private/thing.key", "/home/ubuntu/.config/x.env", "/root/ARENA_materials/day1.ipynb", "/root/notes.txt", "/root/win.pem"]);
    assert!(out.contains("arena-scrub: done (5 hit(s))"), "{out}");
    for secret in [hf(), or_key(), cc(), pem_head()] {
        assert!(!out.contains(&secret) && !err.contains(&secret), "the output carries a secret:\n{out}\n{err}");
    }
}

/// A key file the patterns can't see (not PEM, not a token) left in an .ssh directory is a hit too.
#[test]
fn a_file_left_in_an_ssh_directory_is_a_hit() {
    let r = Root::new("sshleft");
    // Made after the scrub's removal would have run: a stand-in `find` can't help here, so the
    // file is one the removal can't take — a directory's contents are removed, so make it
    // unremovable by putting it under a read-only directory.
    r.write("root/.ssh/keys/odd", "opaque\n");
    let ro = r.path("root/.ssh/keys");
    Command::new("chmod").arg("555").arg(&ro).status().unwrap();
    let (code, out, _) = r.scrub();
    Command::new("chmod").arg("755").arg(&ro).status().unwrap();
    // As root (CI containers) the removal succeeds anyway; as a user it can't, and the scan says so.
    if Path::new(&ro).join("odd").exists() {
        assert_eq!(code, 3, "{out}");
        assert!(parse_scrub(&out).hits.iter().any(|h| h == "/root/.ssh/keys/odd"), "{out}");
    } else {
        assert_eq!(code, 0, "{out}");
    }
}

/// A swap file holds pages of the bake VM's memory: one that's on is turned off and deleted, and
/// one that can't be — or any other swap that's on — stops the bake before anything is written.
#[test]
fn a_swap_that_cant_go_stops_the_scrub() {
    let r = Root::new("swap");
    r.write("proc-swaps", "Filename\tType\tSize\tUsed\tPriority\n/swapfile file 8388604 0 -2\n");
    let (code, out, err) = r.scrub();
    assert_eq!(code, 0, "{out}\n{err}");
    assert_eq!(r.read("calls.log"), format!("swapoff {}/swapfile\n", r.dir.display()));
    assert!(!r.exists("swapfile"));

    let r = Root::new("swap-stuck");
    r.write("proc-swaps", "Filename\tType\tSize\tUsed\tPriority\n/swapfile file 8388604 0 -2\n");
    let (code, out, err) = r.scrub_with(&[("SWAPOFF_EXIT", "1")]);
    assert_eq!(code, 1, "{out}\n{err}");
    assert!(err.trim_end().ends_with("FAILED: couldn't turn the swap file off (memory pressure?) — its pages may hold secrets"), "{err}");
    assert!(!parse_scrub(&out).done && !r.exists("etc/arena-image"), "stopped before the marker: {out}");

    // swapoff said yes but the swap is still on: stop.
    let r = Root::new("swap-lingers");
    r.write("proc-swaps", "Filename\tType\tSize\tUsed\tPriority\n/swapfile file 8388604 0 -2\n");
    let (code, _, err) = r.scrub_with(&[("SWAPOFF_KEEPS", "1")]);
    assert_eq!(code, 1);
    assert!(err.contains("FAILED: a swap is still on after swapoff: /swapfile"), "{err}");

    // /proc/swaps unreadable: whether a swap is on is unknown — stop.
    let r = Root::new("swap-unknown");
    let missing = r.path("no-such-proc-swaps").display().to_string();
    let (code, _, err) = r.scrub_with(&[("ARENA_PROC_SWAPS", &missing)]);
    assert_eq!(code, 1);
    assert!(err.contains("whether a swap is on (its pages would go into the image) is unknown"), "{err}");

    let r = Root::new("swap-partition");
    r.write("proc-swaps", "Filename\tType\tSize\tUsed\tPriority\n/dev/sda2 partition 2097148 0 -2\n");
    let (code, _, err) = r.scrub();
    assert_eq!(code, 1);
    assert!(err.contains("FAILED: a swap (/dev/sda2) is on: its pages would go into the image"), "{err}");
}

/// A scan that couldn't read something isn't a clean scan: grep's error (exit 2) stops the bake,
/// even with nothing found.
#[test]
fn a_scan_that_fails_is_never_clean() {
    let r = Root::new("scanfail");
    r.write("bin/grep", "#!/bin/sh\necho \"grep: /root/x: Input/output error\" >&2\nexit 2\n");
    Command::new("chmod").arg("+x").arg(r.path("bin/grep")).status().unwrap();
    let (code, out, err) = r.scrub();
    assert_eq!(code, 1, "{out}\n{err}");
    assert!(err.trim_end().ends_with("FAILED: the secret scan failed (grep exit 2: grep: /root/x: Input/output error)"), "{err}");
    assert!(!parse_scrub(&out).done, "{out}");
}

/// The free space is zeroed: a file of zeros until the disk is full, flushed to the disk while it
/// still exists, measured, then deleted. A write that ends for any reason but a full disk, or
/// too little written, stops the bake. (dd, df and sync stood in for: a test can't fill a disk.)
#[test]
fn the_free_space_is_zeroed_and_flushed_before_the_file_goes() {
    let stub = |r: &Root| {
        r.write("bin/df", "#!/bin/sh\necho 'Filesystem 1024-blocks Used Available Capacity Mounted on'\necho \"/dev/sda1 1 1 ${DF_KB} 1% /\"\n");
        r.write(
            "bin/dd",
            "#!/bin/sh\nfor a; do case $a in of=*) f=${a#of=};; esac; done\nhead -c \"$DD_BYTES\" /dev/zero > \"$f\"\necho \"dd: error writing '$f': ${DD_ERR:-No space left on device}\" >&2\nexit 1\n",
        );
        r.write("bin/sync", "#!/bin/sh\nfor f; do [ -e \"$f\" ] && echo \"sync $f (there)\" >> \"$(dirname \"$0\")/../calls.log\"; done\nexit 0\n");
        r.write("bin/fstrim", "#!/bin/sh\necho fstrim >> \"$(dirname \"$0\")/../calls.log\"\n");
        for b in ["df", "dd", "sync", "fstrim"] {
            Command::new("chmod").arg("+x").arg(r.path(&format!("bin/{b}"))).status().unwrap();
        }
        std::fs::create_dir_all(r.path("zero")).unwrap();
        r.path("zero").display().to_string()
    };
    // 70000 KB free; at most 64 MB may stay unwritten.
    let r = Root::new("zero-ok");
    let dir = stub(&r);
    let (code, out, err) = r.scrub_with(&[("ARENA_ZERO_DIR", &dir), ("DF_KB", "70000"), ("DD_BYTES", "5000000")]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert_eq!(r.read("calls.log"), format!("sync {dir}/.arena-zero (there)\nfstrim\n"), "flushed while it was there, then trimmed");
    assert!(!r.exists("zero/.arena-zero"), "the zeros are gone");

    for (what, env, want) in [
        ("an I/O error", [("DD_ERR", "Input/output error"), ("DD_BYTES", "5000000")], "didn't end at a full disk: dd: error writing"),
        ("too little", [("DD_ERR", "No space left on device"), ("DD_BYTES", "1000000")], "zeroing the free space wrote 0 MB of 68 MB free"),
    ] {
        let r = Root::new("zero-bad");
        let dir = stub(&r);
        let mut e: Vec<(&str, &str)> = vec![("ARENA_ZERO_DIR", &dir), ("DF_KB", "70000")];
        e.extend(env);
        let (code, out, err) = r.scrub_with(&e);
        assert_eq!(code, 1, "{what}: {out}\n{err}");
        assert!(err.contains(want), "{what}: {err}");
        assert!(!parse_scrub(&out).done && !r.exists("zero/.arena-zero"), "{what}: {out}");
    }
}

#[test]
fn the_scrub_command_runs_the_rendered_script_with_its_inputs() {
    let c = scrub_command("/root/ARENA_materials", "/root/.ssh/id_ed25519", "devtest-image-1", 7);
    assert!(c.starts_with("printf '%s' "), "{c}");
    assert!(c.ends_with("| base64 -d > /root/.arena-scrub.sh && REPO_DIR='/root/ARENA_materials' KEY_REMOTE='/root/.ssh/id_ed25519' NAME='devtest-image-1' BUILT=7 bash /root/.arena-scrub.sh"), "{c}");
    let s = scrub_script();
    for field in ["@SECRET_RE@", "@TOKEN_VARS@", "@REQ_HASH_FN@", "@MARKER@"] {
        assert!(!s.contains(field), "{field} left in the script");
    }
    assert!(s.contains("HF_TOKEN|HUGGING_FACE_HUB_TOKEN") && s.contains("CLAUDE_CODE_OAUTH_TOKEN") && s.contains("OPENROUTER_API_KEY"));
    let out = Command::new("bash").arg("-n").arg("-c").arg(&s).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

mod build_scenarios;
