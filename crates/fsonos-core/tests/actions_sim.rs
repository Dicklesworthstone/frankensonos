//! The action log's undo against `fsonos-sim` over real loopback sockets:
//! capture, change, log, undo, compare — and undo picks the right action.

use fsonos_core::actions::{self, capture_zones};
use fsonos_core::policy::Client;
use fsonos_core::store::{Action, ActionFilter, MemStore, SqliteStore, Store};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;

const STREAM: &str = "x-rincon-mp3radio://stream.example.invalid/first.mp3";
const OTHER: &str = "x-rincon-mp3radio://stream.example.invalid/other.mp3";

fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .spawn()
        .unwrap()
}

fn houses(sim: &SimHandle, lan: &SimLan) -> Vec<HouseholdState> {
    let mut st = HouseholdState::default();
    st.apply_topology(&get_zone_group_state(lan, sim.player("Kitchen").unwrap().ip).unwrap());
    vec![st]
}

fn id(houses: &[HouseholdState], room: &str) -> PlayerId {
    resolve_room(houses, room).unwrap().player.id.clone()
}

fn uri(lan: &SimLan, houses: &[HouseholdState], coordinator: &PlayerId) -> String {
    control::playback(lan, houses, coordinator)
        .unwrap()
        .position
        .uri
}

/// Change `room` (a coordinator) the way a logged action would, logging it
/// with the before-state as `client`.
fn act(
    store: &mut dyn Store,
    lan: &SimLan,
    houses: &[HouseholdState],
    room: &PlayerId,
    client: &Client,
    (volume, source): (u8, &str),
    at: i64,
) -> i64 {
    let (snaps, missed) = capture_zones(lan, houses, &[room.clone(), room.clone()], at);
    assert_eq!((snaps.len(), missed.len()), (1, 0), "{missed:?}");
    control::set_volume(lan, houses, room, volume).unwrap();
    control::play_uri(lan, houses, room, source, "").unwrap();
    actions::record(
        store,
        &Action {
            at,
            client: client.key().into(),
            surface: "mcp".into(),
            intent: format!("volume {volume}, play {source}"),
            decision: "allow".into(),
            result: "done".into(),
            before_state: actions::before_state(&snaps),
            undo_of: None,
        },
    )
    .unwrap()
}

fn undo_restores_volume_and_source(store: &mut dyn Store) {
    let sim = sim();
    let lan = sim.lan();
    let houses = houses(&sim, &lan);
    let kitchen = id(&houses, "Kitchen");
    control::set_volume(&lan, &houses, &kitchen, 20).unwrap();
    control::play_uri(&lan, &houses, &kitchen, STREAM, "").unwrap();

    let agent = Client::Tailnet("tag:agent".into());
    let logged = act(store, &lan, &houses, &kitchen, &agent, (70, OTHER), 100);
    assert_eq!(control::volume(&lan, &houses, &kitchen).unwrap(), 70);
    assert_eq!(uri(&lan, &houses, &kitchen), OTHER);

    let report = actions::undo_last(&lan, &houses, store, None, &Client::Cli, "cli", 200)
        .unwrap()
        .expect("something to undo");
    assert_eq!(report.undone, logged);
    assert!(report.failures.is_empty(), "{report:?}");
    assert_eq!(control::volume(&lan, &houses, &kitchen).unwrap(), 20);
    assert_eq!(uri(&lan, &houses, &kitchen), STREAM);

    // The undo is logged, points at what it undid, and is not undoable.
    let newest = &store
        .recent_actions(&ActionFilter {
            limit: 1,
            ..ActionFilter::default()
        })
        .unwrap()[0];
    assert_eq!(newest.id, report.undo_id);
    assert_eq!(newest.action.undo_of, Some(logged));
    assert_eq!(
        (
            newest.action.client.as_str(),
            newest.action.surface.as_str()
        ),
        ("cli", "cli")
    );
    assert!(
        newest
            .action
            .result
            .starts_with(&format!("undid #{logged}"))
    );
    assert_eq!(
        actions::undo_last(&lan, &houses, store, None, &Client::Cli, "cli", 201).unwrap(),
        None
    );
    sim.shutdown();
}

#[test]
fn undo_restores_volume_and_source_in_memory() {
    undo_restores_volume_and_source(&mut MemStore::default());
}

#[test]
fn undo_restores_volume_and_source_with_fsqlite() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    undo_restores_volume_and_source(&mut store);
    store.close().unwrap();
}

#[test]
fn undo_for_one_client_skips_the_others() {
    let sim = sim();
    let lan = sim.lan();
    let houses = houses(&sim, &lan);
    let kitchen = id(&houses, "Kitchen");
    control::set_volume(&lan, &houses, &kitchen, 10).unwrap();
    let mut store = MemStore::default();

    let agent = Client::Tailnet("tag:agent".into());
    let by_agent = act(
        &mut store,
        &lan,
        &houses,
        &kitchen,
        &agent,
        (40, STREAM),
        100,
    );
    let by_person = act(
        &mut store,
        &lan,
        &houses,
        &kitchen,
        &Client::Cli,
        (60, OTHER),
        110,
    );
    assert!(by_person > by_agent);

    // Undoing the agent's last action restores what it changed from (10),
    // even though the person acted since.
    let report = actions::undo_last(&lan, &houses, &mut store, Some(&agent), &agent, "mcp", 120)
        .unwrap()
        .unwrap();
    assert_eq!(report.undone, by_agent);
    assert_eq!(control::volume(&lan, &houses, &kitchen).unwrap(), 10);
    // The person's action is still the next undoable one overall.
    assert_eq!(
        store.last_undoable_action(None).unwrap().map(|a| a.id),
        Some(by_person)
    );
    sim.shutdown();
}
