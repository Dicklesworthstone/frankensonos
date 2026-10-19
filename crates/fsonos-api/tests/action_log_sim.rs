//! Every mutating call through `Surface::control` is policy-bounded, logged
//! and undoable — exercised against `fsonos-sim` players over real loopback
//! sockets, the way the HTTP API and MCP tools drive it.

use fsonos_api::plan::{
    TransportAction, plan_group, plan_play, plan_transport_set, plan_ungroup_set, plan_volume,
    plan_volume_set,
};
use fsonos_api::surface::{Surface, Survey};
use fsonos_api::{ErrorCode, GroupRequest, NoteCode, PlayRequest, VolumeRequest, ZoneRequest};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;

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
fn household_seeds(sim: &SimHandle) -> Vec<IpAddr> {
    [1, 2]
        .into_iter()
        .filter_map(|generation| {
            sim.players()
                .iter()
                .find(|player| player.generation == generation && !player.hidden)
                .map(|player| player.ip)
        })
        .collect()
}

fn multi_surface(sim: &SimHandle) -> Surface {
    let seeds = household_seeds(sim);
    Surface::new(
        Box::new(sim.lan()),
        Box::new(move |t| {
            seeds
                .iter()
                .map(|&ip| {
                    let mut state = HouseholdState::default();
                    state.apply_topology(
                        &get_zone_group_state(t, ip).map_err(fsonos_core::CoreError::from)?,
                    );
                    Ok(state)
                })
                .collect()
        }),
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(MemStore::default()), "mcp")
}

fn multi_now(sim: &SimHandle) -> Vec<HouseholdState> {
    let lan = sim.lan();
    household_seeds(sim)
        .into_iter()
        .map(|ip| {
            let mut state = HouseholdState::default();
            state.apply_topology(&get_zone_group_state(&lan, ip).unwrap());
            state
        })
        .collect()
}

fn id_in(houses: &[HouseholdState], generation: u16, room: &str) -> PlayerId {
    houses
        .iter()
        .find(|house| {
            house
                .generation()
                .is_some_and(|house_gen| house_gen as u16 == generation)
        })
        .and_then(|house| {
            house
                .rooms
                .iter()
                .find(|candidate| candidate.name.eq_ignore_ascii_case(room))
                .map(|candidate| candidate.primary.clone())
        })
        .unwrap()
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
    let (kitchen, office) = (id(&houses, "Kitchen"), id(&houses, "Office"));
    control::set_volume(&lan, &houses, &kitchen, 90).unwrap();
    let s = surface(&sim);
    let set = |client: &Client, room: &str, level: i64| {
        s.control(client, "set_volume", |h| {
            plan_volume(h, &volume_req(room, level))
        })
        .unwrap()
    };
    // The agent turns the Kitchen down from 90 to 55 (decreases are always
    // allowed; 55 is within a max_step of its cap, so the cap is what bounds
    // the way back); then the owner sets the Office to 60. (A change to the
    // Kitchen itself would leave the agent's undo nothing to put back there:
    // see undo_leaves_a_room_changed_since_unless_the_owner_forces_it.)
    set(&agent(), "Kitchen", 55);
    set(&Client::Cli, "Office", 60);
    // Asking for anyone's newest, the agent still undoes only its own, and
    // the 90 before it comes back only as far as its cap (70), not past it.
    let report = s
        .undo(&agent(), false)
        .unwrap()
        .expect("the agent's own action");
    assert!(report.intent.starts_with("set_volume"), "{report:?}");
    assert_eq!(control::volume(&lan, &houses, &kitchen).unwrap(), 70);
    assert_eq!(control::volume(&lan, &houses, &office).unwrap(), 60);
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

/// Found by guard_invariants_sim: a set the policy refuses for one room
/// must not fail after another room changed, or that change is logged as a
/// denial with nothing to undo.
#[test]
fn a_set_skips_a_room_the_policy_refuses_and_the_rest_stays_undoable() {
    let sim = sim();
    let lan = sim.lan();
    let houses = now(&sim);
    let (kitchen, office) = (id(&houses, "Kitchen"), id(&houses, "Office"));
    // Kitchen left above the agent's cap (70), Office already at it.
    control::set_volume(&lan, &houses, &kitchen, 90).unwrap();
    control::set_volume(&lan, &houses, &office, 70).unwrap();
    let s = surface(&sim);
    let volume = |p: &PlayerId| control::volume(&lan, &houses, p).unwrap();
    let all_to = |level| {
        s.control_set(&agent(), "set_volume", |h| {
            plan_volume_set(h, &volume_req("all", level))
        })
    };

    // Kitchen comes down to the cap; Office, which may not rise, is skipped.
    let outcome = all_to(85).unwrap();
    assert_eq!((volume(&kitchen), volume(&office)), (70, 70));
    assert!(
        outcome
            .notes
            .iter()
            .any(|n| n.code == NoteCode::Skipped && n.detail.starts_with("Office skipped")),
        "{outcome:?}"
    );
    let log = s
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(log[0].action.decision.starts_with("clamp: "), "{log:?}");
    assert!(log[0].action.before_state.is_some(), "{log:?}");
    s.undo(&Client::Cli, false)
        .unwrap()
        .expect("Kitchen's lowering to undo");
    assert_eq!((volume(&kitchen), volume(&office)), (90, 70));

    // Every room refused: the call is denied and changes nothing.
    control::set_volume(&lan, &houses, &kitchen, 70).unwrap();
    let denied = all_to(85).unwrap_err();
    assert_eq!(denied.code, ErrorCode::PolicyDenied, "{denied:?}");
    assert_eq!((volume(&kitchen), volume(&office)), (70, 70));
    sim.shutdown();
}

/// p7vq.3: undo puts an action back only while its rooms are as it left
/// them. A room someone changed since (the Sonos app, here) makes the undo
/// refuse, naming the change, and puts nothing back; the action stays
/// undoable, so the owner's forced undo puts back that same action (an
/// agent may not force).
#[test]
fn undo_refuses_a_set_someone_changed_since_until_the_owner_forces_it() {
    let sim = sim();
    let lan = sim.lan();
    let houses = now(&sim);
    let (kitchen, office) = (id(&houses, "Kitchen"), id(&houses, "Office"));
    control::set_volume(&lan, &houses, &kitchen, 40).unwrap();
    control::set_volume(&lan, &houses, &office, 40).unwrap();
    let s = surface(&sim);
    let volume = |p: &PlayerId| control::volume(&lan, &houses, p).unwrap();
    let all_to = |level| {
        s.control_set(&Client::Cli, "set_volume", |h| {
            plan_volume_set(h, &volume_req("all", level))
        })
        .unwrap()
    };

    all_to(20);
    // The Sonos app moves Kitchen afterwards.
    control::set_volume(&lan, &houses, &kitchen, 55).unwrap();
    let refused = s.undo(&Client::Cli, false).unwrap_err();
    assert_eq!(refused.code, ErrorCode::InvalidArgument, "{refused:?}");
    assert!(
        refused.detail.contains("Kitchen's volume 20 -> 55") && refused.detail.contains("--force"),
        "{refused:?}"
    );
    assert_eq!(
        (volume(&kitchen), volume(&office)),
        (55, 20),
        "nothing put back"
    );

    // An agent may not force; the owner's CLI may, and it puts back that
    // same set (the refusal did not use it up).
    let denied = s.undo_forced(&agent(), false, true).unwrap_err();
    assert_eq!(denied.code, ErrorCode::PolicyDenied, "{denied:?}");
    let report = s
        .undo_forced(&Client::Cli, false, true)
        .unwrap()
        .expect("the refused set, still undoable");
    assert!(report.failures.is_empty(), "{report:?}");
    assert_eq!((volume(&kitchen), volume(&office)), (40, 40));
    sim.shutdown();
}

/// A room grouped into the action's zone since is a change too: putting the
/// zone back would take that room out of its group again.
#[test]
fn undo_refuses_after_a_room_joined_the_group_since() {
    let sim = sim();
    let lan = sim.lan();
    let houses = now(&sim);
    let (kitchen, office) = (id(&houses, "Kitchen"), id(&houses, "Office"));
    let s = surface(&sim);
    s.control(&agent(), "set_volume", |h| {
        plan_volume(h, &volume_req("Kitchen", 30))
    })
    .unwrap();
    // The Sonos app groups the Office into the Kitchen.
    control::join(&lan, &houses, &office, &kitchen).unwrap();
    let refused = s.undo(&agent(), false).unwrap_err();
    assert!(
        refused.detail.contains("Office joined the group"),
        "{refused:?}"
    );
    let grouped = now(&sim);
    assert_eq!(
        grouped
            .iter()
            .find_map(|h| h.coordinator_of(&office))
            .cloned(),
        Some(kitchen),
        "the Office stays grouped"
    );
    sim.shutdown();
}
/// Room names are household-local. An S1 Kitchen having been touched must
/// not hide an S2 Kitchen joining a group since: undo must refuse rather
/// than take the S2 Kitchen back out.
#[test]
fn undo_sees_a_joined_duplicate_room_name_across_households() {
    let sim = SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .s2([
            SimPlayerSpec::new("Living Room", SimModel::One),
            SimPlayerSpec::new("Kitchen", SimModel::One),
        ])
        .spawn()
        .unwrap();
    let lan = sim.lan();
    let houses = multi_now(&sim);
    let s1_kitchen = id_in(&houses, 1, "Kitchen");
    let (s2_living, s2_kitchen) = (
        id_in(&houses, 2, "Living Room"),
        id_in(&houses, 2, "Kitchen"),
    );
    assert_ne!(s1_kitchen, s2_kitchen, "the names match; the rooms do not");
    let surface = multi_surface(&sim);
    surface
        .control_set(&agent(), "set_volume", |house| {
            plan_volume_set(house, &volume_req("all", 30))
        })
        .unwrap();

    control::join(&lan, &houses, &s2_kitchen, &s2_living).unwrap();
    let refused = surface.undo(&agent(), false).unwrap_err();
    assert!(
        refused.detail.contains("Kitchen joined the group"),
        "{refused:?}"
    );
    assert_eq!(
        multi_now(&sim)
            .iter()
            .find_map(|house| house.coordinator_of(&s2_kitchen))
            .cloned(),
        Some(s2_living),
        "the S2 Kitchen stays grouped"
    );

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
