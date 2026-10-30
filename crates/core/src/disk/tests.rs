use super::*;

const G: u64 = GB;

fn fs(dev: u64, mount: &str, free: u64) -> Fs {
    Fs { dev, mount: PathBuf::from(mount), free, total: 75 * G }
}

fn floor(gb: u64) -> Floor {
    Floor::control(gb * G)
}

struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        let d = std::env::temp_dir().join(format!("arena-disk-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        TmpDir(d)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `dir`'s lock once it's free. A child another test spawns while this process holds a lock (even
/// a look's momentary one) keeps a copy of it until its exec — `flock` is per open file — so a
/// lock just let go of can look held for an instant, as `copy_pod_files` waits a look's out.
fn lock_when_free(dir: &Path) -> std::io::Result<std::fs::File> {
    for _ in 0..100 {
        match lock_dir(dir) {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(20)),
            r => return r,
        }
    }
    lock_dir(dir)
}

#[test]
fn the_floor_is_whole_gb_from_zero_with_a_default_of_five() {
    let f = |text: &str| floor_bytes(&Config::parse(text));
    assert_eq!(f(""), Ok(5 * G));
    assert_eq!(f("CONTROL_MIN_FREE_GB= \n"), Ok(5 * G));
    for (v, want) in [("0", 0), ("1", G), (" 12 ", 12 * G), ("100000", 100_000 * G)] {
        assert_eq!(f(&format!("CONTROL_MIN_FREE_GB={v}\n")), Ok(want), "{v}");
    }
    for bad in ["100001", "-1", "5.5", "lots", "5GB"] {
        let e = f(&format!("CONTROL_MIN_FREE_GB={bad}\n")).unwrap_err();
        assert_eq!(e, format!("CONTROL_MIN_FREE_GB must be a whole number of GB, 0..=100000 (got `{bad}`)"));
    }
}

/// The judge, as a table: (free, already held this run, need, floor GB) → go, or the words.
#[test]
fn a_copy_goes_only_when_it_leaves_the_floor_free() {
    let cases: &[(u64, u64, Size, u64, Option<&str>)] = &[
        // Prod on 2026-10-30: 3.1 GB free, the default floor.
        (3_100_000_000, 0, Size::First, 5, Some("only 3.1 GB free on / (floor 5 GB, CONTROL_MIN_FREE_GB)")),
        (3_100_000_000, 0, Size::Later, 5, Some("only 3.1 GB free on /")),
        (3_100_000_000, 0, Size::Measured(0), 5, Some("only 3.1 GB free on /")),
        // Above the floor, but not with the copy on top.
        (7 * G, 0, Size::First, 5, Some("7 GB free on /, and this copy (~3 GB, a first copy, not measured) would leave less than the floor (5 GB, CONTROL_MIN_FREE_GB)")),
        (6 * G, G / 2, Size::Measured(G), 5, Some("this copy (~1 GB, measured) after ~0.5 GB for other copies under way would leave less")),
        // Room: exactly at the floor afterwards is enough.
        (8 * G, 0, Size::First, 5, None),
        (5 * G + G / 10, 0, Size::Later, 5, None),
        (40 * G, 10 * G, Size::First, 5, None),
        // A zero floor: only the copy itself must fit.
        (G, 0, Size::Measured(G), 0, None),
        (G, 0, Size::Measured(G + 1), 0, Some("1 GB free on /, and this copy (~1.0 GB, measured) would leave less than the floor (0 GB")),
        // Counted in the floor (the reaper's): the floor alone.
        (20 * G, 0, Size::InFloor, 20, None),
        (19 * G, 0, Size::InFloor, 20, Some("only 19 GB free on / (floor 20 GB")),
    ];
    for (free, already, size, floor_gb, want) in cases {
        let got = judge(&fs(1, "/", *free), *already, &Need::from(*size), floor(*floor_gb));
        match (got, want) {
            (Ok(()), None) => {}
            (Err(skip), Some(words)) => {
                let text = skip.to_string();
                assert!(text.starts_with("skipped: ") && text.contains(words), "{free} {size:?}: {text}");
                assert!(text.contains("nothing copied; free space on / (`arena config check` names arena's biggest dirs there)"), "{text}");
            }
            (got, want) => panic!("{free} {already} {size:?} {floor_gb}: got {got:?}, want {want:?}"),
        }
    }
    // The line the brief asks for, word for word at its start.
    let skip = judge(&fs(1, "/", 3_100_000_000), 0, &Need::from(Size::First), floor(5)).unwrap_err();
    assert!(skip.to_string().starts_with("skipped: only 3.1 GB free on / (floor 5 GB"), "{skip}");
    // Another floor names its own key.
    let reap = Floor { bytes: 20 * G, key: "REAP_MIN_FREE_GB" };
    assert!(judge(&fs(1, "/srv", G), 0, &Need::from(Size::InFloor), reap).unwrap_err().reason.ends_with("(floor 20 GB, REAP_MIN_FREE_GB)"));
    assert_eq!(Floor::control(5 * G).max(reap), reap);
    assert_eq!(reap.max(Floor::control(30 * G)), Floor::control(30 * G));
    let unknown = Skip::unknown(Path::new("/x"), "gone");
    assert_eq!(unknown.to_string(), "skipped: free space on /x unknown (gone) — nothing copied; check that disk");
}

#[test]
fn a_copys_parts_add_up_and_say_what_they_are() {
    let n = Need::of(&[Size::First, Size::First]);
    assert_eq!((n.bytes, n.words.as_str()), (6 * G, "~6 GB, a first copy, not measured"));
    let n = Need::of(&[Size::Later, Size::Later]);
    assert_eq!((n.bytes, n.words.as_str()), (2 * LATER_COPY, "~0.2 GB, what changed since the last copy, not measured"));
    let n = Need::of(&[Size::First, Size::Later]);
    assert_eq!((n.bytes, n.words.as_str()), (FIRST_COPY + LATER_COPY, "~3.1 GB, not measured"));
    assert_eq!(Need::of(&[Size::Measured(3_500_000_000)]).words, "~3.5 GB, measured");
    assert_eq!(Need::of(&[Size::Measured(u64::MAX), Size::First]).bytes, u64::MAX, "saturates");
}

/// A run's copies are held against the space they share; a pod is admitted whole or not at all.
#[test]
fn a_runs_copies_add_up_and_a_pod_goes_whole_or_not_at_all() {
    let root = fs(1, "/", 14 * G);
    let mut l = Ledger::new(floor(5));
    // One pod's two tiers on one filesystem: ~6 GB together.
    assert_eq!(l.admit(&[(root.clone(), Size::First), (root.clone(), Size::First)]), Ok(()));
    // The next first copy would leave 14 - 6 - 6 = 2 GB: skipped, saying what the run holds.
    let e = l.admit(&[(root.clone(), Size::First), (root.clone(), Size::First)]).unwrap_err().to_string();
    assert!(e.contains("this copy (~6 GB, a first copy, not measured) after ~6 GB for other copies under way"), "{e}");
    // A later copy still fits (6 + 0.2 + 5 ≤ 14), and is held too.
    assert_eq!(l.admit(&[(root.clone(), Size::Later), (root.clone(), Size::Later)]), Ok(()));
    // A filesystem keeps its first reading: a later one saying more is free changes nothing.
    let e = l.admit(&[(fs(1, "/", 100 * G), Size::First)]).unwrap_err().to_string();
    assert!(e.contains("14 GB free on /") && e.contains("after ~6.2 GB"), "{e}");

    // Two filesystems: one fits, the other doesn't — nothing is held for either.
    let (snap, big) = (fs(1, "/", 20 * G), fs(2, "/mnt/big", 6 * G));
    let mut l = Ledger::new(floor(5));
    let e = l.admit(&[(snap.clone(), Size::First), (big.clone(), Size::First)]).unwrap_err().to_string();
    assert!(e.contains("6 GB free on /mnt/big"), "{e}");
    assert_eq!(l.admit(&[(snap.clone(), Size::Measured(15 * G))]), Ok(()), "the failed pod held nothing on /");
    assert_eq!(l.admit(&[(big, Size::Measured(G))]), Ok(()), "nor on /mnt/big");
}

/// Other runs' copies under way count as taken: their reservations, per device, are pruned of
/// processes gone and of ones too old, and an admission holds (then lets go of) its own.
#[test]
fn other_runs_reservations_count_against_the_free_space() {
    let mut r = Reservations::default();
    let taken = |pairs: &[(u64, u64)]| pairs.iter().copied().collect::<BTreeMap<u64, u64>>();
    let now = 1_000_000;
    r.hold(100, 1, &taken(&[(1, 4 * G), (2, G)]), now);
    r.hold(200, 7, &taken(&[(1, 2 * G)]), now);
    r.hold(300, 9, &taken(&[(1, 50 * G)]), now); // its process is gone
    r.hold(400, 3, &taken(&[(1, 50 * G)]), now - RESERVATION_MAX_AGE_SECS); // too old
    r.prune(now, |pid| pid != 300);
    assert_eq!(r.by_dev(), taken(&[(1, 6 * G), (2, G)]));
    // 17 GB free, floor 5: with 6 GB held elsewhere, one first copy of two tiers (6 GB) fits, a second doesn't.
    let mut l = Ledger::with_others(floor(5), r.by_dev());
    let root = fs(1, "/", 17 * G);
    assert_eq!(l.admit(&[(root.clone(), Size::First), (root.clone(), Size::First)]), Ok(()));
    let e = l.admit(&[(root.clone(), Size::First)]).unwrap_err().to_string();
    assert!(e.contains("after ~12 GB for other copies under way"), "{e}");
    assert_eq!(l.taken(), taken(&[(1, 6 * G)]), "only this run's own");
    r.hold(500, 2, &l.taken(), now + 10);
    assert_eq!(r.by_dev()[&1], 12 * G);
    r.release(500, 2);
    r.release(100, 999); // another admission's token: nothing
    assert_eq!(r.by_dev(), taken(&[(1, 6 * G), (2, G)]));
    // This process is an arena test binary; a pid that isn't running isn't.
    assert!(arena_alive(std::process::id()));
    assert!(!arena_alive(u32::MAX - 1));
}

#[test]
fn a_whole_copy_is_marked_beside_the_labels() {
    let d = TmpDir::new("marker");
    assert!(!was_copied(&d.0, "devtest-a"));
    mark_copied(&d.0, "devtest-a").unwrap();
    assert!(was_copied(&d.0, "devtest-a") && !was_copied(&d.0, "devtest-b"));
    assert_eq!(copied_marker(&d.0, "devtest-a"), d.0.join(".arena-copied/devtest-a"));
    // Not a label, so nothing that lists snapshots or pods sees it.
    assert_eq!(crate::restore::parse_label(COPIED_DIR), None);
}

/// A backup from before marking: once, before a run's copies, every pod with a `wNdM` snapshot
/// there is marked (as arena took them until then), and nothing else; from then on, only whole
/// copies mark — a pod's interrupted first copy after that never is.
#[test]
fn a_backup_from_before_marking_is_seeded_once() {
    let d = TmpDir::new("seed");
    assert_eq!(seed_copied(&d.0.join("none")).unwrap(), 0);
    assert!(!d.0.join("none").exists(), "no backup: nothing made");
    let base = d.0.join("backup");
    for dir in ["w1d1/devtest-a", "w1d1/devtest-b", "w2d3/devtest-a", "mylabel/devtest-c", "big/devtest-d", ".arena-locks", "w1d1/.rsync-tmp"] {
        std::fs::create_dir_all(base.join(dir)).unwrap();
    }
    std::fs::write(base.join("w1d1/notes.txt"), "a file, not a pod").unwrap();
    assert_eq!(seed_copied(&base).unwrap(), 2);
    let marked = |pod: &str| was_copied(&base, pod);
    assert!(marked("devtest-a") && marked("devtest-b"));
    assert!(!marked("devtest-c") && !marked("devtest-d") && !marked("notes.txt") && !marked(".rsync-tmp"));
    let names: Vec<String> = std::fs::read_dir(&base).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert!(!names.iter().any(|n| n.starts_with(&format!("{COPIED_DIR}.new"))), "{names:?}");
    // Once: a later first copy (interrupted) isn't taken for a whole one.
    std::fs::create_dir_all(base.join("w2d4/devtest-e")).unwrap();
    assert_eq!(seed_copied(&base).unwrap(), 0);
    assert!(!marked("devtest-e"));
    // A backup with no pods yet starts marking empty: its first copies are first ones, for good.
    let fresh = d.0.join("fresh");
    std::fs::create_dir_all(&fresh).unwrap();
    assert_eq!(seed_copied(&fresh).unwrap(), 0);
    assert!(fresh.join(COPIED_DIR).is_dir());
    std::fs::create_dir_all(fresh.join("w1d1/devtest-a")).unwrap();
    assert_eq!(seed_copied(&fresh).unwrap(), 0);
    assert!(!was_copied(&fresh, "devtest-a"));
}

#[test]
fn levels_say_under_the_floor_and_near_it() {
    for (free, floor_gb, want) in [
        (3 * G, 5, Level::Below),
        (5 * G, 5, Level::Near),
        (9 * G, 5, Level::Near),
        (10 * G, 5, Level::Ok),
        (0, 0, Level::Ok),
    ] {
        assert_eq!(level(free, floor_gb * G), want, "{free} {floor_gb}");
    }
}

#[test]
fn sizes_and_ages_in_words() {
    for (b, want) in [
        (5 * G, "5 GB"),
        (3_100_000_000, "3.1 GB"),
        (G / 10, "0.1 GB"),
        (340_000_000, "0.3 GB"),
        (12_000_000, "12 MB"),
        (1, "1 KB"),
        (0, "0 GB"),
    ] {
        assert_eq!(gb(b), want, "{b}");
    }
    for (s, want) in [(40, "40 s"), (720, "12 min"), (5 * 3600, "5 h"), (47 * 3600, "47 h"), (16 * 86_400 + 5, "16 days")] {
        assert_eq!(age_words(s), want, "{s}");
    }
}

#[test]
fn a_reading_is_of_the_nearest_existing_dir_and_names_its_mount() {
    let d = TmpDir::new("reading");
    let here = reading(&d.0).unwrap();
    assert!(here.total > 0 && here.free <= here.total, "{here:?}");
    assert!(d.0.canonicalize().unwrap().starts_with(&here.mount), "{here:?}");
    // Not created yet: the filesystem it will be on.
    let later = reading(&d.0.join("not/yet/there")).unwrap();
    assert_eq!((later.dev, &later.mount), (here.dev, &here.mount));
    assert_eq!(reading(Path::new("/")).unwrap().mount, PathBuf::from("/"));
}

#[test]
fn a_walk_counts_blocks_hard_links_once_and_stops_at_its_cap() {
    let d = TmpDir::new("walk");
    let (a, b) = (d.0.join("a"), d.0.join("b"));
    std::fs::create_dir_all(a.join("deep")).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(a.join("deep/file"), vec![7u8; 256 * 1024]).unwrap();
    std::fs::hard_link(a.join("deep/file"), b.join("same")).unwrap();
    std::fs::write(b.join("own"), vec![1u8; 64 * 1024]).unwrap();
    let dev = reading(&d.0).unwrap().dev;
    let big = biggest(&[b.clone(), a.clone(), d.0.join("missing")], dev, 5);
    // b is walked first and gets the shared file; a's copy of it is not counted again.
    assert_eq!(big.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>(), [b.clone(), a.clone()]);
    assert!(big[0].1.bytes >= 320 * 1024 && big[1].1.bytes < 256 * 1024, "{big:?}");
    assert!(big[0].1.changed.is_some());
    // Each alone counts it.
    let mut seen = HashSet::new();
    let mut budget = WALK_CAP;
    assert!(tree_use(&a, &mut budget, &mut seen).bytes >= 256 * 1024);
    // The cap: a lower bound, said.
    let mut budget = 1;
    let t = tree_use(&a, &mut budget, &mut HashSet::new());
    assert!(t.capped, "{t:?}");
    assert_eq!(biggest(&[a], dev + 1, 5), [], "another device's dirs are left out");
}

#[test]
fn leftovers_are_found_sized_and_told_apart_from_one_in_use() {
    let d = TmpDir::new("leftovers");
    let tmp = d.0.join("tmp");
    let stage = staging_dir(&tmp, "leh3rc2hdcf31i");
    assert_eq!(stage, tmp.join("arena-replace-leh3rc2hdcf31i"));
    std::fs::create_dir_all(stage.join("ARENA_materials")).unwrap();
    std::fs::write(stage.join("ARENA_materials/notes.py"), vec![b'x'; 100_000]).unwrap();
    std::fs::create_dir_all(tmp.join("arena-replace-")).unwrap(); // no pod id: not ours
    std::fs::create_dir_all(tmp.join("unrelated")).unwrap();
    std::fs::write(tmp.join("arena-replace-file"), "a file, not a staging dir").unwrap();
    let root = d.0.join("backup");
    let partial = root.join("baseline/w1/.fleet.partial-99999999999");
    std::fs::create_dir_all(root.join("baseline/w1/fleet")).unwrap(); // a finished baseline: not a leftover
    // pid 99999999999 is no u32 pid: ignored; this one parses (and no such process runs).
    let partial_ok = root.join("baseline/w1/.fleet.partial-3999999");
    std::fs::create_dir_all(&partial).unwrap();
    std::fs::create_dir_all(&partial_ok).unwrap();
    std::fs::create_dir_all(root.join("w1d1/.rsync-tmp")).unwrap();
    std::fs::write(root.join("w1d1/.rsync-tmp/.notes.py.Ab12Cd"), "half").unwrap();
    std::fs::create_dir_all(root.join("big/.rsync-tmp")).unwrap(); // empty: nothing left
    let now = crate::snapshot::unix_now();

    let found = find_leftovers(&tmp, &[root.clone(), root.clone()], now);
    assert!(found.unread.is_empty(), "{:?}", found.unread);
    let found = found.leftovers;
    let kinds: Vec<(&Path, &LeftoverKind)> = found.iter().map(|l| (l.path.as_path(), &l.kind)).collect();
    // Fresh rsync temp files may be a running pull's: not listed yet.
    assert_eq!(
        kinds,
        [(partial_ok.as_path(), &LeftoverKind::Baseline { pid: 3_999_999 }), (stage.as_path(), &LeftoverKind::Staging { source: "leh3rc2hdcf31i".into() })]
    );
    let staged = found.iter().find(|l| l.path == stage).unwrap();
    assert!(staged.bytes >= 100_000 && !staged.capped && !staged.in_use, "{staged:?}");
    assert!(staged.changed.is_some_and(|c| now.saturating_sub(c) < 600), "{staged:?}");

    // A day later they are.
    let later = find_leftovers(&tmp, std::slice::from_ref(&root), now + RSYNC_TEMP_STALE_SECS + 60).leftovers;
    assert!(later.iter().any(|l| l.kind == LeftoverKind::RsyncTemp && l.path == root.join("w1d1/.rsync-tmp")), "{later:?}");

    // A run holding the staging dir's lock: in use, and said so — nothing to remove.
    let held = lock_when_free(&stage).unwrap();
    assert!(in_use(&stage));
    let e = lock_dir(&stage).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::WouldBlock);
    let busy = find_leftovers(&tmp, &[], now).leftovers;
    let l = busy.iter().find(|l| l.path == stage).unwrap();
    assert!(l.in_use);
    let lines = l.lines(now);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].starts_with(&format!("◷ {} — ", stage.display())) && lines[0].ends_with("in use now by a running arena"), "{lines:?}");
    drop(held);
    drop(lock_when_free(&stage).expect("free again"));
    // Nothing found was touched.
    assert!(stage.join("ARENA_materials/notes.py").exists() && partial_ok.exists() && root.join("w1d1/.rsync-tmp/.notes.py.Ab12Cd").exists());
    // Nothing there: nothing found, and nothing it couldn't read. A dir it can't list: said.
    assert_eq!(find_leftovers(&d.0.join("nowhere"), &[d.0.join("nowhere")], now), Found::default());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let shut = d.0.join("shut");
        std::fs::create_dir_all(&shut).unwrap();
        std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o000)).unwrap();
        let found = find_leftovers(&shut, &[], now);
        std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o700)).unwrap();
        // (root reads anything: then there's nothing to say)
        if std::fs::read_dir("/root").is_err() {
            assert!(found.unread.len() == 1 && found.unread[0].starts_with(&format!("{}: ", shut.display())), "{found:?}");
        }
    }
}

/// A cleanup holds the staging dir's lock while it removes it, so a run waits; once it's gone the
/// run's lock is refused (NotFound: make it again), and a new dir at that path is its own lock.
#[test]
fn a_staging_dir_being_removed_is_never_locked_for_a_copy() {
    let d = TmpDir::new("relock");
    let stage = staging_dir(&d.0, "abc");
    std::fs::create_dir_all(&stage).unwrap();
    let cleanup = lock_dir(&stage).unwrap();
    assert_eq!(lock_dir(&stage).unwrap_err().kind(), std::io::ErrorKind::WouldBlock, "the run waits");
    std::fs::remove_dir_all(&stage).unwrap();
    drop(cleanup);
    assert_eq!(lock_dir(&stage).unwrap_err().kind(), std::io::ErrorKind::NotFound, "gone");
    std::fs::create_dir_all(&stage).unwrap();
    let run = lock_dir(&stage).unwrap();
    assert!(in_use(&stage));
    drop(run);
}

#[test]
fn a_leftover_says_what_it_is_how_to_look_and_how_to_remove_it() {
    let now = 1_793_318_400; // 2026-10-30
    let staging = Leftover {
        path: PathBuf::from("/tmp/arena-replace-leh3rc2hdcf31i"),
        kind: LeftoverKind::Staging { source: "leh3rc2hdcf31i".into() },
        bytes: 3_500_000_000,
        capped: false,
        changed: Some(now - 16 * 86_400),
        in_use: false,
    };
    let lines = staging.lines(now);
    assert_eq!(
        lines[0],
        "⚠ /tmp/arena-replace-leh3rc2hdcf31i — 3.5 GB, last written 16 days ago: pod leh3rc2hdcf31i's home, staged by \
         `pods replace`/`migrate` and kept after a copy that failed or was stopped, or by a later attempt that copied \
         directly (or by a release from before 2026-10-19, which never removed it)"
    );
    assert!(lines[1].contains("it may be the only copy of that pod's work") && lines[1].contains("`arena pods list --no-probe | grep -F -- leh3rc2hdcf31i`"), "{lines:?}");
    assert_eq!(lines[2], "  └ look: du -sh /tmp/arena-replace-leh3rc2hdcf31i; ls -la /tmp/arena-replace-leh3rc2hdcf31i");
    assert_eq!(lines[3], "  └ remove: rm -rf /tmp/arena-replace-leh3rc2hdcf31i");
    // An odd path is quoted for the shell; a capped size is a lower bound; no caution off staging.
    let odd = Leftover {
        path: PathBuf::from("/srv/back up/w1d1/.rsync-tmp"),
        kind: LeftoverKind::RsyncTemp,
        bytes: 2 * G,
        capped: true,
        changed: None,
        in_use: false,
    };
    assert_eq!(odd.commands().1, "rm -rf '/srv/back up/w1d1/.rsync-tmp'");
    assert_eq!(odd.facts(now), "over 2 GB");
    assert_eq!(odd.caution(), None);
    assert_eq!(odd.lines(now).len(), 3);
    // A pod id from a dir's name is quoted in the advice: nothing in it runs when pasted.
    let named = |source: &str| Leftover { kind: LeftoverKind::Staging { source: source.into() }, ..staging.clone() }.caution().unwrap();
    assert!(named("$(touch PWNED)").contains("grep -F -- '$(touch PWNED)'`"), "{}", named("$(touch PWNED)"));
    assert!(named("-rf").contains("grep -F -- '-rf'`"));
    assert!(named("it's").contains(r"grep -F -- 'it'\''s'`"));
}

#[test]
fn skips_are_counted_per_pod_and_kind_and_forgotten() {
    let t0 = 1_793_318_400; // 2026-10-30 00:00 UTC
    let mut s = Skips::default();
    let skip = |pod: &str, what: &str| (pod.to_string(), what.to_string(), "only 3.1 GB free on / (floor 5 GB, CONTROL_MIN_FREE_GB)".to_string());
    s.add(&[skip("alpha", "copy")], t0 + 9 * 3600);
    s.add(&[skip("alpha", "copy"), skip("bravo", "staging")], t0 + 10 * 3600 + 15 * 60);
    assert_eq!(s.entries.len(), 2);
    let a = &s.entries[0];
    assert_eq!((a.pod.as_str(), a.count, a.first, a.last), ("alpha", 2, t0 + 9 * 3600, t0 + 10 * 3600 + 900));
    let now = t0 + 11 * 3600;
    assert_eq!(
        a.line(now),
        "✗ alpha: copy skipped 2× (since 09:00 UTC, last 10:15 UTC) — only 3.1 GB free on / (floor 5 GB, CONTROL_MIN_FREE_GB)"
    );
    assert!(s.entries[1].line(now).starts_with("✗ bravo: staging skipped at 10:15 UTC — "));
    assert_eq!(s.recent(now).len(), 2);
    // A day without one: not recent; the next one starts a new count.
    let next_day = t0 + 10 * 3600 + 900 + SKIP_WINDOW_SECS;
    assert_eq!(s.recent(next_day).len(), 0);
    s.add(&[skip("alpha", "copy")], next_day);
    let a = s.entries.iter().find(|e| e.pod == "alpha").unwrap();
    assert_eq!((a.count, a.first), (1, next_day));
    assert!(a.line(next_day + 60).contains("skipped at 10:15 UTC"), "{}", a.line(next_day + 60));
    assert!(a.line(next_day + 86_400).contains("skipped at 2026-10-31 10:15 UTC"), "{}", a.line(next_day + 86_400));
    // A week later every record that didn't recur is dropped at the next write.
    s.add(&[], next_day + FORGET_SKIPS_AFTER_SECS);
    assert_eq!(s.entries, []);
}

#[test]
fn the_skip_record_is_saved_privately_and_read_back_tolerantly() {
    let d = TmpDir::new("skips");
    let path = d.0.join("state/devtest").join(SKIPS_FILE);
    let new = [("alpha".to_string(), "copy".to_string(), "only 1 GB free".to_string())];
    assert_eq!(record_skips(&path, &new, 100, RECORD_LOCK_WAIT).unwrap(), None);
    assert_eq!(record_skips(&path, &new, 200, RECORD_LOCK_WAIT).unwrap(), None);
    let (s, warning) = Skips::load(&path);
    assert_eq!((warning, s.entries.len(), s.entries[0].count), (None, 1, 2));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
    std::fs::write(&path, "not json").unwrap();
    let (s, warning) = Skips::load(&path);
    assert!(s.entries.is_empty() && warning.is_some_and(|w| w.starts_with("warning: ignoring")));
    // A corrupt record is replaced by the next write, which then holds only the new skip.
    assert!(record_skips(&path, &new, 300, RECORD_LOCK_WAIT).unwrap().is_some());
    assert_eq!(Skips::load(&path).0.entries[0].count, 1);
    assert_eq!(Skips::load(&d.0.join("missing.json")), (Skips::default(), None));
}
