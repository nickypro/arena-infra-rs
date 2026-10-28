//! Missing endpoints on an old running pod must fail work while healthy pods carry on.
use super::*;
use arena_core::remote::FakeRemote;

fn selection(healthy: bool) -> Selected {
    let mut pods = vec![arena_core::Pod {
        id: "missing".into(), name: "devtest-missing".into(), provider: "runpod".into(),
        status: "RUNNING".into(), started_at: Some(1), ..Default::default()
    }];
    if healthy {
        pods.push(arena_core::Pod {
            id: "ok".into(), name: "devtest-ok".into(), provider: "runpod".into(),
            status: "RUNNING".into(), ssh_ip: Some("203.0.113.1".into()), ssh_port: Some(22), ..Default::default()
        });
    }
    Selected::all(pods)
}

#[tokio::test]
async fn backup_reports_missing_running_pods_even_with_healthy_work() {
    let cfg = Config::parse_with_env("", |_| None);
    for healthy in [false, true] {
        let remote = Arc::new(FakeRemote::new());
        let e = handle_backup(remote.clone(), &cfg, true, None, &selection(healthy), &mut PodLocks::default(), &mut Vec::new()).await.unwrap_err();
        let failed = e.downcast_ref::<BackupFailed>().unwrap();
        assert!(failed.pods.iter().any(|n| n == "devtest-missing"));
        assert_eq!(!remote.calls().is_empty(), healthy, "healthy pods still attempted");
    }
}

#[tokio::test]
async fn run_fails_missing_running_pods_after_running_the_healthy_pods() {
    let cfg = Config::parse_with_env("", |_| None);
    let remote = Arc::new(FakeRemote::new());
    let e = handle_run(remote.clone(), &cfg, &selection(true), "true", Duration::from_secs(2), false, true, false, true).await.unwrap_err();
    assert!(e.to_string().contains("1 pod(s) failed"), "{e}");
    assert_eq!(remote.calls().len(), 1);
}

#[tokio::test]
async fn pull_cannot_report_success_when_every_running_pod_was_unreachable() {
    let cfg = Config::parse_with_env("", |_| None);
    let remote: Arc<dyn Remote> = Arc::new(FakeRemote::new());
    let options = PullRun { label: Some("test".into()), max_size: None, remote_path: Some("/root".into()), no_git: true, no_big: true, dry_run: true, yes: true };
    let pulled = pull_pods(&remote, &cfg, "/tmp/arena-unused-endpoint-test", options, &selection(false), "never-called-rsync", LockMode::Held).await.unwrap();
    assert!(pulled.result().unwrap_err().to_string().contains("devtest-missing"));
}
