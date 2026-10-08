//! The action log's undo against `fsonos-sim` over real loopback sockets:
//! capture, change, log, undo, compare — and undo picks the right action.

use fsonos_core::actions::{self, capture_zones};
use fsonos_core::policy::Client;
use fsonos_core::store::{Action, ActionFilter, DjSession, MemStore, SqliteStore, Store};
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

/// Log an action as the CLI that changed what `before` captured.
fn log(store: &mut dyn Store, intent: &str, before: Option<String>, at: i64) -> i64 {
    actions::record(
        store,
        &Action {
            at,
            client: Client::Cli.key().into(),
            surface: "cli".into(),
            intent: intent.into(),
            decision: "allow".into(),
            result: "done".into(),
            before_state: before,
            undo_of: None,
        },
    )
    .unwrap()
}

/// Steer the DJ in `zones` to `mood` the way a logged `dj steer` does:
/// capture the session rows, save the new ones, log.
fn steer(store: &mut dyn Store, zones: &[PlayerId], mood: &str, at: i64) -> i64 {
    let before = actions::capture_sessions(&*store, zones).unwrap();
    for zone in zones {
        store.save_dj_session(&row(zone, mood, at)).unwrap();
    }
    let before = actions::before_state_with(&[], &before);
    log(store, &format!("dj steer {mood}"), before, at)
}

fn row(zone: &PlayerId, mood: &str, at: i64) -> DjSession {
    DjSession {
        coordinator: zone.0.clone(),
        mood: Some(mood.into()),
        constraints: Some(format!(r#"{{"mood":"{mood}"}}"#)),
        expires: at + 3600,
    }
}

fn mood(store: &dyn Store, zone: &PlayerId) -> Option<String> {
    store.dj_session(&zone.0).unwrap().and_then(|s| s.mood)
}

fn undo_puts_dj_steering_back(store: &mut dyn Store) {
    let sim = sim();
    let lan = sim.lan();
    let houses = houses(&sim, &lan);
    let (kitchen, office) = (id(&houses, "Kitchen"), id(&houses, "Office"));
    control::set_volume(&lan, &houses, &office, 15).unwrap();

    steer(store, std::slice::from_ref(&kitchen), "calm", 100);
    let both = steer(store, &[kitchen.clone(), office.clone()], "bright", 110);
    // One action that changes a zone and its steering together.
    let (snaps, missed) = capture_zones(&lan, &houses, std::slice::from_ref(&office), 120);
    assert!(missed.is_empty(), "{missed:?}");
    let sessions = actions::capture_sessions(&*store, std::slice::from_ref(&office)).unwrap();
    control::set_volume(&lan, &houses, &office, 45).unwrap();
    store.save_dj_session(&row(&office, "party", 120)).unwrap();
    let mixed = log(
        store,
        "party in the Office",
        actions::before_state_with(&snaps, &sessions),
        120,
    );

    let undo = |store: &mut dyn Store, at| {
        actions::undo_last(&lan, &houses, store, None, &Client::Cli, "cli", at).unwrap()
    };
    let report = undo(store, 200).expect("the mixed action");
    assert_eq!(report.undone, mixed);
    assert_eq!(report.sessions, std::slice::from_ref(&office));
    assert!(
        report.failures.is_empty() && report.session_failures.is_empty(),
        "{report:?}"
    );
    assert_eq!(control::volume(&lan, &houses, &office).unwrap(), 15);
    assert_eq!(mood(store, &office).as_deref(), Some("bright"));
    assert_eq!(
        report.summary,
        format!(
            "undid #{mixed} (party in the Office): restored 1 of 1 zone(s); steering for Office put back"
        )
    );

    // A zone with no session before the steer is unsteered again.
    let report = undo(store, 201).expect("the steer of both");
    assert_eq!(report.undone, both);
    assert_eq!(report.sessions, [kitchen.clone(), office.clone()]);
    assert_eq!(mood(store, &kitchen).as_deref(), Some("calm"));
    assert_eq!(store.dj_session(&office.0).unwrap(), None);
    assert_eq!(
        report.summary,
        format!("undid #{both} (dj steer bright): steering for Kitchen, Office put back")
    );

    assert!(undo(store, 202).is_some());
    assert!(store.dj_sessions().unwrap().is_empty());
    assert_eq!(undo(store, 203), None);
    sim.shutdown();
}

#[test]
fn undo_puts_dj_steering_back_in_memory() {
    undo_puts_dj_steering_back(&mut MemStore::default());
}

#[test]
fn undo_puts_dj_steering_back_with_fsqlite() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    undo_puts_dj_steering_back(&mut store);
    store.close().unwrap();
}
