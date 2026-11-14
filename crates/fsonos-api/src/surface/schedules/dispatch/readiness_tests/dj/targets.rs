//! Scheduled DJ sets retain their admitted audience through the one durable
//! occurrence claim. Speaker operations use real loopback S1/S2 simulators.

use super::*;
use crate::surface::schedules::{ScheduleRequest, UpdateScheduleRequest};
use fsonos_core::policy::Client;
use fsonos_core::store::ActionFilter;
use std::collections::BTreeSet;

fn target(room: &str) -> ScheduleAction {
    ScheduleAction::DjStart {
        room: room.into(),
        mood: None,
        direction: None,
    }
}

fn probe() -> Probe {
    let probe = Probe::default();
    probe.ready.store(true, Ordering::SeqCst);
    probe.play.store(true, Ordering::SeqCst);
    probe
}

fn id(house: &House, room: &str) -> PlayerId {
    fsonos_core::resolve_room(&house.houses, room)
        .unwrap()
        .coordinator
        .id
        .clone()
}

fn started(probe: &Probe) -> BTreeSet<String> {
    probe
        .started_groups
        .lock()
        .unwrap()
        .iter()
        .map(|id| id.0.clone())
        .collect()
}

fn assert_reads_only(house: &House, since: usize) {
    assert!(
        house.sim.soap_log()[since..]
            .iter()
            .all(|call| call.action.starts_with("Get")),
        "admission and refusals must not change speakers"
    );
}

fn refresh(house: &mut House) {
    house.houses =
        fsonos_core::inventory::survey(&house.sim.lan(), &[], Duration::from_millis(500))
            .unwrap()
            .households;
    house.inventory(Ok(house.houses.clone()));
}

fn aliases(house: &mut House, directory: &tempfile::TempDir, text: &str) {
    let path = directory.path().join("aliases.toml");
    std::fs::write(&path, text).unwrap();
    Arc::get_mut(&mut house.surface).unwrap().aliases_file = Some(path);
}

fn claim(house: &House, schedule: &Schedule) {
    assert_eq!(
        claim_current(&house.surface, schedule, house.at, || Admission::Run).unwrap(),
        Admission::Run
    );
    assert_eq!(house.recorded(), Some(house.at));
}

#[test]
fn a_scheduled_set_deduplicates_groups_and_applies_direction_across_households() {
    for (name, expected) in [
        ("all", 3),
        ("all@S1", 1),
        ("all@S2", 2),
        ("selected", 2),
        ("here", 2),
    ] {
        let probe = probe();
        let mut action = target(name);
        let ScheduleAction::DjStart { direction, .. } = &mut action else {
            unreachable!()
        };
        *direction = Some(DjDirection {
            constraints: DjConstraints {
                include_artists: vec!["Synthetic artist".into()],
                ..DjConstraints::default()
            },
            for_secs: Some(1200),
        });
        let mut house = House::mixed(&action).with_dj(&probe);
        let member = fsonos_core::resolve_room(&house.houses, "Room1@S1")
            .unwrap()
            .player
            .ip;
        soap::join_group(&house.sim.lan(), member, &id(&house, "Room0@S1")).unwrap();
        refresh(&mut house);
        let directory = tempfile::tempdir().unwrap();
        aliases(
            &mut house,
            &directory,
            "[aliases]\nselected = ['Room0@S1', 'Room1@S1', 'Room0@S2']\n[defaults]\nroom = 'selected'\n",
        );
        let scheduler = Scheduler::new(&house.surface);
        house.tick(&scheduler, house.at);
        assert_eq!(house.recorded(), Some(house.at), "{name}");
        assert_eq!(probe.checks.load(Ordering::SeqCst), expected, "{name}");
        assert_eq!(probe.starts.load(Ordering::SeqCst), expected, "{name}");
        assert_eq!(
            started(&probe).len(),
            expected,
            "one start per coordinator: {name}"
        );
        let steering = DjSteer::Set {
            mood: None,
            constraints: SteerConstraints {
                include_artists: vec!["Synthetic artist".into()],
                ..SteerConstraints::default()
            },
            for_secs: Some(1200),
        };
        assert_eq!(
            *probe.previews.lock().unwrap(),
            vec![Some(steering.clone()); expected]
        );
        assert_eq!(*probe.steers.lock().unwrap(), vec![steering; expected]);
        let actions = house
            .surface
            .recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap();
        assert_eq!(
            actions.len(),
            1,
            "one routine occurrence has one action record"
        );
        assert_eq!(
            house
                .sim
                .soap_log()
                .iter()
                .filter(|call| call.action == "Play")
                .count(),
            expected
        );
        assert!(
            house
                .tick(&scheduler, house.at + chrono::TimeDelta::minutes(1))
                .is_empty()
        );
        assert_eq!(probe.starts.load(Ordering::SeqCst), expected);
    }
}

#[test]
fn every_selected_group_must_be_ready_before_the_occurrence_is_claimed() {
    let probe = probe();
    let house = House::mixed(&target("all")).with_dj(&probe);
    *probe.refuse_group.lock().unwrap() = Some(id(&house, "Room1@S2"));
    let scheduler = Scheduler::new(&house.surface);
    let before = house.sim.soap_log().len();
    house.tick(&scheduler, house.at);
    assert_eq!(house.recorded(), None);
    assert_eq!(
        probe.checks.load(Ordering::SeqCst),
        4,
        "readiness reports every group"
    );
    assert_eq!(probe.starts.load(Ordering::SeqCst), 0);
    assert_reads_only(&house, before);
    assert!(
        house
            .surface
            .recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap()
            .is_empty()
    );
    *probe.refuse_group.lock().unwrap() = None;
    house.tick(&scheduler, house.at + chrono::TimeDelta::minutes(1));
    assert_eq!(house.recorded(), Some(house.at));
    assert_eq!(probe.starts.load(Ordering::SeqCst), 4);
}

#[test]
fn a_single_group_that_expands_during_readiness_is_not_admitted() {
    for s2 in [false, true] {
        let probe = probe();
        let house = House::new(s2, &target("Room0")).with_dj(&probe);
        *probe.regroup_on_ready.lock().unwrap() = Some((
            house.sim.lan(),
            house.sim.player("Room1").unwrap().ip,
            id(&house, "Room0"),
        ));
        let schedule = house.surface.load_schedules().unwrap().remove(0);
        assert_eq!(
            ready(&house.surface, &schedule).unwrap_err().code,
            ErrorCode::NotCoordinator
        );
        assert_eq!(house.recorded(), None);
        assert_eq!(probe.starts.load(Ordering::SeqCst), 0);
        assert!(house.sim.soap_log().iter().all(|call| {
            call.action.starts_with("Get")
                || (call.action == "SetAVTransportURI"
                    && call.args.iter().any(|(name, value)| {
                        name == "CurrentURI" && value.starts_with("x-rincon:")
                    }))
        }));
    }
}

#[test]
fn a_claimed_single_group_cannot_expand_even_if_discovery_has_caught_up() {
    for s2 in [false, true] {
        let probe = probe();
        let mut house = House::new(s2, &target("Room0")).with_dj(&probe);
        let schedule = house.surface.load_schedules().unwrap().remove(0);
        let prepared = ready(&house.surface, &schedule).unwrap();
        claim(&house, &schedule);
        soap::join_group(
            &house.sim.lan(),
            house.sim.player("Room1").unwrap().ip,
            &id(&house, "Room0"),
        )
        .unwrap();
        refresh(&mut house);
        let before = house.sim.soap_log().len();
        assert_eq!(
            fire(&house.surface, &schedule, None, prepared)
                .unwrap_err()
                .code,
            ErrorCode::NotCoordinator
        );
        assert_reads_only(&house, before);
        assert_eq!(probe.starts.load(Ordering::SeqCst), 0);
        let scheduler = Scheduler::new(&house.surface);
        assert!(
            house
                .tick(&scheduler, house.at + chrono::TimeDelta::minutes(1))
                .is_empty()
        );
        let actions = house
            .surface
            .recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap();
        assert_eq!(actions.len(), 1);
        assert!(actions[0].action.result.contains("selected group changed"));
    }
}

#[test]
fn alias_edits_and_newly_discovered_households_do_not_expand_a_claimed_run() {
    for alias in [false, true] {
        let probe = probe();
        let mut house =
            House::mixed(&target(if alias { "selected" } else { "all" })).with_dj(&probe);
        let directory = tempfile::tempdir().unwrap();
        if alias {
            aliases(
                &mut house,
                &directory,
                "[aliases]\nselected = ['Room0@S1']\n",
            );
        } else {
            let selected = house
                .houses
                .iter()
                .filter(|h| h.player(&id(&house, "Room0@S1")).is_some())
                .cloned()
                .collect();
            house.inventory(Ok(selected));
        }
        let schedule = house.surface.load_schedules().unwrap().remove(0);
        let prepared = ready(&house.surface, &schedule).unwrap();
        claim(&house, &schedule);
        if alias {
            std::fs::write(
                directory.path().join("aliases.toml"),
                "[aliases]\nselected = ['Room0@S1', 'Room0@S2']\n",
            )
            .unwrap();
        } else {
            house.inventory(Ok(house.houses.clone()));
        }
        fire(&house.surface, &schedule, None, prepared).unwrap();
        let mut expected = BTreeSet::from([id(&house, "Room0@S1").0]);
        if !alias {
            expected.insert(id(&house, "Room1@S1").0);
        }
        assert_eq!(started(&probe), expected);
        let scheduler = Scheduler::new(&house.surface);
        assert!(
            house
                .tick(&scheduler, house.at + chrono::TimeDelta::minutes(1))
                .is_empty()
        );
    }
}

#[test]
fn partial_scheduled_starts_keep_one_complete_receipt_and_are_never_replayed() {
    let probe = probe();
    let house = House::mixed(&target("all")).with_dj(&probe);
    *probe.fail_group.lock().unwrap() = Some(id(&house, "Room1@S2"));
    let scheduler = Scheduler::new(&house.surface);
    house.tick(&scheduler, house.at);
    assert_eq!(house.recorded(), Some(house.at));
    assert_eq!(probe.starts.load(Ordering::SeqCst), 4);
    let actions = house
        .surface
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    assert_eq!(actions.len(), 1);
    let receipt = &actions[0].action.result;
    for room in ["Room0@S1", "Room1@S1", "Room0@S2", "Room1@S2"] {
        assert!(receipt.contains(room), "missing {room} in {receipt}");
    }
    assert!(receipt.contains("lost its acknowledgement"));
    assert!(receipt.contains("do not replay"));
    assert!(!fsonos_core::actions::undoable(
        actions[0].action.before_state.as_deref()
    ));
    assert!(
        house
            .tick(&scheduler, house.at + chrono::TimeDelta::minutes(1))
            .is_empty()
    );
    let restarted = Scheduler::new(&house.surface);
    assert!(
        house
            .tick(&restarted, house.at + chrono::TimeDelta::minutes(2))
            .is_empty()
    );
    assert_eq!(probe.starts.load(Ordering::SeqCst), 4);
}

#[test]
fn every_group_uses_the_saved_creators_caps_and_action_identity() {
    let probe = probe();
    let house = House::mixed(&target("all")).with_dj(&probe);
    *house.surface.policy.write().unwrap() =
        Arc::new(Policy::from_toml("[defaults]\nmax_volume = 25\n").unwrap());
    house
        .surface
        .with_store(|store| {
            let original = store.schedules()?.remove(0);
            let mut changed = original.clone();
            changed.creator = "mcp-stdio".into();
            assert!(store.replace_schedule(&original, &changed)?);
            Ok(())
        })
        .unwrap();
    for player in house.sim.players() {
        soap::set_volume(&house.sim.lan(), player.ip, 80).unwrap();
    }
    let scheduler = Scheduler::new(&house.surface);
    house.tick(&scheduler, house.at);
    assert_eq!(probe.starts.load(Ordering::SeqCst), 4);
    for player in house.sim.players() {
        assert_eq!(soap::get_volume(&house.sim.lan(), player.ip).unwrap(), 25);
    }
    let actions = house
        .surface
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].action.client, "mcp-stdio");
}

#[test]
fn a_claimed_run_rechecks_its_creators_current_policy_before_any_write() {
    let probe = probe();
    let house = House::mixed(&target("all")).with_dj(&probe);
    let schedule = house.surface.load_schedules().unwrap().remove(0);
    let prepared = ready(&house.surface, &schedule).unwrap();
    claim(&house, &schedule);
    *house.surface.policy.write().unwrap() =
        Arc::new(Policy::from_toml("[clients.cli]\ndeny = ['dj_start']\n").unwrap());
    let before = house.sim.soap_log().len();
    assert_eq!(
        fire(&house.surface, &schedule, None, prepared)
            .unwrap_err()
            .code,
        ErrorCode::PolicyDenied
    );
    assert_reads_only(&house, before);
    assert_eq!(probe.starts.load(Ordering::SeqCst), 0);
    let actions = house
        .surface
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    assert_eq!(actions.len(), 1);
    assert!(actions[0].action.decision.starts_with("deny:"));
    let scheduler = Scheduler::new(&house.surface);
    assert!(
        house
            .tick(&scheduler, house.at + chrono::TimeDelta::minutes(1))
            .is_empty()
    );
}

#[test]
fn create_and_edit_accept_complete_sets_without_speaker_writes() {
    let mut house = House::mixed(&target("all"));
    let directory = tempfile::tempdir().unwrap();
    aliases(
        &mut house,
        &directory,
        "[aliases]\nselected = ['Room0@S1', 'Room0@S2']\nincomplete = ['Room0@S1', 'Missing room']\n",
    );
    let request = |zone: &str| ScheduleRequest {
        when: "daily 07:30".into(),
        action: "dj_start".into(),
        zone: Some(zone.into()),
        ..ScheduleRequest::default()
    };
    let before = house.sim.soap_log().len();
    let saved = house
        .surface
        .add_schedule(&Client::Cli, &request("all@S2"))
        .unwrap();
    assert_eq!(saved.zone.as_deref(), Some("all@S2"));
    let updated = house
        .surface
        .update_schedule(
            &Client::Cli,
            &UpdateScheduleRequest {
                id: saved.id,
                expected_revision: saved.revision,
                schedule: request("selected"),
            },
        )
        .unwrap();
    assert_eq!(updated.zone.as_deref(), Some("selected"));
    assert_eq!(updated.creator, "cli");
    assert!(
        house
            .surface
            .add_schedule(&Client::Cli, &request("incomplete"))
            .is_err()
    );
    assert!(
        house
            .surface
            .update_schedule(
                &Client::Cli,
                &UpdateScheduleRequest {
                    id: updated.id,
                    expected_revision: updated.revision.clone(),
                    schedule: request("incomplete"),
                }
            )
            .is_err()
    );
    assert_eq!(
        house
            .surface
            .observed_schedule(updated.id)
            .unwrap()
            .0
            .revision(),
        updated.revision
    );
    assert_reads_only(&house, before);
}
