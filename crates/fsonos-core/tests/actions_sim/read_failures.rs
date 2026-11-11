//! A missing protective read is not proof that later room changes are absent.

use super::*;
use fsonos_core::CoreError;
use fsonos_core::snapshot::{Aspect, RestoreOp};
use fsonos_proto::{ProtoError, Transport};
use std::cell::{Cell, RefCell};
use std::net::IpAddr;

struct Fixture {
    sim: SimHandle,
    lan: SimLan,
    houses: Vec<HouseholdState>,
    rooms: Vec<PlayerId>,
    store: MemStore,
    action: i64,
}

impl Fixture {
    fn new(s2: bool) -> Self {
        let model = if s2 {
            SimModel::One
        } else {
            SimModel::Play5Gen1
        };
        let players = [
            SimPlayerSpec::new("First", model),
            SimPlayerSpec::new("Second", model),
        ];
        let builder = SimHousehold::builder();
        let sim = if s2 {
            builder.s2(players)
        } else {
            builder.s1(players)
        }
        .spawn()
        .unwrap();
        let lan = sim.lan();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, sim.player("First").unwrap().ip).unwrap());
        let houses = vec![house];
        let rooms: Vec<_> = ["First", "Second"]
            .iter()
            .map(|name| id(&houses, name))
            .collect();
        for room in &rooms {
            control::set_volume(&lan, &houses, room, 20).unwrap();
            control::play_uri(&lan, &houses, room, STREAM, "").unwrap();
        }
        let (snaps, missed) = capture_zones(&lan, &houses, &rooms, 100);
        assert!(missed.is_empty(), "{missed:?}");
        for room in &rooms {
            control::set_volume(&lan, &houses, room, 40).unwrap();
            control::play_uri(&lan, &houses, room, OTHER, "").unwrap();
        }
        let after = actions::rooms_after(&lan, &houses, &rooms);
        assert_eq!(after.len(), 2);
        let mut store = MemStore::default();
        let action = log(
            &mut store,
            "play and set levels in both rooms",
            actions::with_after(actions::before_state(&snaps), &after),
            100,
        );
        Self {
            sim,
            lan,
            houses,
            rooms,
            store,
            action,
        }
    }

    fn failing(&self, action: &'static str) -> FailRead {
        FailRead {
            lan: self.lan.clone(),
            host: self.sim.player("Second").unwrap().ip,
            action,
            failed: Cell::new(false),
            writes: RefCell::new(Vec::new()),
        }
    }

    fn retained(&self) {
        assert_eq!(
            self.store.last_undoable_action(None).unwrap().unwrap().id,
            self.action
        );
        assert_eq!(
            self.store
                .recent_actions(&ActionFilter::default())
                .unwrap()
                .len(),
            1,
            "an unverified undo must not consume or log the action"
        );
    }
}

/// Fail exactly one protective read, then serve real simulator responses.
/// Count all speaker writes, including to the earlier, readable room.
struct FailRead {
    lan: SimLan,
    host: IpAddr,
    action: &'static str,
    failed: Cell<bool>,
    writes: RefCell<Vec<(IpAddr, String)>>,
}

impl Transport for FailRead {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let action_name = action.rsplit('#').next().unwrap().trim_end_matches('"');
        if host == self.host && action_name == self.action && !self.failed.replace(true) {
            return Err(ProtoError::Network {
                target: "synthetic room".into(),
                detail: "one lost observation".into(),
            });
        }
        if !action_name.starts_with("Get") && action_name != "Browse" {
            self.writes
                .borrow_mut()
                .push((host, action_name.to_string()));
        }
        self.lan.soap_post(host, path, action, body)
    }
}

#[test]
fn an_unreadable_room_proof_preserves_every_zone_and_the_action() {
    for s2 in [false, true] {
        let mut fixture = Fixture::new(s2);
        let changed = &fixture.rooms[1];
        control::set_volume(&fixture.lan, &fixture.houses, changed, 65).unwrap();
        control::play_uri(&fixture.lan, &fixture.houses, changed, STREAM, "").unwrap();
        for action in ["GetMediaInfo", "GetVolume", "GetMute"] {
            let transport = fixture.failing(action);
            let result = actions::undo_last(
                &transport,
                &fixture.houses,
                &mut fixture.store,
                None,
                &Client::Cli,
                "test",
                110,
            );
            assert!(
                transport.failed.get(),
                "{action} should reach its read fault"
            );
            assert!(
                matches!(result, Err(CoreError::NotUndoable(ref why))
                    if why.contains("Second") && why.contains("could not be verified")),
                "s2={s2}, {action}: {result:?}"
            );
            assert!(
                transport.writes.borrow().is_empty(),
                "{action}: {:?}",
                transport.writes.borrow()
            );
            fixture.retained();
            assert_eq!(
                control::volume(&fixture.lan, &fixture.houses, &fixture.rooms[0]).unwrap(),
                40
            );
            assert_eq!(uri(&fixture.lan, &fixture.houses, &fixture.rooms[0]), OTHER);
            assert_eq!(
                control::volume(&fixture.lan, &fixture.houses, changed).unwrap(),
                65
            );
            assert_eq!(uri(&fixture.lan, &fixture.houses, changed), STREAM);
        }
    }
}

#[test]
fn an_unchanged_house_can_retry_the_same_action_after_the_read_recovers() {
    for s2 in [false, true] {
        let mut fixture = Fixture::new(s2);
        let transport = fixture.failing("GetMute");
        assert!(matches!(
            actions::undo_last(
                &transport,
                &fixture.houses,
                &mut fixture.store,
                None,
                &Client::Cli,
                "test",
                110
            ),
            Err(CoreError::NotUndoable(_))
        ));
        assert!(transport.writes.borrow().is_empty());
        fixture.retained();

        let report = actions::undo_last(
            &transport,
            &fixture.houses,
            &mut fixture.store,
            None,
            &Client::Cli,
            "test",
            111,
        )
        .unwrap()
        .unwrap();
        assert_eq!(report.undone, fixture.action);
        assert!(report.failures.is_empty(), "{report:?}");
        assert!(
            report.zones.iter().all(|(_, zone)| zone.skipped.is_empty()),
            "{report:?}"
        );
        for room in &fixture.rooms {
            assert_eq!(
                control::volume(&fixture.lan, &fixture.houses, room).unwrap(),
                20
            );
            assert_eq!(uri(&fixture.lan, &fixture.houses, room), STREAM);
        }
    }
}

#[test]
fn force_keeps_the_reviewed_action_and_obeys_current_volume_vetting() {
    let mut fixture = Fixture::new(true);
    let second = &fixture.rooms[1];
    control::set_volume(&fixture.lan, &fixture.houses, second, 65).unwrap();
    let transport = fixture.failing("GetVolume");
    assert!(
        actions::undo_last(
            &transport,
            &fixture.houses,
            &mut fixture.store,
            None,
            &Client::Cli,
            "test",
            110,
        )
        .is_err()
    );
    fixture.retained();
    assert!(transport.writes.borrow().is_empty());

    let report = actions::undo_last_vetted(
        &transport,
        &fixture.houses,
        &mut fixture.store,
        None,
        &Client::Cli,
        "test",
        111,
        true,
        &|op| {
            Ok(match op {
                RestoreOp::SetVolume { player, level } => RestoreOp::SetVolume {
                    player: player.clone(),
                    level: (*level).min(12),
                },
                _ => op.clone(),
            })
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(report.undone, fixture.action);
    assert!(report.failures.is_empty(), "{report:?}");
    for room in &fixture.rooms {
        assert_eq!(
            control::volume(&fixture.lan, &fixture.houses, room).unwrap(),
            12
        );
    }
}

#[test]
fn a_room_absent_from_inventory_does_not_block_the_surviving_zone_restore() {
    for s2 in [false, true] {
        let mut fixture = Fixture::new(s2);
        let absent = fixture.rooms[1].clone();
        let mut houses = fixture.houses.clone();
        houses[0].players.retain(|p| p.id != absent);
        houses[0].rooms.retain(|r| r.primary != absent);
        houses[0].groups.retain(|g| g.coordinator != absent);
        let transport = fixture.failing("GetVolume");
        let report = actions::undo_last(
            &transport,
            &houses,
            &mut fixture.store,
            None,
            &Client::Cli,
            "test",
            110,
        )
        .unwrap()
        .unwrap();
        assert_eq!(report.undone, fixture.action);
        assert!(
            !transport.failed.get(),
            "an absent room must not be contacted"
        );
        assert!(
            transport
                .writes
                .borrow()
                .iter()
                .all(|(host, _)| *host != transport.host)
        );
        let missing = &report.zones.iter().find(|(id, _)| *id == absent).unwrap().1;
        assert!(missing.restored.is_empty());
        assert!(
            missing
                .skipped
                .iter()
                .any(|(aspect, _)| *aspect == Aspect::Source)
        );
        assert_eq!(
            control::volume(&fixture.lan, &fixture.houses, &fixture.rooms[0]).unwrap(),
            20
        );
        assert_eq!(
            uri(&fixture.lan, &fixture.houses, &fixture.rooms[0]),
            STREAM
        );
        assert_eq!(
            control::volume(&fixture.lan, &fixture.houses, &absent).unwrap(),
            40
        );
        assert_eq!(uri(&fixture.lan, &fixture.houses, &absent), OTHER);
    }
}
