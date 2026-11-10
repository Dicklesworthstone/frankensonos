//! Undo belongs to the audience captured before a command. Recovery may
//! follow the requested room into a smaller group, but an old snapshot must
//! not silently rejoin the rooms or overwrite the unrelated former leader.

use fsonos_api::plan::{TransportAction, plan_move, plan_transport};
use fsonos_api::surface::{Surface, Survey};
use fsonos_api::{MoveRequest, NoteCode, ZoneRequest};
use fsonos_core::actions::BeforeState;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};

const STREAM: &str = "x-rincon-mp3radio://stream.example.invalid/recovery-undo.mp3";

/// The first Pause gets a real 800 after a handover. Before its refusal is
/// delivered, optionally split the former leader off into its own group.
/// Thus recovery sees a new audience after the original snapshot was taken.
struct Handoff {
    lan: SimLan,
    old: IpAddr,
    old_id: PlayerId,
    new: IpAddr,
    new_id: PlayerId,
    split: bool,
    lose_reply: bool,
    switched: AtomicBool,
    lost: AtomicBool,
}

impl Transport for Handoff {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        header: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let action = header.trim_matches('"').rsplit('#').next().unwrap();
        if host == self.old && action == "Pause" && !self.switched.swap(true, Ordering::SeqCst) {
            soap::delegate_group_and_rejoin(&self.lan, self.old, &self.new_id)?;
            let refused = self.lan.soap_post(host, path, header, body);
            if self.split {
                soap::leave_group(&self.lan, self.old, &self.old_id)?;
            }
            return refused;
        }
        if self.lose_reply && host == self.new && action == "Pause" {
            let response = self.lan.soap_post(host, path, header, body)?;
            fsonos_proto::soap::parse_response(&response, action)?;
            self.lost.store(true, Ordering::SeqCst);
            return Err(lost_reply(host));
        }
        if host == self.new && action == "GetTransportInfo" && self.lost.load(Ordering::SeqCst) {
            return Err(lost_reply(host));
        }
        self.lan.soap_post(host, path, header, body)
    }
}

fn lost_reply(host: IpAddr) -> ProtoError {
    ProtoError::Network {
        target: host.to_string(),
        detail: "injected loss after the recovered Pause".into(),
    }
}

fn houses(lan: &impl Transport, seed: IpAddr) -> Vec<HouseholdState> {
    let mut house = HouseholdState::default();
    house.apply_topology(&get_zone_group_state(lan, seed).unwrap());
    vec![house]
}

fn fixture(s2: bool, split: bool, lose_reply: bool) -> (SimHandle, Surface) {
    let model = if s2 {
        SimModel::One
    } else {
        SimModel::Play5Gen1
    };
    let rooms = [
        SimPlayerSpec::new("Kitchen", model),
        SimPlayerSpec::new("Office", model),
    ];
    let builder = SimHousehold::builder();
    let sim = if s2 {
        builder.s2(rooms)
    } else {
        builder.s1(rooms)
    }
    .spawn()
    .unwrap();
    let old = sim.player("Kitchen").unwrap();
    let new = sim.player("Office").unwrap();
    let (old_ip, new_ip) = (old.ip, new.ip);
    let (old_id, new_id) = (PlayerId(old.uuid.clone()), PlayerId(new.uuid.clone()));
    soap::join_group(&sim.lan(), new_ip, &old_id).unwrap();
    let before = houses(&sim.lan(), new_ip);
    assert_eq!(
        resolve_room(&before, "Office").unwrap().coordinator.id,
        old_id
    );
    control::play_uri(&sim.lan(), &before, &old_id, STREAM, "").unwrap();
    let transport = Handoff {
        lan: sim.lan(),
        old: old_ip,
        old_id,
        new: new_ip,
        new_id,
        split,
        lose_reply,
        switched: AtomicBool::new(false),
        lost: AtomicBool::new(false),
    };
    let survey: Survey = Box::new(move |t| {
        let mut house = HouseholdState::default();
        house.apply_topology(
            &get_zone_group_state(t, new_ip).map_err(fsonos_core::CoreError::from)?,
        );
        Ok(vec![house])
    });
    let surface = Surface::new(
        Box::new(transport),
        survey,
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(MemStore::default()), "test");
    (sim, surface)
}

fn request() -> ZoneRequest {
    ZoneRequest {
        zone: "Office".into(),
    }
}

fn saved(surface: &Surface) -> BeforeState {
    let log = surface
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    assert_eq!(log.len(), 1, "{log:?}");
    BeforeState::parse(
        log[0]
            .action
            .before_state
            .as_deref()
            .expect("an undo boundary"),
    )
    .unwrap()
}

fn assert_boundary_keeps_groups(sim: &SimHandle, surface: &Surface) {
    let saved = saved(surface);
    assert!(saved.not_undoable.is_some(), "{saved:?}");
    assert!(
        saved.zones.is_empty(),
        "do not retain unrelated snapshots: {saved:?}"
    );
    let seed = sim.player("Office").unwrap().ip;
    let before = get_zone_group_state(&sim.lan(), seed).unwrap();
    let mark = sim.soap_log().len();
    for force in [false, true] {
        let failure = surface.undo_forced(&Client::Cli, true, force).unwrap_err();
        assert!(failure.detail.contains("room group changed"), "{failure:?}");
    }
    let log = sim.soap_log();
    assert!(
        log[mark..]
            .iter()
            .all(|entry| entry.action.starts_with("Get") || entry.action == "Browse"),
        "refused undo must not write: {:?}",
        &log[mark..]
    );
    assert_eq!(get_zone_group_state(&sim.lan(), seed).unwrap(), before);
}

#[test]
fn recovered_playback_cannot_undo_a_former_group_after_its_audience_split() {
    for s2 in [false, true] {
        let (sim, surface) = fixture(s2, true, false);
        let outcome = surface
            .control(&Client::Cli, "pause", |rooms| {
                plan_transport(rooms, &request(), TransportAction::Pause)
            })
            .unwrap();
        assert!(
            outcome
                .notes
                .iter()
                .any(|note| note.code == NoteCode::Healed),
            "{outcome:?}"
        );
        assert_boundary_keeps_groups(&sim, &surface);
        sim.shutdown();
    }
}

#[test]
fn same_audience_handover_keeps_its_snapshot_and_can_be_undone() {
    for s2 in [false, true] {
        let (sim, surface) = fixture(s2, false, false);
        surface
            .control(&Client::Cli, "pause", |rooms| {
                plan_transport(rooms, &request(), TransportAction::Pause)
            })
            .unwrap();
        let saved = saved(&surface);
        assert!(saved.not_undoable.is_none(), "{saved:?}");
        assert_eq!(saved.zones.len(), 1, "{saved:?}");
        assert_eq!(saved.zones[0].members.len(), 2, "{saved:?}");
        let restored = surface
            .undo(&Client::Cli, true)
            .unwrap()
            .expect("the paused group");
        assert!(restored.failures.is_empty(), "{restored:?}");
        let now = houses(&sim.lan(), sim.player("Office").unwrap().ip);
        let kitchen = resolve_room(&now, "Kitchen").unwrap();
        let office = resolve_room(&now, "Office").unwrap();
        assert_eq!(kitchen.coordinator.id, office.coordinator.id);
        let ip = kitchen.coordinator.ip;
        assert_eq!(
            soap::get_transport_info(&sim.lan(), ip).unwrap().state,
            TransportState::Playing
        );
        assert_eq!(soap::get_media_info(&sim.lan(), ip).unwrap().uri, STREAM);
        sim.shutdown();
    }
}

#[test]
fn skipped_uncertain_recovery_keeps_a_sets_undo_boundary_even_with_only_a_noop_success() {
    let (sim, surface) = fixture(false, true, true);
    let outcome = surface
        .control_set(&Client::Cli, "pause", |rooms| {
            Ok(vec![
                plan_transport(rooms, &request(), TransportAction::Pause)?,
                // Distinct rooms pass planning. The copy recognizes their
                // shared source group during execution and returns a no-op.
                plan_move(
                    rooms,
                    &MoveRequest {
                        zone: "Kitchen".into(),
                        to: "Office".into(),
                        copy: true,
                    },
                )?,
            ])
        })
        .unwrap();
    assert!(
        !outcome.changed,
        "the only confirmed result was a no-op: {outcome:?}"
    );
    assert!(
        outcome
            .notes
            .iter()
            .any(|note| note.code == NoteCode::Skipped),
        "{outcome:?}"
    );
    let writes: Vec<_> = sim
        .soap_log()
        .into_iter()
        .filter(|entry| entry.action == "Pause")
        .collect();
    assert_eq!(
        writes.len(),
        2,
        "the refused initial Pause and one recovered attempt: {writes:?}"
    );
    assert_boundary_keeps_groups(&sim, &surface);
    sim.shutdown();
}
