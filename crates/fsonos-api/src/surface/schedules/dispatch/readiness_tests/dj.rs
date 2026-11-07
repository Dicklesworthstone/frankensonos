use super::*;
use crate::dj::{DjEngine, DjMoodsDto, DjSpeakers, DjStatusDto, DjSteer, SteerConstraints};
use crate::execute::OutcomeDto;
use crate::plan::DjAction;
use fsonos_core::clock::Clock;
use fsonos_core::dj_direction::{DjConstraints, DjDirection};
use fsonos_core::playback::PlayerPlayback;
use fsonos_core::scenes::{SceneGroup, SceneSource};
use fsonos_core::store::Store;
use fsonos_types::PlayerId;
use std::sync::atomic::AtomicUsize;

#[derive(Clone, Default)]
struct Probe {
    ready: Arc<AtomicBool>,
    checks: Arc<AtomicUsize>,
    starts: Arc<AtomicUsize>,
    fail_after_start: Arc<AtomicBool>,
    previews: Arc<Mutex<Vec<Option<DjSteer>>>>,
    steers: Arc<Mutex<Vec<DjSteer>>>,
}

fn done() -> OutcomeDto {
    OutcomeDto {
        done: "synthetic DJ started".into(),
        changed: true,
        volume: None,
        notes: Vec::new(),
    }
}

impl DjEngine for Probe {
    fn start_ready(
        &self,
        _: DjSpeakers<'_>,
        _: &dyn Store,
        steer: Option<&DjSteer>,
        _: &dyn Clock,
    ) -> Result<(), Failure> {
        self.checks.fetch_add(1, Ordering::SeqCst);
        self.previews.lock().unwrap().push(steer.cloned());
        if self.ready.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(Failure::new(
                ErrorCode::NotReady,
                "DJ library or renderer pending",
            ))
        }
    }

    fn act(
        &self,
        _: DjSpeakers<'_>,
        _: &mut dyn Store,
        action: DjAction,
        _: &dyn Clock,
    ) -> Result<OutcomeDto, Failure> {
        assert_eq!(action, DjAction::Start);
        self.starts.fetch_add(1, Ordering::SeqCst);
        if self.fail_after_start.load(Ordering::SeqCst) {
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
        _: DjSpeakers<'_>,
        _: &mut dyn Store,
        steer: &DjSteer,
        _: &dyn Clock,
    ) -> Result<OutcomeDto, Failure> {
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
        assert_eq!(house.sim.soap_log().len(), before);
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
