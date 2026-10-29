//! `image::build::build` against a fake Hetzner project and a fake VM: every way a bake ends,
//! and the VM deleted in each.

use std::time::Duration;

use serde_json::{json, Value};

use crate::image::build::{build, BuildPlan, Report, VmFate};
use crate::image::cloud::{FakeImageCloud, ImageCall};
use crate::image::Scope;
use crate::pod::Pod;
use crate::remote::{FakeRemote, FakeReply, RemoteCall};
use crate::setup::ProvisionStep;
use crate::ssh::SshTarget;

const SCRUBBED: &str = "arena-image-commit: 3eed962b9bd8\narena-image-req: a40bc7471764\narena-scrub: done (0 hit(s))\n";

fn plan() -> BuildPlan {
    let secs = Duration::from_secs;
    BuildPlan {
        name: "devtest-image-261029-1542".into(),
        build_id: "b1".into(),
        server_type: "cx23".into(),
        scope: Scope::new("devtest", "o", "r"),
        setup: vec![
            ProvisionStep::Scp { label: "copy deploy key", local: "/k".into(), remote: "/root/.ssh/id_ed25519".into(), timeout: secs(60) },
            ProvisionStep::Run { label: "hetzner setup script", cmd: "bash /root/hetzner_setup.sh".into(), timeout: secs(1800) },
        ],
        scrub: "SCRUB arena-scrub.sh".into(),
        arch: "x86".into(),
        disk_gb: 40,
        built: 1_793_288_520,
    }
}

fn target(p: &Pod) -> crate::Result<SshTarget> {
    Ok(SshTarget::for_host("root", p.ssh_ip.as_deref().unwrap_or("?"), 22, None))
}

/// The first server the fake makes is id 1001 at 10.0.0.1.
const HOST: &str = "10.0.0.1:22";

async fn run(cloud: &FakeImageCloud, remote: &FakeRemote, stop: impl std::future::Future<Output = ()>) -> (Report, Vec<String>) {
    let said = std::sync::Mutex::new(Vec::new());
    let r = build(cloud, remote, &plan(), &target, stop, &|l| said.lock().unwrap().push(l.to_string())).await;
    (r, said.into_inner().unwrap())
}

fn never() -> std::future::Pending<()> {
    std::future::pending()
}

fn mutations(cloud: &FakeImageCloud) -> Vec<ImageCall> {
    cloud
        .calls()
        .into_iter()
        .filter(|c| matches!(c, ImageCall::CreateServer { .. } | ImageCall::Stop(_) | ImageCall::CreateImage { .. } | ImageCall::DeleteServer(_) | ImageCall::DeleteImage(_)))
        .collect()
}

fn create_call() -> ImageCall {
    let labels = json!({"arena-image-build": "devtest", "arena-prefix": "devtest", "arena-image-build-id": "b1"});
    let Value::Object(labels) = labels else { unreachable!() };
    ImageCall::CreateServer { name: "devtest-image-261029-1542".into(), server_type: "cx23".into(), labels }
}

/// Set up, scrubbed, scan clean: powered off, snapshotted with the labels — the commit and the
/// requirements hash the scrub read on the VM — and the VM deleted after.
#[tokio::test(start_paused = true)]
async fn a_clean_bake_snapshots_and_deletes_the_vm() {
    let cloud = FakeImageCloud::new(vec![]);
    let remote = FakeRemote::new();
    remote.on(HOST, "arena-scrub.sh", FakeReply::stdout(SCRUBBED));
    let (r, said) = run(&cloud, &remote, never()).await;
    let snap = r.result.expect("an image");
    assert_eq!((snap.commit.as_deref(), snap.req.as_deref(), snap.arena), (Some("3eed962b9bd8"), Some("a40bc7471764"), true));
    assert_eq!(r.vm, VmFate::Deleted { id: "1001".into() });
    let labels = json!({
        "arena-image": "1", "arena-prefix": "devtest", "built": "1793288520", "repo": "o.r", "repo-id": crate::image::repo_id("o", "r"),
        "arena-image-build-id": "b1", "commit": "3eed962b9bd8", "req": "a40bc7471764", "arch": "x86", "disk": "40"
    });
    let Value::Object(labels) = labels else { unreachable!() };
    assert_eq!(
        mutations(&cloud),
        [
            create_call(),
            ImageCall::Stop("1001".into()),
            ImageCall::CreateImage { server: "1001".into(), description: "devtest-image-261029-1542".into(), labels },
            ImageCall::DeleteServer("1001".into()),
        ]
    );
    // Setup's steps, then the scrub — the scrub last of all on the VM.
    let calls = remote.calls_to(HOST);
    assert!(matches!(&calls[0], RemoteCall::Copy { remote, .. } if remote == "/root/.ssh/id_ed25519"), "{calls:?}");
    assert!(matches!(calls.last(), Some(RemoteCall::Exec { cmd, .. }) if cmd == "SCRUB arena-scrub.sh"), "{calls:?}");
    assert!(said.iter().any(|l| l.contains("the scan found nothing")), "{said:?}");
    assert!(r.setup_took.is_some() && r.snapshot_took.is_some());
}

/// A secret the scrub didn't remove: no power-off, no snapshot — the files named — and the VM
/// deleted.
#[tokio::test(start_paused = true)]
async fn a_scan_hit_refuses_the_snapshot() {
    let cloud = FakeImageCloud::new(vec![]);
    let remote = FakeRemote::new();
    let out = "arena-image-commit: abc\narena-image-req: r\narena-scan: /root/notes.txt\narena-scan: /etc/x.key\narena-scrub: done (2 hit(s))\n";
    remote.on(HOST, "arena-scrub.sh", FakeReply::exit(3, "").with_stdout(out));
    let (r, _) = run(&cloud, &remote, never()).await;
    let e = r.result.unwrap_err();
    assert!(e.starts_with("refusing to snapshot: the scan found a secret in 2 file(s) after the scrub: /root/notes.txt, /etc/x.key"), "{e}");
    assert_eq!(mutations(&cloud), [create_call(), ImageCall::DeleteServer("1001".into())]);
    assert_eq!(r.vm, VmFate::Deleted { id: "1001".into() });
}

/// A scrub that didn't finish (or said nothing about the checkout) never gets a snapshot either.
#[tokio::test(start_paused = true)]
async fn a_scrub_that_fails_or_says_too_little_refuses_the_snapshot() {
    for (reply, want) in [
        (FakeReply::exit(1, "FAILED: cloud-init clean failed (the image would boot as this VM)"), "the scrub failed (exit 1): FAILED: cloud-init clean failed"),
        (FakeReply::stdout("arena-scrub: done (0 hit(s))\n"), "didn't say the checkout's commit"),
        (FakeReply::stdout("arena-image-commit: a\narena-image-req: b\n"), "the scrub failed (exit 0)"),
    ] {
        let cloud = FakeImageCloud::new(vec![]);
        let remote = FakeRemote::new();
        remote.on(HOST, "arena-scrub.sh", reply);
        let (r, _) = run(&cloud, &remote, never()).await;
        let e = r.result.unwrap_err();
        assert!(e.contains(want), "{e}");
        assert_eq!(mutations(&cloud), [create_call(), ImageCall::DeleteServer("1001".into())]);
    }
}

/// Setup failed: no scrub, no snapshot; the VM deleted.
#[tokio::test(start_paused = true)]
async fn a_failed_setup_deletes_the_vm() {
    let cloud = FakeImageCloud::new(vec![]);
    let remote = FakeRemote::new();
    remote.script(HOST, [FakeReply::ok(), FakeReply::exit(1, "FAILED: the repo's install_uv.sh failed")]);
    let (r, _) = run(&cloud, &remote, never()).await;
    let e = r.result.unwrap_err();
    assert!(e.starts_with("setup failed at hetzner setup script, exit 1: FAILED: the repo's install_uv.sh failed"), "{e}");
    assert_eq!(mutations(&cloud), [create_call(), ImageCall::DeleteServer("1001".into())]);
    assert!(!remote.calls_to(HOST).iter().any(|c| matches!(c, RemoteCall::Exec { cmd, .. } if cmd.contains("scrub"))));
}

/// The snapshot action failed: the half-made image goes, and the VM.
#[tokio::test(start_paused = true)]
async fn a_failed_snapshot_deletes_the_image_and_the_vm() {
    let cloud = FakeImageCloud::new(vec![]);
    *cloud.fail_snapshot.lock().unwrap() = true;
    let remote = FakeRemote::new();
    remote.on(HOST, "arena-scrub.sh", FakeReply::stdout(SCRUBBED));
    let (r, _) = run(&cloud, &remote, never()).await;
    assert!(r.result.unwrap_err().contains("disk error"));
    let m = mutations(&cloud);
    assert!(matches!(&m[..], [_, ImageCall::Stop(_), ImageCall::CreateImage { .. }, ImageCall::DeleteImage(1002), ImageCall::DeleteServer(s)] if s == "1001"), "{m:?}");
    assert!(cloud.snaps.lock().unwrap().is_empty() && cloud.servers.lock().unwrap().is_empty());
    assert_eq!(r.orphan_image, None);
}

/// Ctrl+C in the middle of setup: the setup is dropped, nothing is snapshotted, the VM goes.
#[tokio::test(start_paused = true)]
async fn ctrl_c_during_setup_deletes_the_vm() {
    let cloud = FakeImageCloud::new(vec![]);
    let remote = FakeRemote::new();
    remote.script(HOST, [FakeReply::ok(), FakeReply::hang()]);
    let (r, _) = run(&cloud, &remote, tokio::time::sleep(Duration::from_secs(300))).await;
    assert_eq!(r.result.unwrap_err(), "interrupted (Ctrl+C)");
    assert_eq!(mutations(&cloud), [create_call(), ImageCall::DeleteServer("1001".into())]);
    assert!(cloud.servers.lock().unwrap().is_empty());
}

/// Ctrl+C while the create is in flight: the server may exist though its id was never seen —
/// the one labelled with this bake's id is found and deleted; another bake's of the same name
/// stays.
#[tokio::test(start_paused = true)]
async fn ctrl_c_during_the_create_finds_the_vm_by_its_build_id() {
    let cloud = FakeImageCloud::new(vec![]);
    *cloud.create_delay.lock().unwrap() = Duration::from_secs(60);
    // What the cut-off POST made, and another bake's VM of the same name, which stays.
    let vm = |id: &str| Pod { id: id.into(), name: "devtest-image-261029-1542".into(), ..Default::default() };
    cloud.add_server(vm("77"), json!({"arena-image-build": "devtest", "arena-image-build-id": "b1"}));
    cloud.add_server(vm("78"), json!({"arena-image-build": "devtest", "arena-image-build-id": "b0"}));
    let remote = FakeRemote::new();
    let (r, _) = run(&cloud, &remote, tokio::time::sleep(Duration::from_secs(5))).await;
    assert_eq!(r.result.unwrap_err(), "interrupted (Ctrl+C)");
    assert_eq!(r.vm, VmFate::Deleted { id: "77".into() });
    assert!(cloud.calls().contains(&ImageCall::Servers("arena-image-build-id=b1".into())));
    assert_eq!(cloud.servers.lock().unwrap().iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["78"]);
}

/// A create that failed: whether Hetzner made the server is looked up by this bake's id — a
/// refusal made none (and a namesake, another bake's, is never touched); an answer lost after
/// Hetzner took the request did, and that server is deleted.
#[tokio::test(start_paused = true)]
async fn a_failed_create_deletes_only_what_this_bake_made() {
    let cloud = FakeImageCloud::new(vec![]);
    *cloud.fail_create.lock().unwrap() = Some("hetzner create HTTP 409: uniqueness_error".into());
    cloud.add_server(Pod { id: "77".into(), name: "devtest-image-261029-1542".into(), ..Default::default() }, json!({"arena-image-build-id": "b0"}));
    let remote = FakeRemote::new();
    let (r, _) = run(&cloud, &remote, never()).await;
    assert!(r.result.unwrap_err().starts_with("creating the VM: "));
    assert_eq!(r.vm, VmFate::NeverMade);
    assert_eq!(mutations(&cloud), [create_call()]);
    let looks = cloud.calls().iter().filter(|c| matches!(c, ImageCall::Servers(s) if s == "arena-image-build-id=b1")).count();
    assert_eq!(looks, crate::image::build::DELETE_TRIES as usize, "looked for a while");
    assert!(remote.calls().is_empty());

    let cloud = FakeImageCloud::new(vec![]);
    *cloud.create_answer_lost.lock().unwrap() = true;
    let (r, _) = run(&cloud, &remote, never()).await;
    assert!(r.result.unwrap_err().contains("connection reset"));
    assert_eq!(r.vm, VmFate::Deleted { id: "1001".into() });
    assert!(cloud.servers.lock().unwrap().is_empty());
}

/// The snapshot's answer lost (or Ctrl+C before it came): this bake's snapshot is found by its
/// id label and deleted with the VM — never another image.
#[tokio::test(start_paused = true)]
async fn a_snapshot_whose_answer_was_lost_is_found_and_deleted() {
    let cloud = FakeImageCloud::new(vec![]);
    *cloud.snapshot_answer_lost.lock().unwrap() = true;
    let other = crate::image::parse_snapshot(&json!({"id": 5, "status": "available", "labels": {"arena-image": "1"}})).unwrap();
    cloud.snaps.lock().unwrap().push(other);
    let remote = FakeRemote::new();
    remote.on(HOST, "arena-scrub.sh", FakeReply::stdout(SCRUBBED));
    let (r, _) = run(&cloud, &remote, never()).await;
    assert!(r.result.unwrap_err().starts_with("snapshotting: "));
    assert!(cloud.calls().contains(&ImageCall::Snapshots(Some("arena-image-build-id=b1".into()))));
    assert_eq!(cloud.snaps.lock().unwrap().iter().map(|s| s.id).collect::<Vec<_>>(), [5]);
    assert_eq!(r.vm, VmFate::Deleted { id: "1001".into() });
}

/// A delete that keeps failing is retried, then reported as left behind (the caller shouts);
/// one that fails once and then works is just deleted.
#[tokio::test(start_paused = true)]
async fn a_delete_that_fails_is_retried_then_reported() {
    for (fails, want_left) in [(1, false), (10, true)] {
        let cloud = FakeImageCloud::new(vec![]);
        *cloud.fail_server_deletes.lock().unwrap() = fails;
        let remote = FakeRemote::new();
        remote.on(HOST, "arena-scrub.sh", FakeReply::stdout(SCRUBBED));
        let (r, _) = run(&cloud, &remote, never()).await;
        assert!(r.result.is_ok(), "the image is made either way");
        let deletes = cloud.calls().iter().filter(|c| matches!(c, ImageCall::DeleteServer(_))).count();
        if want_left {
            assert_eq!(deletes, crate::image::build::DELETE_TRIES as usize);
            assert!(matches!(&r.vm, VmFate::Left { id, why } if id == "1001" && why.contains("503")), "{:?}", r.vm);
        } else {
            assert_eq!(deletes, 2);
            assert_eq!(r.vm, VmFate::Deleted { id: "1001".into() });
        }
    }
}
