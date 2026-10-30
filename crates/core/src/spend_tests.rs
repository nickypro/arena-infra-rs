//! Table tests for the spend ledger's fold, views and state file.

use super::*;
use crate::error::Error;

const T0: u64 = 1_793_000_000 - 1_793_000_000 % 3_600; // an hour boundary (2026-10-28)
const H: u64 = 3_600;
const M: u64 = 60;

fn naming() -> Naming<'static> {
    Naming { prefix: "devtest", list: &[] }
}

fn pod(provider: &str, id: &str, name: &str, status: &str, price: Option<f64>) -> Pod {
    Pod {
        provider: provider.into(),
        id: id.into(),
        name: name.into(),
        status: status.into(),
        cost_per_hr: price,
        ..Pod::default()
    }
}

fn running(provider: &str, id: &str, name: &str, price: f64) -> Pod {
    pod(provider, id, name, "RUNNING", Some(price))
}

/// A listing where every provider in `ok` listed `pods` (theirs) and every one in `failed` failed.
fn listing(ok: &[&str], failed: &[&str], pods: &[Pod]) -> Listing {
    let mut r: Vec<(String, crate::error::Result<Vec<Pod>>)> = ok
        .iter()
        .map(|p| (p.to_string(), Ok(pods.iter().filter(|x| x.provider == *p).cloned().collect())))
        .collect();
    r.extend(failed.iter().map(|p| (p.to_string(), Err(Error::provider("list HTTP 503")))));
    Listing::from_results(r)
}

fn see(state: SpendState, ok: &[&str], failed: &[&str], pods: &[Pod], now: u64) -> SpendState {
    let l = listing(ok, failed, pods);
    let listed = l.pods();
    observe(state, &l, &listed, &naming(), now)
}

/// A listing every 20 minutes from `from` to `to` (inclusive), as the snapshot cron makes.
fn watch(mut state: SpendState, ok: &[&str], pods: &[Pod], from: u64, to: u64) -> SpendState {
    let mut t = from;
    while t <= to {
        state = see(state, ok, &[], pods, t);
        t += 20 * M;
    }
    state
}

/// Every provider listed (empty) 10 minutes before `T0`: a machine first seen at `T0` didn't
/// run unseen before.
fn tracked() -> SpendState {
    see(SpendState::default(), &["runpod", "vast", "hetzner"], &[], &[], T0 - 10 * M)
}

fn given(owner: &str) -> OwnerRule {
    OwnerRule::Given(owner.into())
}

fn created(state: SpendState, p: &Pod, owner: &OwnerRule, now: u64) -> SpendState {
    apply_event(state, &Event::Created { pod: p, owner }, now)
}

fn machine<'a>(s: &'a SpendState, provider: &str, id: &str) -> &'a Machine {
    s.machines.iter().find(|m| m.provider == provider && m.id == id).expect("a row")
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

fn rep(s: &SpendState, since: Option<u64>, now: u64) -> Report {
    report(s, &naming(), since, now, None, None)
}

fn owner_row<'a>(r: &'a Report, owner: Option<&str>) -> &'a OwnerRow {
    r.owners.iter().find(|o| o.owner.as_deref() == owner).unwrap_or_else(|| panic!("no row for {owner:?}: {:?}", r.owners))
}

// ---- the fold ------------------------------------------------------------------------

#[test]
fn a_name_reused_for_someone_else_is_a_new_row_and_the_old_one_keeps_its_owner() {
    let a1 = running("hetzner", "101", "devtest-alpha", 0.01);
    let a2 = running("hetzner", "202", "devtest-alpha", 0.01);
    let mut s = created(SpendState::default(), &a1, &given("alice"), T0);
    s = see(s, &["hetzner"], &[], std::slice::from_ref(&a1), T0 + H);
    s = apply_event(s, &Event::Terminated { provider: Some("hetzner"), id: "101" }, T0 + 2 * H);
    s = created(s, &a2, &given("bob"), T0 + 2 * H + M);
    s = see(s, &["hetzner"], &[], std::slice::from_ref(&a2), T0 + 3 * H + M);
    assert_eq!(s.machines.len(), 2, "{:#?}", s.machines);
    let old = machine(&s, "hetzner", "101");
    assert_eq!((old.owner.as_deref(), old.ended.map(|e| e.how)), (Some("alice"), Some(EndHow::Terminated)));
    assert_eq!(old.ended.unwrap().at, T0 + 2 * H, "a terminate's end is exact");
    assert!(close(old.accrued, 0.02), "2 h at €0.01: {}", old.accrued);
    let new = machine(&s, "hetzner", "202");
    assert_eq!((new.owner.as_deref(), new.ended), (Some("bob"), None));
    let r = rep(&s, None, T0 + 3 * H + M);
    assert!(close(owner_row(&r, Some("alice")).compute["EUR"], 0.02));
    assert!(close(owner_row(&r, Some("bob")).compute["EUR"], 0.01));
    assert_eq!(owner_row(&r, Some("alice")).machines, ["alpha"]);
}

#[test]
fn an_owner_is_assigned_and_fixed_later_by_name_or_instance_id() {
    let p = running("runpod", "rp1", "devtest-bravo", 0.20);
    let mut s = see(SpendState::default(), &["runpod"], &[], std::slice::from_ref(&p), T0);
    assert_eq!(machine(&s, "runpod", "rp1").owner, None);
    let names = vec!["devtest-bravo".to_string()];
    let (s2, what) = assign(s.clone(), &names, "bravo", Some("carol"), T0 + M).unwrap();
    assert_eq!(what, Assigned::Machine { name: "devtest-bravo".into(), provider: "runpod".into(), id: "rp1".into(), was: None });
    s = s2;
    let (s2, what) = assign(s, &[], "rp1", Some("dave"), T0 + 2 * M).unwrap();
    assert!(matches!(what, Assigned::Machine { was: Some(ref w), .. } if w == "carol"));
    assert_eq!(machine(&s2, "runpod", "rp1").owner.as_deref(), Some("dave"));
    let (s3, _) = assign(s2, &[], "rp1", None, T0 + 3 * M).unwrap();
    assert_eq!(machine(&s3, "runpod", "rp1").owner, None, "cleared");
    assert!(assign(s3, &[], "nope", Some("x"), T0).unwrap_err().contains("no machine"));
}

#[test]
fn a_name_held_by_two_running_instances_needs_the_id() {
    let mut s = see(
        SpendState::default(),
        &["runpod"],
        &[],
        &[running("runpod", "a", "devtest-x", 0.2), running("runpod", "b", "devtest-x", 0.2)],
        T0,
    );
    let err = assign(s.clone(), &["devtest-x".into()], "x", Some("al"), T0).unwrap_err();
    assert!(err.contains("2 running instances"), "{err}");
    s = assign(s, &[], "b", Some("al"), T0).unwrap().0;
    assert_eq!(machine(&s, "runpod", "b").owner.as_deref(), Some("al"));
}

#[test]
fn a_long_silence_is_charged_at_the_higher_end_and_flagged_as_erring_either_way() {
    let p = running("vast", "9", "devtest-c", 0.30);
    let mut s = see(SpendState::default(), &["vast"], &[], std::slice::from_ref(&p), T0);
    s = see(s, &["vast"], &[], std::slice::from_ref(&p), T0 + 10 * M);
    s = see(s, &["vast"], &[], std::slice::from_ref(&p), T0 + 3 * H);
    let m = machine(&s, "vast", "9");
    assert_eq!(m.segs.len(), 2, "the silence splits the segment: {:?}", m.segs);
    assert!(close(m.accrued, 0.90), "3 h at $0.30: {}", m.accrued);
    let r = rep(&s, None, T0 + 3 * H);
    let row = &r.machines[0];
    assert!(row.estimates.contains(&Est::Gap), "{:?}", row.estimates);
    assert_eq!(Est::Gap.errs(), Errs::Either, "it may have been stopped, or run at another price, meanwhile");
    assert!(close(row.estimated, 0.30 * (3.0 - 10.0 / 60.0)), "{}", row.estimated);
    // Short silences aren't estimates.
    let mut s = see(SpendState::default(), &["vast"], &[], std::slice::from_ref(&p), T0);
    s = see(s, &["vast"], &[], &[p], T0 + 20 * M);
    assert!(!machine(&s, "vast", "9").estimate);
}

#[test]
fn a_machine_that_vanished_between_listings_ends_at_the_first_listing_without_it() {
    let p = running("vast", "9", "devtest-c", 0.60);
    let mut s = watch(tracked(), &["vast"], &[p], T0, T0 + H);
    s = see(s, &["vast"], &[], &[], T0 + H + 10 * M);
    let m = machine(&s, "vast", "9");
    assert_eq!(m.ended, Some(Ended { at: T0 + H + 10 * M, how: EndHow::Vanished }));
    assert!(close(m.accrued, 0.70), "{}", m.accrued);
    let row = &rep(&s, None, T0 + 2 * H).machines[0];
    assert_eq!(row.estimates, [Est::Vanished]);
    assert!(close(row.estimated, 0.10), "the 10 min after the last sighting: {}", row.estimated);
}

#[test]
fn a_listing_that_shows_a_vanished_machine_again_reopens_it() {
    let p = running("vast", "9", "devtest-c", 0.60);
    let mut s = see(SpendState::default(), &["vast"], &[], std::slice::from_ref(&p), T0);
    s = see(s, &["vast"], &[], &[], T0 + 5 * M);
    assert!(!machine(&s, "vast", "9").is_open());
    s = see(s, &["vast"], &[], &[p], T0 + 10 * M);
    let m = machine(&s, "vast", "9");
    assert!(m.is_open());
    assert_eq!(s.machines.len(), 1);
    assert!(close(m.accrued, 0.10), "{}", m.accrued);
}

#[test]
fn a_machine_arena_terminated_that_still_lists_for_a_while_stays_ended() {
    let p = running("hetzner", "7", "devtest-a", 0.01);
    let s = created(SpendState::default(), &p, &given("al"), T0);
    let s = apply_event(s, &Event::Terminated { provider: Some("hetzner"), id: "7" }, T0 + H);
    let deleting = pod("hetzner", "7", "devtest-a", "deleting", Some(0.01));
    let s = see(s, &["hetzner"], &[], std::slice::from_ref(&deleting), T0 + H + M);
    assert_eq!(machine(&s, "hetzner", "7").ended, Some(Ended { at: T0 + H, how: EndHow::Terminated }), "winding down");
    // Still running 10 minutes on: the terminate didn't take.
    let s = see(s, &["hetzner"], &[], std::slice::from_ref(&p), T0 + H + TERMINATE_GRACE);
    assert!(machine(&s, "hetzner", "7").is_open());
}

#[test]
fn an_incomplete_listing_ends_no_machine_on_the_provider_that_failed() {
    let rp = running("runpod", "rp1", "devtest-a", 0.20);
    let hz = running("hetzner", "7", "devtest-b", 0.01);
    let mut s = see(SpendState::default(), &["runpod", "hetzner"], &[], &[rp.clone(), hz.clone()], T0);
    // RunPod fails; Hetzner lists, without its server: only Hetzner's row ends.
    s = see(s, &["hetzner"], &["runpod"], &[], T0 + 10 * M);
    assert!(machine(&s, "runpod", "rp1").is_open(), "a failed listing ends nothing");
    assert_eq!(machine(&s, "runpod", "rp1").last_seen, T0, "nor moves it");
    assert!(!machine(&s, "hetzner", "7").is_open());
    // Every provider failed: nothing changes at all.
    let before = s.clone();
    let after = see(s, &[], &["runpod", "hetzner"], &[], T0 + 20 * M);
    assert_eq!(after, before);
}

#[test]
fn a_provider_not_asked_ends_nothing() {
    let rp = running("runpod", "rp1", "devtest-a", 0.20);
    let s = see(SpendState::default(), &["runpod"], &[], &[rp], T0);
    let s = see(s, &["hetzner"], &[], &[], T0 + H);
    assert!(machine(&s, "runpod", "rp1").is_open());
}

#[test]
fn a_create_that_a_listing_doesnt_show_yet_isnt_gone() {
    let p = running("runpod", "new", "devtest-a", 0.20);
    let s = created(SpendState::default(), &p, &given("al"), T0);
    let s = see(s, &["runpod"], &[], &[], T0 + 30);
    assert!(machine(&s, "runpod", "new").is_open(), "within the create grace");
    let s = see(s, &["runpod"], &[], &[], T0 + CREATE_GRACE + 1);
    assert!(!machine(&s, "runpod", "new").is_open(), "past it");
}

#[test]
fn stop_and_start_bill_the_volume_disk_only_while_stopped() {
    // RunPod: $0.20/h + a 40 GB container disk while running ($0.10/GB-month), a 20 GB volume
    // stopped ($0.20/GB-month).
    let mut p = running("runpod", "rp1", "devtest-a", 0.20);
    p.disk_gb = Some(40.0);
    p.volume_gb = Some(20.0);
    let disk = 40.0 * 0.10 / 720.0;
    let vol = 20.0 * 0.20 / 720.0;
    let s = created(SpendState::default(), &p, &given("al"), T0);
    let s = apply_event(s, &Event::Stopped { provider: None, id: "rp1" }, T0 + H);
    let s = apply_event(s, &Event::Started { provider: Some("runpod"), id: "rp1" }, T0 + 3 * H);
    let s = apply_event(s, &Event::Terminated { provider: Some("runpod"), id: "rp1" }, T0 + 4 * H);
    let m = machine(&s, "runpod", "rp1");
    let want = 2.0 * (0.20 + disk) + 2.0 * vol;
    assert!(close(m.accrued, want), "{} vs {want}: {:?}", m.accrued, m.segs);
    assert!(!m.estimate, "every change was recorded when made: {:?}", m.segs);
    let row = &rep(&s, None, T0 + 5 * H).machines[0];
    assert!(close(row.hours, 2.0), "running hours only: {}", row.hours);
}

#[test]
fn a_stop_seen_by_a_listing_is_charged_at_the_higher_rate_up_to_it() {
    let mut p = running("runpod", "rp1", "devtest-a", 0.40);
    p.volume_gb = Some(0.0);
    let mut s = watch(tracked(), &["runpod"], std::slice::from_ref(&p), T0, T0 + H);
    p.status = "EXITED".into();
    s = watch(s, &["runpod"], &[p], T0 + H + 10 * M, T0 + 2 * H);
    let m = machine(&s, "runpod", "rp1");
    // Running for the hour seen, the 10 min to the listing that saw it stopped at the
    // higher end, then nothing (no volume).
    assert!(close(m.accrued, 0.40 * (1.0 + 10.0 / 60.0)), "{}", m.accrued);
    assert!(!m.estimate, "a 10-minute step isn't an estimate");
}

#[test]
fn a_price_change_starts_a_new_segment() {
    let mut s = watch(tracked(), &["vast"], &[running("vast", "9", "devtest-c", 0.30)], T0, T0 + H);
    s = watch(s, &["vast"], &[running("vast", "9", "devtest-c", 0.50)], T0 + H + 5 * M, T0 + 2 * H + 5 * M);
    let m = machine(&s, "vast", "9");
    assert_eq!(m.segs.len(), 2);
    assert!(close(m.accrued, 0.30 + 0.50 * (5.0 / 60.0) + 0.50), "{}", m.accrued);
}

#[test]
fn a_listing_without_a_price_keeps_the_instances_last_one() {
    let s = created(SpendState::default(), &running("runpod", "rp1", "devtest-a", 0.25), &OwnerRule::None, T0);
    let s = watch(s, &["runpod"], &[pod("runpod", "rp1", "devtest-a", "RUNNING", None)], T0 + 20 * M, T0 + H);
    let m = machine(&s, "runpod", "rp1");
    assert_eq!(m.segs.len(), 1, "remembered = the same rate: {:?}", m.segs);
    assert!(close(m.accrued, 0.25));
    // Never priced at all: 0, flagged as erring low.
    let s = watch(SpendState::default(), &["runpod"], &[pod("runpod", "x", "devtest-b", "RUNNING", None)], T0, T0 + H);
    let row = &rep(&s, None, T0 + H).machines[0];
    assert_eq!((row.amount, row.estimates.as_slice()), (0.0, [Est::StartUnknown, Est::Unpriced].as_slice()));
    assert_eq!(Est::Unpriced.errs(), Errs::Low);
}

#[test]
fn two_currencies_are_never_summed() {
    let mut s = created(SpendState::default(), &running("hetzner", "7", "devtest-h", 0.01), &given("al"), T0);
    s = created(s, &running("runpod", "rp1", "devtest-r", 0.20), &given("al"), T0);
    s = see(s, &["hetzner", "runpod"], &[], &[running("hetzner", "7", "devtest-h", 0.01), running("runpod", "rp1", "devtest-r", 0.20)], T0 + H);
    let r = rep(&s, None, T0 + H);
    let al = owner_row(&r, Some("al"));
    assert_eq!(al.compute.keys().collect::<Vec<_>>(), ["EUR", "USD"]);
    assert!(close(al.compute["EUR"], 0.01) && close(al.compute["USD"], 0.20));
    assert_eq!(amounts(&al.total), "€0.010 + $0.20");
    // A Hetzner account read as USD sums with RunPod's.
    s.currencies.insert("hetzner".into(), "USD".into());
    let r = rep(&s, None, T0 + H);
    assert_eq!(amounts(&owner_row(&r, Some("al")).compute), "$0.21");
}

#[test]
fn only_this_fleets_names_get_rows_but_a_tracked_machine_renamed_away_stays_tracked() {
    let mine = running("runpod", "a", "devtest-a", 0.2);
    let theirs = running("runpod", "b", "arena9-z", 0.2);
    let s = see(SpendState::default(), &["runpod"], &[], &[mine.clone(), theirs], T0);
    assert_eq!(s.machines.len(), 1);
    let renamed = running("runpod", "a", "scratch", 0.2);
    let s = see(s, &["runpod"], &[], &[renamed], T0 + 10 * M);
    let m = machine(&s, "runpod", "a");
    assert!(m.is_open(), "renamed, not gone");
    assert_eq!(m.name, "scratch");
}

#[test]
fn a_machine_seen_after_tracking_began_starts_at_its_providers_creation_time() {
    let s = see(SpendState::default(), &["hetzner"], &[], &[], T0);
    let mut p = running("hetzner", "7", "devtest-h", 0.012);
    p.created_at = Some(T0 + 10 * M);
    let s = see(s, &["hetzner"], &[], std::slice::from_ref(&p), T0 + H);
    let m = machine(&s, "hetzner", "7");
    assert_eq!((m.start, m.start_how), (T0 + 10 * M, StartHow::Provider));
    assert!(close(m.accrued, 0.012 * 50.0 / 60.0), "{}", m.accrued);
    let row = &rep(&s, None, T0 + H).machines[0];
    assert_eq!(row.estimates, [Est::BeforeSeen], "50 min before it was seen");
    // Created before tracking began: counted from the first sighting, flagged as erring low.
    let mut old = p.clone();
    old.created_at = Some(T0 - 30 * 86_400);
    let s = see(SpendState::default(), &["hetzner"], &[], &[old], T0);
    let m = machine(&s, "hetzner", "7");
    assert_eq!((m.start, m.start_how), (T0, StartHow::Seen));
    assert!(rep(&s, None, T0 + H).machines[0].estimates.contains(&Est::StartUnknown));
}

#[test]
fn a_replacement_inherits_the_owner_of_the_machine_it_replaces_only_when_asked() {
    let old = running("runpod", "a", "devtest-x", 0.2);
    let new = running("vast", "9", "devtest-x", 0.2);
    let s = created(SpendState::default(), &old, &given("al"), T0);
    let inherited = created(s.clone(), &new, &OwnerRule::Inherit, T0 + H);
    assert_eq!(machine(&inherited, "vast", "9").owner.as_deref(), Some("al"));
    let plain = created(s, &new, &OwnerRule::None, T0 + H);
    assert_eq!(machine(&plain, "vast", "9").owner, None, "a forgotten --owner stays visible");
}

#[test]
fn an_event_for_an_id_on_two_providers_changes_nothing() {
    let s = created(SpendState::default(), &running("vast", "77", "devtest-a", 0.2), &OwnerRule::None, T0);
    let s = created(s, &running("hetzner", "77", "devtest-b", 0.01), &OwnerRule::None, T0);
    let s = apply_event(s, &Event::Terminated { provider: None, id: "77" }, T0 + H);
    assert!(s.machines.iter().all(Machine::is_open));
    let s = apply_event(s, &Event::Terminated { provider: Some("vast"), id: "77" }, T0 + H);
    assert!(!machine(&s, "vast", "77").is_open() && machine(&s, "hetzner", "77").is_open());
}

#[test]
fn the_dashboards_refreshes_dont_rewrite_an_unchanged_machine() {
    let p = running("runpod", "a", "devtest-a", 0.2);
    let s = see(SpendState::default(), &["runpod"], &[], std::slice::from_ref(&p), T0);
    let same = see(s.clone(), &["runpod"], &[], std::slice::from_ref(&p), T0 + 5);
    assert_eq!(same, s, "under REFRESH: nothing to write");
    let moved = see(s.clone(), &["runpod"], &[], &[p], T0 + REFRESH);
    assert_eq!(moved.machines[0].last_seen, T0 + REFRESH);
    // A clock that went back records nothing.
    let back = see(moved.clone(), &["runpod"], &[], &[running("runpod", "a", "devtest-a", 0.9)], T0);
    assert_eq!(back.machines[0].segs, moved.machines[0].segs);
}

#[test]
fn a_provider_listing_a_machine_as_terminated_ends_it() {
    let p = running("vast", "9", "devtest-a", 0.2);
    let s = see(SpendState::default(), &["vast"], &[], &[p], T0);
    let s = see(s, &["vast"], &[], &[pod("vast", "9", "devtest-a", "DESTROYED", Some(0.2))], T0 + 10 * M);
    assert_eq!(machine(&s, "vast", "9").ended.map(|e| e.how), Some(EndHow::Gone));
}

#[test]
fn an_older_listing_neither_ends_nor_reopens_what_a_newer_observation_recorded() {
    let p = running("vast", "9", "devtest-c", 0.30);
    let s = see(tracked(), &["vast"], &[], std::slice::from_ref(&p), T0 + H);
    // A listing taken before that sighting (a slow command, a clock gone back) without it.
    let s = see(s, &["vast"], &[], &[], T0 + 30 * M);
    assert!(machine(&s, "vast", "9").is_open(), "older than its last sighting: says nothing");
    // Terminated at T0+2H; a listing from just before still shows it running.
    let s = apply_event(s, &Event::Terminated { provider: Some("vast"), id: "9" }, T0 + 2 * H);
    let s = see(s, &["vast"], &[], std::slice::from_ref(&p), T0 + 2 * H - M);
    assert_eq!(machine(&s, "vast", "9").ended, Some(Ended { at: T0 + 2 * H, how: EndHow::Terminated }));
    // Vanished at T0+3H (a later one), then a listing from before that shows it: still ended.
    let s = see(s, &["vast"], &[], std::slice::from_ref(&p), T0 + 2 * H + TERMINATE_GRACE);
    assert!(machine(&s, "vast", "9").is_open(), "the terminate didn't take");
    let s = see(s, &["vast"], &[], &[], T0 + 3 * H);
    let s = see(s, &["vast"], &[], &[p], T0 + 3 * H - M);
    assert_eq!(machine(&s, "vast", "9").ended, Some(Ended { at: T0 + 3 * H, how: EndHow::Vanished }));
}

#[test]
fn a_long_unpriced_silence_is_both_a_gap_and_unpriced() {
    let p = pod("runpod", "x", "devtest-b", "RUNNING", None);
    let mut s = see(tracked(), &["runpod"], &[], std::slice::from_ref(&p), T0);
    s = see(s, &["runpod"], &[], &[p], T0 + 3 * H);
    let row = &rep(&s, None, T0 + 3 * H).machines[0];
    assert_eq!(row.estimates, [Est::Gap, Est::Unpriced], "the zero is still flagged as erring low");
}

#[test]
fn a_machine_first_seen_after_its_provider_went_unlisted_may_have_run_before() {
    let rp = running("runpod", "rp1", "devtest-a", 0.30);
    // Tracking began on Hetzner; RunPod failed to list for a day, then shows a pod it doesn't
    // date (RunPod v1, Vast): it may have run all along.
    let s = see(SpendState::default(), &["hetzner"], &["runpod"], &[], T0);
    let s = see(s, &["hetzner", "runpod"], &[], std::slice::from_ref(&rp), T0 + 24 * H);
    let m = machine(&s, "runpod", "rp1");
    assert!(m.start_unsure && m.start == T0 + 24 * H, "{m:?}");
    let row = &rep(&s, None, T0 + 25 * H).machines[0];
    assert_eq!(row.estimates, [Est::StartUnknown]);
    assert_eq!(Est::StartUnknown.errs(), Errs::Low);
    // RunPod listed (empty) ten minutes before it appeared: it's new.
    let s = see(SpendState::default(), &["runpod"], &[], &[], T0);
    let s = see(s, &["runpod"], &[], &[], T0 + 24 * H - 10 * M);
    let s = see(s, &["runpod"], &[], &[rp], T0 + 24 * H);
    assert!(!machine(&s, "runpod", "rp1").start_unsure);
    assert!(rep(&s, None, T0 + 25 * H).machines[0].estimates.is_empty());
}

#[test]
fn the_last_listing_time_moves_at_most_every_refresh() {
    let s = see(SpendState::default(), &["runpod"], &[], &[], T0);
    assert_eq!(s.listed["runpod"], T0);
    let same = see(s.clone(), &["runpod"], &[], &[], T0 + 5);
    assert_eq!(same, s, "the dashboard's refreshes don't rewrite the file");
    assert_eq!(see(s.clone(), &["runpod"], &[], &[], T0 + REFRESH).listed["runpod"], T0 + REFRESH);
    assert_eq!(see(s, &["runpod"], &[], &[], T0 - H).listed["runpod"], T0, "a clock gone back never moves it");
}

// ---- the window ----------------------------------------------------------------------

#[test]
fn since_clips_machines_and_keys_to_the_window() {
    let day = 86_400;
    let p = running("hetzner", "7", "devtest-h", 0.01);
    let mut s = created(SpendState::default(), &p, &given("al"), T0);
    let mut k = KeyInfo { hash: "k1".into(), name: Some("devtest-h".into()), usage: Some(1.0), ..KeyInfo::default() };
    s = observe_keys(s, std::slice::from_ref(&k), &naming(), T0);
    // Observed every 20 min for two days (a cron), the key's usage growing $0.10 an hour.
    let mut t = T0 + 20 * M;
    while t <= T0 + 2 * day {
        s = see(s, &["hetzner"], &[], std::slice::from_ref(&p), t);
        k.usage = Some(1.0 + 0.1 * ((t - T0) / H) as f64);
        s = observe_keys(s, std::slice::from_ref(&k), &naming(), t);
        t += 20 * M;
    }
    let now = T0 + 2 * day;
    let all = rep(&s, None, now);
    assert!(close(all.total.compute["EUR"], 0.01 * 48.0), "{:?}", all.total);
    let since = floor_hour(T0 + day);
    let r = rep(&s, Some(since), now);
    assert!(close(r.total.compute["EUR"], 0.01 * ((now - since) as f64 / 3_600.0)), "{:?}", r.total);
    let al = owner_row(&r, Some("al"));
    assert!(close(al.ai, 0.1 * ((now - since) / H) as f64), "the usage since then: {}", al.ai);
    assert!(close(owner_row(&all, Some("al")).ai, 1.0 + 0.1 * 48.0), "all time: {}", owner_row(&all, Some("al")).ai);
}

#[test]
fn a_key_not_read_near_the_windows_start_is_flagged_as_erring_high() {
    let day = 86_400;
    // Read at $0 on Monday, $10 on Wednesday; a window from Tuesday counts all $10, flagged.
    let s = observe_keys(SpendState::default(), &[key("k1", "devtest-a", 0.0)], &naming(), T0);
    let s = observe_keys(s, &[key("k1", "devtest-a", 10.0)], &naming(), T0 + 2 * day);
    let r = rep(&s, Some(T0 + day), T0 + 2 * day);
    assert!(r.keys[0].base_stale && close(r.keys[0].usd, 10.0), "{:?}", r.keys);
    assert!(owner_row(&r, None).ai_estimate);
    assert!(notes(&r).iter().any(|n| n.contains("not read near the window's start") && n.contains("errs high")), "{:?}", notes(&r));
    // Read across Tuesday's start at $0 (the hourly cron): exact.
    let s = observe_keys(SpendState::default(), &[key("k1", "devtest-a", 0.0)], &naming(), T0);
    let s = observe_keys(s, &[key("k1", "devtest-a", 0.0)], &naming(), T0 + day + H);
    let s = observe_keys(s, &[key("k1", "devtest-a", 10.0)], &naming(), T0 + 2 * day);
    let r = rep(&s, Some(T0 + day), T0 + 2 * day);
    assert!(!r.keys[0].base_stale && close(r.keys[0].usd, 10.0));
    assert!(!owner_row(&r, None).ai_estimate);
}

#[test]
fn parse_since_takes_a_date_or_an_rfc3339_time() {
    assert_eq!(parse_since("2026-11-02"), Ok(1_793_577_600));
    assert_eq!(parse_since("2026-11-02T09:00:00Z"), Ok(1_793_577_600 + 9 * H));
    assert!(parse_since("monday").is_err());
    assert!(parse_since("2026-13-02").is_err());
}

// ---- AI keys ------------------------------------------------------------------------

fn key(hash: &str, name: &str, usage: f64) -> KeyInfo {
    KeyInfo { hash: hash.into(), name: Some(name.into()), usage: Some(usage), ..KeyInfo::default() }
}

#[test]
fn a_rotated_key_keeps_its_final_usage_with_the_old_owner() {
    let p = running("hetzner", "7", "devtest-a", 0.01);
    let mut s = created(SpendState::default(), &p, &given("alice"), T0);
    s = key_minted(s, "kA", "devtest-a", None, None, T0 + M);
    assert_eq!(key_owner(&s, "devtest-a"), Some("alice"));
    s = observe_keys(s, &[key("kA", "devtest-a", 1.25)], &naming(), T0 + H);
    // Rotate: the key as listed just before the delete is recorded with it, in one write.
    s = keys_deleted(s, &[key("kA", "devtest-a", 1.50)], &naming(), T0 + 2 * H);
    assert_eq!(s.keys[0].gone, Some(KeyGone { at: T0 + 2 * H, how: KeyEndHow::Deleted }));
    s = apply_event(s, &Event::Terminated { provider: Some("hetzner"), id: "7" }, T0 + 2 * H);
    let b = running("hetzner", "8", "devtest-a", 0.01);
    s = created(s, &b, &given("bob"), T0 + 2 * H + M);
    assert_eq!(handover_needed(&s, "devtest-a", "bob"), None, "the old key is gone");
    s = key_minted(s, "kB", "devtest-a", None, None, T0 + 2 * H + 2 * M);
    s = observe_keys(s, &[key("kB", "devtest-a", 0.30)], &naming(), T0 + 3 * H);
    let r = rep(&s, None, T0 + 3 * H);
    assert!(close(owner_row(&r, Some("alice")).ai, 1.50), "{:?}", r.owners);
    assert!(close(owner_row(&r, Some("bob")).ai, 0.30));
    assert!(!owner_row(&r, Some("alice")).ai_estimate, "deleted by arena: exact");
}

#[test]
fn a_key_deleted_outside_arena_keeps_its_last_seen_usage_flagged() {
    let mut s = observe_keys(SpendState::default(), &[key("k1", "devtest-a", 0.40)], &naming(), T0);
    s = observe_keys(s, &[], &naming(), T0 + H);
    assert_eq!(s.keys[0].gone, Some(KeyGone { at: T0 + H, how: KeyEndHow::Vanished }));
    let r = rep(&s, None, T0 + H);
    let row = owner_row(&r, None);
    assert!(close(row.ai, 0.40) && row.ai_estimate);
    assert!(notes(&r).iter().any(|n| n.contains("deleted outside arena")), "{:?}", notes(&r));
}

#[test]
fn a_key_minted_before_its_machine_adopts_the_machines_first_owner() {
    let s = observe_keys(SpendState::default(), &[key("k1", "devtest-a", 0.0)], &naming(), T0);
    assert_eq!(s.keys[0].owner, None);
    let s = created(s, &running("runpod", "rp", "devtest-a", 0.2), &given("al"), T0 + H);
    assert_eq!((s.keys[0].owner.as_deref(), s.keys[0].owner_how), (Some("al"), Some(KeyOwnerHow::Adopted)));
    // A key that already has an owner keeps it: the handover rotates it instead.
    let s = apply_event(s, &Event::Terminated { provider: Some("runpod"), id: "rp" }, T0 + 2 * H);
    let s = created(s, &running("runpod", "rp2", "devtest-a", 0.2), &given("bo"), T0 + 2 * H);
    assert_eq!(s.keys[0].owner.as_deref(), Some("al"));
    assert_eq!(handover_needed(&s, "devtest-a", "bo"), Some("al"));
    assert_eq!(handover_needed(&s, "devtest-a", "al"), None);
    assert_eq!(handover_needed(&s, "devtest-b", "bo"), None, "no key: nothing to hand over");
}

#[test]
fn other_fleets_keys_are_not_tracked_and_each_usage_value_keeps_the_span_it_was_read_over() {
    let s = observe_keys(SpendState::default(), &[key("k1", "devtest-a", 0.1), key("k2", "arena9-a", 9.0)], &naming(), T0);
    assert_eq!(s.keys.len(), 1);
    let s = observe_keys(s, &[key("k1", "devtest-a", 0.2)], &naming(), T0 + 10 * M);
    let s = observe_keys(s, &[key("k1", "devtest-a", 0.2)], &naming(), T0 + 30 * M);
    let s = observe_keys(s, &[key("k1", "devtest-a", 0.3)], &naming(), T0 + H + M);
    let u = |from, to, usd| Usage { from, to, usd };
    assert_eq!(s.keys[0].usage, [u(T0, T0, 0.1), u(T0 + 10 * M, T0 + 30 * M, 0.2), u(T0 + H + M, T0 + H + M, 0.3)]);
    // A lower reading never lowers it (it extends the span of the value it had).
    let s = observe_keys(s, &[key("k1", "devtest-a", 0.1)], &naming(), T0 + 2 * H);
    assert_eq!(s.keys[0].usage.last(), Some(&u(T0 + H + M, T0 + 2 * H, 0.3)));
}

#[test]
fn a_final_reading_on_a_clock_that_went_back_still_counts() {
    let s = observe_keys(SpendState::default(), &[key("k1", "devtest-a", 1.0)], &naming(), T0 + H);
    // The delete's listing, stamped five minutes earlier, read $2: never lost.
    let s = keys_deleted(s, &[key("k1", "devtest-a", 2.0)], &naming(), T0 + 55 * M);
    assert!(close(s.keys[0].last_usage(), 2.0), "{:?}", s.keys[0].usage);
    assert_eq!(s.keys[0].gone.map(|g| g.how), Some(KeyEndHow::Deleted));
    // A listing older than the last reading doesn't call the key gone.
    let s = observe_keys(SpendState::default(), &[key("k2", "devtest-b", 1.0)], &naming(), T0 + H);
    let s = observe_keys(s, &[], &naming(), T0);
    assert_eq!(s.keys[0].gone, None);
}

#[test]
fn the_keys_are_due_a_listing_hourly() {
    let s = SpendState::default();
    assert!(keys_due(&s, T0), "never listed");
    let s = observe_keys(s, &[], &naming(), T0);
    assert!(!keys_due(&s, T0 + 30 * M));
    assert!(keys_due(&s, T0 + KEY_SAMPLE_EVERY));
    assert!(keys_due(&s, T0 - H), "a clock gone back");
}

#[test]
fn old_key_samples_thin_to_one_a_day() {
    let mut u = Vec::new();
    for i in 0..(40 * 24) {
        sample(&mut u, T0 + i * H, i as f64);
    }
    // The last 31 days hourly, the 9 before them one a day.
    assert!(u.len() <= 31 * 24 + 11, "{}", u.len());
    assert!(u.len() >= 31 * 24, "{}", u.len());
    assert_eq!(u.last().unwrap().usd, (40 * 24 - 1) as f64);
}

// ---- RunPod billing ----------------------------------------------------------------

fn billing_body(from: &str, to: &str, records: &[(&str, &str, f64)], count: Option<usize>) -> Value {
    let recs: Vec<Value> = records
        .iter()
        .map(|(id, start, usd)| serde_json::json!({"podId": id, "startTime": start, "endTime": start, "totalAmount": usd, "gpuAmount": usd, "diskAmount": 0, "cpuAmount": 0}))
        .collect();
    serde_json::json!({
        "metadata": {"query": {"bucketSize": "hour", "startTime": from, "endTime": to}, "recordCount": count.unwrap_or(recs.len()), "uniquePodCount": 1},
        "records": recs,
    })
}

#[test]
fn the_per_pod_billing_answer_is_parsed_and_a_paged_or_odd_one_refused() {
    // As read live 2026-10-30 (GET /v2/billing/pods?bucketSize=hour…).
    let v = billing_body("2026-10-27T00:00:00Z", "2026-10-28T00:00:00Z", &[("pqguv7aqyhmsgu", "2026-10-27T11:00:00Z", 0.17897615057881922)], None);
    let b = parse_pod_billing(&v, T0).unwrap();
    let h = crate::snapshot::parse_rfc3339("2026-10-27T11:00:00Z").unwrap();
    assert!(close(b.pods["pqguv7aqyhmsgu"][&h], 0.178976));
    assert!(parse_pod_billing(&billing_body("2026-10-27T00:00:00Z", "2026-10-28T00:00:00Z", &[("a", "2026-10-27T11:00:00Z", 0.1)], Some(5)), T0).unwrap_err().contains("paged"));
    let mut daily = v.clone();
    daily["metadata"]["query"]["bucketSize"] = "day".into();
    assert!(parse_pod_billing(&daily, T0).is_err());
    let mut neg = v;
    neg["records"][0]["totalAmount"] = (-1.0).into();
    assert!(parse_pod_billing(&neg, T0).is_err());
}

#[test]
fn settled_runpod_hours_are_what_runpod_billed_and_younger_ones_the_higher_reading() {
    let p = running("runpod", "rp1", "devtest-a", 0.30);
    let mut s = created(SpendState::default(), &p, &given("al"), T0);
    for i in 1..=12 {
        s = see(s, &["runpod"], &[], std::slice::from_ref(&p), T0 + i * 20 * M);
    }
    let now = T0 + 4 * H;
    // Billing: hours 0 and 1 settled ($0.28 and $0.31), hour 2 still filling ($0.10 of it),
    // hour 3 not there yet.
    let mut hours = BTreeMap::new();
    hours.insert(T0, 0.28);
    hours.insert(T0 + H, 0.31);
    hours.insert(T0 + 2 * H, 0.10);
    let billed = PodBilled { from: T0, to: now, pods: [("rp1".to_string(), hours)].into(), at: now - M };
    let account = Billed { buckets: vec![(T0, 0.28), (T0 + H, 0.31), (T0 + 2 * H, 0.50)], from: T0, to: now };
    let r = report(&s, &naming(), None, now, Some(&billed), Some(&account));
    let row = &r.machines[0];
    assert_eq!(row.source, Source::Billed);
    assert!(close(row.amount, 0.28 + 0.31 + 0.30 + 0.30), "{}", row.amount);
    assert_eq!(row.billed, Some(0.28 + 0.31 + 0.10));
    let a = r.runpod.unwrap();
    assert!(close(a.rows, 0.69) && close(a.account, 1.09), "{a:?}");
    assert!(notes(&r).iter().any(|n| n.contains("accounted for $0.69 of $1.09")), "{:?}", notes(&r));
}

#[test]
fn billing_corrects_a_runpod_estimate_in_its_settled_hours() {
    // Seen at T0 and again 3 h later: a gap, but RunPod billed those hours.
    let p = running("runpod", "rp1", "devtest-a", 0.30);
    let mut s = see(SpendState::default(), &["runpod"], &[], &[], T0 - H);
    s = created(s, &p, &OwnerRule::None, T0);
    s = see(s, &["runpod"], &[], std::slice::from_ref(&p), T0 + 3 * H);
    let now = T0 + 6 * H;
    let hours: BTreeMap<u64, f64> = [(T0, 0.3), (T0 + H, 0.0), (T0 + 2 * H, 0.0)].into();
    let billed = PodBilled { from: T0 - H, to: now, pods: [("rp1".to_string(), hours)].into(), at: now };
    let r = report(&s, &naming(), None, now, Some(&billed), None);
    let row = &r.machines[0];
    assert!(row.estimates.is_empty(), "billing settled every hour of the gap: {:?}", row.estimates);
    assert!(close(row.amount, 0.30), "{}", row.amount);
}

#[test]
fn a_settled_hour_the_answer_has_no_bucket_for_billed_nothing() {
    // Seen at T0 and again 3 h later (a gap metered at $0.30/h); RunPod billed hour 0 only —
    // stopped meanwhile with no volume, say.
    let p = running("runpod", "rp1", "devtest-a", 0.30);
    let mut s = see(SpendState::default(), &["runpod"], &[], &[], T0 - H);
    s = created(s, &p, &OwnerRule::None, T0);
    s = see(s, &["runpod"], &[], std::slice::from_ref(&p), T0 + 3 * H);
    let now = T0 + 6 * H;
    let sparse = PodBilled { from: T0 - H, to: now, pods: [("rp1".to_string(), [(T0, 0.3)].into())].into(), at: now };
    let row = &report(&s, &naming(), None, now, Some(&sparse), None).machines[0];
    assert!(close(row.amount, 0.30) && row.estimates.is_empty(), "{row:?}");
    assert_eq!(row.source, Source::Billed);
    // An answer without the pod at all says nothing about it: metering stands.
    let none = PodBilled { from: T0 - H, to: now, pods: BTreeMap::new(), at: now };
    let row = &report(&s, &naming(), None, now, Some(&none), None).machines[0];
    assert!(close(row.amount, 0.90) && row.estimates == [Est::Gap], "{row:?}");
}

#[test]
fn a_window_starting_mid_hour_takes_billing_for_whole_hours_inside_it_only() {
    let p = running("runpod", "rp1", "devtest-a", 1.0);
    let mut s = created(SpendState::default(), &p, &given("al"), T0);
    for i in 1..=12 {
        s = see(s, &["runpod"], &[], std::slice::from_ref(&p), T0 + i * 20 * M);
    }
    let now = T0 + 4 * H;
    let hours: BTreeMap<u64, f64> = (0..4).map(|i| (T0 + i * H, 1.0)).collect();
    let billed = PodBilled { from: T0, to: now, pods: [("rp1".to_string(), hours)].into(), at: now };
    let account = Billed { buckets: (0..4).map(|i| (T0 + i * H, 1.0)).collect(), from: T0, to: now };
    let r = report(&s, &naming(), Some(T0 + 30 * M), now, Some(&billed), Some(&account));
    let row = &r.machines[0];
    // The half hour metered, then three billed hours.
    assert!(close(row.amount, 3.5), "{row:?}");
    assert_eq!(row.billed, Some(3.0));
    let a = r.runpod.unwrap();
    assert!(close(a.rows, 3.0) && close(a.account, 3.0), "the footer takes the same whole hours: {a:?}");
}

#[test]
fn billing_back_to_a_pods_creation_counts_what_ran_before_arena_saw_it() {
    // Tracking began on Hetzner at T0; RunPod failed to list; its pod, made at T0-2H, is
    // first seen at T0+3H and watched to T0+6H.
    let mut p = running("runpod", "rp1", "devtest-a", 0.30);
    p.created_at = Some(T0 - 2 * H);
    let s = see(SpendState::default(), &["hetzner"], &["runpod"], &[], T0);
    let s = watch(s, &["runpod"], std::slice::from_ref(&p), T0 + 3 * H, T0 + 6 * H);
    let m = machine(&s, "runpod", "rp1");
    assert_eq!((m.start, m.start_how, m.start_unsure, m.created), (T0 + 3 * H, StartHow::Seen, true, Some(T0 - 2 * H)));
    let now = T0 + 6 * H;
    let hours: BTreeMap<u64, f64> = (0..8).map(|i| (T0 - 2 * H + i * H, 0.3)).collect();
    let since = Some(T0 - 2 * H);
    let billed = PodBilled { from: T0 - 2 * H, to: now + 2 * H, pods: [("rp1".to_string(), hours)].into(), at: now + 2 * H };
    let row = &report(&s, &naming(), since, now, Some(&billed), None).machines[0];
    assert!(row.estimates.is_empty(), "billing covers every hour before it was seen: {row:?}");
    assert!(close(row.amount, 0.3 * 8.0), "{row:?}");
    // Without billing: from its first sighting, flagged as erring low.
    let row = &report(&s, &naming(), since, now, None, None).machines[0];
    assert_eq!(row.estimates, [Est::StartUnknown]);
    assert!(close(row.amount, 0.9), "{row:?}");
}

// ---- renderers ---------------------------------------------------------------------

fn sample_state() -> SpendState {
    let mut s = created(SpendState::default(), &running("hetzner", "7", "devtest-alpha", 0.0104), &given("alice"), T0);
    s = created(s, &running("runpod", "rp1", "devtest-bravo", 0.25), &given("alice"), T0);
    s = created(s, &running("runpod", "rp2", "devtest-charlie", 0.25), &OwnerRule::None, T0);
    s = key_minted(s, "kA", "devtest-bravo", None, None, T0);
    let pods = [running("hetzner", "7", "devtest-alpha", 0.0104), running("runpod", "rp1", "devtest-bravo", 0.25), running("runpod", "rp2", "devtest-charlie", 0.25)];
    s = see(s, &["hetzner", "runpod"], &[], &pods, T0 + 2 * H);
    observe_keys(s, &[key("kA", "devtest-bravo", 1.5)], &naming(), T0 + 2 * H)
}

#[test]
fn the_owner_table_has_a_no_owner_row_last_and_totals_per_currency() {
    let r = rep(&sample_state(), None, T0 + 2 * H);
    let text = render_owners(&r);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0].split_whitespace().collect::<Vec<_>>(), ["OWNER", "MACHINES", "HOURS", "COMPUTE", "AI", "TOTAL"]);
    assert!(lines[1].starts_with("alice") && lines[1].contains("alpha, bravo") && lines[1].contains("€0.021 + $0.50"), "{text}");
    assert!(lines[1].contains("$1.50") && lines[1].contains("€0.021 + $2.00"), "{text}");
    assert!(lines[2].starts_with(NO_OWNER) && lines[2].contains("charlie"), "{text}");
    assert!(lines[3].starts_with("total") && lines[3].contains("3 instance(s)") && lines[3].contains("€0.021 + $2.50"), "{text}");
    assert!(lines[4].contains("tracked since"), "{text}");
}

#[test]
fn the_machine_table_and_csvs() {
    let r = rep(&sample_state(), None, T0 + 2 * H);
    let text = render_machines(&r, &naming());
    assert!(text.lines().next().unwrap().starts_with("NAME"));
    assert!(text.contains("alpha") && text.contains("hetzner"), "{text}");
    assert!(text.contains("running (seen 0s ago)"), "{text}");
    let csv = csv_machines(&r);
    let head = csv.lines().next().unwrap();
    assert_eq!(head, "name,id,provider,gpu,owner,start,end,end_how,hours,currency,cost,source,estimate,estimated");
    assert!(csv.lines().any(|l| l.starts_with("devtest-alpha,7,hetzner,") && l.contains(",EUR,0.0208,metered,")), "{csv}");
    let owners = csv_owners(&r);
    assert_eq!(owners.lines().next().unwrap(), "owner,machines,instances,hours,compute_usd,compute_eur,ai_usd,total_usd,total_eur,estimate");
    assert!(owners.lines().any(|l| l.starts_with("alice,alpha bravo,2,4.0000,0.5000,0.0208,1.5000,2.0000,0.0208,")), "{owners}");
    assert!(owners.lines().any(|l| l.starts_with("(no owner),charlie,1,")), "{owners}");
    assert!(owners.lines().last().unwrap().starts_with("total,"));
    assert_eq!(csv_field("a,b"), "\"a,b\"");
}

#[test]
fn the_report_serializes_for_json() {
    let r = rep(&sample_state(), Some(T0), T0 + 2 * H);
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v["owners"][0]["owner"], "alice");
    assert_eq!(v["machines"][0]["source"], "metered");
    assert!(v["since"].is_u64());
}

#[test]
fn owners_are_checked() {
    assert_eq!(parse_owner(" alice "), Ok("alice".into()));
    assert!(parse_owner("").is_err());
    assert!(parse_owner("a,b").is_err());
    assert!(parse_owner(NO_OWNER).is_err());
    assert!(parse_owner(&"x".repeat(OWNER_MAX + 1)).is_err());
}

// ---- the state file ----------------------------------------------------------------

struct TmpDir(std::path::PathBuf);

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tmp(tag: &str) -> TmpDir {
    let d = std::env::temp_dir().join(format!("arena-spend-{tag}-{}-{}", std::process::id(), crate::snapshot::unix_now()));
    std::fs::create_dir_all(&d).unwrap();
    TmpDir(d)
}

#[test]
fn the_state_file_is_private_round_trips_and_isnt_rewritten_when_nothing_changed() {
    use std::os::unix::fs::PermissionsExt;
    let d = tmp("file");
    let path = d.0.join("devtest").join(SPEND_FILE);
    let (s, w) = record(&path, T0, |_| sample_state()).unwrap();
    assert!(w.is_none());
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(load(&path), (s.clone(), Loaded::Ok));
    let before = std::fs::metadata(&path).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    record(&path, T0, |s| s).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), before, "unchanged: not rewritten");
}

#[test]
fn an_unreadable_or_newer_ledger_is_moved_aside_not_overwritten() {
    let d = tmp("aside");
    let path = d.0.join(SPEND_FILE);
    std::fs::write(&path, r#"{"version": 99, "machines": []}"#).unwrap();
    let (_, w) = record(&path, T0, |s| s).unwrap();
    let w = w.expect("a warning");
    assert!(w.contains("unsupported version 99") && w.contains("moved to"), "{w}");
    let aside = d.0.join(format!("{SPEND_FILE}.ignored-{T0}"));
    assert!(std::fs::read_to_string(aside).unwrap().contains("99"));
    assert_eq!(load(&path).1, Loaded::Ok);
    record_quietly(None, T0, |s| s);
}

#[test]
fn a_second_ignored_ledger_in_the_same_second_gets_a_name_of_its_own() {
    let d = tmp("aside2");
    let path = d.0.join(SPEND_FILE);
    std::fs::write(&path, "first").unwrap();
    record(&path, T0, |s| s).unwrap();
    std::fs::write(&path, "second").unwrap();
    let (_, w) = record(&path, T0, |s| s).unwrap();
    assert!(w.unwrap().contains(&format!("{SPEND_FILE}.ignored-{T0}-1")));
    assert_eq!(std::fs::read_to_string(d.0.join(format!("{SPEND_FILE}.ignored-{T0}"))).unwrap(), "first");
    assert_eq!(std::fs::read_to_string(d.0.join(format!("{SPEND_FILE}.ignored-{T0}-1"))).unwrap(), "second");
}

// ---- the HTTP reads ----------------------------------------------------------------

#[tokio::test]
async fn per_pod_billing_is_one_get_with_the_key_in_the_header_only() {
    use crate::http::test_server::{canned, serve};
    let body = billing_body("2026-10-27T00:00:00Z", "2026-10-28T00:00:00Z", &[("rp1", "2026-10-27T11:00:00Z", 0.5)], None).to_string();
    let srv = serve(vec![canned(200, "application/json", &body), canned(401, "application/json", r#"{"error":"bad key"}"#)]);
    let api = HttpSpendApi::with_runpod(&format!("{}/v2", srv.base), "rpa_SECRET");
    let from = crate::snapshot::parse_rfc3339("2026-10-27T00:00:00Z").unwrap();
    let b = api.pod_billing(from, from + 86_400, from + 86_400).await.unwrap().unwrap();
    assert_eq!(b.pods["rp1"].values().sum::<f64>(), 0.5);
    let line = srv.requests.lock().unwrap()[0].clone();
    assert!(line.starts_with("GET /v2/billing/pods?bucketSize=hour&startTime=2026-10-27T00%3A00%3A00Z"), "{line}");
    assert!(!line.contains("rpa_SECRET"));
    assert!(srv.headers.lock().unwrap()[0].iter().any(|h| h.eq_ignore_ascii_case("authorization: Bearer rpa_SECRET")));
    let err = api.pod_billing(from, from + 86_400, from + 86_400).await.unwrap().unwrap_err();
    assert!(err.contains("401"), "{err}");
    assert!(HttpSpendApi::from_config(&crate::config::Config::parse("")).pod_billing(0, 1, 1).await.is_none(), "no key: not asked");
}
