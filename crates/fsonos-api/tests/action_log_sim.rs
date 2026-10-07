//! Every mutating call through `Surface::control` is policy-bounded, logged
//! and undoable — exercised against `fsonos-sim` players over real loopback
//! sockets, the way the HTTP API and MCP tools drive it.

use fsonos_api::plan::{plan_group, plan_play, plan_volume};
use fsonos_api::surface::{Surface, Survey};
use fsonos_api::{GroupRequest, PlayRequest, VolumeRequest};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;

const STREAM: &str = "x-rincon-mp3radio://stream.example.invalid/first.mp3";
const OTHER: &str = "x-rincon-mp3radio://stream.example.invalid/other.mp3";

fn agent() -> Client {
    Client::Tailnet("tag:agent".into())
}

fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .spawn()
        .unwrap()
}

fn surface(sim: &SimHandle) -> Surface {
    let kitchen = sim.player("Kitchen").unwrap().ip;
    let survey: Survey = Box::new(move |t| {
        let mut st = HouseholdState::default();
        st.apply_topology(&get_zone_group_state(t, kitchen).map_err(fsonos_core::CoreError::from)?);
        Ok(vec![st])
    });
    Surface::new(
        Box::new(sim.lan()),
        survey,
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(MemStore::default()), "mcp")
}

/// The households as they are right now (the surface caches its survey).
fn now(sim: &SimHandle) -> Vec<HouseholdState> {
    let lan = sim.lan();
    let mut st = HouseholdState::default();
    st.apply_topology(&get_zone_group_state(&lan, sim.player("Kitchen").unwrap().ip).unwrap());
    vec![st]
}

fn id(houses: &[HouseholdState], room: &str) -> PlayerId {
    resolve_room(houses, room).unwrap().player.id.clone()
}

fn volume_req(zone: &str, volume: i64) -> VolumeRequest {
    VolumeRequest {
        zone: zone.into(),
        volume: Some(volume),
        delta: None,
        group: false,
    }
}

#[test]
fn clamped_volume_is_logged_with_its_note_and_undone() {
    let sim = sim();
    let lan = sim.lan();
    let houses = now(&sim);
    let kitchen = id(&houses, "Kitchen");
    control::set_volume(&lan, &houses, &kitchen, 60).unwrap();
    let s = surface(&sim);

    // An agent asks for 90: the default policy caps rooms at 70.
    let outcome = s
        .control(&agent(), "set_volume", |h| {
            plan_volume(h, &volume_req("Kitchen", 90))
        })
        .unwrap();
    assert_eq!(outcome.volume, Some(70), "{outcome:?}");
    assert!(!outcome.notes.is_empty(), "the clamp is noted: {outcome:?}");
    assert_eq!(control::volume(&lan, &houses, &kitchen).unwrap(), 70);

    let log = s
        .recent_actions(&agent(), &ActionFilter::default())
        .unwrap();
    assert_eq!(log.len(), 1);
    let a = &log[0].action;
    assert_eq!(
        (a.client.as_str(), a.surface.as_str()),
        ("tag:agent", "mcp")
    );
    assert!(a.intent.starts_with("set_volume: Volume"), "{}", a.intent);
    assert!(a.decision.starts_with("clamp: "), "{}", a.decision);
    assert!(a.before_state.is_some());

    let report = s
        .undo(&agent(), false)
        .unwrap()
        .expect("one action to undo");
    assert_eq!(report.undone, log[0].id);
    assert!(report.failures.is_empty(), "{report:?}");
    assert_eq!(control::volume(&lan, &houses, &kitchen).unwrap(), 60);
    // The undo is logged too, and there is nothing further to undo.
    let log = s
        .recent_actions(&agent(), &ActionFilter::default())
        .unwrap();
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].action.undo_of, Some(report.undone));
    assert_eq!(s.undo(&agent(), false).unwrap(), None);
    sim.shutdown();
}

#[test]
fn undo_restores_the_uri_and_the_group() {
    let sim = sim();
    let lan = sim.lan();
    let houses = now(&sim);
    let kitchen = id(&houses, "Kitchen");
    control::play_uri(&lan, &houses, &kitchen, STREAM, "").unwrap();
    let s = surface(&sim);
    let uri = || {
        control::playback(&lan, &now(&sim), &kitchen)
            .unwrap()
            .position
            .uri
    };

    s.control(&agent(), "play", |h| {
        plan_play(
            h,
            &PlayRequest {
                zone: "Kitchen".into(),
                source_uri: OTHER.into(),
                title: None,
            },
        )
    })
    .unwrap();
    assert_eq!(uri(), OTHER);
    s.undo(&agent(), false).unwrap().unwrap();
    assert_eq!(uri(), STREAM);

    // Office joins Kitchen's group; undo puts it back on its own.
    s.control(&agent(), "group", |h| {
        plan_group(
            h,
            &GroupRequest {
                zone: "Office".into(),
                to: "Kitchen".into(),
            },
        )
    })
    .unwrap();
    let grouped = now(&sim);
    let office = id(&grouped, "Office");
    assert_eq!(grouped[0].coordinator_of(&office), Some(&kitchen));
    let report = s.undo(&agent(), false).unwrap().unwrap();
    assert!(report.failures.is_empty(), "{report:?}");
    let after = now(&sim);
    assert_eq!(
        after[0].coordinator_of(&office),
        Some(&office),
        "{report:?}"
    );
    sim.shutdown();
}

#[test]
fn denied_calls_are_logged_and_not_undoable() {
    let sim = sim();
    let s = surface(&sim);
    // Unidentified callers may only use read-only tools.
    let err = s
        .control(&Client::Unknown, "set_volume", |h| {
            plan_volume(h, &volume_req("Kitchen", 10))
        })
        .unwrap_err();
    assert_eq!(err.exit_code(), 5, "{err:?}");
    let log = s
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].action.client, "unknown");
    assert!(log[0].action.decision.starts_with("deny: "));
    assert_eq!(log[0].action.before_state, None);
    assert_eq!(s.undo(&Client::Cli, false).unwrap(), None);
    sim.shutdown();
}
