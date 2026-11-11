//! A level undo addresses only its captured player and fields, leaving current
//! playback, grouping and unrelated levels alone on both simulated generations.

use fsonos_core::actions::{self, BeforeState, LevelScope};
use fsonos_core::policy::Client;
use fsonos_core::snapshot::{Aspect, RestoreOp, ZoneSnapshot};
use fsonos_core::store::{Action, ActionFilter, MemStore, Store};
use fsonos_core::{CoreError, HouseholdState, control, grouping, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};

const FIRST: &str = "x-rincon-mp3radio://stream.example.invalid/first.mp3";
const LATER: &str = "x-rincon-mp3radio://stream.example.invalid/later.mp3";

fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Alpha", SimModel::Play5Gen1),
            SimPlayerSpec::new("Beta", SimModel::Play5Gen1),
        ])
        .s2([
            SimPlayerSpec::new("Gamma", SimModel::One),
            SimPlayerSpec::new("Delta", SimModel::One),
        ])
        .spawn()
        .unwrap()
}

fn houses(sim: &SimHandle, lan: &SimLan) -> Vec<HouseholdState> {
    ["Alpha", "Gamma"]
        .iter()
        .map(|name| {
            let mut house = HouseholdState::default();
            house.apply_topology(&get_zone_group_state(lan, sim.player(name).unwrap().ip).unwrap());
            house
        })
        .collect()
}

fn id(houses: &[HouseholdState], name: &str) -> PlayerId {
    resolve_room(houses, name).unwrap().player.id.clone()
}

fn scope(player: &PlayerId, volume: bool, mute: bool) -> LevelScope {
    LevelScope {
        player: player.clone(),
        volume,
        mute,
    }
}

fn capture(lan: &SimLan, houses: &[HouseholdState], coordinator: &PlayerId) -> Vec<ZoneSnapshot> {
    let (snaps, missing) =
        actions::capture_zones(lan, houses, std::slice::from_ref(coordinator), 100);
    assert!(missing.is_empty(), "{missing:?}");
    snaps
}

fn recorded(store: &mut MemStore, before: Option<String>) -> i64 {
    actions::record(
        store,
        &Action {
            at: 100,
            client: Client::Cli.key().into(),
            surface: "test".into(),
            intent: "adjust selected levels".into(),
            decision: "allow".into(),
            result: "done".into(),
            before_state: before,
            undo_of: None,
        },
    )
    .unwrap()
}

fn before(
    lan: &SimLan,
    houses: &[HouseholdState],
    snaps: &[ZoneSnapshot],
    scope: &[LevelScope],
) -> Option<String> {
    actions::with_level_after(
        actions::with_level_scope(actions::before_state(snaps), scope),
        &actions::capture_level_after(lan, houses, scope),
    )
}

struct LevelsOnly {
    lan: SimLan,
    volume: bool,
    mute: bool,
    unreadable_volume: Option<IpAddr>,
    writes: AtomicUsize,
}

impl LevelsOnly {
    fn new(lan: &SimLan, volume: bool, mute: bool) -> Self {
        Self {
            lan: lan.clone(),
            volume,
            mute,
            unreadable_volume: None,
            writes: AtomicUsize::new(0),
        }
    }
}

impl Transport for LevelsOnly {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.rsplit('#').next().unwrap().trim_end_matches('"');
        match name {
            "GetVolume" if self.volume => {
                if self.unreadable_volume == Some(host) {
                    return Err(ProtoError::Malformed("unreadable volume".into()));
                }
            }
            "GetMute" if self.mute => {}
            "SetVolume" if self.volume => {
                self.writes.fetch_add(1, Ordering::Relaxed);
            }
            "SetMute" if self.mute => {
                self.writes.fetch_add(1, Ordering::Relaxed);
            }
            _ => panic!("a scoped undo attempted unrelated {name}"),
        }
        self.lan.soap_post(host, path, action, body)
    }
}

#[test]
fn a_room_volume_undo_leaves_new_music_mute_and_peer_levels_alone() {
    let sim = sim();
    let lan = sim.lan();
    for (name, peer) in [("Alpha", "Beta"), ("Gamma", "Delta")] {
        let h = houses(&sim, &lan);
        assert!(
            grouping::group(
                &lan,
                &h,
                &resolve_room(&h, name).unwrap(),
                &[resolve_room(&h, peer).unwrap()],
            )
            .is_complete()
        );
        let h = houses(&sim, &lan);
        let (player, other) = (id(&h, name), id(&h, peer));
        control::set_volume(&lan, &h, &player, 23).unwrap();
        control::set_volume(&lan, &h, &other, 36).unwrap();
        control::play_uri(&lan, &h, &player, FIRST, "").unwrap();
        let snaps = capture(&lan, &h, &player);
        control::set_volume(&lan, &h, &player, 55).unwrap();
        let mut store = MemStore::default();
        let saved = recorded(
            &mut store,
            before(&lan, &h, &snaps, &[scope(&player, true, false)]),
        );

        // Unrelated controls after the action cannot be restored or block it.
        control::play_uri(&lan, &h, &player, LATER, "").unwrap();
        control::pause(&lan, &h, &player).unwrap();
        control::set_mute(&lan, &h, &player, true).unwrap();
        control::set_volume(&lan, &h, &other, 47).unwrap();
        let playback = control::playback(&lan, &h, &player).unwrap();
        let only = LevelsOnly::new(&lan, true, false);
        let report = actions::undo_last(&only, &h, &mut store, None, &Client::Cli, "test", 110)
            .unwrap()
            .unwrap();
        assert_eq!(report.undone, saved);
        assert_eq!(report.zones[0].1.restored, [Aspect::Volume]);
        assert!(report.zones[0].1.skipped.is_empty(), "{report:?}");
        assert_eq!(only.writes.load(Ordering::Relaxed), 1);
        assert_eq!(control::volume(&lan, &h, &player).unwrap(), 23);
        assert_eq!(control::volume(&lan, &h, &other).unwrap(), 47);
        assert!(soap::get_mute(&lan, sim.player(name).unwrap().ip).unwrap());
        assert_eq!(control::playback(&lan, &h, &player).unwrap(), playback);
    }
}

#[test]
fn a_mute_undo_preserves_the_later_volume_and_needs_no_volume_read() {
    let sim = sim();
    let lan = sim.lan();
    let h = houses(&sim, &lan);
    for name in ["Alpha", "Gamma"] {
        let player = id(&h, name);
        let snaps = capture(&lan, &h, &player);
        control::set_mute(&lan, &h, &player, true).unwrap();
        let mut store = MemStore::default();
        recorded(
            &mut store,
            before(&lan, &h, &snaps, &[scope(&player, false, true)]),
        );
        control::set_volume(&lan, &h, &player, 64).unwrap();
        let only = LevelsOnly::new(&lan, false, true);
        let report = actions::undo_last(&only, &h, &mut store, None, &Client::Cli, "test", 110)
            .unwrap()
            .unwrap();
        assert_eq!(report.zones[0].1.restored, [Aspect::Mute]);
        assert_eq!(control::volume(&lan, &h, &player).unwrap(), 64);
        assert!(!soap::get_mute(&lan, sim.player(name).unwrap().ip).unwrap());
        assert_eq!(only.writes.load(Ordering::Relaxed), 1);
    }
}

#[test]
fn a_group_level_undo_keeps_exact_players_after_a_coordinator_handover() {
    let sim = sim();
    let lan = sim.lan();
    for (name, peer) in [("Alpha", "Beta"), ("Gamma", "Delta")] {
        let h = houses(&sim, &lan);
        assert!(
            grouping::group(
                &lan,
                &h,
                &resolve_room(&h, name).unwrap(),
                &[resolve_room(&h, peer).unwrap()]
            )
            .is_complete()
        );
        let h = houses(&sim, &lan);
        let (player, other) = (id(&h, name), id(&h, peer));
        control::set_volume(&lan, &h, &player, 12).unwrap();
        control::set_volume(&lan, &h, &other, 44).unwrap();
        let snaps = capture(&lan, &h, &player);
        control::set_group_volume(&lan, &h, &player, 70).unwrap();
        let mut store = MemStore::default();
        let scope = [scope(&player, true, false), scope(&other, true, false)];
        recorded(&mut store, before(&lan, &h, &snaps, &scope));
        sim.reelect_coordinator(peer).unwrap();
        let h = houses(&sim, &lan);
        let only = LevelsOnly::new(&lan, true, false);
        actions::undo_last(&only, &h, &mut store, None, &Client::Cli, "test", 110)
            .unwrap()
            .unwrap();
        assert_eq!(control::volume(&lan, &h, &player).unwrap(), 12);
        assert_eq!(control::volume(&lan, &h, &other).unwrap(), 44);
        assert_eq!(resolve_room(&h, name).unwrap().coordinator.id, other);
        assert_eq!(only.writes.load(Ordering::Relaxed), 2);
    }
}

#[test]
fn missing_or_unreadable_touched_field_proof_preserves_the_action_without_writes() {
    let sim = sim();
    let lan = sim.lan();
    let h = houses(&sim, &lan);
    let player = id(&h, "Alpha");
    let snaps = capture(&lan, &h, &player);
    control::set_volume(&lan, &h, &player, 55).unwrap();
    let scope = [scope(&player, true, false)];
    for missing_after in [true, false] {
        let mut store = MemStore::default();
        let proof = if missing_after {
            actions::with_level_scope(actions::before_state(&snaps), &scope)
        } else {
            before(&lan, &h, &snaps, &scope)
        };
        let saved = recorded(&mut store, proof);
        let mut only = LevelsOnly::new(&lan, true, false);
        if !missing_after {
            only.unreadable_volume = Some(sim.player("Alpha").unwrap().ip);
        }
        let error =
            actions::undo_last(&only, &h, &mut store, None, &Client::Cli, "test", 110).unwrap_err();
        assert!(matches!(error, CoreError::NotUndoable(_)), "{error}");
        assert_eq!(only.writes.load(Ordering::Relaxed), 0);
        assert_eq!(store.last_undoable_action(None).unwrap().unwrap().id, saved);
        assert_eq!(
            store
                .recent_actions(&ActionFilter::default())
                .unwrap()
                .len(),
            1
        );
    }
}

#[test]
fn changed_target_fields_are_protected_but_an_explicit_force_restores_only_them() {
    let sim = sim();
    let lan = sim.lan();
    let h = houses(&sim, &lan);
    let player = id(&h, "Alpha");
    control::set_volume(&lan, &h, &player, 23).unwrap();
    let snaps = capture(&lan, &h, &player);
    control::set_volume(&lan, &h, &player, 55).unwrap();
    let mut store = MemStore::default();
    recorded(
        &mut store,
        before(&lan, &h, &snaps, &[scope(&player, true, false)]),
    );
    control::set_volume(&lan, &h, &player, 67).unwrap();
    let only = LevelsOnly::new(&lan, true, false);
    assert!(matches!(
        actions::undo_last(&only, &h, &mut store, None, &Client::Cli, "test", 110),
        Err(CoreError::NotUndoable(_))
    ));
    assert_eq!(only.writes.load(Ordering::Relaxed), 0);
    let report = actions::undo_last_vetted(
        &only,
        &h,
        &mut store,
        None,
        &Client::Cli,
        "test",
        110,
        true,
        &|op| {
            Ok(match op {
                RestoreOp::SetVolume { player, .. } => RestoreOp::SetVolume {
                    player: player.clone(),
                    level: 20,
                },
                _ => panic!("unrelated restore operation"),
            })
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(report.zones[0].1.restored, [Aspect::Volume]);
    assert_eq!(
        control::volume(&lan, &h, &player).unwrap(),
        20,
        "current policy still vets a forced undo"
    );
}

#[test]
fn a_former_primary_is_not_redirected_to_its_new_bonded_room_primary() {
    let sim = sim();
    let lan = sim.lan();
    let mut h = houses(&sim, &lan);
    let (player, replacement) = (id(&h, "Alpha"), id(&h, "Beta"));
    let snaps = capture(&lan, &h, &player);
    control::set_volume(&lan, &h, &player, 55).unwrap();
    let mut store = MemStore::default();
    recorded(
        &mut store,
        before(&lan, &h, &snaps, &[scope(&player, true, false)]),
    );
    h[0].rooms
        .retain(|room| !room.players.contains(&replacement));
    let room = h[0]
        .rooms
        .iter_mut()
        .find(|room| room.primary == player)
        .unwrap();
    room.primary = replacement.clone();
    room.players.push(replacement);
    let only = LevelsOnly::new(&lan, false, false);
    for force in [false, true] {
        let error = actions::undo_last_vetted(
            &only,
            &h,
            &mut store,
            None,
            &Client::Cli,
            "test",
            110,
            force,
            &|_| panic!("a changed primary must fail before vetting"),
        )
        .unwrap_err();
        assert!(matches!(error, CoreError::NotUndoable(_)), "{error}");
    }
}

#[test]
fn a_capped_unmute_is_remuted_before_its_prior_volume_is_restored() {
    let sim = sim();
    let lan = sim.lan();
    let h = houses(&sim, &lan);
    let player = id(&h, "Alpha");
    control::set_mute(&lan, &h, &player, true).unwrap();
    control::set_volume(&lan, &h, &player, 80).unwrap();
    let snaps = capture(&lan, &h, &player);
    control::set_volume(&lan, &h, &player, 30).unwrap();
    control::set_mute(&lan, &h, &player, false).unwrap();
    let mut store = MemStore::default();
    recorded(
        &mut store,
        before(&lan, &h, &snaps, &[scope(&player, true, true)]),
    );
    let log_start = sim.soap_log().len();
    let only = LevelsOnly::new(&lan, true, true);
    actions::undo_last(&only, &h, &mut store, None, &Client::Cli, "test", 110)
        .unwrap()
        .unwrap();
    let writes: Vec<_> = sim
        .soap_log()
        .into_iter()
        .skip(log_start)
        .filter(|call| call.action.starts_with("Set"))
        .map(|call| call.action)
        .collect();
    assert_eq!(writes, ["SetMute", "SetVolume"]);
    assert_eq!(control::volume(&lan, &h, &player).unwrap(), 80);
    assert!(soap::get_mute(&lan, sim.player("Alpha").unwrap().ip).unwrap());
}

#[test]
fn scoped_rows_round_trip_without_changing_legacy_snapshot_semantics() {
    let sim = sim();
    let lan = sim.lan();
    let h = houses(&sim, &lan);
    let player = id(&h, "Alpha");
    let snaps = capture(&lan, &h, &player);
    let legacy = actions::before_state(&snaps).unwrap();
    assert!(BeforeState::parse(&legacy).unwrap().level_scope.is_none());
    let scopes = [scope(&player, true, false), scope(&player, false, true)];
    let scoped = before(&lan, &h, &snaps, &scopes).unwrap();
    let decoded = BeforeState::parse(&scoped).unwrap();
    assert_eq!(decoded.zones, snaps);
    assert_eq!(decoded.level_scope, Some(vec![scope(&player, true, true)]));
    assert_eq!(decoded.level_after.len(), 1);
    assert!(decoded.level_after[0].volume.is_some());
    assert!(decoded.level_after[0].mute.is_some());
    let empty = actions::with_level_scope(Some(legacy), &[]).unwrap();
    assert_eq!(
        BeforeState::parse(&empty).unwrap().level_scope,
        Some(Vec::new())
    );
}
