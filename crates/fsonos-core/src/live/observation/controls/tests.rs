use super::*;
use crate::HouseholdState;
use crate::live::{LiveConfig, Snapshot};
use crate::playback::Playback;
use crate::reconcile::Reconciler;
use fsonos_proto::control::{join_group, set_mute, set_volume};
use fsonos_proto::net::Lan;
use fsonos_proto::play_mode::set_play_mode;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::PlayMode;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

type Calls = Arc<Mutex<Vec<(IpAddr, String)>>>;
type Fault = Arc<Mutex<Option<(IpAddr, &'static str)>>>;

struct Wire {
    inner: Arc<dyn Transport + Send + Sync>,
    calls: Calls,
    fault: Fault,
    stop_on: Arc<Mutex<Option<&'static str>>>,
    stop: Arc<AtomicBool>,
}

impl Transport for Wire {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        self.calls.lock().unwrap().push((host, name.into()));
        if self
            .fault
            .lock()
            .unwrap()
            .is_some_and(|(at, call)| at == host && call == name)
        {
            return Err(ProtoError::SoapFault {
                code: 401,
                reason: "test service unavailable".into(),
            });
        }
        let result = self.inner.soap_post(host, path, action, body);
        if *self.stop_on.lock().unwrap() == Some(name) {
            self.stop.store(true, Ordering::Release);
        }
        result
    }
}

struct Rig {
    sim: SimHandle,
    engine: Engine,
    model: Model,
    calls: Calls,
    fault: Fault,
    stop_on: Arc<Mutex<Option<&'static str>>>,
    coordinator: PlayerId,
    member: PlayerId,
    host: IpAddr,
    member_host: IpAddr,
}

fn fixture(generation: u8) -> Rig {
    let builder = SimHousehold::builder();
    let sim = match generation {
        1 => builder.s1([
            SimPlayerSpec::new("Den", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ]),
        _ => builder.s2([
            SimPlayerSpec::new("Den", SimModel::One),
            SimPlayerSpec::new("Office", SimModel::One),
        ]),
    }
    .spawn()
    .unwrap();
    let host = sim.player("Den").unwrap().ip;
    let member_host = sim.player("Office").unwrap().ip;
    let coordinator = PlayerId(sim.player("Den").unwrap().uuid.clone());
    let member = PlayerId(sim.player("Office").unwrap().uuid.clone());
    join_group(&sim.lan(), member_host, &coordinator).unwrap();
    let mut house = HouseholdState::default();
    house.apply_topology(&get_zone_group_state(&sim.lan(), host).unwrap());
    assert_eq!(house.groups.len(), 1);
    let lan = Arc::new(
        Lan::start()
            .unwrap()
            .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect()),
    );
    let now = Instant::now();
    let mut rec = Reconciler::new(Duration::from_secs(300), Duration::from_secs(300), now);
    rec.households.push(house);
    let stop = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let fault = Arc::new(Mutex::new(None));
    let stop_on = Arc::new(Mutex::new(None));
    let engine = Engine {
        transport: Arc::new(Wire {
            inner: Arc::new(sim.lan()),
            calls: Arc::clone(&calls),
            fault: Arc::clone(&fault),
            stop_on: Arc::clone(&stop_on),
            stop: Arc::clone(&stop),
        }),
        lan,
        config: LiveConfig::new(vec![host]),
        snapshot: Arc::new(Mutex::new(Snapshot::default())),
        subscribers: Arc::new(Mutex::new(Vec::new())),
        wake: Arc::new(AtomicBool::new(false)),
        stop,
    };
    let model = Model {
        ticked: (now, SystemTime::now()),
        remembered: Vec::new(),
        rec,
        playback: Playback::default(),
        progress: super::super::super::progress::Watchdog::default(),
        sink: None,
        surveyed_at: Some(now),
        last_error: None,
    };
    Rig {
        sim,
        engine,
        model,
        calls,
        fault,
        stop_on,
        coordinator,
        member,
        host,
        member_host,
    }
}

#[test]
fn a_survey_recovers_room_and_group_controls_without_any_notifications() {
    for generation in [1, 2] {
        let mut rig = fixture(generation);
        let events = super::super::survey(&rig.engine, &mut rig.model);
        rig.engine.publish(&rig.model, events);
        // No callback or subscriptions exist: all recovery uses real SOAP.
        set_volume(&rig.sim.lan(), rig.host, 19).unwrap();
        set_volume(&rig.sim.lan(), rig.member_host, 37).unwrap();
        set_mute(&rig.sim.lan(), rig.member_host, true).unwrap();
        set_play_mode(
            &rig.sim.lan(),
            rig.host,
            PlayMode::from_wire("SHUFFLE_NOREPEAT").unwrap(),
        )
        .unwrap();
        let group = get_group_volume(&rig.sim.lan(), rig.host).unwrap();
        rig.calls.lock().unwrap().clear();
        let events = super::super::survey(&rig.engine, &mut rig.model);
        assert!(events.iter().any(|event| {
            matches!(event, LiveEvent::Playback { player, changes }
                if player == &rig.member
                    && changes.volume == Some(37)
                    && changes.mute == Some(true))
        }));
        assert!(events.iter().any(|event| {
            matches!(event, LiveEvent::Playback { player, changes }
                if player == &rig.coordinator
                    && changes.play_mode.as_deref() == Some("SHUFFLE_NOREPEAT"))
        }));
        rig.engine.publish(&rig.model, events);
        let snapshot = rig.engine.snapshot.lock().unwrap();
        let room = snapshot.playback.of(&rig.member).unwrap();
        assert_eq!((room.volume, room.mute), (Some(37), Some(true)));
        assert_eq!(
            room.play_mode, None,
            "member settings are not group settings"
        );
        let coordinator = snapshot.playback.of(&rig.coordinator).unwrap();
        assert_eq!(coordinator.volume, Some(19));
        assert_eq!(coordinator.group_volume, Some(group));
        assert_eq!(coordinator.play_mode.as_deref(), Some("SHUFFLE_NOREPEAT"));
        drop(snapshot);
        let calls = rig.calls.lock().unwrap().clone();
        assert_eq!(
            calls.len(),
            11,
            "five transport, four room, two group reads"
        );
        assert!(calls.iter().all(|(_, action)| action.starts_with("Get")));
        assert!(calls.iter().all(|(host, action)| {
            !matches!(action.as_str(), "GetGroupVolume" | "GetTransportSettings")
                || *host == rig.host
        }));
        assert!(super::super::survey(&rig.engine, &mut rig.model).is_empty());
        rig.sim.shutdown();
    }
}

#[test]
fn a_partial_room_read_keeps_both_old_values_and_other_rooms_still_recover() {
    let mut rig = fixture(2);
    survey(&rig.engine, &mut rig.model, &mut Vec::new());
    let old = rig.model.playback.of(&rig.member).unwrap().clone();
    set_volume(&rig.sim.lan(), rig.member_host, 48).unwrap();
    set_mute(&rig.sim.lan(), rig.member_host, true).unwrap();
    set_volume(&rig.sim.lan(), rig.host, 31).unwrap();
    *rig.fault.lock().unwrap() = Some((rig.member_host, "GetMute"));
    let mut events = Vec::new();
    survey(&rig.engine, &mut rig.model, &mut events);
    assert_eq!(rig.model.playback.of(&rig.member), Some(&old));
    assert_eq!(
        rig.model.playback.of(&rig.coordinator).unwrap().volume,
        Some(31)
    );
    assert!(!events.iter().any(|event| {
        matches!(event, LiveEvent::Playback { player, .. } if player == &rig.member)
    }));
    *rig.fault.lock().unwrap() = None;
    survey(&rig.engine, &mut rig.model, &mut Vec::new());
    let room = rig.model.playback.of(&rig.member).unwrap();
    assert_eq!((room.volume, room.mute), (Some(48), Some(true)));
    rig.sim.shutdown();
}

#[test]
fn an_unsupported_optional_service_does_not_block_other_controls_or_transport() {
    for failed in ["GetGroupVolume", "GetTransportSettings"] {
        let mut rig = fixture(2);
        *rig.fault.lock().unwrap() = Some((rig.host, failed));
        let events = super::super::survey(&rig.engine, &mut rig.model);
        assert!(!events.is_empty());
        let state = rig.model.playback.of(&rig.coordinator).unwrap();
        assert!(state.transport.is_some());
        assert!(state.volume.is_some());
        assert!(state.mute.is_some());
        assert_eq!(state.group_volume.is_none(), failed == "GetGroupVolume");
        assert_eq!(state.play_mode.is_none(), failed == "GetTransportSettings");
        rig.sim.shutdown();
    }
}

#[test]
fn offline_members_and_duplicate_topology_are_not_probed() {
    let mut rig = fixture(2);
    rig.model.rec.health.missing(&rig.member, "test offline");
    let duplicate = rig.model.rec.households[0].clone();
    rig.model.rec.households.push(duplicate);
    survey(&rig.engine, &mut rig.model, &mut Vec::new());
    let calls = rig.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 4);
    assert!(calls.iter().all(|(host, _)| *host == rig.host));
    assert!(rig.model.playback.of(&rig.member).is_none());
    rig.sim.shutdown();
}

#[test]
fn shutdown_between_room_reads_never_commits_a_partial_sample() {
    for stop_on in [None, Some("GetVolume"), Some("GetMute")] {
        let mut rig = fixture(2);
        *rig.stop_on.lock().unwrap() = stop_on;
        if stop_on.is_none() {
            rig.engine.stop.store(true, Ordering::Release);
        }
        let result = room_levels(&rig.engine.via(), rig.host, || {
            rig.engine.stop.load(Ordering::Acquire)
        });
        assert!(result.unwrap().is_none());
        let expected = match stop_on {
            None => 0,
            Some("GetVolume") => 1,
            _ => 2,
        };
        assert_eq!(rig.calls.lock().unwrap().len(), expected);
        let mut events = Vec::new();
        survey(&rig.engine, &mut rig.model, &mut events);
        assert!(events.is_empty());
        assert_eq!(rig.calls.lock().unwrap().len(), expected);
        assert!(rig.model.playback.of(&rig.coordinator).is_none());
        rig.sim.shutdown();
    }
}
