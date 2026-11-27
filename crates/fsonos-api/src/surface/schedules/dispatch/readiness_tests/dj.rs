use super::*;
use crate::dj::{DjEngine, DjMoodsDto, DjSpeakers, DjStatusDto, DjSteer, SteerConstraints};
use crate::execute::OutcomeDto;
use crate::plan::DjAction;
use fsonos_core::clock::Clock;
use fsonos_core::dj_direction::{DjConstraints, DjDirection};
use fsonos_core::playback::PlayerPlayback;
use fsonos_core::policy::Client;
use fsonos_core::scenes::{SceneGroup, SceneSource};
use fsonos_core::store::Store;
use fsonos_types::PlayerId;
use std::sync::atomic::AtomicUsize;

#[derive(Clone, Default)]
struct Probe {
    ready: Arc<AtomicBool>,
    checks: Arc<AtomicUsize>,
    starts: Arc<AtomicUsize>,
    load_checks: Arc<AtomicUsize>,
    loads: Arc<AtomicUsize>,
    loaded_clients: Arc<Mutex<Vec<String>>>,
    audiences: Arc<Mutex<Vec<(&'static str, Vec<PlayerId>)>>>,
    fail_after_start: Arc<AtomicBool>,
    previews: Arc<Mutex<Vec<Option<DjSteer>>>>,
    steers: Arc<Mutex<Vec<DjSteer>>>,
    checked_groups: Arc<Mutex<Vec<PlayerId>>>,
    started_groups: Arc<Mutex<Vec<PlayerId>>>,
    refuse_group: Arc<Mutex<Option<PlayerId>>>,
    fail_group: Arc<Mutex<Option<PlayerId>>>,
    play: Arc<AtomicBool>,
    regroup_on_ready: Arc<Mutex<Option<(SimLan, IpAddr, PlayerId)>>>,
}

impl Probe {
    fn remember_audience(&self, at: DjSpeakers<'_>, action: &'static str) {
        let mut members = at
            .households
            .iter()
            .flat_map(|house| &house.groups)
            .find(|group| group.coordinator == *at.coordinator)
            .map_or_else(Vec::new, |group| group.members.clone());
        members.sort_by(|left, right| left.0.cmp(&right.0));
        self.audiences.lock().unwrap().push((action, members));
    }
}

fn done() -> OutcomeDto {
    OutcomeDto {
        batch: None,
        done: "synthetic DJ started".into(),
        changed: true,
        volume: None,
        notes: Vec::new(),
    }
}

impl DjEngine for Probe {
    fn start_ready(
        &self,
        at: DjSpeakers<'_>,
        _: &dyn Store,
        steer: Option<&DjSteer>,
        _: &dyn Clock,
    ) -> Result<(), Failure> {
        self.checks.fetch_add(1, Ordering::SeqCst);
        self.checked_groups
            .lock()
            .unwrap()
            .push(at.coordinator.clone());
        self.previews.lock().unwrap().push(steer.cloned());
        if let Some((transport, member, coordinator)) = self.regroup_on_ready.lock().unwrap().take()
        {
            soap::join_group(&transport, member, &coordinator).unwrap();
        }
        if self.ready.load(Ordering::SeqCst)
            && self.refuse_group.lock().unwrap().as_ref() != Some(at.coordinator)
        {
            Ok(())
        } else {
            Err(Failure::new(
                ErrorCode::NotReady,
                "DJ library or renderer pending",
            ))
        }
    }

    fn load_ready(
        &self,
        at: DjSpeakers<'_>,
        store: &dyn Store,
        steer: Option<&DjSteer>,
        clock: &dyn Clock,
    ) -> Result<(), Failure> {
        self.load_checks.fetch_add(1, Ordering::SeqCst);
        self.start_ready(at, store, steer, clock)
    }

    fn load_as(
        &self,
        at: DjSpeakers<'_>,
        _: &mut dyn Store,
        _: &dyn Clock,
        client: &Client,
    ) -> Result<OutcomeDto, Failure> {
        self.remember_audience(at, "load");
        self.loads.fetch_add(1, Ordering::SeqCst);
        self.loaded_clients
            .lock()
            .unwrap()
            .push(client.key().into());
        let player = at
            .households
            .iter()
            .find_map(|house| house.player(at.coordinator))
            .unwrap();
        if soap::get_transport_info(at.transport, player.ip)
            .map_err(fsonos_core::CoreError::from)?
            .state
            == TransportState::Playing
        {
            soap::pause(at.transport, player.ip).map_err(fsonos_core::CoreError::from)?;
        }
        soap::set_av_transport_uri(
            at.transport,
            player.ip,
            "x-rincon-mp3radio://stream.example.invalid/prepared-dj.mp3",
            "",
        )
        .map_err(fsonos_core::CoreError::from)?;
        if self.fail_after_start.load(Ordering::SeqCst) {
            Err(Failure::new(
                ErrorCode::OutcomeUncertain,
                "synthetic silent load lost its acknowledgement",
            ))
        } else {
            Ok(done())
        }
    }

    fn act(
        &self,
        at: DjSpeakers<'_>,
        _: &mut dyn Store,
        action: DjAction,
        _: &dyn Clock,
    ) -> Result<OutcomeDto, Failure> {
        assert_eq!(action, DjAction::Start);
        self.remember_audience(at, "start");
        self.starts.fetch_add(1, Ordering::SeqCst);
        self.started_groups
            .lock()
            .unwrap()
            .push(at.coordinator.clone());
        if self.play.load(Ordering::SeqCst) {
            let player = at
                .households
                .iter()
                .find_map(|h| h.player(at.coordinator))
                .unwrap();
            soap::set_av_transport_uri(
                at.transport,
                player.ip,
                "x-rincon-mp3radio://stream.example.invalid/scheduled-dj.mp3",
                "",
            )
            .map_err(fsonos_core::CoreError::from)?;
            soap::play(at.transport, player.ip).map_err(fsonos_core::CoreError::from)?;
        }
        if self.fail_after_start.load(Ordering::SeqCst)
            || self.fail_group.lock().unwrap().as_ref() == Some(at.coordinator)
        {
            Err(Failure::new(
                ErrorCode::OutcomeUncertain,
                "synthetic start lost its acknowledgement",
            ))
        } else {
            Ok(done())
        }
    }

    fn steer(
        &self,
        at: DjSpeakers<'_>,
        _: &mut dyn Store,
        steer: &DjSteer,
        _: &dyn Clock,
    ) -> Result<OutcomeDto, Failure> {
        self.remember_audience(at, "steer");
        self.steers.lock().unwrap().push(steer.clone());
        Ok(done())
    }

    fn status(
        &self,
        _: DjSpeakers<'_>,
        _: &dyn Store,
        _: Option<u32>,
        _: &dyn Clock,
    ) -> Result<DjStatusDto, Failure> {
        Err(Failure::new(
            ErrorCode::NotImplemented,
            "synthetic DJ has no status",
        ))
    }

    fn moods(
        &self,
        _: Option<DjSpeakers<'_>>,
        _: &dyn Store,
        _: &dyn Clock,
    ) -> Result<DjMoodsDto, Failure> {
        Err(Failure::new(
            ErrorCode::NotImplemented,
            "synthetic DJ has no mood directory",
        ))
    }

    fn feeds(&self, _: &PlayerId) -> bool {
        false
    }

    fn on_playback(&self, _: DjSpeakers<'_>, _: &mut dyn Store, _: &PlayerPlayback, _: &dyn Clock) {
    }

    fn until_program_changes(&self, _: &dyn Clock) -> Option<u64> {
        Some(900)
    }
}

impl House {
    fn with_dj(mut self, probe: &Probe) -> Self {
        let surface = Arc::try_unwrap(self.surface)
            .ok()
            .expect("fixture has one surface owner");
        self.surface = Arc::new(surface.with_dj(Box::new(probe.clone())));
        self
    }
}

fn action() -> ScheduleAction {
    ScheduleAction::DjStart {
        room: "Room0".into(),
        mood: None,
        direction: None,
    }
}

#[test]
fn a_scheduled_dj_waits_without_claiming_and_bounds_readiness_probes() {
    for s2 in [false, true] {
        let probe = Probe::default();
        let house = House::new(s2, &action()).with_dj(&probe);
        let scheduler = Scheduler::new(&house.surface);
        let before = house.sim.soap_log().len();
        house.tick(&scheduler, house.at);
        for second in [1, 10, 29] {
            assert!(
                house
                    .tick(&scheduler, house.at + chrono::TimeDelta::seconds(second))
                    .is_empty()
            );
        }
        assert_eq!(probe.checks.load(Ordering::SeqCst), 1);
        assert_eq!(probe.starts.load(Ordering::SeqCst), 0);
        assert_eq!(house.recorded(), None);
        assert!(
            house.sim.soap_log()[before..]
                .iter()
                .all(|call| call.action.starts_with("Get"))
        );
        house.tick(&scheduler, house.at + chrono::TimeDelta::minutes(1));
        assert_eq!(probe.checks.load(Ordering::SeqCst), 2);
        assert_eq!(house.recorded(), None);

        probe.ready.store(true, Ordering::SeqCst);
        house.tick(&scheduler, house.at + chrono::TimeDelta::minutes(2));
        assert_eq!(probe.starts.load(Ordering::SeqCst), 1);
        assert_eq!(house.recorded(), Some(house.at));
        assert!(
            house
                .tick(&scheduler, house.at + chrono::TimeDelta::minutes(3))
                .is_empty()
        );
        assert_eq!(probe.starts.load(Ordering::SeqCst), 1);
    }
}

mod targets;

#[test]
fn preflight_receives_the_exact_legacy_direction_and_automatic_requests() {
    let directed = DjDirection {
        constraints: DjConstraints {
            include_artists: vec!["Synthetic artist".into()],
            ..DjConstraints::default()
        },
        for_secs: Some(1200),
    };
    let cases = [
        (None, None, None),
        (
            Some("calm".to_owned()),
            None,
            Some(DjSteer::Set {
                mood: Some("calm".into()),
                constraints: SteerConstraints::default(),
                for_secs: Some(900),
            }),
        ),
        (
            None,
            Some(directed),
            Some(DjSteer::Set {
                mood: None,
                constraints: SteerConstraints {
                    include_artists: vec!["Synthetic artist".into()],
                    ..SteerConstraints::default()
                },
                for_secs: Some(1200),
            }),
        ),
        (None, Some(DjDirection::default()), Some(DjSteer::Clear)),
    ];
    for (mood, direction, expected) in cases {
        let probe = Probe::default();
        let house = House::new(
            true,
            &ScheduleAction::DjStart {
                room: "Room0".into(),
                mood,
                direction,
            },
        )
        .with_dj(&probe);
        let scheduler = Scheduler::new(&house.surface);
        house.tick(&scheduler, house.at);
        assert_eq!(*probe.previews.lock().unwrap(), vec![expected.clone()]);
        assert!(probe.steers.lock().unwrap().is_empty());
        assert_eq!(house.recorded(), None);

        probe.ready.store(true, Ordering::SeqCst);
        house.tick(&scheduler, house.at + chrono::TimeDelta::minutes(1));
        assert_eq!(
            *probe.steers.lock().unwrap(),
            expected.into_iter().collect::<Vec<_>>()
        );
        assert_eq!(probe.starts.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn unresolved_dj_dependencies_expire_without_a_late_start() {
    let probe = Probe::default();
    let house = House::new(true, &action()).with_dj(&probe);
    let scheduler = Scheduler::new(&house.surface);
    house.tick(&scheduler, house.at);
    assert_eq!(house.recorded(), None);
    let late = house.at + chrono::TimeDelta::minutes(11);
    assert!(matches!(
        house.tick(&scheduler, late).as_slice(),
        [Ran::Skipped { .. }]
    ));
    assert_eq!(house.recorded(), Some(house.at));
    probe.ready.store(true, Ordering::SeqCst);
    assert!(house.tick(&scheduler, late).is_empty());
    assert_eq!(probe.starts.load(Ordering::SeqCst), 0);
}

#[test]
fn an_error_after_dj_dispatch_never_reopens_the_consumed_occurrence() {
    let probe = Probe::default();
    probe.ready.store(true, Ordering::SeqCst);
    probe.fail_after_start.store(true, Ordering::SeqCst);
    let house = House::new(true, &action()).with_dj(&probe);
    let scheduler = Scheduler::new(&house.surface);
    house.tick(&scheduler, house.at);
    assert_eq!(house.recorded(), Some(house.at));
    for minute in 1..4 {
        assert!(
            house
                .tick(&scheduler, house.at + chrono::TimeDelta::minutes(minute))
                .is_empty()
        );
    }
    assert_eq!(probe.starts.load(Ordering::SeqCst), 1);
}

#[test]
fn missing_engine_or_store_cannot_pass_admission() {
    let house = House::new(true, &action());
    let scheduler = Scheduler::new(&house.surface);
    house.tick(&scheduler, house.at);
    assert_eq!(house.recorded(), None);

    let probe = Probe::default();
    probe.ready.store(true, Ordering::SeqCst);
    let mut house = House::new(true, &action()).with_dj(&probe);
    let schedule = house.surface.load_schedules().unwrap().remove(0);
    Arc::get_mut(&mut house.surface).unwrap().log = None;
    assert_eq!(
        ready(&house.surface, &schedule).unwrap_err().code,
        ErrorCode::Internal
    );
    assert_eq!(probe.checks.load(Ordering::SeqCst), 0);
}

#[test]
fn a_scheduled_scene_waits_for_its_dj_before_consuming_or_changing_any_room() {
    let probe = Probe::default();
    let house = House::new(
        true,
        &ScheduleAction::ApplyScene {
            name: "Evening".into(),
        },
    )
    .with_dj(&probe);
    house.scene();
    house
        .surface
        .with_store(|store| {
            let mut scene = scenes::load(store, "Evening").unwrap();
            scene.groups.push(SceneGroup {
                coordinator: "Room0".into(),
                members: Vec::new(),
                source: SceneSource::Dj {
                    mood: None,
                    direction: None,
                },
                playing: None,
            });
            Ok(scenes::save(store, &scene, house.at.timestamp()).unwrap())
        })
        .unwrap();
    let scheduler = Scheduler::new(&house.surface);
    let before = house.sim.soap_log().len();
    house.tick(&scheduler, house.at);
    assert_eq!(house.recorded(), None);
    assert_eq!(house.sim.soap_log().len(), before);
    assert_eq!(probe.starts.load(Ordering::SeqCst), 0);
    probe.ready.store(true, Ordering::SeqCst);
    house.tick(&scheduler, house.at + chrono::TimeDelta::minutes(1));
    assert_eq!(house.recorded(), Some(house.at));
    assert_eq!(probe.starts.load(Ordering::SeqCst), 1);
}

fn silent_scene(house: &House, writer: &Client, direction: Option<DjDirection>) {
    let scene = Scene {
        name: "Prepared DJ".into(),
        groups: vec![SceneGroup {
            coordinator: "Room0".into(),
            members: Vec::new(),
            source: SceneSource::Dj {
                mood: None,
                direction,
            },
            playing: Some(false),
        }],
        volumes: BTreeMap::new(),
        mutes: BTreeMap::new(),
        writer: Some(writer.key().into()),
    };
    house
        .surface
        .with_store(|store| Ok(scenes::save(store, &scene, house.at.timestamp())))
        .unwrap()
        .unwrap()
        .unwrap();
}

#[test]
fn a_silent_dj_scene_loads_its_direction_without_play_or_implicit_fade_and_caps() {
    for s2 in [false, true] {
        let probe = Probe::default();
        probe.ready.store(true, Ordering::SeqCst);
        probe.play.store(true, Ordering::SeqCst);
        let house = House::new(s2, &action()).with_dj(&probe);
        let client = Client::Tailnet("tag:listener".into());
        let constraints = DjConstraints {
            include_artists: vec!["Synthetic artist".into()],
            strict: true,
            ..DjConstraints::default()
        };
        silent_scene(
            &house,
            &Client::Cli,
            Some(DjDirection {
                constraints,
                for_secs: Some(1200),
            }),
        );
        let host = house.sim.player("Room0").unwrap().ip;
        soap::set_volume(&house.sim.lan(), host, 85).unwrap();
        soap::set_av_transport_uri(
            &house.sim.lan(),
            host,
            "x-rincon-mp3radio://stream.example.invalid/before-dj.mp3",
            "",
        )
        .unwrap();
        soap::play(&house.sim.lan(), host).unwrap();
        let before = house.sim.soap_log().len();
        let outcome = house
            .surface
            .apply_scene_with(
                &client,
                "Prepared DJ",
                &crate::surface::scenes::SceneApplyRequest { fade_secs: Some(1) },
            )
            .unwrap();
        assert!(outcome.complete && outcome.changed, "{outcome:?}");
        assert!(
            outcome
                .steps
                .iter()
                .any(|step| step.contains("ready without playback")),
            "{outcome:?}"
        );
        assert_eq!(probe.starts.load(Ordering::SeqCst), 0);
        assert_eq!(probe.loads.load(Ordering::SeqCst), 1);
        assert_eq!(probe.load_checks.load(Ordering::SeqCst), 1);
        assert_eq!(
            *probe.loaded_clients.lock().unwrap(),
            vec![client.key().to_owned()]
        );
        let steers = probe.steers.lock().unwrap();
        assert!(matches!(
            steers.as_slice(),
            [DjSteer::Set { mood: None, constraints, for_secs: Some(1200) }]
                if constraints.strict
                    && constraints.include_artists == ["Synthetic artist"]
        ));
        assert_eq!(
            *probe.previews.lock().unwrap(),
            vec![Some(steers[0].clone())]
        );
        assert!(
            house.sim.soap_log()[before..]
                .iter()
                .all(|call| call.action != "Play" && call.action != "SetVolume")
        );
        assert_ne!(
            soap::get_transport_info(&house.sim.lan(), host)
                .unwrap()
                .state,
            TransportState::Playing
        );
        assert_eq!(soap::get_volume(&house.sim.lan(), host).unwrap(), 85);
    }
}

#[test]
fn silent_dj_loading_requires_pause_and_dj_start_for_both_scene_identities() {
    for s2 in [false, true] {
        for denied in ["pause", "dj_start"] {
            for writer_denied in [false, true] {
                let probe = Probe::default();
                probe.ready.store(true, Ordering::SeqCst);
                let house = House::new(s2, &action()).with_dj(&probe);
                *house.surface.policy.write().unwrap() = Arc::new(
                    Policy::from_toml(&format!(
                        "[clients.\"tag:blocked\"]\ndeny = [\"{denied}\"]\n"
                    ))
                    .unwrap(),
                );
                let cli = Client::Cli;
                let blocked = Client::Tailnet("tag:blocked".into());
                let (writer, caller) = if writer_denied {
                    (&blocked, &cli)
                } else {
                    (&cli, &blocked)
                };
                silent_scene(&house, writer, None);
                let before = house.sim.soap_log().len();
                let outcome = house.surface.apply_scene(caller, "Prepared DJ").unwrap();
                assert!(outcome.complete && !outcome.changed, "{outcome:?}");
                assert!(
                    outcome
                        .notes
                        .iter()
                        .any(|note| note.code == crate::failure::NoteCode::Skipped)
                );
                assert_eq!(probe.loads.load(Ordering::SeqCst), 0);
                assert_eq!(probe.starts.load(Ordering::SeqCst), 0);
                assert_eq!(probe.load_checks.load(Ordering::SeqCst), 0);
                assert!(probe.steers.lock().unwrap().is_empty());
                assert!(
                    house.sim.soap_log()[before..]
                        .iter()
                        .all(|call| call.action.starts_with("Get") || call.action == "Browse")
                );
            }
        }
    }
}

#[test]
fn scheduled_silent_dj_scenes_retry_readiness_but_never_replay_a_dispatched_load() {
    for s2 in [false, true] {
        for failed in [false, true] {
            let probe = Probe::default();
            let house = House::new(
                s2,
                &ScheduleAction::ApplyScene {
                    name: "Prepared DJ".into(),
                },
            )
            .with_dj(&probe);
            silent_scene(&house, &Client::Cli, None);
            let scheduler = Scheduler::new(&house.surface);
            let before = house.sim.soap_log().len();
            house.tick(&scheduler, house.at);
            assert_eq!(house.recorded(), None);
            assert_eq!(probe.load_checks.load(Ordering::SeqCst), 1);
            assert_eq!(probe.loads.load(Ordering::SeqCst), 0);
            assert_eq!(house.sim.soap_log().len(), before);
            probe.ready.store(true, Ordering::SeqCst);
            probe.fail_after_start.store(failed, Ordering::SeqCst);
            house.tick(&scheduler, house.at + chrono::TimeDelta::minutes(1));
            assert_eq!(house.recorded(), Some(house.at));
            assert_eq!(probe.loads.load(Ordering::SeqCst), 1);
            assert_eq!(probe.starts.load(Ordering::SeqCst), 0);
            assert!(
                house.sim.soap_log()[before..]
                    .iter()
                    .all(|call| call.action != "Play")
            );
            for minute in 2..4 {
                assert!(
                    house
                        .tick(&scheduler, house.at + chrono::TimeDelta::minutes(minute))
                        .is_empty()
                );
            }
            assert_eq!(probe.loads.load(Ordering::SeqCst), 1);
        }
    }
}

#[test]
fn a_dj_scene_passes_its_confirmed_group_to_steering_and_playback() {
    for s2 in [false, true] {
        for start in [false, true] {
            let probe = Probe::default();
            probe.ready.store(true, Ordering::SeqCst);
            probe.play.store(true, Ordering::SeqCst);
            let house = House::new(s2, &action()).with_dj(&probe);
            let scene = Scene {
                name: "Together".into(),
                groups: vec![SceneGroup {
                    coordinator: "Room0".into(),
                    members: vec!["Room1".into()],
                    source: SceneSource::Dj {
                        mood: Some("calm".into()),
                        direction: None,
                    },
                    playing: Some(start),
                }],
                volumes: BTreeMap::new(),
                mutes: BTreeMap::new(),
                writer: Some(Client::Cli.key().into()),
            };
            house
                .surface
                .with_store(|store| Ok(scenes::save(store, &scene, house.at.timestamp())))
                .unwrap()
                .unwrap()
                .unwrap();
            // The survey intentionally retains the initial, separate groups.
            // Both engine calls need the layout confirmed by scene execution.
            let before = house.sim.soap_log().len();
            let outcome = house.surface.apply_scene(&Client::Cli, "Together").unwrap();
            assert!(outcome.complete && outcome.changed, "{outcome:?}");
            let mut expected: Vec<_> = ["Room0", "Room1"]
                .iter()
                .map(|room| PlayerId(house.sim.player(room).unwrap().uuid.clone()))
                .collect();
            expected.sort_by(|left, right| left.0.cmp(&right.0));
            assert_eq!(
                *probe.audiences.lock().unwrap(),
                vec![
                    ("steer", expected.clone()),
                    (if start { "start" } else { "load" }, expected),
                ]
            );
            assert_eq!(probe.starts.load(Ordering::SeqCst), usize::from(start));
            assert_eq!(probe.loads.load(Ordering::SeqCst), usize::from(!start));
            assert_eq!(
                house.sim.soap_log()[before..]
                    .iter()
                    .filter(|call| call.action == "Play")
                    .count(),
                usize::from(start)
            );
        }
    }
}
