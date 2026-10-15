//! Household-qualified scenes through the same Surface used by CLI, HTTP,
//! MCP and scheduled apply_scene actions. All players serve real loopback
//! sockets; names, household ids and music are synthetic.

use chrono::{DateTime, Utc};
use fsonos_api::plan::{plan_group, plan_ungroup, plan_volume};
use fsonos_api::surface::scenes::SceneDefinition;
use fsonos_api::surface::schedules::{Ran, ScheduleRequest, Scheduler};
use fsonos_api::surface::{Surface, Survey};
use fsonos_api::{ErrorCode, GroupRequest, NoteCode, VolumeRequest, ZoneRequest};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::scenes::{self, Scene};
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_core::{HouseholdState, control, resolve_room, snapshot};
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

const OLD_STREAM: &str = "x-rincon-mp3radio://stream.example.invalid/old.mp3";
const NEW_STREAM: &str = "x-rincon-mp3radio://stream.example.invalid/new.mp3";
const ROOMS: [&str; 4] = ["Kitchen@S1", "Office@S1", "Kitchen@S2", "Office@S2"];

fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .s2([
            SimPlayerSpec::pair("Kitchen", SimModel::One),
            SimPlayerSpec::new("Office", SimModel::Play1),
        ])
        .spawn()
        .unwrap()
}

fn surface(sim: &SimHandle, store: MemStore) -> Surface {
    let survey: Survey = Box::new(|t| {
        Ok(fsonos_core::inventory::survey(t, &[], Duration::from_millis(500))?.households)
    });
    Surface::new(
        Box::new(sim.lan()),
        survey,
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(store), "mcp")
}

fn house(sim: &SimHandle) -> Vec<HouseholdState> {
    fsonos_core::inventory::survey(&sim.lan(), &[], Duration::from_millis(500))
        .unwrap()
        .households
}

fn definition(value: serde_json::Value) -> SceneDefinition {
    serde_json::from_value(value).unwrap()
}

fn both() -> SceneDefinition {
    definition(json!({
        "groups": [
            {"coordinator": "kitchen@s1", "members": ["office@S1"],
             "source": {"kind": "uri", "uri": OLD_STREAM}},
            {"coordinator": "Kitchen@S2", "members": ["Office@S2"],
             "source": {"kind": "uri", "uri": NEW_STREAM}}
        ],
        "volumes": {"Kitchen@S1": 11, "Office@S1": 12, "Kitchen@S2": 31, "Office@S2": 32},
        "mutes": {"Office@S1": true, "Office@S2": false}
    }))
}

fn set_volume(s: &Surface, zone: &str, level: i64) {
    let req = VolumeRequest {
        zone: zone.into(),
        volume: Some(level),
        delta: None,
        group: false,
    };
    s.control(&Client::Cli, "set_volume", |h| plan_volume(h, &req))
        .unwrap();
}

fn levels(sim: &SimHandle) -> Vec<(u8, bool)> {
    let households = house(sim);
    let lan = sim.lan();
    ROOMS
        .iter()
        .map(|name| {
            let target = resolve_room(&households, name).unwrap();
            let state = snapshot::capture(&lan, &households, &target.coordinator.id, 0).unwrap();
            let level = state
                .levels
                .iter()
                .find(|level| level.player == target.player.id)
                .unwrap();
            (level.volume, level.mute)
        })
        .collect()
}

fn joined(sim: &SimHandle) -> Vec<bool> {
    let households = house(sim);
    ["S1", "S2"]
        .iter()
        .map(|generation| {
            let lead = resolve_room(&households, &format!("Kitchen@{generation}")).unwrap();
            let member = resolve_room(&households, &format!("Office@{generation}")).unwrap();
            lead.player.id == member.coordinator.id
        })
        .collect()
}

#[test]
fn a_written_scene_controls_duplicate_named_rooms_and_each_households_source() {
    let sim = sim();
    let s = surface(&sim, MemStore::default());
    let saved = s.save_scene_from(&Client::Cli, "Both", &both()).unwrap();
    assert_eq!(saved.groups.len(), 2);
    assert_eq!(saved.volumes.len(), 4);
    assert_eq!(saved.mutes.len(), 2);
    assert!(saved.groups.iter().all(|g| g.coordinator.contains('@')));
    assert_eq!(s.scene(&Client::Cli, "Both").unwrap(), saved);

    let applied = s.apply_scene(&Client::Cli, "Both").unwrap();
    assert!(applied.complete && applied.changed, "{applied:?}");
    assert_eq!(joined(&sim), [true, true]);
    assert_eq!(
        levels(&sim),
        [(11, false), (12, true), (31, false), (32, false)]
    );
    let households = house(&sim);
    for (zone, uri) in [("Kitchen@S1", OLD_STREAM), ("Kitchen@S2", NEW_STREAM)] {
        let target = resolve_room(&households, zone).unwrap();
        let playing = control::playback(&sim.lan(), &households, &target.player.id).unwrap();
        assert_eq!(playing.transport.state, TransportState::Playing);
        assert_eq!(playing.position.uri, uri);
    }
    assert!(
        applied
            .steps
            .iter()
            .filter(|step| step.contains(" joins "))
            .all(|step| { step.contains("Office@") && step.contains("Kitchen@") })
    );
    let again = s.apply_scene(&Client::Cli, "Both").unwrap();
    assert!(!again.changed && again.steps.is_empty(), "{again:?}");
}

#[test]
fn capturing_and_undoing_a_two_household_scene_preserves_all_four_rooms() {
    let sim = sim();
    let s = surface(&sim, MemStore::default());
    s.save_scene_from(&Client::Cli, "Both", &both()).unwrap();
    s.apply_scene(&Client::Cli, "Both").unwrap();
    let captured = s.save_scene(&Client::Cli, "Remember").unwrap();
    assert_eq!(captured.groups.len(), 2);
    assert_eq!(captured.volumes.len(), 4);
    assert_eq!(captured.mutes.len(), 4);
    let original = levels(&sim);

    for (name, level) in ROOMS.iter().zip([41, 42, 51, 52]) {
        set_volume(&s, name, level);
    }
    for name in ["Office@S1", "Office@S2"] {
        let req = ZoneRequest { zone: name.into() };
        s.control(&Client::Cli, "ungroup", |h| plan_ungroup(h, &req))
            .unwrap();
    }
    let changed = levels(&sim);
    assert_eq!(joined(&sim), [false, false]);
    let applied = s.apply_scene(&Client::Cli, "Remember").unwrap();
    assert!(applied.complete, "{applied:?}");
    assert_eq!(levels(&sim), original);
    assert_eq!(joined(&sim), [true, true]);
    let log = s
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    assert_eq!(log[0].action.intent, "apply_scene: Remember");
    assert!(log[0].action.before_state.is_some());

    let undo = s.undo(&Client::Cli, false).unwrap().unwrap();
    assert!(undo.failures.is_empty(), "{undo:?}");
    assert_eq!(levels(&sim), changed);
    assert_eq!(joined(&sim), [false, false]);
}

#[test]
fn each_favorite_is_resolved_in_its_selected_household_before_the_scene_is_saved() {
    let sim = sim();
    let s = surface(&sim, MemStore::default());
    let def = definition(json!({"groups": [
        {"coordinator": "Kitchen@S1", "source": {"kind": "favorite", "name": "Aria"}},
        {"coordinator": "Kitchen@S2", "source": {"kind": "favorite", "name": "Aria"}}
    ]}));
    let saved = s.save_scene_from(&Client::Cli, "Favorites", &def).unwrap();
    let old = saved.groups[0].source.uri.as_deref().unwrap();
    let new = saved.groups[1].source.uri.as_deref().unwrap();
    assert!(old.contains("0SimS1TrackB"), "{old}");
    assert!(new.contains("0SimS2TrackB"), "{new}");
    assert_ne!(old, new);
    let applied = s.apply_scene(&Client::Cli, "Favorites").unwrap();
    assert!(applied.complete, "{applied:?}");
    let households = house(&sim);
    for (zone, expected) in [("Kitchen@S1", old), ("Kitchen@S2", new)] {
        let target = resolve_room(&households, zone).unwrap();
        let playing = control::playback(&sim.lan(), &households, &target.player.id).unwrap();
        assert_eq!(playing.position.uri, expected);
        assert_eq!(playing.transport.state, TransportState::Playing);
    }

    // S1 still has the favorite; S2 must not borrow that account or save an
    // unchecked favorite merely because its canonical room is qualified.
    sim.retain_favorites(2, |uri| !uri.contains("TrackB"))
        .unwrap();
    let missing = s
        .save_scene_from(&Client::Cli, "Missing", &def)
        .unwrap_err();
    assert_eq!(missing.code, ErrorCode::UnknownFavorite);
    assert!(s.scene(&Client::Cli, "Missing").is_err());
}

#[test]
fn ambiguous_or_cross_household_definitions_never_save_or_mutate_a_speaker() {
    let sim = sim();
    let s = surface(&sim, MemStore::default());
    let before = levels(&sim);
    let at = sim.soap_log().len();
    for (value, expected) in [
        (
            json!({"groups": [{"coordinator": "Kitchen"}]}),
            ErrorCode::AmbiguousRoom,
        ),
        (
            json!({"groups": [{"coordinator": "Kitchen@S1", "members": ["Office"]}]}),
            ErrorCode::AmbiguousRoom,
        ),
        (json!({"volumes": {"Kitchen": 5}}), ErrorCode::AmbiguousRoom),
        (json!({"mutes": {"Office": true}}), ErrorCode::AmbiguousRoom),
        (
            json!({"groups": [{"coordinator": "Kitchen@S1", "members": ["Office@S2"]}]}),
            ErrorCode::CrossHouseholdGroup,
        ),
        (
            json!({"groups": [{"coordinator": "Kit@S1"}]}),
            ErrorCode::UnknownRoom,
        ),
    ] {
        let err = s
            .save_scene_from(&Client::Cli, "Refused", &definition(value))
            .unwrap_err();
        assert_eq!(err.code, expected, "{err:?}");
        assert!(s.scene(&Client::Cli, "Refused").is_err());
    }
    assert!(
        sim.soap_log()[at..]
            .iter()
            .all(|entry| { entry.action.starts_with("Get") || entry.action == "Browse" })
    );
    assert_eq!(levels(&sim), before);
}

#[test]
fn duplicate_physical_settings_are_rejected_even_when_the_keys_differ() {
    let sim = sim();
    let s = surface(&sim, MemStore::default());
    let households = house(&sim);
    let target = resolve_room(&households, "Kitchen@S1").unwrap();
    let uuid = &target.player.id.0;
    let qualified = format!("Kitchen@{}", target.household.id.as_ref().unwrap().0);
    for alias in [uuid.as_str(), qualified.as_str(), "kitchen@s1"] {
        for value in [
            json!({"volumes": {"Kitchen@S1": 10, (alias): 20}}),
            json!({"mutes": {"Kitchen@S1": true, (alias): false}}),
        ] {
            let err = s
                .save_scene_from(&Client::Cli, "Collision", &definition(value))
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidArgument, "{err:?}");
        }
    }
    assert!(s.scene(&Client::Cli, "Collision").is_err());
}

#[test]
fn an_ambiguous_legacy_scene_is_rejected_before_snapshot_reads() {
    let sim = sim();
    let mut store = MemStore::default();
    let legacy: Scene = serde_json::from_value(json!({
        "name": "Legacy", "groups": [{"coordinator": "Kitchen", "playing": true}],
        "volumes": {"Office@S2": 90}
    }))
    .unwrap();
    scenes::save(&mut store, &legacy, 0).unwrap();
    let s = surface(&sim, store);
    let err = s.apply_scene(&Client::Cli, "Legacy").unwrap_err();
    assert_eq!(err.code, ErrorCode::AmbiguousRoom);
    let log = sim.soap_log();
    assert!(log.iter().all(|entry| entry.action.starts_with("Get")));
    assert!(!log.iter().any(|entry| matches!(
        entry.action.as_str(),
        "GetVolume" | "GetMute" | "GetPositionInfo" | "GetMediaInfo" | "GetTransportInfo"
    )));
    assert_eq!(levels(&sim), [(20, false); 4]);
}

#[test]
fn a_qualified_volume_scene_is_still_bounded_by_its_writers_policy() {
    let sim = sim();
    let s = surface(&sim, MemStore::default());
    let agent = Client::Tailnet("tag:agent".into());
    set_volume(&s, "Kitchen@S2", 40);
    let def = definition(json!({"volumes": {"Kitchen@S2": 90}}));
    s.save_scene_from(&agent, "Capped", &def).unwrap();
    let applied = s.apply_scene(&Client::Cli, "Capped").unwrap();
    assert!(applied.complete, "{applied:?}");
    let got = levels(&sim);
    assert_eq!(got[0].0, 20, "S1 Kitchen is untouched");
    assert!(got[2].0 > 40 && got[2].0 <= 70, "{got:?}");
    assert!(
        applied
            .notes
            .iter()
            .any(|n| n.code == NoteCode::VolumeClamped)
    );
}

#[test]
fn a_player_id_scene_keeps_its_room_when_the_house_has_duplicate_names() {
    let sim = sim();
    let s = surface(&sim, MemStore::default());
    let households = house(&sim);
    let office = resolve_room(&households, "Office@S2").unwrap();
    let kitchen = resolve_room(&households, "Kitchen@S2").unwrap();
    let office_id: PlayerId = office.player.id.clone();
    let req = GroupRequest {
        zone: office_id.0.clone(),
        to: kitchen.player.id.0.clone(),
    };
    s.control(&Client::Cli, "group", |h| plan_group(h, &req))
        .unwrap();
    let def = definition(json!({"volumes": {(office_id.0): 9}}));
    let saved = s.save_scene_from(&Client::Cli, "Member", &def).unwrap();
    assert!(saved.volumes.keys().all(|key| key.starts_with("Office@")));
    s.apply_scene(&Client::Cli, "Member").unwrap();
    let got = levels(&sim);
    assert_eq!(got[1].0, 20, "S1 Office is untouched");
    assert_eq!(got[2].0, 20, "S2 coordinator volume is untouched");
    assert_eq!(got[3].0, 9, "the S2 member gets its own volume");
}

#[test]
fn the_persisted_scheduler_applies_a_qualified_scene_once() {
    let sim = sim();
    let s = Arc::new(surface(&sim, MemStore::default()));
    s.save_scene_from(&Client::Cli, "Scheduled", &both())
        .unwrap();
    let schedule = s
        .add_schedule(
            &Client::Cli,
            &ScheduleRequest {
                when: "in 1m".into(),
                action: "apply_scene".into(),
                scene: Some("Scheduled".into()),
                ..ScheduleRequest::default()
            },
        )
        .unwrap();
    let at = DateTime::parse_from_rfc3339(schedule.next.as_deref().unwrap())
        .unwrap()
        .to_utc();
    let scheduler = Scheduler::new(&s);
    assert_eq!(
        scheduler.tick_at(at, &Utc),
        [Ran::Fired {
            id: schedule.id,
            at
        }]
    );
    assert!(
        scheduler.wait(Duration::from_secs(20)),
        "scheduled scene did not finish"
    );
    assert_eq!(joined(&sim), [true, true]);
    assert_eq!(
        levels(&sim),
        [(11, false), (12, true), (31, false), (32, false)]
    );
    assert!(scheduler.tick_at(at, &Utc).is_empty());
    let log = s
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    assert_eq!(
        log.iter()
            .filter(|entry| entry.action.intent == "apply_scene: Scheduled")
            .count(),
        1
    );
}
