//! Golden tests of `image list`, `image prune` and `image build`'s output (docs/OUTPUT.md), at
//! default and at `-v`, against a fake Hetzner project (`FakeImageCloud`) and, for the bake, a
//! fake VM (`FakeRemote`), on a paused clock at a fixed time. `-v` is what they printed before.

use std::sync::Arc;

use arena_core::image::cloud::FakeImageCloud;
use arena_core::image::{self, Snapshot};
use arena_core::provider::hetzner::ServerType;
use arena_core::remote::{FakeRemote, FakeReply};
use arena_core::Config;
use serde_json::json;

use super::{build, list, prune, BuildHow};
use crate::output_test::rendered;
use crate::Interrupt;

/// 2026-10-29 17:18 UTC.
const NOW: u64 = 1_793_294_280;
const DAY: u64 = 86_400;

fn cfg(extra: &str) -> Config {
    Config::parse(&format!(
        "MACHINE_NAME_PREFIX=devtest\nARENA_REPO_OWNER=o\nARENA_REPO_NAME=r\nGIT_SSH_KEY_LOCAL=/nonexistent/devtest_deploy_key\n\
         HETZNER_SERVER_TYPE=cx23\nAGENT_NOTES=0\nVSCODE_PREINSTALL=0\n{extra}"
    ))
}

fn types() -> Vec<ServerType> {
    let ty = |name: &str, mem: f64, disk: u32, eur: f64| ServerType {
        name: name.into(),
        architecture: Some("x86".into()),
        memory_gb: Some(mem),
        disk_gb: Some(disk),
        eur_per_hour: Some(eur),
        ..Default::default()
    };
    vec![ty("cx23", 4.0, 40, 0.0104), ty("cx33", 8.0, 80, 0.0168)]
}

/// One of ours, built `age` ago with requirements `req`.
fn ours(cloud: &FakeImageCloud, id: u64, age: u64, req: &str) {
    let labels = json!({
        "arena-image": "1", "arena-prefix": "devtest", "repo": "o.r", "repo-id": image::repo_id("o", "r"),
        "built": (NOW - age).to_string(), "commit": "3eed962b9bd8", "req": req
    });
    let snap: Snapshot = image::parse_snapshot(&json!({
        "id": id, "description": format!("devtest-image-{id}"), "status": "available", "image_size": 5.2,
        "disk_size": 40, "architecture": "x86", "created": "2026-10-26T11:43:43Z", "labels": labels.clone()
    }))
    .unwrap();
    let serde_json::Value::Object(m) = labels else { unreachable!() };
    cloud.snap_labels.lock().unwrap().insert(id, m);
    cloud.snaps.lock().unwrap().push(snap);
}

/// The repo's requirements, as `repo_requirements` reads them.
fn repo(req: &str) -> Result<(String, String), String> {
    Ok(("4bd1e0f2a9c3".into(), req.into()))
}

/// `run` at default and at `-v`, each on a fresh project (`cloud()`), the prompt answered `y`.
async fn both<F, Fut>(cloud: impl Fn() -> FakeImageCloud, run: F) -> (String, String)
where
    F: Fn(Arc<FakeImageCloud>) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let mut out = Vec::new();
    for verbose in [false, true] {
        let c = Arc::new(cloud());
        out.push(rendered(verbose, &["y"], run(c)).await);
    }
    let loud = out.pop().unwrap();
    (out.pop().unwrap(), loud)
}

/// In use and current: the table and the cost; the resolution the table marks is `-v`'s.
#[tokio::test(start_paused = true)]
async fn list_in_use() {
    let cloud = || {
        let c = FakeImageCloud::new(types());
        ours(&c, 449623170, 3 * DAY, "6f6144f01222");
        c
    };
    let cfg = &cfg("HETZNER_IMAGE=arena\n");
    let (quiet, loud) = both(cloud, |c| async move { list(c.as_ref(), cfg, NOW, repo("6f6144f01222")).await }).await;
    assert_eq!(quiet, QUIET_LIST_IN_USE.strip_prefix('\n').unwrap(), "default:\n{quiet}");
    assert_eq!(loud, LOUD_LIST_IN_USE.strip_prefix('\n').unwrap(), "-v:\n{loud}");
}

/// Not in use (`HETZNER_IMAGE` another image), the image old and its requirements changed: one
/// line says it's not in use, each note shows.
#[tokio::test(start_paused = true)]
async fn list_not_in_use_with_notes() {
    let cloud = || {
        let c = FakeImageCloud::new(types());
        ours(&c, 449623170, 20 * DAY, "6f6144f01222");
        c
    };
    let cfg = &cfg("");
    let (quiet, loud) = both(cloud, |c| async move { list(c.as_ref(), cfg, NOW, repo("a40bc7471764")).await }).await;
    assert_eq!(quiet, QUIET_LIST_NOT_IN_USE.strip_prefix('\n').unwrap(), "default:\n{quiet}");
    assert_eq!(loud, LOUD_LIST_NOT_IN_USE.strip_prefix('\n').unwrap(), "-v:\n{loud}");
}

/// Two images, the newer kept: what goes, a question with what it saves, what went.
#[tokio::test(start_paused = true)]
async fn prune_one() {
    let cloud = || {
        let c = FakeImageCloud::new(types());
        ours(&c, 449600001, 9 * DAY, "6f6144f01222");
        ours(&c, 449623170, 3 * DAY, "6f6144f01222");
        c
    };
    let cfg = &cfg("HETZNER_IMAGE=arena\n");
    let (quiet, loud) = both(cloud, |c| async move { prune(c.as_ref(), cfg, 1, false, false, NOW).await }).await;
    assert_eq!(quiet, QUIET_PRUNE.strip_prefix('\n').unwrap(), "default:\n{quiet}");
    assert_eq!(loud, LOUD_PRUNE.strip_prefix('\n').unwrap(), "-v:\n{loud}");
}

/// The dry run: one line with what it makes and costs; the plan is `-v`'s.
#[tokio::test(start_paused = true)]
async fn build_dry_run() {
    let cfg = cfg("");
    let how = || BuildHow { server_type: None, dry_run: true, yes: false, now: NOW };
    let remote = FakeRemote::new();
    let (quiet, loud) = both(|| FakeImageCloud::new(types()), |c| {
        let (cfg, remote) = (&cfg, &remote);
        async move { build(c.as_ref(), remote, cfg, how(), Interrupt::manual()).await }
    })
    .await;
    assert_eq!(quiet, QUIET_BUILD_DRY.strip_prefix('\n').unwrap(), "default:\n{quiet}");
    assert_eq!(loud, LOUD_BUILD_DRY.strip_prefix('\n').unwrap(), "-v:\n{loud}");
}

/// A bake: the question, a line as each long step starts, the image; how to use it.
#[tokio::test(start_paused = true)]
async fn build_an_image() {
    const SCRUBBED: &str = "arena-image-commit: 3eed962b9bd8\narena-image-req: a40bc7471764\narena-scrub: done (0 hit(s))\n";
    let cfg = cfg("");
    let how = || BuildHow { server_type: None, dry_run: false, yes: false, now: NOW };
    let remote = || {
        let r = FakeRemote::new();
        // The first server the fake makes is id 1001 at 10.0.0.1.
        r.on("10.0.0.1:22", "arena-scrub.sh", FakeReply::stdout(SCRUBBED));
        r
    };
    let (quiet, loud) = both(|| FakeImageCloud::new(types()), |c| {
        let cfg = &cfg;
        let remote = remote();
        async move { build(c.as_ref(), &remote, cfg, how(), Interrupt::manual()).await }
    })
    .await;
    assert_eq!(quiet, QUIET_BUILD.strip_prefix('\n').unwrap(), "default:\n{quiet}");
    assert_eq!(loud, LOUD_BUILD.strip_prefix('\n').unwrap(), "-v:\n{loud}");
}

const QUIET_LIST_IN_USE: &str = r#"
ID         BUILT (UTC)       AGE    SIZE  €/MONTH  MIN DISK  COMMIT        REQ
449623170  2026-10-26 17:18   3d  5.2 GB    €0.06     40 GB  3eed962b9bd8  same  ← arena

1 image(s), 5.2 GB stored, ≈ €0.06/month.
"#;

const LOUD_LIST_IN_USE: &str = r#"
ID         BUILT (UTC)       AGE    SIZE  €/MONTH  MIN DISK  COMMIT        REQ
449623170  2026-10-26 17:18   3d  5.2 GB    €0.06     40 GB  3eed962b9bd8  same  ← arena

1 image(s), 5.2 GB stored, ≈ €0.06/month.
The repo now: commit 4bd1e0f2a9c3, requirements 6f6144f01222.
For cx23 (HETZNER_IMAGE=arena):
  image arena → snapshot 449623170 (devtest-image-449623170, built 2026-10-26, 3d old, commit 3eed962b9bd8)
"#;

const QUIET_LIST_NOT_IN_USE: &str = r#"
ID         BUILT (UTC)       AGE    SIZE  €/MONTH  MIN DISK  COMMIT        REQ
449623170  2026-10-09 17:18  20d  5.2 GB    €0.06     40 GB  3eed962b9bd8  differ  ← arena

1 image(s), 5.2 GB stored, ≈ €0.06/month.
Not in use: HETZNER_IMAGE=ubuntu-24.04 (set it to arena for the newest that fits)
⚠ built 20 days ago (over 14): VMs from it fast-forward the course repo at setup — `arena image build` makes a current one
⚠ the repo's requirements (install_uv.sh / requirements.txt) changed since it was built: every VM from it reinstalls the env at setup (~10 min) — `arena image build` makes a current one
"#;

const LOUD_LIST_NOT_IN_USE: &str = r#"
ID         BUILT (UTC)       AGE    SIZE  €/MONTH  MIN DISK  COMMIT        REQ
449623170  2026-10-09 17:18  20d  5.2 GB    €0.06     40 GB  3eed962b9bd8  differ  ← arena

1 image(s), 5.2 GB stored, ≈ €0.06/month.
The repo now: commit 4bd1e0f2a9c3, requirements a40bc7471764.
For cx23 (HETZNER_IMAGE=ubuntu-24.04 (set to arena to use these)):
  image arena → snapshot 449623170 (devtest-image-449623170, built 2026-10-09, 20d old, commit 3eed962b9bd8)
  ⚠ built 20 days ago (over 14): VMs from it fast-forward the course repo at setup — `arena image build` makes a current one
  ⚠ the repo's requirements (install_uv.sh / requirements.txt) changed since it was built: every VM from it reinstalls the env at setup (~10 min) — `arena image build` makes a current one
"#;

const QUIET_PRUNE: &str = r#"
  delete  449600001 (devtest-image-449600001, built 2026-10-20, 9d old, commit 3eed962b9bd8, €0.06/month)
Delete 1 image(s) (saves ≈ €0.06/month)? [y/N] y
✓ deleted image 449600001
"#;

const LOUD_PRUNE: &str = r#"
  keep    449623170 (devtest-image-449623170, built 2026-10-26, 3d old, commit 3eed962b9bd8)
  delete  449600001 (devtest-image-449600001, built 2026-10-20, 9d old, commit 3eed962b9bd8, €0.06/month)
Delete 1 image(s) and 0 build VM(s)?
Proceed? [y/N] y
✓ deleted image 449600001
"#;

const QUIET_BUILD_DRY: &str = r#"
Would bake devtest-image-261029-1718 on a cx23 VM (~20 min of €0.0104/h, deleted after; the image ≈ €0.07/month)
"#;

const LOUD_BUILD_DRY: &str = r#"
Bake an arena image for o/r (prefix devtest):
  VM        devtest-image-261029-1718: cx23 (x86, 40 GB disk, €0.0104/h), from ubuntu-24.04, deleted at the end
  setup     copy deploy key → authorize keys → hostname → copy hetzner setup script → repo update → hetzner setup script
  scrub     deploy key, authorized keys, tokens, history, ~/.name, ~/.arena, swap, machine-id, SSH host keys, cloud-init, /tmp, logs
  scan      the homes and /etc for the backup's secret patterns and private keys — any hit: no snapshot
  snapshot  labelled arena-image, built, repo, commit, req (requirements hash), arch, disk; boots on 40 GB disks and up: cx23, cx33
  cost      VM ≈ 20 min of €0.0104/h; the image ≈ 6 GB × €0.0119/GB/month ≈ €0.07/month
Dry run — nothing created.
"#;

const QUIET_BUILD: &str = r#"
Bake devtest-image-261029-1718 on a cx23 VM (~20 min of €0.0104/h, deleted after; the image ≈ €0.07/month)? [y/N] y
creating devtest-image-261029-1718 (cx23, ubuntu-24.04)…
setting up devtest-image-261029-1718 (10.0.0.1) — `pods setup`'s steps, no tokens (~10 min)…
scrubbing it (keys, tokens, history, name, swap, identity, logs) and scanning for secrets…
✓ scrubbed; the scan found nothing (commit 3eed962b9bd8, requirements a40bc7471764); powering off…
snapshotting the disk (a few minutes)…
✓ deleted the build VM devtest-image-261029-1718 (id 1001)
✓ image 1002 (devtest-image-261029-1718): 5.8 GB, ≈ €0.07/month, took 0m00s
  Use it: HETZNER_IMAGE=arena (or --image arena on pods create/up)
"#;

const LOUD_BUILD: &str = r#"
Bake an arena image for o/r (prefix devtest):
  VM        devtest-image-261029-1718: cx23 (x86, 40 GB disk, €0.0104/h), from ubuntu-24.04, deleted at the end
  setup     copy deploy key → authorize keys → hostname → copy hetzner setup script → repo update → hetzner setup script
  scrub     deploy key, authorized keys, tokens, history, ~/.name, ~/.arena, swap, machine-id, SSH host keys, cloud-init, /tmp, logs
  scan      the homes and /etc for the backup's secret patterns and private keys — any hit: no snapshot
  snapshot  labelled arena-image, built, repo, commit, req (requirements hash), arch, disk; boots on 40 GB disks and up: cx23, cx33
  cost      VM ≈ 20 min of €0.0104/h; the image ≈ 6 GB × €0.0119/GB/month ≈ €0.07/month
Create the VM and bake the image?
Proceed? [y/N] y
creating devtest-image-261029-1718 (cx23, ubuntu-24.04)…
setting up devtest-image-261029-1718 (10.0.0.1) — `pods setup`'s steps, no tokens (~10 min)…
scrubbing it (keys, tokens, history, name, swap, identity, logs) and scanning for secrets…
✓ scrubbed; the scan found nothing (commit 3eed962b9bd8, requirements a40bc7471764); powering off…
snapshotting the disk (a few minutes)…
✓ deleted the build VM devtest-image-261029-1718 (id 1001)

✓ image 1002 (devtest-image-261029-1718): 5.8 GB, ≈ €0.07/month, boots on 40 GB disks and up. Took 0m00s (setup 0m00s, snapshot 0m00s).
  Use it: HETZNER_IMAGE=arena in the config (the newest that fits), or `--image arena` on pods create/up.
"#;
