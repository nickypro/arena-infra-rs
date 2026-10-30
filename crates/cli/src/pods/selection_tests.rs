use std::sync::{Arc, Mutex};

use arena_core::remote::FakeRemote;
use arena_core::selector::SelectArgs;
use arena_core::{Config, Pod, PodSpec, Provider, Result};
use async_trait::async_trait;
use clap::Parser;

use crate::cli::{Cli, Cmd};
use crate::keys::{handle_keys, keys_preview, keys_targets, keys_with, KeysAction, KeysCmd};
use crate::pods::run::{misplaced_run_flags, pod_name_as_command};
use crate::pods::selection::{select, Unscoped};
use super::lifecycle_tests::FakeKeys;
use super::{handle_pods, PodCmd};

/// A fleet that records `(op, id)` for every mutation, and counts list calls.
struct FakeProvider {
    pods: Vec<Pod>,
    calls: Mutex<Vec<(&'static str, String)>>,
    lists: Mutex<usize>,
    /// GPU counts `enrich` fills in (by pod name) — the details a bare list omits.
    enriched: Vec<(&'static str, u32)>,
}

impl FakeProvider {
    fn calls(&self) -> Vec<(&'static str, String)> {
        self.calls.lock().unwrap().clone()
    }
    fn record(&self, op: &'static str, id: &str) {
        self.calls.lock().unwrap().push((op, id.to_string()));
    }
}

#[async_trait]
impl Provider for FakeProvider {
    fn name(&self) -> &'static str {
        "runpod"
    }
    fn describe(&self, _spec: &PodSpec) -> String {
        String::new()
    }
    async fn list_pods(&self) -> Result<Vec<Pod>> {
        *self.lists.lock().unwrap() += 1;
        Ok(self.pods.clone())
    }
    async fn enrich(&self, pods: &mut [Pod]) -> Result<()> {
        for p in pods.iter_mut() {
            if let Some((_, n)) = self.enriched.iter().find(|(name, _)| *name == p.name) {
                p.gpu_count = Some(*n);
            }
        }
        Ok(())
    }
    async fn create_pod(&self, _spec: &PodSpec) -> Result<Pod> {
        unimplemented!("not exercised by selection tests")
    }
    async fn stop_pod(&self, id: &str) -> Result<()> {
        self.record("stop", id);
        Ok(())
    }
    async fn restart_pod(&self, id: &str) -> Result<()> {
        self.record("restart", id);
        Ok(())
    }
    async fn terminate_pod(&self, id: &str) -> Result<()> {
        self.record("terminate", id);
        Ok(())
    }
    async fn pod_spec(&self, _id: &str) -> Result<PodSpec> {
        let mut spec = PodSpec::from_config(&Config::parse(""));
        spec.env = vec![("KEEP".into(), "1".into())];
        Ok(spec)
    }
    async fn reimage_pod(&self, id: &str, _image: &str, _env: &[(String, String)]) -> Result<()> {
        self.record("reimage", id);
        Ok(())
    }
    fn restart_wipes_container_disk(&self, pod: &Pod) -> bool {
        pod.provider != "hetzner" // as the fleet provider routes it
    }
    // REVIEW sf SF3: as the fleet routes it — the capability table's answer for the pod's
    // provider (a Shadeform pod has no stop; no pod in the existing fixtures is on Shadeform, so
    // their tests are unchanged).
    fn supports(&self, op: arena_core::capability::Op, pod: Option<&Pod>) -> arena_core::capability::Support {
        use arena_core::capability::{support, Backend, Support};
        pod.and_then(|p| Backend::named(&p.provider)).map_or(Support::Yes, |b| support(b, op))
    }
}

fn pod(name: &str, status: &str, provider: &str, gpus: Option<u32>) -> Pod {
    Pod {
        id: format!("id-{name}"),
        name: name.to_string(),
        provider: provider.into(),
        status: status.to_string(),
        gpu_count: gpus,
        ..Default::default()
    }
}

/// apple (1 GPU), bloom (2), cloud (count unknown until enriched), zebra (stopped), and
/// a hetzner VM — listed out of order.
fn fleet() -> FakeProvider {
    FakeProvider {
        pods: vec![
            pod("arena8-zebra", "EXITED", "runpod", Some(1)),
            pod("arena8-bloom", "RUNNING", "runpod", Some(2)),
            pod("arena8-apple", "RUNNING", "runpod", Some(1)),
            pod("arena8-cloud", "RUNNING", "runpod", None),
            pod("arena8-vm", "running", "hetzner", None),
        ],
        calls: Mutex::new(Vec::new()),
        lists: Mutex::new(0),
        enriched: vec![("arena8-cloud", 2)],
    }
}

fn cfg() -> Config {
    Config::parse("MACHINE_NAME_PREFIX=arena8\nMACHINE_NAME_LIST=(apple bloom cloud mayor vm zebra)\n")
}

fn args(targets: &[&str]) -> SelectArgs {
    SelectArgs::exact(targets.iter().copied())
}

async fn names(f: &FakeProvider, a: &SelectArgs) -> std::result::Result<Vec<String>, String> {
    select(f, &cfg(), a, Unscoped::All)
        .await
        .map(|s| s.pods.into_iter().map(|p| p.name).collect())
        .map_err(|e| e.to_string())
}

fn parse(argv: &[&str]) -> std::result::Result<Cmd, clap::Error> {
    Cli::try_parse_from(["arena"].iter().chain(argv).copied()).map(|c| c.cmd)
}

fn pods_cmd(argv: &[&str]) -> PodCmd {
    match parse(&[&["pods"][..], argv].concat()).unwrap_or_else(|e| panic!("{argv:?}: {e}")) {
        Cmd::Pods(p) => p,
        _ => unreachable!(),
    }
}

/// Run `pods <argv…>` against a fresh fleet; the recorded mutations, or the error.
async fn run_pods(argv: &[&str]) -> (FakeProvider, std::result::Result<(), String>) {
    let f = fleet();
    let r = handle_pods(pods_cmd(argv), &f, Arc::new(FakeRemote::new()), &cfg(), true).await.map_err(|e| e.to_string());
    (f, r)
}

fn ids(names: &[&str]) -> Vec<String> {
    names.iter().map(|n| format!("id-arena8-{n}")).collect()
}

/// The command lines the dashboard runs as children (arena-tui's `runs.rs`: its menu's
/// start/rename/jobs/logs, the add-pod form's `up` and `offers`), which it builds without
/// this parser, parse as it means them: the pod a target and never a JOB (review ai-review
/// codex #3), `--yes` global, every flag of the form's there.
#[test]
fn the_dashboards_command_lines_parse_as_it_means_them() {
    let full = |argv: &[&str]| {
        Cli::try_parse_from(["arena", "--config", "/c.env", "--provider", "hetzner"].iter().chain(argv).copied()).unwrap_or_else(|e| panic!("{argv:?}: {e}"))
    };
    let pod = vec!["devtest-alpha".to_string()];
    let Cmd::Pods(PodCmd::Logs { tail, follow, sel }) = full(&["pods", "logs", "devtest-alpha", "--tail", "100"]).cmd else { panic!() };
    assert_eq!((tail, follow, arena_core::jobs::split_job_arg(&sel.targets).unwrap()), (100, false, (pod.clone(), None)));
    let Cmd::Pods(PodCmd::Jobs { kill, sel }) = full(&["pods", "jobs", "devtest-alpha"]).cmd else { panic!() };
    assert_eq!((kill, arena_core::jobs::split_job_arg(&sel.targets).unwrap()), (None, (pod.clone(), None)));
    let c = full(&["pods", "start", "devtest-alpha", "--yes"]);
    assert!(c.yes && matches!(&c.cmd, Cmd::Pods(PodCmd::Start { sel, no_setup: false, dry_run: false, .. }) if sel.targets == pod));
    let c = full(&["pods", "rename", "devtest-alpha", "bravo", "--yes"]);
    let Cmd::Pods(PodCmd::Rename { target, new_name, dry_run: false, .. }) = c.cmd else { panic!() };
    assert!(c.yes && target.as_deref() == Some("devtest-alpha") && new_name.as_deref() == Some("bravo"));
    let c = full(&["pods", "up", "--add", "2", "--server-type", "cx23", "--yes"]);
    assert!(c.yes && matches!(&c.cmd, Cmd::Pods(PodCmd::Up { add: Some(2), server_type: Some(t), dry_run: false, .. }) if t == "cx23"));
    let up = ["pods", "up", "--add", "1", "--gpu", "NVIDIA RTX A4000", "--gpus", "2", "--cloud", "COMMUNITY,SECURE", "--disk", "100", "--volume", "0", "--check", "--yes"];
    let Cmd::Pods(PodCmd::Up { gpu, gpus, cloud, disk, volume, check, retry_mins: 0, keep_trying: false, .. }) = full(&up).cmd else { panic!() };
    assert_eq!((gpu.as_deref(), gpus, cloud.as_deref(), disk, volume, check), (Some("NVIDIA RTX A4000"), Some(2), Some("COMMUNITY,SECURE"), Some(100), Some(0), true));
    // The form's retry field: `--retry-mins N` before `--yes`, on any provider; rounds a
    // minute apart (`--retry-secs`' default), never `--keep-trying`.
    for mins in ["10", "30", "120", "1440"] {
        let c = full(&["pods", "up", "--add", "1", "--server-type", "cx23", "--retry-mins", mins, "--yes"]);
        let Cmd::Pods(PodCmd::Up { retry_mins, retry_secs: 60, keep_trying: false, .. }) = c.cmd else { panic!("{mins}") };
        assert!(c.yes && retry_mins.to_string() == mins, "{mins}");
        let gpu = ["pods", "up", "--add", "2", "--gpu", "NVIDIA RTX A4000", "--gpus", "1", "--cloud", "COMMUNITY", "--disk", "100", "--volume", "0", "--retry-mins", mins, "--yes"];
        let Cmd::Pods(PodCmd::Up { retry_mins, add: Some(2), .. }) = full(&gpu).cmd else { panic!("{mins}") };
        assert_eq!(retry_mins.to_string(), mins);
    }
    let Cmd::Offers { gpu, cloud, gpus, server_type, json: true, .. } = full(&["offers", "--json", "--gpu", "NVIDIA RTX A4000", "--cloud", "SECURE", "--gpus", "1"]).cmd else { panic!() };
    assert_eq!((gpu.as_deref(), cloud.as_deref(), gpus, server_type), (Some("NVIDIA RTX A4000"), Some("SECURE"), Some(1), None));
    let Cmd::Offers { server_type, json: true, .. } = full(&["offers", "--json", "--server-type", "cx33"]).cmd else { panic!() };
    assert_eq!(server_type.as_deref(), Some("cx33"));
    // The type field's catalogue: `offers <provider> --json`.
    for p in ["runpod", "vast", "hetzner"] {
        let Cmd::Offers { view, json: true, .. } = full(&["offers", p, "--json"]).cmd else { panic!() };
        assert_eq!(view.as_deref(), Some(p));
    }
}

#[tokio::test]
async fn the_resolver_picks_in_list_order_and_fails_loudly_on_typos() {
    let f = fleet();
    assert_eq!(names(&f, &SelectArgs::default()).await.unwrap(), ["arena8-apple", "arena8-bloom", "arena8-cloud", "arena8-vm", "arena8-zebra"]);
    assert_eq!(names(&f, &args(&["bloom", "arena8-apple"])).await.unwrap(), ["arena8-apple", "arena8-bloom"]);
    assert_eq!(names(&f, &args(&["apple..cloud"])).await.unwrap(), ["arena8-apple", "arena8-bloom", "arena8-cloud"]);
    assert_eq!(names(&f, &SelectArgs { on: Some("hetzner".into()), ..Default::default() }).await.unwrap(), ["arena8-vm"]);
    // The regression guard, now for every token: a typo (include OR exclude) is an
    // error naming it and the closest pod — never a quietly smaller (or larger) set.
    let e = names(&f, &args(&["arena8-zebrra"])).await.unwrap_err();
    assert!(e.contains("`arena8-zebrra`: no pod has that name or id — did you mean arena8-zebra?"), "{e}");
    let e = names(&f, &SelectArgs { exclude: vec!["aple".into()], ..Default::default() }).await.unwrap_err();
    assert!(e.contains("--exclude `aple`: no pod has that name or id — did you mean arena8-apple?"), "{e}");
    // A list name with no pod, and a narrowed-to-nothing selection, are errors too.
    assert!(names(&f, &args(&["mayor"])).await.unwrap_err().contains("no pod named arena8-mayor right now"));
    let e = names(&f, &SelectArgs { on: Some("vast".into()), ..Default::default() }).await.unwrap_err();
    assert!(e.starts_with("nothing selected: all pods → 5, --on vast → 0"), "{e}");
}

#[tokio::test]
async fn gpus_fetches_the_details_a_listing_omits() {
    // cloud's list entry has no GPU count; `--gpus` enriches first, so it's found.
    let f = fleet();
    let two = SelectArgs { gpus: Some(2), ..Default::default() };
    assert_eq!(names(&f, &two).await.unwrap(), ["arena8-bloom", "arena8-cloud"]);
    // Still unknown after enriching (the VM): never matches.
    let none = SelectArgs { gpus: Some(0), on: Some("hetzner".into()), ..Default::default() };
    assert!(names(&f, &none).await.unwrap_err().contains("(1 with an unknown GPU count)"));
}

#[tokio::test]
async fn a_command_that_changes_pods_needs_names_or_all_before_listing() {
    let f = fleet();
    for a in [SelectArgs::default(), SelectArgs { gpus: Some(1), ..Default::default() }] {
        let e = select(&f, &cfg(), &a, Unscoped::Refuse("stop")).await.unwrap_err().to_string();
        assert_eq!(e, "name the pods to stop (names, ids or a range like apple..mayor), or pass --all");
    }
    // A malformed range fails before listing too.
    assert!(select(&f, &cfg(), &args(&["cloud..apple"]), Unscoped::All).await.is_err());
    assert_eq!(*f.lists.lock().unwrap(), 0, "nothing was listed");
}

/// REVIEW sf SF3 (money): one Shadeform pod anywhere in a `pods stop --all` selection makes
/// the whole stop bail up front (`stop_refusal` over `sel.pods`) — so the routine nightly
/// `pods stop --all --wipe-ok` (balance.rs counts on it) stops *nothing*: every RunPod pod
/// keeps billing all night because an overflow VM exists. The refusal also counts pods that
/// aren't stop targets at all: a Shadeform instance already `deleting` (listed TERMINATED,
/// billing nothing) blocks the batch the same way, unlike every other non-billing pod,
/// which is "skip … — not running". A pod that can't be stopped must be skipped and said
/// (and may fail the exit), not veto the rest of `--all`.
#[tokio::test]
async fn review_sf_sf3_a_shadeform_pod_does_not_veto_stop_all() {
    let mut vetoed = Vec::new();
    for status in ["RUNNING", "TERMINATED"] {
        let mut f = fleet();
        f.pods.push(pod("arena8-mayor", status, "shadeform", Some(1)));
        let r = handle_pods(pods_cmd(&["stop", "--all", "--wipe-ok"]), &f, Arc::new(FakeRemote::new()), &cfg(), true)
            .await
            .map_err(|e| e.to_string());
        let got: Vec<String> = f.calls().into_iter().map(|(op, id)| format!("{op} {id}")).collect();
        let want: Vec<String> = ids(&["apple", "bloom", "cloud", "vm"]).into_iter().map(|id| format!("stop {id}")).collect();
        if got != want {
            vetoed.push(format!("Shadeform pod {status}: stopped {got:?}, not {want:?} ({r:?})"));
        }
    }
    assert!(vetoed.is_empty(), "a Shadeform pod vetoed `pods stop --all`:\n{}", vetoed.join("\n"));
}

/// SF3, the rest of it: `stop --all` with a billing Shadeform pod stops every other pod, says
/// the Shadeform one is skipped, and exits non-zero (it still bills); an explicit stop of
/// only that pod is refused with nothing stopped; a `deleting` one (TERMINATED) is just "not
/// running" — exit 0.
#[tokio::test]
async fn sf3_stop_skips_shadeform_pods_exits_non_zero_and_refuses_them_alone() {
    let mut f = fleet();
    f.pods.push(pod("arena8-mayor", "RUNNING", "shadeform", Some(1)));
    let r = handle_pods(pods_cmd(&["stop", "--all", "--wipe-ok"]), &f, Arc::new(FakeRemote::new()), &cfg(), true).await;
    let e = r.unwrap_err().to_string();
    assert!(e.contains("1 pod(s) still billing") && e.contains("arena8-mayor") && e.contains("no stop"), "{e}");
    assert_eq!(f.calls().len(), 4, "every other running pod stopped: {:?}", f.calls());
    // Only the Shadeform pod: refused, nothing stopped.
    let mut f = fleet();
    f.pods.push(pod("arena8-mayor", "RUNNING", "shadeform", Some(1)));
    let e = handle_pods(pods_cmd(&["stop", "mayor"]), &f, Arc::new(FakeRemote::new()), &cfg(), true).await.unwrap_err().to_string();
    assert!(e.starts_with("can't stop arena8-mayor — Shadeform has no stop"), "{e}");
    assert!(f.calls().is_empty());
    // Already deleting: not a stop target — the rest stop and the exit is 0.
    let mut f = fleet();
    f.pods.push(pod("arena8-mayor", "TERMINATED", "shadeform", Some(1)));
    handle_pods(pods_cmd(&["stop", "--all", "--wipe-ok"]), &f, Arc::new(FakeRemote::new()), &cfg(), true).await.unwrap();
    assert_eq!(f.calls().len(), 4);
    // `start` of only the Shadeform pod: refused the same way, nothing sent.
    let e = handle_pods(pods_cmd(&["start", "mayor"]), &f, Arc::new(FakeRemote::new()), &cfg(), true).await.unwrap_err().to_string();
    assert!(e.starts_with("can't start arena8-mayor — Shadeform has no stop"), "{e}");
}

#[tokio::test]
async fn stop_stops_exactly_the_selected_running_pods() {
    // The fleet's RunPod pods have no volume (the fake's spec), so stopping them needs
    // --wipe-ok (a stopped RunPod pod keeps no data); the Hetzner VM keeps its disk.
    let cases: &[(&[&str], &[&str])] = &[
        (&["stop", "bloom", "apple", "--wipe-ok"], &["apple", "bloom"]),
        (&["stop", "apple..cloud", "--exclude", "bloom", "--wipe-ok"], &["apple", "cloud"]),
        // zebra is EXITED: skipped (said, not stopped); the VM is `running` (any case).
        (&["stop", "--all", "--wipe-ok"], &["apple", "bloom", "cloud", "vm"]),
        (&["stop", "--all", "--on", "runpod", "--gpus", "2", "--wipe-ok"], &["bloom", "cloud"]),
        // the old spellings: one positional; `--all` narrowed by --include/--exclude
        (&["stop", "arena8-apple", "--wipe-ok"], &["apple"]),
        (&["stop", "--all", "--include", "bloom", "--include", "cloud", "--exclude", "cloud", "--wipe-ok"], &["bloom"]),
        // a dry run stops nothing (and needs no --wipe-ok to preview)
        (&["stop", "--all", "--dry-run"], &[]),
        // a VM keeps its disk: no --wipe-ok needed
        (&["stop", "vm"], &["vm"]),
    ];
    for (argv, stopped) in cases {
        let (f, r) = run_pods(argv).await;
        r.unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        let got: Vec<String> = f.calls().into_iter().map(|(op, id)| format!("{op} {id}")).collect();
        let want: Vec<String> = ids(stopped).into_iter().map(|id| format!("stop {id}")).collect();
        assert_eq!(got, want, "{argv:?}");
    }
    for (argv, err) in [
        (&["stop"][..], "name the pods to stop"),
        (&["stop", "--exclude", "apple"], "name the pods to stop"),
        (&["stop", "bloon"], "did you mean arena8-bloom?"),
        (&["stop", "--all", "--exclude", "bloon"], "--exclude `bloon`"),
        (&["stop", "apple", "--all"], "pass target names or --all, not both"),
        // a volume-less RunPod pod loses everything: refused without --wipe-ok, even
        // with --yes (run_pods passes yes) — and nothing at all is stopped, VM included
        (&["stop", "apple", "vm"], "refusing to stop arena8-apple without --wipe-ok"),
        (&["stop", "--all"], "refusing to stop arena8-apple, arena8-bloom, arena8-cloud without --wipe-ok"),
    ] {
        let (f, r) = run_pods(argv).await;
        let e = r.unwrap_err();
        assert!(e.contains(err), "{argv:?}: {e}");
        assert!(f.calls().is_empty(), "{argv:?}");
    }
}

#[tokio::test]
async fn reimage_reimages_exactly_the_selected_pods() {
    // reimage refuses without a readable public key, so give it one.
    let dir = std::env::temp_dir().join(format!("arena-sel-reimage-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("k"), "private").unwrap();
    std::fs::write(dir.join("k.pub"), "ssh-ed25519 AAAAtest arena-test").unwrap();
    let cfg = Config::parse(&format!(
        "{}SHARED_SSH_KEY_PATH={}\nIMAGE=img:1\n",
        "MACHINE_NAME_PREFIX=arena8\nMACHINE_NAME_LIST=(apple bloom cloud mayor vm zebra)\n",
        dir.join("k").display()
    ));
    let cases: &[(&[&str], &[&str])] = &[
        (&["reimage", "bloom", "zebra", "--skip-proxy", "--wipe-ok", "--no-setup"], &["bloom", "zebra"]),
        (&["reimage", "--all", "--exclude", "apple..cloud,vm", "--skip-proxy", "--wipe-ok", "--no-setup"], &["zebra"]),
        (&["reimage", "apple..bloom", "--dry-run", "--no-setup"], &[]),
    ];
    for (argv, reimaged) in cases {
        let f = fleet();
        handle_pods(pods_cmd(argv), &f, Arc::new(FakeRemote::new()), &cfg, true)
            .await
            .unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        let got: Vec<String> = f.calls().into_iter().map(|(op, id)| format!("{op} {id}")).collect();
        let want: Vec<String> = ids(reimaged).into_iter().map(|id| format!("reimage {id}")).collect();
        assert_eq!(got, want, "{argv:?}");
    }
    for argv in [&["reimage"][..], &["reimage", "--gpus", "1"], &["reimage", "appel"]] {
        let f = fleet();
        assert!(handle_pods(pods_cmd(argv), &f, Arc::new(FakeRemote::new()), &cfg, true).await.is_err(), "{argv:?}");
        assert!(f.calls().is_empty(), "{argv:?}");
    }
    // A selection with a pod its provider can't reimage (the Hetzner VM) is refused whole —
    // dry run or not, before the wipe gate, the confirm or any provider call — naming it, why,
    // what instead, and how to leave it out.
    for argv in [
        &["reimage", "--all", "--exclude", "apple..cloud", "--skip-proxy", "--wipe-ok", "--no-setup"][..],
        &["reimage", "vm", "zebra", "--dry-run"],
    ] {
        let f = fleet();
        let e = handle_pods(pods_cmd(argv), &f, Arc::new(FakeRemote::new()), &cfg, true).await.unwrap_err().to_string();
        assert!(e.starts_with("refusing to reimage anything — 1 of the 2 pod(s) can't be:\n  arena8-vm (hetzner): arena can't reimage a Hetzner VM"), "{argv:?}: {e}");
        assert!(e.contains("`arena pods up <name>`") && e.ends_with("leave these out: --exclude arena8-vm)"), "{argv:?}: {e}");
        assert!(f.calls().is_empty(), "{argv:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn terminate_takes_one_pod_by_the_same_matcher() {
    let (f, r) = run_pods(&["terminate", "bloom", "--skip-proxy"]).await;
    r.unwrap();
    assert_eq!(f.calls(), [("terminate", "id-arena8-bloom".to_string())]);
    let (f, r) = run_pods(&["terminate", "id-arena8-apple", "--skip-proxy", "--dry-run"]).await;
    r.unwrap();
    assert!(f.calls().is_empty());
    // No ranges, no `all` token, no typos, no ambiguity — and nothing terminated.
    for (argv, err) in [
        (&["terminate", "apple..cloud"][..], "takes one pod — `apple..cloud` is a range"),
        (&["terminate", "all"], "`all` isn't accepted"),
        (&["terminate", "blom"], "did you mean arena8-bloom?"),
    ] {
        let (f, r) = run_pods(argv).await;
        assert!(r.unwrap_err().contains(err), "{argv:?}");
        assert!(f.calls().is_empty(), "{argv:?}");
    }
    let mut f = fleet();
    f.pods.push(Pod { id: "id-second-bloom".into(), ..f.pods[1].clone() });
    let e = handle_pods(pods_cmd(&["terminate", "bloom"]), &f, Arc::new(FakeRemote::new()), &cfg(), true)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("`bloom` matches 2 pods") && e.contains("id-second-bloom"), "{e}");
    assert!(f.calls().is_empty());
    // A target and --all together is refused by the parser (it used to mean "all").
    assert!(parse(&["pods", "terminate", "apple", "--all"]).is_err());
}

#[tokio::test]
async fn keys_target_machine_names_with_or_without_a_pod() {
    let f = fleet();
    let keys = |a: SelectArgs, known: Vec<String>| {
        let f = &f;
        async move {
            keys_targets(f, &cfg(), &a, &known).await.map(|(n, live)| (n, live.pods.len())).map_err(|e| e.to_string())
        }
    };
    // A range spans list names without a pod (mayor): a key can precede its pod.
    assert_eq!(
        keys(args(&["cloud..vm"]), vec![]).await.unwrap(),
        (vec!["arena8-cloud".to_string(), "arena8-mayor".into(), "arena8-vm".into()], 2)
    );
    // A spare list name alone (revoke a terminated pod's key) is fine; a typo is not.
    assert_eq!(keys(args(&["mayor"]), vec![]).await.unwrap(), (vec!["arena8-mayor".to_string()], 0));
    assert!(keys(args(&["mayr"]), vec![]).await.unwrap_err().contains("did you mean arena8-mayor?"));
    // A name that left the list but still holds a key (or a CSV row) can still be
    // targeted; a near-miss of it is suggested.
    let known = vec!["arena8-old".to_string()];
    assert_eq!(keys(args(&["old"]), known.clone()).await.unwrap(), (vec!["arena8-old".to_string()], 0));
    assert!(keys(args(&["olf"]), known.clone()).await.unwrap_err().contains("did you mean arena8-old?"));
    assert!(keys(args(&["old"]), vec![]).await.unwrap_err().contains("no pod has that name"));
    // --all = every current pod (not the spare names or key holders), narrowed as usual.
    let all = SelectArgs { all: true, exclude: vec!["vm".into()], ..Default::default() };
    assert_eq!(keys(all, known).await.unwrap().0, ["arena8-apple", "arena8-bloom", "arena8-cloud", "arena8-zebra"]);
    assert!(keys(SelectArgs::default(), vec![]).await.unwrap_err().contains("name the machines"));
    // --gpus fetches the details a listing omits, as `select` does (cloud's count).
    let two = SelectArgs { all: true, gpus: Some(2), ..Default::default() };
    assert_eq!(keys(two, vec![]).await.unwrap().0, ["arena8-bloom", "arena8-cloud"]);
    // Without a provisioning key the command stops before anything (no network).
    let Cmd::Keys(k) = parse(&["keys", "gen", "apple", "--dry-run"]).unwrap() else { unreachable!() };
    let e = handle_keys(k, &f, Arc::new(FakeRemote::new()), &cfg(), true).await.unwrap_err().to_string();
    assert!(e.contains("set OPENROUTER_PROVISIONING_KEY first"), "{e}");
}

/// A temp keys dir (per process + test), removed when dropped.
struct KeysDir(std::path::PathBuf);
impl Drop for KeysDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn keys_dir(tag: &str) -> KeysDir {
    let d = std::env::temp_dir().join(format!("arena-keys-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    KeysDir(d)
}

/// `keys <argv…>` against the fleet, `api` and `dir`.
async fn keys_cmd(argv: &[&str], api: &FakeKeys, dir: &std::path::Path) -> std::result::Result<(), String> {
    let Cmd::Keys(k) = parse(&[&["keys"][..], argv].concat()).unwrap_or_else(|e| panic!("{argv:?}: {e}")) else {
        unreachable!()
    };
    keys_with(k, &fleet(), Arc::new(FakeRemote::new()), &cfg(), api, dir, true).await.map_err(|e| e.to_string())
}

#[tokio::test]
async fn keys_gen_rotate_revoke_act_on_exactly_the_resolved_machines() {
    let dir = keys_dir("act");
    // bloom already has a key; an old cohort's key and an unrelated one must be ignored.
    let api = FakeKeys::with(&[("h-b", "arena8-bloom"), ("h-7", "arena7-apple"), ("h-x", "admin")]);
    // gen: a range + a spare list name; bloom is skipped (exists), the rest minted.
    keys_cmd(&["gen", "apple..cloud", "mayor", "--exclude", "cloud"], &api, &dir.0).await.unwrap();
    assert_eq!(api.calls(), ["create arena8-apple", "create arena8-mayor"]);
    let csv = std::fs::read_to_string(dir.0.join("openrouter_api_keys.csv")).unwrap();
    let rows: Vec<String> = arena_core::apikeys::parse_csv(&csv).into_iter().map(|(h, _)| h).collect();
    assert_eq!(rows, ["arena8-apple", "arena8-mayor"]);
    assert_eq!(std::fs::read_link(dir.0.join("openrouter_api_keys.csv")).unwrap(), std::path::PathBuf::from("arena8_openrouter_keys.csv"));
    // rotate: deletes each machine's key(s), mints fresh — only for the --on selection.
    api.calls.lock().unwrap().clear();
    keys_cmd(&["rotate", "--all", "--on", "runpod", "--gpus", "2"], &api, &dir.0).await.unwrap();
    assert_eq!(api.calls(), ["delete h-b", "create arena8-bloom", "create arena8-cloud"]);
    // revoke: exactly the named machine's key; the CSV is left as it was.
    api.calls.lock().unwrap().clear();
    keys_cmd(&["revoke", "mayor"], &api, &dir.0).await.unwrap();
    assert_eq!(api.calls(), ["delete h-new-arena8-mayor"]);
    assert!(std::fs::read_to_string(dir.0.join("openrouter_api_keys.csv")).unwrap().contains("arena8-mayor,"));
    // A machine that left the list and has no pod, but still has a key: revocable by
    // name (it's a key holder); a typo of it is still an error naming it.
    let api = FakeKeys::with(&[("h-o", "arena8-oldbox")]);
    keys_cmd(&["revoke", "oldbox"], &api, &dir.0).await.unwrap();
    assert_eq!(api.calls(), ["delete h-o"]);
    let e = keys_cmd(&["revoke", "oldbx"], &FakeKeys::with(&[("h-o", "arena8-oldbox")]), &dir.0).await.unwrap_err();
    assert!(e.contains("did you mean arena8-oldbox?"), "{e}");
    // Dry runs change nothing on either side.
    let api = FakeKeys::with(&[("h-b", "arena8-bloom")]);
    for argv in [&["gen", "--all", "--dry-run"][..], &["rotate", "apple", "--dry-run"], &["revoke", "--all", "--dry-run"]] {
        keys_cmd(argv, &api, &dir.0).await.unwrap_or_else(|e| panic!("{argv:?}: {e}"));
    }
    assert!(api.calls().is_empty());
}

#[test]
fn keys_dry_run_names_every_resolved_machine_and_what_happens() {
    let rows = vec![("arena8-apple".to_string(), "arena8-apple".to_string(), 0), ("arena8-bloom".into(), "arena8-bloom".into(), 1)];
    assert_eq!(
        keys_preview(KeysAction::Gen, 5.0, &rows),
        [
            "Dry-run — would mint a $5.00-cap key for 2 machine(s):",
            "  arena8-apple",
            "  arena8-bloom — already exists, would skip (`arena keys rotate arena8-bloom` replaces it)",
        ]
    );
    assert_eq!(keys_preview(KeysAction::Rotate, 2.5, &rows)[1..], ["  arena8-apple — none yet, would just mint", "  arena8-bloom — deletes 1 existing"]);
    assert_eq!(keys_preview(KeysAction::Revoke, 5.0, &rows)[1..], ["  arena8-apple — no such key, nothing to revoke", "  arena8-bloom — deletes 1"]);
}

/// `pods stop --all` stops every billing pod — a STARTING/PROVISIONING v2 pod (it
/// bills) used to be missed, as only `RUNNING` matched — and leaves stopped ones alone.
/// The same status rule applies to a stop of named pods (the merged selector path).
#[tokio::test]
async fn stop_all_targets_every_billing_status() {
    let fleet = || FakeProvider {
        pods: vec![
            pod("arena8-apple", "RUNNING", "runpod", Some(1)),
            pod("arena8-bloom", "STARTING", "runpod", Some(1)),
            pod("arena8-cider", "PROVISIONING", "runpod", Some(1)),
            pod("arena8-dune", "EXITED", "runpod", Some(1)),
            pod("arena8-elm", "TERMINATED", "runpod", Some(1)),
            pod("arena8-fig", "ERROR", "runpod", Some(1)),
            pod("arena8-gale", "running", "runpod", Some(1)),
        ],
        calls: Mutex::new(Vec::new()),
        lists: Mutex::new(0),
        enriched: vec![],
    };
    // (argv, stopped) — the fake's RunPod pods have no volume, so a real stop needs
    // --wipe-ok (lane E's gate); a dry run previews without it.
    let cases: &[(&[&str], &[&str])] = &[
        (&["stop", "--all", "--dry-run"], &[]),
        (&["stop", "--all", "--wipe-ok"], &["apple", "bloom", "cider", "fig", "gale"]),
        // named: a starting pod is stopped; an exited one is skipped (said, not stopped)
        (&["stop", "bloom", "dune", "--wipe-ok"], &["bloom"]),
        (&["stop", "dune", "elm"], &[]),
    ];
    for (argv, stopped) in cases {
        let f = fleet();
        handle_pods(pods_cmd(argv), &f, Arc::new(FakeRemote::new()), &cfg(), true)
            .await
            .unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        let got: Vec<String> = f.calls().into_iter().map(|(op, id)| format!("{op} {id}")).collect();
        let want: Vec<String> = ids(stopped).into_iter().map(|id| format!("stop {id}")).collect();
        assert_eq!(got, want, "{argv:?}");
    }
}

/// `pods stop --help` states the rule the handler applies (`status::is_billing`), not
/// the old RUNNING-only one: an ERROR / PROVISIONING pod is stopped too, and needs
/// --wipe-ok like a running one — the help must not promise otherwise. The one-line
/// summary in `arena pods --help` warns that a stop wipes the container disk.
#[test]
fn stop_help_states_the_billing_rule() {
    use clap::CommandFactory;
    let cli = crate::cli::Cli::command();
    let stop = cli.find_subcommand("pods").and_then(|p| p.find_subcommand("stop")).expect("pods stop");
    let about = stop.get_about().map(|s| s.to_string()).unwrap_or_default();
    let long = stop.get_long_about().map(|s| s.to_string()).unwrap_or_default();
    assert!(about.contains("billing") && !about.contains("running pods") && about.contains("WIPES"), "{about}");
    for want in ["starting", "provisioning", "ERROR", "--wipe-ok", "skipped"] {
        assert!(long.contains(want), "missing {want:?}: {long}");
    }
}

/// Finding #37: `-n` counts only --provider's pods (`provider_pod_count`), so "TOTAL
/// number of pods" read as a fleet total — `-n 4` on a mixed fleet would leave 5 — and the
/// `proxy` group called itself read-only although `proxy apply` writes and reloads nginx.
#[test]
fn help_says_what_n_counts_and_that_proxy_apply_writes() {
    use clap::CommandFactory;
    let cli = crate::cli::Cli::command();
    let pods = cli.find_subcommand("pods").expect("pods");
    for sub in ["create", "up"] {
        let cmd = pods.find_subcommand(sub).expect(sub);
        let arg = |id: &str| {
            cmd.get_arguments().find(|a| a.get_id() == id).and_then(|a| a.get_help()).map(|h| h.to_string()).unwrap()
        };
        let n = arg("count");
        assert!(!n.contains("TOTAL") && n.contains("ON --provider") && n.contains("other providers don't count"), "{sub}: {n}");
        assert!(arg("add").contains("--provider"), "{sub}");
    }
    let proxy = cli.find_subcommand("proxy").expect("proxy");
    let about = proxy.get_about().map(|s| s.to_string()).unwrap_or_default();
    assert!(!about.contains("read-only") && about.contains("write + reload (apply)"), "{about}");
}

/// The help stays short enough to be read (the operator's complaint: one-line summaries
/// that ran to 500 characters). Walks the whole command tree: every command has a one-line
/// summary (`about`: its line in the parent's command list) of at most 70 characters,
/// every argument has help whose first line is at most 100 and whose `--help` text is at
/// most 300, and every command's `--help` text (`long_about`) is at most 800. Details
/// belong in docs/REFERENCE.md / FEATURE_MAP.md, not here.
#[test]
fn help_text_stays_short() {
    use clap::CommandFactory;
    const ABOUT_MAX: usize = 70;
    const ARG_HELP_MAX: usize = 100;
    const ARG_LONG_HELP_MAX: usize = 300;
    const LONG_ABOUT_MAX: usize = 800;
    // Justified exceptions to LONG_ABOUT_MAX, by command path (`"arena pods up"`): none.
    const LONG_ABOUT_EXCEPTIONS: &[&str] = &[];
    fn walk(cmd: &clap::Command, path: String, bad: &mut Vec<String>) {
        let len = |s: &str| s.chars().count();
        let about = cmd.get_about().map(|s| s.to_string()).unwrap_or_default();
        if about.trim().is_empty() || about.contains('\n') || len(&about) > ABOUT_MAX {
            bad.push(format!("{path}: about, {} chars: {about:?}", len(&about)));
        }
        let long = cmd.get_long_about().map(|s| s.to_string()).unwrap_or_default();
        if len(&long) > LONG_ABOUT_MAX && !LONG_ABOUT_EXCEPTIONS.contains(&path.as_str()) {
            bad.push(format!("{path}: long help, {} chars", len(&long)));
        }
        for arg in cmd.get_arguments() {
            let id = arg.get_id();
            let help = arg.get_help().map(|s| s.to_string()).unwrap_or_default();
            let first = help.lines().next().unwrap_or_default();
            if first.trim().is_empty() || len(first) > ARG_HELP_MAX {
                bad.push(format!("{path} --{id}: help, {} chars: {help:?}", len(first)));
            }
            let long = arg.get_long_help().map(|s| s.to_string()).unwrap_or_default();
            if len(&long) > ARG_LONG_HELP_MAX {
                bad.push(format!("{path} --{id}: long help, {} chars", len(&long)));
            }
        }
        for sub in cmd.get_subcommands() {
            walk(sub, format!("{path} {}", sub.get_name()), bad);
        }
    }
    let mut cli = crate::cli::Cli::command();
    cli.build();
    let mut bad = Vec::new();
    walk(&cli, "arena".to_string(), &mut bad);
    assert!(bad.is_empty(), "help text missing or too long (details go in docs/REFERENCE.md):\n{}", bad.join("\n"));
}

/// Shortening the help never drops a warning: the commands and flags that wipe, delete or
/// terminate still say so, in the words an operator scans for.
#[test]
fn dangerous_commands_keep_their_warnings() {
    use clap::CommandFactory;
    let cli = crate::cli::Cli::command();
    let find = |path: &[&str]| {
        path.iter().fold(&cli, |c, name| c.find_subcommand(name).unwrap_or_else(|| panic!("no command {path:?}")))
    };
    let text = |s: Option<&clap::builder::StyledStr>| s.map(|s| s.to_string()).unwrap_or_default();
    for (path, word) in [(&["pods", "stop"][..], "WIPES"), (&["pods", "restart"], "WIPES"), (&["pods", "reimage"], "WIPES")] {
        let about = text(find(path).get_about());
        assert!(about.contains(word), "{path:?}: {about}");
    }
    for (path, word) in [
        (&["pods", "terminate"][..], "IRREVERSIBLE"),
        (&["pods", "up"], "TERMINATED"),
        (&["pods", "reap"], "REAP_ENABLED=1"),
        (&["pods", "reap", "run"], "TERMINATE"),
        (&["archive", "down"], "EVERY BACKUP"),
        (&["teardown"], "deletes nothing"),
    ] {
        let long = text(find(path).get_long_about());
        assert!(long.contains(word), "{path:?}: {long}");
    }
    for (path, id, word) in [
        (&["pods", "terminate"][..], "all", "EVERY"),
        (&["pods", "stop"], "wipe_ok", "WIPED"),
        (&["pods", "restart"], "wipe_ok", "WIPED"),
        (&["pods", "set-branch"], "hard", "DESTRUCTIVE"),
        (&["pods", "setup"], "force", "DISCARDS"),
        (&["pods", "up"], "check", "TERMINATE"),
        (&["pods", "reap", "run"], "apply", "TERMINATE"),
        (&["archive", "down"], "delete_volumes", "DELETE"),
        (&["archive", "forget"], "delete_archived", "for good"),
        (&["pods", "migrate", "finish"], "discard_stopped", "losing"),
        (&["update"], "skip_attestation", "NOT check build provenance"),
    ] {
        let arg = find(path).get_arguments().find(|a| a.get_id() == id).unwrap_or_else(|| panic!("{path:?} {id}"));
        let help = text(arg.get_help());
        assert!(help.contains(word), "{path:?} --{id}: {help}");
    }
}

#[tokio::test]
async fn a_selection_flag_after_runs_command_is_refused_not_widened() {
    // `run hostname -t apple` parses as the command `hostname -t apple` with NO targets
    // (= every pod): refused before anything is listed or run.
    for argv in [
        &["run", "hostname", "-t", "apple"][..],
        &["run", "-t", "apple", "hostname", "--exclude", "bloom"],
        &["run", "uptime", "--on=vast"],
        &["run", "rm", "-rf", "/tmp/x", "--dry-run"],
    ] {
        let (f, r) = run_pods(argv).await;
        let e = r.unwrap_err();
        assert!(e.contains("after the command would be part of the command") && e.contains("Nothing was run"), "{argv:?}: {e}");
        assert_eq!(*f.lists.lock().unwrap(), 0, "{argv:?}");
    }
    let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    for (cmd, bad) in [
        (&["hostname", "-t", "apple"][..], Some("`-t`")),
        (&["x", "--target=apple", "--gpus", "2"], Some("`--target=apple --gpus`")),
        (&["x", "--include", "a", "--dryrun"], Some("`--include --dryrun`")),
        (&["pytest", "-x", "--background"], Some("`--background`")),
        (&["pytest", "--bg"], Some("`--bg`")),
        (&["sleep", "--bgcolor"], None),
        // a quoted command is one word; flags that only look alike are left alone
        (&["tmux kill-session -t lab"], None),
        (&["tar", "-tvf", "x.tar"], None),
        (&["git", "branch", "--all"], None),
        (&["apt-get", "install", "-y", "jq"], None),
        (&["ls", "--only-dirs"], None),
    ] {
        let got = misplaced_run_flags(&words(cmd));
        match bad {
            None => assert_eq!(got, None, "{cmd:?}"),
            Some(b) => assert!(got.as_deref().is_some_and(|e| e.starts_with(b)), "{cmd:?}: {got:?}"),
        }
    }
}

#[test]
fn run_refuses_a_command_that_starts_with_a_pods_name() {
    let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let names = [("devtest-hz4", "hz4"), ("devtest-apple", "apple")];
    for (cmd, bad) in [
        // the `-t` forgotten: the pod's name (full or short) taken as the command
        (&["devtest-hz4", "git status"][..], Some(("devtest-hz4", "devtest-hz4"))),
        (&["apple", "ls", "-la"], Some(("apple", "devtest-apple"))),
        (&["apple ls"], Some(("apple", "devtest-apple"))),
        // ordinary commands, a name inside a command, the `command` escape, other words
        (&["git", "status"], None),
        (&["ls apple"], None),
        (&["command", "apple", "x"], None),
        (&["apples"], None),
        (&["devtest-hz"], None),
    ] {
        let got = pod_name_as_command(&words(cmd), &names);
        match bad {
            None => assert_eq!(got, None, "{cmd:?}"),
            Some((first, full)) => {
                let e = got.unwrap_or_else(|| panic!("{cmd:?}: not refused"));
                assert!(e.starts_with(&format!("`{first}` is a pod's name ({full})")), "{cmd:?}: {e}");
                assert!(e.contains("Nothing was run") && e.contains(&format!("-t {first} '")), "{cmd:?}: {e}");
            }
        }
    }
    assert_eq!(pod_name_as_command(&[], &names), None);
    assert!(pod_name_as_command(&words(&["apple"]), &names).is_some_and(|e| e.contains("on all 2 selected pod(s)")));
}

#[test]
fn every_old_spelling_still_parses() {
    let ok: &[&[&str]] = &[
        &["pods", "stop", "apple"],
        &["pods", "stop", "--all", "--include", "apple", "--exclude", "bloom"],
        &["pods", "reimage", "apple", "bloom"],
        &["pods", "reimage", "--all", "--exclude", "apple"],
        &["pods", "terminate", "apple"],
        &["pods", "terminate", "--all"],
        &["pods", "setup"],
        &["pods", "setup", "bulk", "apple", "--dry-run"],
        &["pods", "copy-keys", "apple", "--include", "bloom", "--exclude", "cloud"],
        &["pods", "cp", "f.txt", "--include", "apple", "--exclude", "bloom"],
        &["pods", "cp", "f.txt", "/root/x/", "-r"],
        &["pods", "backup"],
        &["pods", "backup", "apple", "--no-pull"],
        &["pods", "set-branch", "main", "apple"],
        &["pods", "set-branch", "main", "--all", "--hard"],
        &["pods", "init-branches", "--week", "1", "--day", "2"],
        &["pods", "pull", "w1d2", "--dry-run"],
        &["pods", "run", "nvidia-smi", "-L"],
        &["pods", "test"],
        &["pods", "test", "--deep", "apple", "--json"],
        &["keys", "gen", "apple", "bloom", "--limit", "5"],
        &["keys", "gen", "--all", "--copy"],
        &["keys", "rotate", "apple"],
        &["keys", "rotate", "--all"],
        &["keys", "revoke", "apple"],
        &["keys", "revoke", "--all"],
        // and the new syntax
        &["pods", "run", "-t", "apple..mayor", "--exclude", "bloom", "--gpus", "2", "--on", "runpod", "nvidia-smi"],
        &["pods", "run", "--target", "apple,bloom", "--target", "cloud", "echo", "hi"],
        &["pods", "cp", "f.txt", "-t", "apple"],
        &["pods", "pull", "-t", "apple..bloom"],
        &["pods", "pull", "apple..bloom", "--exclude", "cloud", "--label", "w1d2"],
        &["pods", "pull", "bravo", "--dry-run"],
        &["pods", "test", "all", "--on", "vast"],
        &["pods", "init-branches", "apple", "bloom"],
        &["pods", "backup", "apple..cloud", "--exclude", "bloom"],
    ];
    for argv in ok {
        parse(argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
    }
    // `run`'s targets are flags: everything after the command is the command.
    match parse(&["pods", "run", "-t", "apple", "echo", "-t", "x"]).unwrap() {
        Cmd::Pods(PodCmd::Run { sel, command, .. }) => {
            assert_eq!((sel.targets, command), (vec!["apple".to_string()], vec!["echo".into(), "-t".into(), "x".into()]));
        }
        _ => panic!("not pods run"),
    }
    // The legacy narrowing of --all by --include, and the parsed selection itself.
    let Cmd::Pods(PodCmd::Stop { sel, .. }) = parse(&["pods", "stop", "--all", "--include", "apple"]).unwrap() else {
        panic!("not pods stop")
    };
    assert_eq!(sel.args(), SelectArgs { targets: vec!["apple".into()], ..Default::default() });
    let Cmd::Keys(KeysCmd::Gen { sel, .. }) = parse(&["keys", "gen", "apple", "--on", "runpod"]).unwrap() else {
        panic!("not keys gen")
    };
    assert_eq!(sel.args(), SelectArgs { targets: vec!["apple".into()], on: Some("runpod".into()), ..Default::default() });
    for bad in [&["pods", "terminate", "apple", "--all"][..], &["pods", "run", "--gpus", "two", "x"]] {
        assert!(parse(bad).is_err(), "{bad:?}");
    }
}
