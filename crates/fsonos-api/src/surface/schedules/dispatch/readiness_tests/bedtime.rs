//! A scheduled bedtime set keeps the exact groups admitted before its durable
//! claim. Unready reads may wait; claimed partial writes never run again.

use super::*;
use crate::surface::schedules::{ScheduleRequest, UpdateScheduleRequest};
use fsonos_core::policy::Client;
use fsonos_core::store::ActionFilter;
use fsonos_types::PlayerId;
use std::collections::BTreeSet;

fn fading(room: &str) -> ScheduleAction {
    ScheduleAction::Pause {
        room: room.into(),
        fade_secs: 1,
    }
}

fn id(house: &House, room: &str) -> PlayerId {
    fsonos_core::resolve_room(&house.houses, room)
        .unwrap()
        .coordinator
        .id
        .clone()
}

fn play(house: &House) {
    for household in &house.houses {
        for group in &household.groups {
            let host = household.player(&group.coordinator).unwrap().ip;
            soap::set_av_transport_uri(
                &house.sim.lan(),
                host,
                "x-rincon-mp3radio://stream.example.invalid/bedtime.mp3",
                "",
            )
            .unwrap();
            soap::play(&house.sim.lan(), host).unwrap();
        }
    }
    for player in house.sim.players() {
        soap::set_volume(&house.sim.lan(), player.ip, 28).unwrap();
    }
}

fn aliases(house: &mut House, directory: &tempfile::TempDir, contents: &str) {
    let path = directory.path().join("aliases.toml");
    std::fs::write(&path, contents).unwrap();
    Arc::get_mut(&mut house.surface).unwrap().aliases_file = Some(path);
}

fn assert_reads_only(house: &House, since: usize) {
    assert!(
        house.sim.soap_log()[since..]
            .iter()
            .all(|call| call.action.starts_with("Get"))
    );
}

fn claim(house: &House, schedule: &Schedule) {
    assert_eq!(
        claim_current(&house.surface, schedule, house.at, None, || Admission::Run).unwrap(),
        Admission::Run
    );
    assert_eq!(house.recorded(), Some(house.at));
}

#[test]
fn a_scheduled_fade_deduplicates_groups_and_retains_them_across_an_alias_edit() {
    let mut house = House::mixed(&fading("selected"));
    let member = fsonos_core::resolve_room(&house.houses, "Room1@S1")
        .unwrap()
        .player
        .ip;
    soap::join_group(&house.sim.lan(), member, &id(&house, "Room0@S1")).unwrap();
    house.houses =
        fsonos_core::inventory::survey(&house.sim.lan(), &[], Duration::from_millis(500))
            .unwrap()
            .households;
    house.inventory(Ok(house.houses.clone()));
    let directory = tempfile::tempdir().unwrap();
    aliases(
        &mut house,
        &directory,
        "[aliases]\nselected = ['Room0@S1', 'Room1@S1', 'Room0@S2']\n",
    );
    play(&house);
    let schedule = house.surface.load_schedules().unwrap().remove(0);
    let before = house.sim.soap_log().len();
    let prepared = ready(&house.surface, &schedule).unwrap();
    assert_reads_only(&house, before);
    claim(&house, &schedule);
    std::fs::write(
        directory.path().join("aliases.toml"),
        "[aliases]\nselected = ['Room1@S2']\n",
    )
    .unwrap();
    let before = house.sim.soap_log().len();
    fire(&house.surface, &schedule, None, prepared).unwrap();
    let paused: BTreeSet<_> = house.sim.soap_log()[before..]
        .iter()
        .filter(|call| call.action == "Pause")
        .map(|call| call.player.clone())
        .collect();
    assert_eq!(
        paused,
        [id(&house, "Room0@S1").0, id(&house, "Room0@S2").0]
            .into_iter()
            .collect()
    );
    let untouched = fsonos_core::resolve_room(&house.houses, "Room1@S2")
        .unwrap()
        .coordinator
        .ip;
    assert_eq!(
        soap::get_transport_info(&house.sim.lan(), untouched)
            .unwrap()
            .state,
        TransportState::Playing
    );
    for player in house.sim.players() {
        assert_eq!(soap::get_volume(&house.sim.lan(), player.ip).unwrap(), 28);
    }
    let actions = house
        .surface
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].action.client, "cli");
    let scheduler = Scheduler::new(&house.surface);
    assert!(
        house
            .tick(&scheduler, house.at + chrono::TimeDelta::minutes(1))
            .is_empty()
    );
}

#[test]
fn an_unreadable_group_defers_the_entire_occurrence_before_any_fade() {
    let house = House::mixed(&fading("all"));
    play(&house);
    let unavailable = id(&house, "Room1@S2");
    house
        .sim
        .upnp_fault(&unavailable.0, "GetTransportInfo", 702)
        .unwrap();
    let scheduler = Scheduler::new(&house.surface);
    let before = house.sim.soap_log().len();
    house.tick(&scheduler, house.at);
    assert_eq!(house.recorded(), None);
    assert_reads_only(&house, before);
    house.sim.clear_faults(&unavailable.0).unwrap();
    house.tick(&scheduler, house.at + chrono::TimeDelta::minutes(1));
    assert_eq!(house.recorded(), Some(house.at));
    assert_eq!(
        house.sim.soap_log()[before..]
            .iter()
            .filter(|call| call.action == "Pause")
            .count(),
        4
    );
}

#[test]
fn a_partial_scheduled_fade_is_consumed_and_keeps_every_targets_result() {
    for s2 in [false, true] {
        let house = House::new(s2, &fading("all"));
        play(&house);
        house.sim.upnp_fault("Room1", "Pause", 702).unwrap();
        let schedule = house.surface.load_schedules().unwrap().remove(0);
        let prepared = ready(&house.surface, &schedule).unwrap();
        claim(&house, &schedule);
        let before = house.sim.soap_log().len();
        let failure = fire(&house.surface, &schedule, None, prepared).unwrap_err();
        assert_eq!(failure.code, ErrorCode::OutcomeUncertain);
        assert!(failure.detail.contains("incomplete for 1 target(s)"));
        assert!(failure.detail.contains("Room1@"));
        assert!(failure.hint.contains("will not be retried"));
        assert_eq!(
            house.sim.soap_log()[before..]
                .iter()
                .filter(|call| call.action == "Pause")
                .count(),
            2
        );
        assert_eq!(
            soap::get_transport_info(&house.sim.lan(), house.sim.player("Room0").unwrap().ip)
                .unwrap()
                .state,
            TransportState::Paused
        );
        assert_eq!(
            soap::get_transport_info(&house.sim.lan(), house.sim.player("Room1").unwrap().ip)
                .unwrap()
                .state,
            TransportState::Playing
        );
        for player in house.sim.players() {
            assert_eq!(soap::get_volume(&house.sim.lan(), player.ip).unwrap(), 28);
        }
        let actions = house
            .surface
            .recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap();
        assert_eq!(actions.len(), 1);
        assert!(actions[0].action.result.contains("Room1@"));
        assert!(
            actions[0]
                .action
                .before_state
                .as_deref()
                .unwrap()
                .contains("fade-out")
        );
        let after = house.sim.soap_log().len();
        let scheduler = Scheduler::new(&house.surface);
        for minute in 1..4 {
            assert!(
                house
                    .tick(&scheduler, house.at + chrono::TimeDelta::minutes(minute))
                    .is_empty()
            );
        }
        assert_eq!(house.sim.soap_log().len(), after);
    }
}

#[test]
fn a_claimed_bedtime_rechecks_its_creators_policy_before_speaker_writes() {
    let house = House::mixed(&fading("all"));
    play(&house);
    let schedule = house.surface.load_schedules().unwrap().remove(0);
    let prepared = ready(&house.surface, &schedule).unwrap();
    claim(&house, &schedule);
    *house.surface.policy.write().unwrap() =
        Arc::new(Policy::from_toml("[clients.cli]\ndeny = ['pause']\n").unwrap());
    let before = house.sim.soap_log().len();
    assert_eq!(
        fire(&house.surface, &schedule, None, prepared)
            .unwrap_err()
            .code,
        ErrorCode::PolicyDenied
    );
    assert_reads_only(&house, before);
    let actions = house
        .surface
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].action.client, "cli");
    assert!(actions[0].action.decision.starts_with("deny:"));
}

#[test]
fn create_and_edit_accept_faded_room_sets_without_touching_speakers() {
    let mut house = House::mixed(&fading("all"));
    let directory = tempfile::tempdir().unwrap();
    aliases(
        &mut house,
        &directory,
        "[aliases]\nselected = ['Room0@S1', 'Room0@S2']\n",
    );
    let before = house.sim.soap_log().len();
    let saved = house
        .surface
        .add_schedule(
            &Client::Cli,
            &ScheduleRequest {
                when: "daily 22:30".into(),
                action: "pause".into(),
                zone: Some("all@S2".into()),
                fade_secs: Some(10),
                ..ScheduleRequest::default()
            },
        )
        .unwrap();
    assert_eq!(saved.zone.as_deref(), Some("all@S2"));
    let changed = house
        .surface
        .update_schedule(
            &Client::Cli,
            &UpdateScheduleRequest {
                id: saved.id,
                expected_revision: saved.revision,
                schedule: ScheduleRequest {
                    when: "daily 22:30".into(),
                    action: "pause".into(),
                    zone: Some("selected".into()),
                    fade_secs: Some(10),
                    ..ScheduleRequest::default()
                },
            },
        )
        .unwrap();
    assert_eq!(changed.zone.as_deref(), Some("selected"));
    assert_eq!(changed.fade_secs, Some(10));
    assert_reads_only(&house, before);
}
