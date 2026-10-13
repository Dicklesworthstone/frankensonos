//! Relocation through the shared CLI/HTTP/MCP surface, not just the core.
//! Speaker I/O uses the real simulator; the DJ probe observes ownership calls.

use super::*;
use crate::dj::DjSteer;
use fsonos_core::clock::SystemClock;
use fsonos_core::playback::PlayerPlayback;
use fsonos_core::store::{DjSession, MemStore};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use std::collections::HashSet;

#[derive(Default)]
struct ProbeState {
    feeds: HashSet<PlayerId>,
    held: HashSet<PlayerId>,
    moves: Vec<(PlayerId, PlayerId)>,
}

struct Probe(Arc<Mutex<ProbeState>>);

impl DjEngine for Probe {
    fn act(
        &self,
        _: DjSpeakers<'_>,
        _: &mut dyn Store,
        _: DjAction,
        _: &dyn Clock,
    ) -> Result<OutcomeDto, Failure> {
        unreachable!("relocation must not restart the DJ")
    }

    fn steer(
        &self,
        _: DjSpeakers<'_>,
        _: &mut dyn Store,
        _: &DjSteer,
        _: &dyn Clock,
    ) -> Result<OutcomeDto, Failure> {
        unreachable!()
    }

    fn status(
        &self,
        _: DjSpeakers<'_>,
        _: &dyn Store,
        _: Option<u32>,
        _: &dyn Clock,
    ) -> Result<DjStatusDto, Failure> {
        Err(Failure::invalid("probe has no status"))
    }

    fn moods(
        &self,
        _: Option<DjSpeakers<'_>>,
        _: &dyn Store,
        _: &dyn Clock,
    ) -> Result<DjMoodsDto, Failure> {
        unreachable!()
    }

    fn feeds(&self, coordinator: &PlayerId) -> bool {
        self.0.lock().unwrap().feeds.contains(coordinator)
    }

    fn moving(&self, from: &PlayerId) {
        self.0.lock().unwrap().held.insert(from.clone());
    }

    fn moved(&self, from: &PlayerId, to: &PlayerId) {
        let mut state = self.0.lock().unwrap();
        assert!(state.held.remove(from), "the move must hold its feed first");
        state.moves.push((from.clone(), to.clone()));
        if from != to && state.feeds.remove(from) {
            state.feeds.insert(to.clone());
        }
    }

    fn on_playback(
        &self,
        _: DjSpeakers<'_>,
        _: &mut dyn Store,
        _: &PlayerPlayback,
        _: &dyn Clock,
    ) {
        unreachable!("this surface has no live model")
    }
}

struct Fixture {
    sim: SimHandle,
    surface: Surface,
    probe: Arc<Mutex<ProbeState>>,
    from: PlayerId,
    to: PlayerId,
}

impl Fixture {
    fn new(source: &str, target: &str, grouped: bool) -> Self {
        let sim = SimHousehold::builder()
            .s1([
                SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
                SimPlayerSpec::new("Office", SimModel::Play5Gen1),
            ])
            .s2([
                SimPlayerSpec::new("Den", SimModel::One),
                SimPlayerSpec::new("Bedroom", SimModel::One),
            ])
            .spawn()
            .unwrap();
        let from = PlayerId(sim.player(source).unwrap().uuid.clone());
        let to = PlayerId(sim.player(target).unwrap().uuid.clone());
        if grouped {
            soap::join_group(&sim.lan(), sim.player(target).unwrap().ip, &from).unwrap();
        }
        let households: Vec<HouseholdState> = ["Kitchen", "Den"]
            .into_iter()
            .map(|room| {
                let mut house = HouseholdState::default();
                let topology =
                    get_zone_group_state(&sim.lan(), sim.player(room).unwrap().ip).unwrap();
                house.apply_topology(&topology);
                house
            })
            .collect();
        let mut store = MemStore::default();
        for (id, mood) in [(&from, "calm"), (&to, "bright")] {
            store
                .save_dj_session(&DjSession {
                    coordinator: id.0.clone(),
                    mood: Some(mood.into()),
                    constraints: None,
                    expires: i64::MAX,
                })
                .unwrap();
        }
        let probe = Arc::new(Mutex::new(ProbeState {
            feeds: HashSet::from([from.clone(), to.clone()]),
            ..ProbeState::default()
        }));
        let surface = Surface::new(
            Box::new(sim.lan()),
            Box::new(move |_| Ok(households.clone())),
            Policy::default(),
            Box::new(SystemClock),
        )
        .with_dj(Box::new(Probe(Arc::clone(&probe))))
        .with_action_log(Box::new(store), "test");
        Self {
            sim,
            surface,
            probe,
            from,
            to,
        }
    }

    fn copy(&self) -> Result<OutcomeDto, Failure> {
        self.surface.control(&Client::Cli, "move", |_| {
            Ok(Command::Move {
                from: self.from.clone(),
                to: self.to.clone(),
                copy: true,
            })
        })
    }

    fn sessions(&self) -> Vec<DjSession> {
        self.surface
            .with_store(|store| store.dj_sessions())
            .unwrap()
            .unwrap()
    }

    fn assert_held_in_place(&self) {
        let state = self.probe.lock().unwrap();
        assert!(state.held.is_empty());
        assert_eq!(state.moves, [(self.from.clone(), self.from.clone())]);
        assert!(state.feeds.contains(&self.from));
        assert!(state.feeds.contains(&self.to));
    }
}

fn play(sim: &SimHandle, room: &str) {
    let host = sim.player(room).unwrap().ip;
    let uri = format!("x-rincon-mp3radio://example.invalid/{room}.mp3");
    soap::set_av_transport_uri(&sim.lan(), host, &uri, "").unwrap();
    soap::play(&sim.lan(), host).unwrap();
}

#[test]
fn an_empty_copy_preserves_destination_playback_and_both_dj_sessions() {
    for (source, target) in [("Kitchen", "Den"), ("Den", "Kitchen")] {
        let f = Fixture::new(source, target, false);
        play(&f.sim, target);
        let sessions = f.sessions();
        let host = f.sim.player(target).unwrap().ip;
        let before = soap::get_position_info(&f.sim.lan(), host).unwrap();
        let calls = f.sim.soap_log().len();
        let outcome = f.copy().unwrap();
        assert!(!outcome.changed);
        assert!(outcome.done.starts_with("nothing to move"));
        f.assert_held_in_place();
        assert_eq!(f.sessions(), sessions);
        assert!(
            f.sim.soap_log()[calls..]
                .iter()
                .all(|call| call.action.starts_with("Get") || call.action == "Browse")
        );
        assert_eq!(soap::get_position_info(&f.sim.lan(), host).unwrap(), before);
        assert_eq!(
            soap::get_transport_info(&f.sim.lan(), host).unwrap().state,
            TransportState::Playing
        );
        assert!(f.surface.undo(&Client::Cli, false).unwrap().is_none());
    }
}

#[test]
fn a_same_group_copy_is_unchanged_on_the_shared_surface() {
    for (source, target) in [("Kitchen", "Office"), ("Den", "Bedroom")] {
        let f = Fixture::new(source, target, true);
        play(&f.sim, source);
        let before = f.sessions();
        let calls = f.sim.soap_log().len();
        assert!(!f.copy().unwrap().changed);
        f.assert_held_in_place();
        assert_eq!(f.sessions(), before);
        assert!(
            f.sim.soap_log()[calls..]
                .iter()
                .all(|call| call.action.starts_with("Get") || call.action == "Browse")
        );
        assert!(f.surface.undo(&Client::Cli, false).unwrap().is_none());
    }
}

#[test]
fn destination_failure_releases_the_hold_without_transferring_the_dj() {
    let f = Fixture::new("Kitchen", "Den", false);
    play(&f.sim, "Kitchen");
    let sessions = f.sessions();
    f.sim.upnp_fault("Den", "SetAVTransportURI", 701).unwrap();
    let failed = f.copy().unwrap_err();
    assert_eq!(failed.upnp_code, Some(701));
    f.assert_held_in_place();
    assert_eq!(f.sessions(), sessions);
    assert_eq!(
        soap::get_transport_info(&f.sim.lan(), f.sim.player("Kitchen").unwrap().ip)
            .unwrap()
            .state,
        TransportState::Playing
    );
    // Execution failures may have changed the destination, so keep undo data.
    assert!(
        f.surface
            .with_store(|store| store.last_undoable_action(None))
            .unwrap()
            .unwrap()
            .is_some()
    );
}

#[test]
fn verified_copy_transfers_the_feed_and_steering_only_after_playback() {
    for (source, target) in [("Kitchen", "Den"), ("Den", "Kitchen")] {
        let f = Fixture::new(source, target, false);
        play(&f.sim, source);
        assert!(f.copy().unwrap().changed);
        let state = f.probe.lock().unwrap();
        assert!(state.held.is_empty());
        assert_eq!(state.moves, [(f.from.clone(), f.to.clone())]);
        assert!(!state.feeds.contains(&f.from));
        assert!(state.feeds.contains(&f.to));
        drop(state);
        let sessions = f.sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].coordinator, f.to.0);
        assert_eq!(sessions[0].mood.as_deref(), Some("calm"));
        assert_eq!(
            soap::get_transport_info(&f.sim.lan(), f.sim.player(target).unwrap().ip)
                .unwrap()
                .state,
            TransportState::Playing
        );
    }
}

#[test]
fn unchanged_results_have_no_undo_but_clamps_and_failures_keep_it() {
    let mut outcome = OutcomeDto {
        done: "unchanged".into(),
        changed: false,
        volume: None,
        notes: Vec::new(),
    };
    let before = Some("snapshot".into());
    assert!(log_entry(&Ok(outcome.clone()), before.clone(), 0).2.is_none());
    outcome.notes.push(Note {
        code: NoteCode::VolumeClamped,
        detail: "lowered a room before the command".into(),
    });
    assert_eq!(log_entry(&Ok(outcome), before.clone(), 0).2, before);
    let failed = Err(Failure::new(ErrorCode::PlayerUnreachable, "unreachable"));
    assert_eq!(log_entry(&failed, before.clone(), 0).2, before);
}
