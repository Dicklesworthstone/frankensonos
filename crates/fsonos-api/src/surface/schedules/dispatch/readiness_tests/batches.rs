//! Scheduled ordinary controls must not report partial delivery as success.

use super::*;
use fsonos_core::policy::Client;
use fsonos_core::store::ActionFilter;

#[test]
fn a_partial_scheduled_pause_or_volume_reports_failure_without_reopening_the_occurrence() {
    for s2 in [false, true] {
        for (action, verb) in [
            (
                ScheduleAction::Pause {
                    room: "all".into(),
                    fade_secs: 0,
                },
                "Pause",
            ),
            (
                ScheduleAction::Volume {
                    room: "all".into(),
                    level: 10,
                },
                "SetVolume",
            ),
        ] {
            let h = House::new(s2, &action);
            for room in ["Room0", "Room1"] {
                let ip = h.sim.player(room).unwrap().ip;
                soap::set_volume(&h.sim.lan(), ip, 25).unwrap();
                soap::set_av_transport_uri(
                    &h.sim.lan(),
                    ip,
                    "x-rincon-mp3radio://stream.example.invalid/schedule.mp3",
                    "",
                )
                .unwrap();
                soap::play(&h.sim.lan(), ip).unwrap();
            }
            h.sim.upnp_fault("Room1", verb, 702).unwrap();
            let schedule = h.surface.load_schedules().unwrap().remove(0);
            let prepared = ready(&h.surface, &schedule).unwrap();
            assert_eq!(
                claim_current(&h.surface, &schedule, h.at, prepared.as_ref(), || {
                    Admission::Run
                })
                .unwrap(),
                Admission::Run
            );
            let before = h.sim.soap_log().len();
            let failure = fire(&h.surface, &schedule, None, prepared).unwrap_err();
            assert_eq!(failure.code, ErrorCode::OutcomeUncertain);
            assert!(failure.detail.contains("incomplete for 1 target(s)"));
            assert!(failure.detail.contains("Room1@"));
            assert!(failure.detail.contains("UPNP_FAULT"));
            assert!(failure.cli_text().contains("will not be retried"));
            assert_eq!(h.recorded(), Some(h.at));
            let sent: Vec<_> = h.sim.soap_log()[before..]
                .iter()
                .filter(|call| call.action == verb)
                .map(|call| call.room.clone())
                .collect();
            assert_eq!(sent.len(), 2);
            assert!(sent.contains(&"Room0".to_owned()));
            assert!(sent.contains(&"Room1".to_owned()));
            let ip = h.sim.player("Room0").unwrap().ip;
            if verb == "Pause" {
                assert_eq!(
                    soap::get_transport_info(&h.sim.lan(), ip).unwrap().state,
                    TransportState::Paused
                );
            } else {
                assert_eq!(soap::get_volume(&h.sim.lan(), ip).unwrap(), 10);
            }
            let logged = h
                .surface
                .recent_actions(&Client::Cli, &ActionFilter::default())
                .unwrap();
            assert_eq!(logged.len(), 1);
            assert!(logged[0].action.result.contains("Room1@"));
            assert!(logged[0].action.result.contains("UPNP_FAULT"));
            let after = h.sim.soap_log().len();
            let scheduler = Scheduler::new(&h.surface);
            for minute in 1..4 {
                assert!(
                    h.tick(&scheduler, h.at + chrono::TimeDelta::minutes(minute))
                        .is_empty()
                );
            }
            assert_eq!(h.sim.soap_log().len(), after);
        }
    }
}

fn ordinary_controls(room: &str) -> [(ScheduleAction, u32, &'static str); 3] {
    [
        (
            ScheduleAction::Pause {
                room: room.into(),
                fade_secs: 0,
            },
            0,
            "Pause",
        ),
        (
            ScheduleAction::Volume {
                room: room.into(),
                level: 10,
            },
            0,
            "SetVolume",
        ),
        (
            ScheduleAction::Volume {
                room: room.into(),
                level: 10,
            },
            1,
            "SetVolume",
        ),
    ]
}

fn set_fade(house: &House, secs: u32) {
    *house.surface.policy.write().unwrap() =
        Arc::new(Policy::from_toml(&format!("[defaults]\nfade_secs = {secs}\n")).unwrap());
}

fn play(house: &House) {
    for household in &house.houses {
        for group in &household.groups {
            let host = household.player(&group.coordinator).unwrap().ip;
            soap::set_av_transport_uri(
                &house.sim.lan(),
                host,
                "x-rincon-mp3radio://stream.example.invalid/scheduled-controls.mp3",
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

fn claim(house: &House, schedule: &Schedule, prepared: Option<&PreparedRun>) {
    assert_eq!(
        claim_current(&house.surface, schedule, house.at, prepared, || {
            Admission::Run
        })
        .unwrap(),
        Admission::Run
    );
    assert_eq!(house.recorded(), Some(house.at));
}

#[test]
fn claimed_controls_keep_both_households_and_member_volumes_across_an_alias_edit() {
    for (action, fade_secs, verb) in ordinary_controls("selected") {
        let mut house = House::mixed(&action);
        set_fade(&house, fade_secs);
        let leader = fsonos_core::resolve_room(&house.houses, "Room0@S1")
            .unwrap()
            .player
            .id
            .clone();
        let member = fsonos_core::resolve_room(&house.houses, "Room1@S1")
            .unwrap()
            .player
            .ip;
        soap::join_group(&house.sim.lan(), member, &leader).unwrap();
        house.houses =
            fsonos_core::inventory::survey(&house.sim.lan(), &[], Duration::from_millis(500))
                .unwrap()
                .households;
        house.inventory(Ok(house.houses.clone()));
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("aliases.toml");
        std::fs::write(&path, "[aliases]\nselected = ['Room1@S1', 'Room0@S2']\n").unwrap();
        Arc::get_mut(&mut house.surface).unwrap().aliases_file = Some(path.clone());
        play(&house);
        let schedule = house.surface.load_schedules().unwrap().remove(0);
        let prepared = ready(&house.surface, &schedule).unwrap();
        claim(&house, &schedule, prepared.as_ref());
        std::fs::write(&path, "[aliases]\nselected = ['Room1@S2']\n").unwrap();
        let before = house.sim.soap_log().len();
        fire(&house.surface, &schedule, None, prepared).unwrap();
        let addressed: std::collections::BTreeSet<_> = house.sim.soap_log()[before..]
            .iter()
            .filter(|call| call.action == verb)
            .map(|call| call.player.clone())
            .collect();
        let first = if verb == "Pause" {
            "Room0@S1"
        } else {
            "Room1@S1"
        };
        let expected = [first, "Room0@S2"]
            .map(|room| {
                fsonos_core::resolve_room(&house.houses, room)
                    .unwrap()
                    .player
                    .id
                    .0
                    .clone()
            })
            .into_iter()
            .collect();
        assert_eq!(addressed, expected);
        let untouched = fsonos_core::resolve_room(&house.houses, "Room1@S2")
            .unwrap()
            .player
            .ip;
        assert_eq!(soap::get_volume(&house.sim.lan(), untouched).unwrap(), 28);
        assert_eq!(
            soap::get_transport_info(&house.sim.lan(), untouched)
                .unwrap()
                .state,
            TransportState::Playing
        );
        if verb == "SetVolume" {
            assert_eq!(soap::get_volume(&house.sim.lan(), member).unwrap(), 10);
            let leader_ip = fsonos_core::resolve_room(&house.houses, "Room0@S1")
                .unwrap()
                .player
                .ip;
            assert_eq!(soap::get_volume(&house.sim.lan(), leader_ip).unwrap(), 28);
        }
        let actions = house
            .surface
            .recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap();
        assert_eq!(actions.len(), 1);
        let scheduler = Scheduler::new(&house.surface);
        let after = house.sim.soap_log().len();
        assert!(
            house
                .tick(&scheduler, house.at + chrono::TimeDelta::minutes(1))
                .is_empty()
        );
        assert_eq!(house.sim.soap_log().len(), after);
    }
}

#[test]
fn claimed_controls_refuse_an_expanded_group_without_writing_or_reopening_the_occurrence() {
    for s2 in [false, true] {
        for (action, fade_secs, _) in ordinary_controls("Room0") {
            let house = House::new(s2, &action);
            set_fade(&house, fade_secs);
            play(&house);
            let schedule = house.surface.load_schedules().unwrap().remove(0);
            let prepared = ready(&house.surface, &schedule).unwrap();
            claim(&house, &schedule, prepared.as_ref());
            let leader = fsonos_core::resolve_room(&house.houses, "Room0")
                .unwrap()
                .player
                .id
                .clone();
            soap::join_group(
                &house.sim.lan(),
                house.sim.player("Room1").unwrap().ip,
                &leader,
            )
            .unwrap();
            let current =
                fsonos_core::inventory::survey(&house.sim.lan(), &[], Duration::from_millis(500))
                    .unwrap()
                    .households;
            house.inventory(Ok(current));
            let before = house.sim.soap_log().len();
            let failure = fire(&house.surface, &schedule, None, prepared).unwrap_err();
            assert_eq!(failure.code, ErrorCode::NotCoordinator);
            assert!(
                house.sim.soap_log()[before..]
                    .iter()
                    .all(|call| call.action.starts_with("Get"))
            );
            for player in house.sim.players() {
                assert_eq!(soap::get_volume(&house.sim.lan(), player.ip).unwrap(), 28);
            }
            assert_eq!(
                soap::get_transport_info(&house.sim.lan(), house.sim.player("Room0").unwrap().ip)
                    .unwrap()
                    .state,
                TransportState::Playing
            );
            let actions = house
                .surface
                .recent_actions(&Client::Cli, &ActionFilter::default())
                .unwrap();
            assert_eq!(actions.len(), 1);
            let scheduler = Scheduler::new(&house.surface);
            let after = house.sim.soap_log().len();
            assert!(
                house
                    .tick(&scheduler, house.at + chrono::TimeDelta::minutes(1))
                    .is_empty()
            );
            assert_eq!(house.sim.soap_log().len(), after);
        }
    }
}

struct RegroupAfterVolume {
    inner: Arc<dyn Transport + Send + Sync>,
    lan: SimLan,
    member: IpAddr,
    coordinator: fsonos_types::PlayerId,
    changed: std::sync::atomic::AtomicBool,
}

impl Transport for RegroupAfterVolume {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let reply = self.inner.soap_post(host, path, action, body)?;
        if action.trim_matches('"').ends_with("#SetVolume")
            && !self.changed.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            soap::join_group(&self.lan, self.member, &self.coordinator)?;
        }
        Ok(reply)
    }
}

#[test]
fn a_volume_fade_rechecks_the_admitted_group_before_its_next_frame() {
    let mut house = House::new(
        true,
        &ScheduleAction::Volume {
            room: "Room0".into(),
            level: 10,
        },
    );
    set_fade(&house, 1);
    play(&house);
    let schedule = house.surface.load_schedules().unwrap().remove(0);
    let prepared = ready(&house.surface, &schedule).unwrap();
    claim(&house, &schedule, prepared.as_ref());
    let transport = RegroupAfterVolume {
        inner: Arc::clone(&house.surface.transport),
        lan: house.sim.lan(),
        member: house.sim.player("Room1").unwrap().ip,
        coordinator: fsonos_core::resolve_room(&house.houses, "Room0")
            .unwrap()
            .player
            .id
            .clone(),
        changed: std::sync::atomic::AtomicBool::new(false),
    };
    Arc::get_mut(&mut house.surface).unwrap().transport = Arc::new(transport);
    let before = house.sim.soap_log().len();
    let failure = fire(&house.surface, &schedule, None, prepared).unwrap_err();
    assert_eq!(failure.code, ErrorCode::OutcomeUncertain);
    assert!(failure.hint.contains("will not be retried"));
    assert_eq!(
        house.sim.soap_log()[before..]
            .iter()
            .filter(|call| call.action == "SetVolume")
            .count(),
        1
    );
    assert_eq!(
        soap::get_volume(&house.sim.lan(), house.sim.player("Room1").unwrap().ip).unwrap(),
        28
    );
    let actions = house
        .surface
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    assert_eq!(actions.len(), 1);
    assert!(
        actions[0]
            .action
            .before_state
            .as_deref()
            .unwrap()
            .contains("failed volume fade")
    );
    let scheduler = Scheduler::new(&house.surface);
    let after = house.sim.soap_log().len();
    assert!(
        house
            .tick(&scheduler, house.at + chrono::TimeDelta::minutes(1))
            .is_empty()
    );
    assert_eq!(house.sim.soap_log().len(), after);
}

#[test]
fn lost_control_acknowledgements_preserve_confirmed_or_uncertain_outcomes_without_replay() {
    for s2 in [false, true] {
        for (action, fade_secs, verb) in ordinary_controls("Room0") {
            let house = House::new(s2, &action);
            set_fade(&house, fade_secs);
            play(&house);
            let schedule = house.surface.load_schedules().unwrap().remove(0);
            let prepared = ready(&house.surface, &schedule).unwrap();
            claim(&house, &schedule, prepared.as_ref());
            *house.lose_write.lock().unwrap() = Some(verb);
            let before = house.sim.soap_log().len();
            let result = fire(&house.surface, &schedule, None, prepared);
            if verb == "Pause" {
                // The core confirms the inactive state without replaying Pause.
                assert!(result.is_ok(), "{result:?}");
            } else {
                let failure = result.unwrap_err();
                assert_eq!(failure.code, ErrorCode::OutcomeUncertain);
                assert!(!failure.retryable());
                assert!(failure.hint.contains("will not be retried"));
            }
            assert!(house.lose_write.lock().unwrap().is_none());
            assert_eq!(
                house.sim.soap_log()[before..]
                    .iter()
                    .filter(|call| call.action == verb)
                    .count(),
                1
            );
            let host = house.sim.player("Room0").unwrap().ip;
            if verb == "Pause" {
                assert_eq!(
                    soap::get_transport_info(&house.sim.lan(), host)
                        .unwrap()
                        .state,
                    TransportState::Paused
                );
            } else {
                assert!(soap::get_volume(&house.sim.lan(), host).unwrap() < 28);
            }
            let actions = house
                .surface
                .recent_actions(&Client::Cli, &ActionFilter::default())
                .unwrap();
            assert_eq!(actions.len(), 1);
            assert!(actions[0].action.result.contains(if verb == "Pause" {
                "paused"
            } else {
                "may have succeeded"
            }));
            let scheduler = Scheduler::new(&house.surface);
            let after = house.sim.soap_log().len();
            assert!(
                house
                    .tick(&scheduler, house.at + chrono::TimeDelta::minutes(1))
                    .is_empty()
            );
            assert_eq!(house.sim.soap_log().len(), after);
        }
    }
}
