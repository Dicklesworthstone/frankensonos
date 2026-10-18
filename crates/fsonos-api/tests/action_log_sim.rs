//! Every mutating call through `Surface::control` is policy-bounded, logged
//! and undoable — exercised against `fsonos-sim` players over real loopback
//! sockets, the way the HTTP API and MCP tools drive it.

use fsonos_api::plan::{
    TransportAction, plan_group, plan_play, plan_transport_set, plan_ungroup_set, plan_volume,
    plan_volume_set,
};
use fsonos_api::surface::{Surface, Survey};
use fsonos_api::{GroupRequest, PlayRequest, VolumeRequest, ZoneRequest};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};

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
    // In room terms, not debug output.
    assert!(
        a.intent.starts_with("set_volume: Kitchen to "),
        "{}",
        a.intent
    );
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
fn an_agent_undoes_only_its_own_action_and_only_up_to_its_cap() {
    let sim = sim();
    let lan = sim.lan();
    let houses = now(&sim);
    let kitchen = id(&houses, "Kitchen");
    control::set_volume(&lan, &houses, &kitchen, 90).unwrap();
    let s = surface(&sim);
    let set = |client: &Client, level: i64| {
        s.control(client, "set_volume", |h| {
            plan_volume(h, &volume_req("Kitchen", level))
        })
        .unwrap()
    };
    // The agent turns the Kitchen down from 90; then the owner sets 60.
    set(&agent(), 10);
    set(&Client::Cli, 60);
    // Asking for anyone's newest, the agent still undoes only its own, and
    // the 90 before it comes back only as far as its cap (70), not past it.
    let report = s
        .undo(&agent(), false)
        .unwrap()
        .expect("the agent's own action");
    assert!(report.intent.starts_with("set_volume"), "{report:?}");
    assert_eq!(control::volume(&lan, &houses, &kitchen).unwrap(), 70);
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
fn ungrouping_everywhere_is_seen_by_the_next_zones_read() {
    let sim = sim();
    let s = surface(&sim);
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
    // This read caches the grouped survey.
    let grouped = s.zones(&agent()).unwrap();
    assert_eq!(grouped.len(), 1, "{grouped:?}");
    assert_eq!(grouped[0].members.len(), 2, "{grouped:?}");

    s.control_set(&agent(), "ungroup", |h| {
        plan_ungroup_set(
            h,
            &ZoneRequest {
                zone: "everywhere".into(),
            },
        )
    })
    .unwrap();
    assert_eq!(now(&sim)[0].groups.len(), 2, "the speakers regrouped");
    let apart = s.zones(&agent()).unwrap();
    assert_eq!(
        apart.len(),
        2,
        "the next read is not the cached survey: {apart:?}"
    );
    assert!(apart.iter().all(|z| z.members.len() == 1), "{apart:?}");
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

#[test]
fn a_set_of_rooms_acts_on_each_room_and_undoes_as_one() {
    let sim = sim();
    let lan = sim.lan();
    let houses = now(&sim);
    let (kitchen, office) = (id(&houses, "Kitchen"), id(&houses, "Office"));
    control::set_volume(&lan, &houses, &kitchen, 30).unwrap();
    control::set_volume(&lan, &houses, &office, 40).unwrap();
    control::play_uri(&lan, &houses, &kitchen, STREAM, "").unwrap();
    control::play_uri(&lan, &houses, &office, OTHER, "").unwrap();
    let s = surface(&sim);
    let state = |p: &PlayerId| {
        control::playback(&lan, &now(&sim), p)
            .unwrap()
            .transport
            .state
    };
    let volume = |p: &PlayerId| control::volume(&lan, &houses, p).unwrap();
    assert_eq!(
        (state(&kitchen), state(&office)),
        (TransportState::Playing, TransportState::Playing)
    );

    // `pause everywhere` reaches both groups (they play apart) as one action.
    s.control_set(&agent(), "pause", |h| {
        plan_transport_set(
            h,
            &ZoneRequest {
                zone: "everywhere".into(),
            },
            TransportAction::Pause,
        )
    })
    .unwrap();
    assert_ne!(state(&kitchen), TransportState::Playing);
    assert_ne!(state(&office), TransportState::Playing);

    // A volume on `all` sets every room, logged as a single action.
    let outcome = s
        .control_set(&agent(), "set_volume", |h| {
            plan_volume_set(h, &volume_req("all", 20))
        })
        .unwrap();
    assert!(outcome.changed, "{outcome:?}");
    assert_eq!((volume(&kitchen), volume(&office)), (20, 20));
    let log = s
        .recent_actions(&agent(), &ActionFilter::default())
        .unwrap();
    assert_eq!(log.len(), 2, "one action per set: {log:?}");
    let newest = &log[0].action;
    assert!(
        newest.intent.starts_with("set_volume: ")
            && newest.intent.contains("Kitchen to 20")
            && newest.intent.contains("Office to 20"),
        "{}",
        newest.intent
    );
    assert!(newest.before_state.is_some());

    // One undo puts every room's volume back...
    let report = s
        .undo(&agent(), false)
        .unwrap()
        .expect("the volume to undo");
    assert!(report.failures.is_empty(), "{report:?}");
    assert_eq!((volume(&kitchen), volume(&office)), (30, 40));
    // ...and the next one resumes both groups.
    let report = s.undo(&agent(), false).unwrap().expect("the pause to undo");
    assert!(report.failures.is_empty(), "{report:?}");
    assert_eq!(
        (state(&kitchen), state(&office)),
        (TransportState::Playing, TransportState::Playing)
    );
    sim.shutdown();
}
