//! Authored native playlist scenes across both real loopback generations.
//! Catalog identity is checked before scene writes and at source dispatch.

use chrono::{DateTime, Utc};
use fsonos_api::surface::scenes::SceneDefinition;
use fsonos_api::surface::schedules::{Ran, ScheduleRequest, Scheduler};
use fsonos_api::{NoteCode, Surface};
use fsonos_core::HouseholdState;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::scenes::{self, Scene};
use fsonos_core::store::MemStore;
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{HouseholdId, PlayerId, TransportState};
use serde_json::json;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

struct CatalogTransport {
    lan: SimLan,
    fault: Arc<AtomicU8>,
    regroup: (IpAddr, PlayerId),
}

impl Transport for CatalogTransport {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.lan.soap_post(host, path, action, body)?;
        if action.contains("#Browse") && body.contains("<ObjectID>SQ:</ObjectID>") {
            let mode = self.fault.load(Ordering::SeqCst);
            if mode == 6 {
                self.fault.store(7, Ordering::SeqCst);
            } else if mode == 7 {
                // Native source preflight was the preceding catalog read.
                // An uncapped room joins during the final dispatch read.
                soap::join_group(&self.lan, self.regroup.0, &self.regroup.1)?;
                self.fault.store(8, Ordering::SeqCst);
            }
            return match mode {
                1 => Ok(response.replace("Evening", "Renamed")),
                2 => Ok(response.replace("SQ:7", "SQ:99")),
                3 => Err(ProtoError::Network {
                    target: "simulated catalog".into(),
                    detail: "temporarily unavailable".into(),
                }),
                _ => Ok(response),
            };
        }
        if action.contains("#AddURIToQueue")
            && self
                .fault
                .compare_exchange(4, 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            return Err(ProtoError::Network {
                target: "simulated queue".into(),
                detail: "reply lost after native insertion".into(),
            });
        }
        if action.contains("#SetAVTransportURI") && body.contains("x-rincon:") {
            // Another controller renames the saved playlist after regrouping.
            // Its preflight was valid; source dispatch must read it again.
            let _ = self
                .fault
                .compare_exchange(5, 1, Ordering::SeqCst, Ordering::SeqCst);
        }
        Ok(response)
    }
}

fn fixture(policy: Policy, store: MemStore) -> (SimHandle, Arc<Surface>, Arc<AtomicU8>) {
    let sim = SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .s2([
            SimPlayerSpec::new("Den", SimModel::One),
            SimPlayerSpec::new("Study", SimModel::One),
        ])
        .spawn()
        .unwrap();
    let houses: Vec<_> = ["Kitchen", "Den"]
        .iter()
        .map(|room| {
            let player = sim.player(room).unwrap();
            (player.ip, player.household.clone())
        })
        .collect();
    let fault = Arc::new(AtomicU8::new(0));
    let surface = Surface::new(
        Box::new(CatalogTransport {
            lan: sim.lan(),
            fault: fault.clone(),
            regroup: (
                sim.player("Study").unwrap().ip,
                PlayerId(sim.player("Den").unwrap().uuid.clone()),
            ),
        }),
        Box::new(move |t| {
            houses
                .iter()
                .map(|(host, id)| {
                    let mut house = HouseholdState {
                        id: Some(HouseholdId(id.clone())),
                        ..HouseholdState::default()
                    };
                    house.apply_topology(
                        &get_zone_group_state(t, *host).map_err(fsonos_core::CoreError::from)?,
                    );
                    Ok(house)
                })
                .collect()
        }),
        policy,
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(store), "test");
    (sim, Arc::new(surface), fault)
}

fn definition(room: &str, playing: bool) -> SceneDefinition {
    serde_json::from_value(json!({
        "groups": [{"coordinator": room, "source": {"kind": "sonos_playlist", "name": "SQ:7"}, "playing": playing}]
    })).unwrap()
}

fn seed(sim: &SimHandle, room: &str) {
    let player = sim.player(room).unwrap();
    soap::add_uri_to_queue(
        &sim.lan(),
        player.ip,
        "x-file-cifs://example.invalid/old.flac",
        "",
        false,
    )
    .unwrap();
    soap::set_av_transport_uri(
        &sim.lan(),
        player.ip,
        "x-rincon-mp3radio://example.invalid/old.mp3",
        "",
    )
    .unwrap();
    soap::play(&sim.lan(), player.ip).unwrap();
}

fn assert_reads(sim: &SimHandle, from: usize) {
    assert!(
        sim.soap_log()[from..]
            .iter()
            .all(|call| call.action.starts_with("Get") || call.action == "Browse")
    );
}

#[test]
fn native_playlist_scenes_pin_both_households_and_play_or_load_their_current_contents() {
    for playing in [false, true] {
        let (sim, surface, _) = fixture(Policy::default(), MemStore::default());
        for (room, generation) in [("Kitchen", 1), ("Den", 2)] {
            seed(&sim, room);
            let start = sim.soap_log().len();
            let saved = surface
                .save_scene_from(&Client::Cli, room, &definition(room, playing))
                .unwrap();
            assert_reads(&sim, start);
            let source = &saved.groups[0].source;
            assert_eq!(source.kind, "sonos_playlist");
            assert_eq!(source.name.as_deref(), Some("SQ:7"));
            assert!(source.uri.is_none());
            let reference = source.playlist.as_ref().unwrap();
            assert_eq!(reference.household, format!("SIM_HOUSEHOLD_S{generation}"));
            assert_eq!(reference.title, format!("Evening & Friends S{generation}"));
            assert_eq!(surface.scene(&Client::Cli, room).unwrap(), saved);
            let start = sim.soap_log().len();
            let applied = surface.apply_scene(&Client::Cli, room).unwrap();
            assert!(applied.complete && applied.changed, "{applied:?}");
            let host = sim.player(room).unwrap().ip;
            let queue = browse_snapshot(&sim.lan(), host, QUEUE).unwrap();
            let uris: Vec<_> = queue
                .objects
                .iter()
                .map(|item| item.res.as_ref().unwrap().uri.as_str())
                .collect();
            assert_eq!(uris.len(), 3);
            assert_eq!(uris[0], uris[2]);
            assert!(
                uris.iter()
                    .all(|uri| uri.contains(&format!("0SimS{generation}")))
            );
            assert_eq!(soap::get_position_info(&sim.lan(), host).unwrap().track, 1);
            assert_eq!(
                soap::get_transport_info(&sim.lan(), host).unwrap().state
                    == TransportState::Playing,
                playing
            );
            if !playing {
                assert!(
                    sim.soap_log()[start..]
                        .iter()
                        .all(|call| call.action != "Play")
                );
            }
            assert!(
                applied
                    .done
                    .contains("undo cannot restore replaced queue contents")
            );
        }
    }
}

#[test]
fn native_playlist_sources_are_rechecked_before_regrouping_caps_or_queue_writes() {
    for fault_mode in [1, 2, 3] {
        let (sim, surface, fault) = fixture(Policy::default(), MemStore::default());
        seed(&sim, "Den");
        let mut def = definition("Den", true);
        def.groups[0].members.push("Study".into());
        surface
            .save_scene_from(&Client::Tailnet("tag:agent".into()), "Check first", &def)
            .unwrap();
        soap::set_volume(&sim.lan(), sim.player("Den").unwrap().ip, 95).unwrap();
        let before = browse_snapshot(&sim.lan(), sim.player("Den").unwrap().ip, QUEUE).unwrap();
        fault.store(fault_mode, Ordering::SeqCst);
        let start = sim.soap_log().len();
        assert!(surface.apply_scene(&Client::Cli, "Check first").is_err());
        assert_reads(&sim, start);
        assert_eq!(
            browse_snapshot(&sim.lan(), sim.player("Den").unwrap().ip, QUEUE).unwrap(),
            before
        );
    }
}

#[test]
fn a_persisted_native_id_cannot_be_retargeted_to_another_household() {
    let mut store = MemStore::default();
    let scene: Scene = serde_json::from_value(json!({
        "name": "Wrong household", "groups": [{"coordinator": "Den", "playing": true,
            "source": {"kind": "sonos_playlist", "playlist": {"household": "SIM_HOUSEHOLD_S1", "id": "SQ:7", "title": "Evening & Friends S1"}}}]
    })).unwrap();
    scenes::save(&mut store, &scene, 1).unwrap();
    assert_eq!(scenes::load(&store, "Wrong household").unwrap(), scene);
    let (sim, surface, _) = fixture(Policy::default(), store);
    let start = sim.soap_log().len();
    let error = surface
        .apply_scene(&Client::Cli, "Wrong household")
        .unwrap_err();
    assert!(error.detail.contains("saved household"), "{error:?}");
    assert_reads(&sim, start);
}

#[test]
fn a_new_group_coordinator_can_load_a_playlist_and_obeys_the_savers_volume_cap() {
    let policy = Policy::from_toml("[defaults]\nmax_volume = 24\n").unwrap();
    let (sim, surface, _) = fixture(policy, MemStore::default());
    let kitchen = sim.player("Kitchen").unwrap();
    let office = sim.player("Office").unwrap();
    soap::join_group(
        &sim.lan(),
        office.ip,
        &fsonos_types::PlayerId(kitchen.uuid.clone()),
    )
    .unwrap();
    soap::set_volume(&sim.lan(), office.ip, 80).unwrap();
    let mut def = definition("Office", true);
    def.groups[0].members.push("Kitchen".into());
    surface
        .save_scene_from(
            &Client::Tailnet("tag:restricted".into()),
            "Lead from Office",
            &def,
        )
        .unwrap();
    let applied = surface
        .apply_scene(&Client::Cli, "Lead from Office")
        .unwrap();
    assert!(applied.complete, "{applied:?}");
    assert!(
        applied
            .notes
            .iter()
            .any(|note| note.code == NoteCode::VolumeClamped)
    );
    assert!(soap::get_volume(&sim.lan(), office.ip).unwrap() <= 24);
    assert_eq!(
        soap::get_transport_info(&sim.lan(), office.ip)
            .unwrap()
            .state,
        TransportState::Playing
    );
    let topology = get_zone_group_state(&sim.lan(), office.ip).unwrap();
    assert!(
        topology
            .groups
            .iter()
            .any(|group| group.coordinator.0 == office.uuid && group.members.len() == 2)
    );
}

#[test]
fn caller_and_writer_permissions_omit_the_whole_native_playlist_operation() {
    for (tool, playing) in [("play_favorite", true), ("pause", false)] {
        for denied_writer in [false, true] {
            let policy = Policy::from_toml(&format!(
                "[clients.\"tag:restricted\"]\ndeny = [\"{tool}\"]\n"
            ))
            .unwrap();
            let (sim, surface, fault) = fixture(policy, MemStore::default());
            let restricted = Client::Tailnet("tag:restricted".into());
            let cli = Client::Cli;
            let (writer, caller) = if denied_writer {
                (&restricted, &cli)
            } else {
                (&cli, &restricted)
            };
            surface
                .save_scene_from(writer, "Restricted", &definition("Den", playing))
                .unwrap();
            // An omitted source must not stall permitted scene work on its unavailable catalog.
            fault.store(3, Ordering::SeqCst);
            let start = sim.soap_log().len();
            let applied = surface.apply_scene(caller, "Restricted").unwrap();
            assert!(!applied.changed && applied.steps.is_empty(), "{applied:?}");
            assert!(
                applied
                    .notes
                    .iter()
                    .any(|note| note.code == NoteCode::Skipped && note.detail.contains(tool))
            );
            assert_reads(&sim, start);
        }
    }
}

#[test]
fn native_playlist_sources_use_the_existing_playback_permission_and_exact_tool() {
    for permission in ["play_favorite", "playback"] {
        let policy = Policy::from_toml(&format!(
            "[clients.\"tag:restricted\"]\nallow = [\"save_scene\", \"apply_scene\", \"{permission}\"]\n"
        ))
        .unwrap();
        let (sim, surface, _) = fixture(policy, MemStore::default());
        let client = Client::Tailnet("tag:restricted".into());
        surface
            .save_scene_from(&client, "Permitted", &definition("Den", true))
            .unwrap();
        let applied = surface.apply_scene(&client, "Permitted").unwrap();
        assert!(applied.complete && applied.changed, "{applied:?}");
        assert_eq!(
            soap::get_transport_info(&sim.lan(), sim.player("Den").unwrap().ip)
                .unwrap()
                .state,
            TransportState::Playing
        );
    }
}

#[test]
fn a_playlist_changed_after_preflight_is_not_loaded_after_regrouping() {
    let (sim, surface, fault) = fixture(Policy::default(), MemStore::default());
    seed(&sim, "Den");
    let mut def = definition("Den", true);
    def.groups[0].members.push("Study".into());
    surface
        .save_scene_from(&Client::Cli, "Fresh source", &def)
        .unwrap();
    fault.store(5, Ordering::SeqCst);
    let start = sim.soap_log().len();
    let applied = surface.apply_scene(&Client::Cli, "Fresh source").unwrap();
    assert!(
        !applied.complete && applied.failed.len() == 1,
        "{applied:?}"
    );
    assert_eq!(fault.load(Ordering::SeqCst), 1);
    assert!(
        sim.soap_log()[start..]
            .iter()
            .all(|call| call.action != "AddURIToQueue" && call.action != "Play")
    );
    assert_eq!(
        browse_snapshot(&sim.lan(), sim.player("Den").unwrap().ip, QUEUE)
            .unwrap()
            .objects
            .len(),
        1
    );
}

#[test]
fn a_room_joining_during_catalog_resolution_does_not_receive_unprepared_playback() {
    let (sim, surface, fault) = fixture(Policy::default(), MemStore::default());
    seed(&sim, "Den");
    soap::set_volume(&sim.lan(), sim.player("Study").unwrap().ip, 95).unwrap();
    surface
        .save_scene_from(
            &Client::Tailnet("tag:agent".into()),
            "Confirmed audience",
            &definition("Den", true),
        )
        .unwrap();
    fault.store(6, Ordering::SeqCst);
    let start = sim.soap_log().len();
    let applied = surface
        .apply_scene(&Client::Cli, "Confirmed audience")
        .unwrap();
    assert!(
        !applied.complete && applied.failed.len() == 1,
        "{applied:?}"
    );
    assert_eq!(fault.load(Ordering::SeqCst), 8);
    assert!(
        sim.soap_log()[start..]
            .iter()
            .all(|call| call.action != "AddURIToQueue" && call.action != "Play")
    );
    assert_eq!(
        browse_snapshot(&sim.lan(), sim.player("Den").unwrap().ip, QUEUE)
            .unwrap()
            .objects
            .len(),
        1
    );
}

#[test]
fn malformed_or_caller_supplied_playlist_bindings_are_never_saved() {
    let (sim, surface, _) = fixture(Policy::default(), MemStore::default());
    let start = sim.soap_log().len();
    for source in [
        json!({"kind": "sonos_playlist", "name": "Evening"}),
        json!({"kind": "sonos_playlist", "name": "SQ:7/1"}),
        json!({"kind": "sonos_playlist", "name": "SQ:7", "uri": "file:///fake.rsq"}),
        json!({"kind": "sonos_playlist", "name": "SQ:7", "playlist": {"household": "other", "id": "SQ:7", "title": "Forged"}}),
        json!({"kind": "keep", "playlist": {"household": "other", "id": "SQ:7", "title": "Forged"}}),
    ] {
        let definition: SceneDefinition =
            serde_json::from_value(json!({"groups": [{"coordinator": "Den", "source": source}]}))
                .unwrap();
        assert!(
            surface
                .save_scene_from(&Client::Cli, "Invalid", &definition)
                .is_err()
        );
    }
    assert_reads(&sim, start);
    assert!(surface.scene(&Client::Cli, "Invalid").is_err());
}

#[test]
fn scheduled_native_playlist_waits_for_catalog_readiness_and_fires_once() {
    let (sim, surface, fault) = fixture(Policy::default(), MemStore::default());
    surface
        .save_scene_from(&Client::Cli, "Morning playlist", &definition("Den", true))
        .unwrap();
    let schedule = surface
        .add_schedule(
            &Client::Cli,
            &ScheduleRequest {
                when: "in 1m".into(),
                action: "apply_scene".into(),
                scene: Some("Morning playlist".into()),
                ..ScheduleRequest::default()
            },
        )
        .unwrap();
    let at = DateTime::parse_from_rfc3339(schedule.next.as_deref().unwrap())
        .unwrap()
        .to_utc();
    let scheduler = Scheduler::new(&surface);
    fault.store(3, Ordering::SeqCst);
    let before = sim.soap_log().len();
    assert_eq!(
        scheduler.tick_at(at, &Utc),
        [Ran::Fired {
            id: schedule.id,
            at
        }]
    );
    assert!(scheduler.wait(Duration::from_secs(10)));
    assert!(
        surface.schedules(&Client::Cli).unwrap()[0]
            .last_fired
            .is_none()
    );
    assert_reads(&sim, before);
    fault.store(0, Ordering::SeqCst);
    assert_eq!(
        scheduler.tick_at(at + chrono::TimeDelta::minutes(1), &Utc),
        [Ran::Fired {
            id: schedule.id,
            at
        }]
    );
    assert!(scheduler.wait(Duration::from_secs(10)));
    assert!(
        scheduler
            .tick_at(at + chrono::TimeDelta::minutes(2), &Utc)
            .is_empty()
    );
    assert_eq!(
        sim.soap_log()[before..]
            .iter()
            .filter(|call| call.action == "AddURIToQueue")
            .count(),
        1
    );
    assert_eq!(
        soap::get_transport_info(&sim.lan(), sim.player("Den").unwrap().ip)
            .unwrap()
            .state,
        TransportState::Playing
    );
}

#[test]
fn an_uncertain_native_playlist_insertion_is_reported_without_another_append_or_play() {
    let (sim, surface, fault) = fixture(Policy::default(), MemStore::default());
    seed(&sim, "Den");
    surface
        .save_scene_from(&Client::Cli, "Uncertain", &definition("Den", true))
        .unwrap();
    fault.store(4, Ordering::SeqCst);
    let start = sim.soap_log().len();
    let applied = surface.apply_scene(&Client::Cli, "Uncertain").unwrap();
    assert!(
        !applied.complete && applied.failed.len() == 1,
        "{applied:?}"
    );
    let calls = sim.soap_log();
    assert_eq!(
        calls[start..]
            .iter()
            .filter(|call| call.action == "AddURIToQueue")
            .count(),
        1
    );
    assert!(
        calls[start..]
            .iter()
            .all(|call| call.action != "Play" && call.action != "RemoveTrackRangeFromQueue")
    );
}

#[test]
fn a_scheduled_uncertain_playlist_insertion_consumes_its_occurrence_without_retry() {
    let (sim, surface, fault) = fixture(Policy::default(), MemStore::default());
    surface
        .save_scene_from(&Client::Cli, "Once", &definition("Den", true))
        .unwrap();
    let schedule = surface
        .add_schedule(
            &Client::Cli,
            &ScheduleRequest {
                when: "in 1m".into(),
                action: "apply_scene".into(),
                scene: Some("Once".into()),
                ..ScheduleRequest::default()
            },
        )
        .unwrap();
    let at = DateTime::parse_from_rfc3339(schedule.next.as_deref().unwrap())
        .unwrap()
        .to_utc();
    let scheduler = Scheduler::new(&surface);
    fault.store(4, Ordering::SeqCst);
    let start = sim.soap_log().len();
    assert_eq!(
        scheduler.tick_at(at, &Utc),
        [Ran::Fired {
            id: schedule.id,
            at
        }]
    );
    assert!(scheduler.wait(Duration::from_secs(10)));
    assert!(
        surface.schedules(&Client::Cli).unwrap()[0]
            .last_fired
            .is_some()
    );
    assert!(
        scheduler
            .tick_at(at + chrono::TimeDelta::minutes(1), &Utc)
            .is_empty()
    );
    let calls = sim.soap_log();
    assert_eq!(
        calls[start..]
            .iter()
            .filter(|call| call.action == "AddURIToQueue")
            .count(),
        1
    );
    assert!(calls[start..].iter().all(|call| call.action != "Play"));
}
