//! A scene proves its permitted music sources before changing any group.
//! Temporary source failures leave scheduled occurrences available to retry.

use chrono::{DateTime, Utc};
use fsonos_api::surface::scenes::{SceneApplyRequest, SceneDefinition};
use fsonos_api::surface::schedules::{Ran, ScheduleRequest, Scheduler};
use fsonos_api::{ErrorCode, NoteCode, Surface};
use fsonos_core::HouseholdState;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::scenes;
use fsonos_core::store::MemStore;
use fsonos_proto::content;
use fsonos_proto::control as soap;
use fsonos_proto::didl::{learn_spotify_params, spotify_bare_didl, spotify_bare_uri};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{HouseholdId, TransportState};
use serde_json::{Value, json};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const ORIGINAL: &str = "x-rincon-mp3radio://example.invalid/original.mp3";

struct CatalogTransport {
    lan: SimLan,
    unavailable: Arc<AtomicBool>,
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
        if action.contains("#Browse")
            && body.contains("<ObjectID>FV:2</ObjectID>")
            && self.unavailable.load(Ordering::SeqCst)
        {
            return Err(ProtoError::Network {
                target: "synthetic favorites catalog".into(),
                detail: "temporarily unavailable".into(),
            });
        }
        Ok(response)
    }
}

fn fixture(policy: Policy) -> (SimHandle, Arc<Surface>, Arc<AtomicBool>) {
    fixture_with_store(policy, MemStore::default())
}

fn fixture_with_store(
    policy: Policy,
    store: MemStore,
) -> (SimHandle, Arc<Surface>, Arc<AtomicBool>) {
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
    let witnesses: Vec<_> = ["Kitchen", "Den"]
        .iter()
        .map(|name| {
            let player = sim.player(name).unwrap();
            (player.ip, player.household.clone())
        })
        .collect();
    let unavailable = Arc::new(AtomicBool::new(false));
    let surface = Surface::new(
        Box::new(CatalogTransport {
            lan: sim.lan(),
            unavailable: Arc::clone(&unavailable),
        }),
        Box::new(move |t| {
            witnesses
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
    .with_action_log(Box::new(store), "scene-sources-test");
    (sim, Arc::new(surface), unavailable)
}

fn seed(sim: &SimHandle, room: &str) {
    let host = sim.player(room).unwrap().ip;
    soap::set_volume(&sim.lan(), host, 85).unwrap();
    soap::set_av_transport_uri(&sim.lan(), host, ORIGINAL, "").unwrap();
    soap::play(&sim.lan(), host).unwrap();
}

fn definition(room: &str, member: &str, source: &Value, playing: bool) -> SceneDefinition {
    serde_json::from_value(json!({
        "groups": [{"coordinator": room, "members": [member], "source": source, "playing": playing}],
        "volumes": {(room): 20, (member): 15}
    }))
    .unwrap()
}

fn favorite() -> Value {
    json!({"kind": "favorite", "name": "Sim Radio"})
}

fn spotify() -> Value {
    json!({"kind": "uri", "uri": "spotify:track:0SimQueueTrack00000001"})
}

fn assert_read_only(sim: &SimHandle, from: usize) {
    let calls = sim.soap_log();
    assert!(
        calls[from..]
            .iter()
            .all(|call| call.action.starts_with("Get") || call.action == "Browse"),
        "scene sent a mutation before its sources were ready: {:?}",
        &calls[from..]
    );
}

#[test]
fn a_missing_favorite_in_one_household_preserves_every_groups_playback_and_levels() {
    let (sim, surface, _) = fixture(Policy::default());
    for room in ["Kitchen", "Den"] {
        seed(&sim, room);
    }
    let scene: SceneDefinition = serde_json::from_value(json!({
        "groups": [
            {"coordinator": "Kitchen", "members": ["Office"], "source": favorite()},
            {"coordinator": "Den", "members": ["Study"], "source": favorite()}
        ],
        "volumes": {"Kitchen": 20, "Den": 25},
        "mutes": {"Office": true}
    }))
    .unwrap();
    surface
        .save_scene_from(&Client::Cli, "Dinner", &scene)
        .unwrap();
    sim.retain_favorites(2, |uri| !uri.starts_with("x-rincon-mp3radio:"))
        .unwrap();
    let before = sim.soap_log().len();
    let error = surface
        .apply_scene_with(
            &Client::Tailnet("tag:agent".into()),
            "Dinner",
            &SceneApplyRequest { fade_secs: Some(1) },
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::UnknownFavorite);
    assert_read_only(&sim, before);
    for room in ["Kitchen", "Den"] {
        let host = sim.player(room).unwrap().ip;
        assert_eq!(soap::get_volume(&sim.lan(), host).unwrap(), 85);
        assert_eq!(
            soap::get_media_info(&sim.lan(), host).unwrap().uri,
            ORIGINAL
        );
        assert_eq!(
            soap::get_transport_info(&sim.lan(), host).unwrap().state,
            TransportState::Playing
        );
    }
}

#[test]
fn missing_spotify_linkage_cannot_pause_music_or_change_groups_for_play_or_silent_load() {
    for (room, member, generation) in [("Kitchen", "Office", 1), ("Den", "Study", 2)] {
        for (playing, state) in [
            (false, TransportState::Playing),
            (false, TransportState::Paused),
            (true, TransportState::Playing),
        ] {
            let (sim, surface, _) = fixture(Policy::default());
            seed(&sim, room);
            if state == TransportState::Paused {
                soap::pause(&sim.lan(), sim.player(room).unwrap().ip).unwrap();
            }
            surface
                .save_scene_from(
                    &Client::Cli,
                    "Spotify scene",
                    &definition(room, member, &spotify(), playing),
                )
                .unwrap();
            sim.retain_favorites(generation, |_| false).unwrap();
            let before = sim.soap_log().len();
            let error = surface
                .apply_scene(&Client::Tailnet("tag:agent".into()), "Spotify scene")
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::RenderParamsMissing);
            assert_read_only(&sim, before);
            assert_eq!(
                soap::get_transport_info(&sim.lan(), sim.player(room).unwrap().ip)
                    .unwrap()
                    .state,
                state
            );
        }
    }
}

fn schedule(surface: &Arc<Surface>, name: &str) -> (Scheduler, i64, DateTime<Utc>) {
    let saved = surface
        .add_schedule(
            &Client::Cli,
            &ScheduleRequest {
                when: "in 1m".into(),
                action: "apply_scene".into(),
                scene: Some(name.into()),
                ..ScheduleRequest::default()
            },
        )
        .unwrap();
    let at = DateTime::parse_from_rfc3339(saved.next.as_deref().unwrap())
        .unwrap()
        .to_utc();
    (Scheduler::new(surface), saved.id, at)
}

#[test]
fn scheduled_favorite_and_spotify_scenes_retry_catalog_reads_before_claiming_then_play_once() {
    for (room, member) in [("Kitchen", "Office"), ("Den", "Study")] {
        for source in [favorite(), spotify()] {
            let (sim, surface, unavailable) = fixture(Policy::default());
            seed(&sim, room);
            surface
                .save_scene_from(
                    &Client::Cli,
                    "Morning",
                    &definition(room, member, &source, true),
                )
                .unwrap();
            let (scheduler, id, at) = schedule(&surface, "Morning");
            unavailable.store(true, Ordering::SeqCst);
            let before = sim.soap_log().len();
            assert_eq!(scheduler.tick_at(at, &Utc), [Ran::Fired { id, at }]);
            assert!(scheduler.wait(Duration::from_secs(10)));
            assert!(
                surface.schedules(&Client::Cli).unwrap()[0]
                    .last_fired
                    .is_none()
            );
            assert_read_only(&sim, before);
            // Repeated scheduler ticks do not hammer a temporarily down catalog.
            assert!(
                scheduler
                    .tick_at(at + chrono::TimeDelta::seconds(1), &Utc)
                    .is_empty()
            );
            unavailable.store(false, Ordering::SeqCst);
            assert_eq!(
                scheduler.tick_at(at + chrono::TimeDelta::minutes(1), &Utc),
                [Ran::Fired { id, at }]
            );
            assert!(scheduler.wait(Duration::from_secs(10)));
            assert!(
                surface.schedules(&Client::Cli).unwrap()[0]
                    .last_fired
                    .is_some()
            );
            assert!(
                scheduler
                    .tick_at(at + chrono::TimeDelta::minutes(2), &Utc)
                    .is_empty()
            );
            assert_eq!(
                sim.soap_log()[before..]
                    .iter()
                    .filter(|call| call.action == "Play")
                    .count(),
                1
            );
            assert_eq!(
                soap::get_transport_info(&sim.lan(), sim.player(room).unwrap().ip)
                    .unwrap()
                    .state,
                TransportState::Playing
            );
        }
    }
}

#[test]
fn an_unavailable_scheduled_source_expires_without_changing_the_house() {
    let (sim, surface, unavailable) = fixture(Policy::default());
    seed(&sim, "Den");
    surface
        .save_scene_from(
            &Client::Cli,
            "Morning",
            &definition("Den", "Study", &favorite(), true),
        )
        .unwrap();
    let (scheduler, id, at) = schedule(&surface, "Morning");
    unavailable.store(true, Ordering::SeqCst);
    let before = sim.soap_log().len();
    assert_eq!(scheduler.tick_at(at, &Utc), [Ran::Fired { id, at }]);
    assert!(scheduler.wait(Duration::from_secs(10)));
    assert_eq!(
        scheduler.tick_at(at + chrono::TimeDelta::minutes(11), &Utc),
        [Ran::Skipped { id, at }]
    );
    unavailable.store(false, Ordering::SeqCst);
    assert!(
        scheduler
            .tick_at(at + chrono::TimeDelta::minutes(12), &Utc)
            .is_empty()
    );
    assert_read_only(&sim, before);
}

#[test]
fn a_source_denied_by_policy_does_not_block_the_scenes_permitted_work() {
    let policy =
        Policy::from_toml("[clients.\"tag:agent\"]\ndeny = [\"play_favorite\"]\n").unwrap();
    let (sim, surface, unavailable) = fixture(policy);
    surface
        .save_scene_from(
            &Client::Cli,
            "Quiet",
            &definition("Den", "Study", &favorite(), true),
        )
        .unwrap();
    unavailable.store(true, Ordering::SeqCst);
    let result = surface
        .apply_scene(&Client::Tailnet("tag:agent".into()), "Quiet")
        .unwrap();
    assert!(result.complete && result.changed, "{result:?}");
    assert!(
        result
            .notes
            .iter()
            .any(|note| note.code == NoteCode::Skipped)
    );
    assert_eq!(
        soap::get_volume(&sim.lan(), sim.player("Den").unwrap().ip).unwrap(),
        20
    );
}

#[test]
fn scene_authoring_keeps_catalog_sources_for_recall_and_episodes_play_natively() {
    for (room, member) in [("Kitchen", "Office"), ("Den", "Study")] {
        let (sim, surface, _) = fixture(Policy::default());
        let before = sim.soap_log().len();
        for kind in ["artist", "show"] {
            let source = json!({
                "kind": "uri",
                "uri": format!("spotify:{kind}:0123456789ABCDEFabcdef")
            });
            surface
                .save_scene_from(
                    &Client::Cli,
                    "Catalog",
                    &definition(room, member, &source, true),
                )
                .unwrap();
            assert_eq!(
                surface.apply_scene(&Client::Cli, "Catalog").unwrap_err().code,
                ErrorCode::SpotifyAuthRequired
            );
        }
        assert_read_only(&sim, before);

        let source = json!({
            "kind": "uri",
            "uri": "spotify:episode:0123456789ABCDEFabcdef"
        });
        surface
            .save_scene_from(
                &Client::Cli,
                "Episode",
                &definition(room, member, &source, true),
            )
            .unwrap();
        let result = surface.apply_scene(&Client::Cli, "Episode").unwrap();
        assert!(result.complete && result.changed, "{result:?}");
        assert_eq!(
            soap::get_transport_info(&sim.lan(), sim.player(room).unwrap().ip)
                .unwrap()
                .state,
            TransportState::Playing
        );
    }
}

#[test]
fn catalog_sources_without_a_session_preserve_the_house_and_leave_schedules_unclaimed() {
    for (room, member) in [("Kitchen", "Office"), ("Den", "Study")] {
        for kind in ["artist", "show"] {
            let source_uri = format!("spotify:{kind}:0123456789ABCDEFabcdef");
            let scene: scenes::Scene = serde_json::from_value(json!({
                "name": "Legacy",
                "groups": [{
                    "coordinator": room,
                    "members": [member],
                    "source": {
                        "kind": "uri",
                        "uri": source_uri
                    },
                    "playing": true
                }],
                "volumes": {(room): 20, (member): 15},
                "mutes": {(member): true}
            }))
            .unwrap();
            let mut store = MemStore::default();
            scenes::save(&mut store, &scene, 0).unwrap();
            let mut paused = scene.clone();
            paused.name = "Already paused".into();
            paused.groups[0].members.clear();
            paused.groups[0].playing = Some(false);
            paused.volumes.clear();
            paused.mutes.clear();
            scenes::save(&mut store, &paused, 0).unwrap();
            let policy =
                Policy::from_toml("[clients.\"tag:blocked\"]\ndeny = [\"play\"]\n").unwrap();
            let (sim, surface, _) = fixture_with_store(policy, store);
            seed(&sim, room);
            let before = sim.soap_log().len();
            let error = surface
                .apply_scene_with(
                    &Client::Tailnet("tag:agent".into()),
                    "Legacy",
                    &SceneApplyRequest { fade_secs: Some(1) },
                )
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::SpotifyAuthRequired);

            let (scheduler, id, at) = schedule(&surface, "Legacy");
            assert_eq!(scheduler.tick_at(at, &Utc), [Ran::Fired { id, at }]);
            assert!(scheduler.wait(Duration::from_secs(10)));
            assert!(
                surface.schedules(&Client::Cli).unwrap()[0]
                    .last_fired
                    .is_none()
            );
            assert_read_only(&sim, before);
            let host = sim.player(room).unwrap().ip;
            assert_eq!(soap::get_volume(&sim.lan(), host).unwrap(), 85);
            assert_eq!(
                soap::get_transport_info(&sim.lan(), host).unwrap().state,
                TransportState::Playing
            );

            // A failed legacy attempt can leave a bare renderer selected.
            // Recall must still resolve real tracks or episodes before writes.
            let catalog = content::browse_all(&sim.lan(), host, "FV:2").unwrap();
            let sid = learn_spotify_params(&catalog).unwrap().sid;
            let renderer = spotify_bare_uri(&source_uri, sid);
            let metadata = spotify_bare_didl(&source_uri, "Legacy source", sid);
            soap::stop(&sim.lan(), host).unwrap();
            soap::set_av_transport_uri(&sim.lan(), host, &renderer, &metadata).unwrap();
            let before_resume = sim.soap_log().len();
            let error = surface.apply_scene(&Client::Cli, "Legacy").unwrap_err();
            assert_eq!(error.code, ErrorCode::SpotifyAuthRequired);
            assert_eq!(
                scheduler.tick_at(at + chrono::TimeDelta::minutes(1), &Utc),
                [Ran::Fired { id, at }]
            );
            assert!(scheduler.wait(Duration::from_secs(10)));
            assert!(
                surface.schedules(&Client::Cli).unwrap()[0]
                    .last_fired
                    .is_none()
            );
            let error = surface
                .apply_scene(&Client::Cli, "Already paused")
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::SpotifyAuthRequired);
            assert_read_only(&sim, before_resume);
            assert_eq!(
                soap::get_transport_info(&sim.lan(), host).unwrap().state,
                TransportState::Stopped
            );

            // A forbidden source does not obstruct permitted room changes.
            let result = surface
                .apply_scene(&Client::Tailnet("tag:blocked".into()), "Legacy")
                .unwrap();
            assert!(result.complete && result.changed, "{result:?}");
            assert!(
                result
                    .notes
                    .iter()
                    .any(|note| note.code == NoteCode::Skipped)
            );
            assert_eq!(soap::get_volume(&sim.lan(), host).unwrap(), 20);
        }
    }
}

#[test]
fn a_captured_renderer_source_still_resumes_without_selecting_it_again() {
    for room in ["Kitchen", "Den"] {
        let (sim, surface, _) = fixture(Policy::default());
        seed(&sim, room);
        surface.save_scene(&Client::Cli, "Captured").unwrap();
        let host = sim.player(room).unwrap().ip;
        soap::pause(&sim.lan(), host).unwrap();
        let before = sim.soap_log().len();
        let result = surface.apply_scene(&Client::Cli, "Captured").unwrap();
        assert!(result.complete && result.changed, "{result:?}");
        let log = sim.soap_log();
        assert_eq!(
            log[before..]
                .iter()
                .filter(|call| call.action == "Play")
                .count(),
            1
        );
        assert!(
            log[before..]
                .iter()
                .all(|call| call.action != "SetAVTransportURI")
        );
        assert_eq!(
            soap::get_transport_info(&sim.lan(), host).unwrap().state,
            TransportState::Playing
        );
    }
}

#[test]
fn silent_loading_requires_both_caller_and_writer_pause_permission() {
    for (room, member) in [("Kitchen", "Office"), ("Den", "Study")] {
        for source in [favorite(), spotify()] {
            for writer_denied in [false, true] {
                let policy =
                    Policy::from_toml("[clients.\"tag:blocked\"]\ndeny = [\"pause\"]\n").unwrap();
                let (sim, surface, unavailable) = fixture(policy);
                seed(&sim, room);
                let cli = Client::Cli;
                let blocked = Client::Tailnet("tag:blocked".into());
                let (writer, caller) = if writer_denied {
                    (&blocked, &cli)
                } else {
                    (&cli, &blocked)
                };
                surface
                    .save_scene_from(writer, "Quiet", &definition(room, member, &source, false))
                    .unwrap();
                // An omitted source does not need its catalog to be ready.
                unavailable.store(true, Ordering::SeqCst);
                let before = sim.soap_log().len();
                let result = surface.apply_scene(caller, "Quiet").unwrap();
                assert!(result.complete && result.changed, "{result:?}");
                assert!(
                    result
                        .notes
                        .iter()
                        .any(|note| note.code == NoteCode::Skipped)
                );
                let calls = sim.soap_log();
                let transport_writes = [
                    "Pause",
                    "Stop",
                    "Play",
                    "AddURIToQueue",
                    "RemoveTrackRangeFromQueue",
                ];
                assert!(
                    calls[before..].iter().all(|call| {
                        !transport_writes.contains(&call.action.as_str())
                            && (call.room != room || call.action != "SetAVTransportURI")
                    }),
                    "a denied silent load changed playback: {:?}",
                    &calls[before..]
                );
                let host = sim.player(room).unwrap().ip;
                assert_eq!(
                    soap::get_media_info(&sim.lan(), host).unwrap().uri,
                    ORIGINAL
                );
                assert_eq!(
                    soap::get_transport_info(&sim.lan(), host).unwrap().state,
                    TransportState::Playing
                );
                assert_eq!(soap::get_volume(&sim.lan(), host).unwrap(), 20);
                let mut house = HouseholdState::default();
                house.apply_topology(&get_zone_group_state(&sim.lan(), host).unwrap());
                let joined = house
                    .rooms
                    .iter()
                    .find(|entry| entry.name == member)
                    .unwrap();
                assert_eq!(joined.coordinator.0, sim.player(room).unwrap().uuid);
            }
        }
    }
}
