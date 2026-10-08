//! Scenes through `Surface`, against `fsonos-sim` players over real sockets:
//! save the house, change it, apply the scene (only the steps the house
//! needs, volumes within the caller's caps), undo the apply, and the
//! failures every surface reports (UNKNOWN_SCENE, CROSS_HOUSEHOLD_GROUP,
//! INVALID_ARGUMENT, POLICY_DENIED, and no store).

use fsonos_api::plan::{
    TransportAction, plan_group, plan_play, plan_transport, plan_ungroup, plan_volume,
};
use fsonos_api::surface::{Surface, Survey};
use fsonos_api::{ErrorCode, GroupRequest, NoteCode, PlayRequest, VolumeRequest, ZoneRequest};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::scenes::{self, Scene, SceneGroup, SceneSource};
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_sim::{SimHandle, SimHousehold};
use fsonos_types::{PlayerId, TransportState};
use std::collections::BTreeMap;
use std::time::Duration;

const STREAM: &str = "x-rincon-mp3radio://stream.example.invalid/evening.mp3";

fn survey() -> Survey {
    Box::new(|t| Ok(fsonos_core::inventory::survey(t, &[], Duration::from_millis(500))?.households))
}

fn surface(sim: &SimHandle, store: MemStore) -> Surface {
    Surface::new(
        Box::new(sim.lan()),
        survey(),
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(store), "mcp")
}

fn agent() -> Client {
    Client::Tailnet("tag:agent".into())
}

/// The households as they are right now (the surface caches its survey).
fn house(sim: &SimHandle) -> Vec<HouseholdState> {
    fsonos_core::inventory::survey(&sim.lan(), &[], Duration::from_millis(500))
        .unwrap()
        .households
}

fn id(houses: &[HouseholdState], room: &str) -> PlayerId {
    resolve_room(houses, room).unwrap().player.id.clone()
}

/// Kitchen's volume, whether Office plays in Kitchen's group, and what
/// Kitchen's group is doing and on.
fn kitchen(sim: &SimHandle) -> (u8, bool, TransportState, String) {
    let lan = sim.lan();
    let houses = house(sim);
    let (kitchen, office) = (id(&houses, "Kitchen"), id(&houses, "Office"));
    let together = houses
        .iter()
        .any(|h| h.coordinator_of(&office) == Some(&kitchen));
    let playback = control::playback(&lan, &houses, &kitchen).unwrap();
    (
        control::volume(&lan, &houses, &kitchen).unwrap(),
        together,
        playback.transport.state,
        playback.position.uri,
    )
}

fn volume(s: &Surface, client: &Client, zone: &str, level: i64) {
    let req = VolumeRequest {
        zone: zone.into(),
        volume: Some(level),
        delta: None,
        group: false,
    };
    s.control(client, "set_volume", |h| plan_volume(h, &req))
        .unwrap();
}

fn zone(name: &str) -> ZoneRequest {
    ZoneRequest { zone: name.into() }
}

#[test]
fn apply_puts_the_saved_house_back_and_undo_restores_what_it_changed() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let s = surface(&sim, MemStore::default());
    let cli = Client::Cli;
    let play = PlayRequest {
        zone: "Kitchen".into(),
        source_uri: STREAM.into(),
        title: None,
    };
    s.control(&cli, "play", |h| plan_play(h, &play)).unwrap();
    volume(&s, &cli, "Kitchen", 30);
    let group = GroupRequest {
        zone: "Office".into(),
        to: "Kitchen".into(),
    };
    s.control(&cli, "group", |h| plan_group(h, &group)).unwrap();

    let saved = s.save_scene(&cli, " Evening ").unwrap();
    assert_eq!(saved.name, "Evening");
    let lead = saved
        .groups
        .iter()
        .find(|g| g.coordinator == "Kitchen")
        .expect("Kitchen leads a group");
    assert_eq!(lead.members, ["Office"], "{saved:?}");
    assert_eq!(
        (lead.source.kind.as_str(), lead.source.uri.as_deref()),
        ("uri", Some(STREAM))
    );
    assert!(lead.playing);
    assert_eq!(saved.volumes.get("Kitchen"), Some(&30));
    assert_eq!(
        saved.groups.len(),
        3,
        "Kitchen+Office, Living Room, Bedroom"
    );
    let listed = s.scenes(&Client::Unknown).unwrap();
    assert_eq!(listed.len(), 1, "anyone may read the scenes");
    assert_eq!(listed[0], saved);
    assert_eq!(s.scene(&cli, "EVENING").unwrap(), saved);

    // Change the house: Office leaves, Kitchen louder and paused.
    s.control(&cli, "ungroup", |h| plan_ungroup(h, &zone("Office")))
        .unwrap();
    volume(&s, &cli, "Kitchen", 60);
    s.control(&cli, "pause", |h| {
        plan_transport(h, &zone("Kitchen"), TransportAction::Pause)
    })
    .unwrap();
    let changed = kitchen(&sim);
    assert_eq!(
        (changed.0, changed.1),
        (60, false),
        "the house changed: {changed:?}"
    );

    let applied = s.apply_scene(&cli, "evening").unwrap();
    assert!(applied.changed && applied.complete, "{applied:?}");
    assert_eq!(applied.scene, "Evening");
    assert!(
        applied.steps.iter().any(|s| s == "Office joins Kitchen")
            && applied.steps.iter().any(|s| s == "Kitchen volume 30"),
        "{applied:?}"
    );
    assert_eq!(
        kitchen(&sim),
        (30, true, TransportState::Playing, STREAM.to_string()),
        "{applied:?}"
    );

    // The house matches now: nothing is sent, and nothing logged.
    let again = s.apply_scene(&cli, "Evening").unwrap();
    assert!(!again.changed && again.steps.is_empty(), "{again:?}");
    let log = s.recent_actions(&cli, &ActionFilter::default()).unwrap();
    assert_eq!(log[0].action.intent, "apply_scene: Evening", "{log:?}");
    assert!(log[0].action.before_state.is_some());
    assert_eq!(
        log.iter()
            .filter(|a| a.action.intent.starts_with("apply_scene"))
            .count(),
        1
    );

    // Undo puts back what the apply changed.
    let report = s.undo(&cli, false).unwrap().expect("the apply to undo");
    assert_eq!(report.undone, log[0].id);
    assert!(report.failures.is_empty(), "{report:?}");
    let undone = kitchen(&sim);
    assert_eq!((undone.0, undone.1), (60, false), "{undone:?}");
    assert_ne!(undone.2, TransportState::Playing, "{undone:?}");
    sim.shutdown();
}

#[test]
fn an_agent_applies_a_scene_within_its_volume_cap() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let s = surface(&sim, MemStore::default());
    volume(&s, &Client::Cli, "Kitchen", 90);
    s.save_scene(&Client::Cli, "Loud").unwrap();
    volume(&s, &Client::Cli, "Kitchen", 40);

    let applied = s.apply_scene(&agent(), "Loud").unwrap();
    let level = kitchen(&sim).0;
    assert!(
        level > 40 && level <= 70,
        "capped at 70: {level}, {applied:?}"
    );
    assert!(
        applied
            .notes
            .iter()
            .any(|n| n.code == NoteCode::VolumeClamped),
        "{applied:?}"
    );
    let log = s
        .recent_actions(&agent(), &ActionFilter::default())
        .unwrap();
    assert!(log[0].action.decision.starts_with("clamp: "), "{log:?}");

    // Unidentified callers may read scenes, not save or apply them; the
    // refusal is logged.
    let denied = s.save_scene(&Client::Unknown, "Mine").unwrap_err();
    assert_eq!(denied.code, ErrorCode::PolicyDenied, "{denied:?}");
    let denied = s.apply_scene(&Client::Unknown, "Loud").unwrap_err();
    assert_eq!(denied.code, ErrorCode::PolicyDenied, "{denied:?}");
    let log = s
        .recent_actions(&agent(), &ActionFilter::default())
        .unwrap();
    assert!(log[0].action.decision.starts_with("deny: "), "{log:?}");
    assert_eq!(s.scenes(&Client::Unknown).unwrap().len(), 1);
    sim.shutdown();
}

#[test]
fn scene_failures_carry_their_codes() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let mut store = MemStore::default();
    // S1 and S2 rooms can never group; such a scene saves but cannot apply.
    let across = Scene {
        name: "Across".into(),
        groups: vec![SceneGroup {
            coordinator: "Kitchen".into(),
            members: vec!["Living Room".into()],
            source: SceneSource::Keep,
            playing: false,
        }],
        volumes: BTreeMap::new(),
        mutes: BTreeMap::new(),
    };
    scenes::save(&mut store, &across, 0).unwrap();
    let s = surface(&sim, store);

    let err = s.apply_scene(&Client::Cli, "Across").unwrap_err();
    assert_eq!(
        (err.code, err.status()),
        (ErrorCode::CrossHouseholdGroup, 422),
        "{err:?}"
    );
    let err = s.apply_scene(&Client::Cli, "Acros").unwrap_err();
    assert_eq!(
        (err.code, err.status(), err.exit_code()),
        (ErrorCode::UnknownScene, 404, 3),
        "{err:?}"
    );
    assert_eq!(err.suggestions, ["Across"], "{err:?}");
    let err = s.save_scene(&Client::Cli, "  ").unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument, "{err:?}");
    let err = s.save_scene(&Client::Cli, &"x".repeat(65)).unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument, "{err:?}");

    let deleted = s.delete_scene(&Client::Cli, "across").unwrap();
    assert_eq!(deleted.deleted, "Across");
    assert!(s.scenes(&Client::Cli).unwrap().is_empty());
    let err = s.delete_scene(&Client::Cli, "Across").unwrap_err();
    assert_eq!(err.code, ErrorCode::UnknownScene, "{err:?}");

    // Without a store there are no scenes to keep.
    let bare = Surface::new(
        Box::new(sim.lan()),
        survey(),
        Policy::default(),
        Box::new(SystemClock),
    );
    let err = bare.scenes(&Client::Cli).unwrap_err();
    assert_eq!(err.code, ErrorCode::NotImplemented, "{err:?}");
    sim.shutdown();
}
