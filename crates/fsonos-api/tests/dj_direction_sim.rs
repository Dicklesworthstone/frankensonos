//! Reusable DJ direction survives scene capture and durable schedules. A
//! recording engine observes the stored session at Start, before it could
//! select its first work; simulator sockets exercise the surrounding surface.

use chrono::{DateTime, FixedOffset, TimeDelta, Utc};
use fsonos_api::dj::{
    DjEngine, DjMoodDto, DjMoodsDto, DjSpeakers, DjStatusDto, DjSteer, DjSteeringDto,
    SteerConstraints,
};
use fsonos_api::plan::{DjAction, plan_dj_steer};
use fsonos_api::surface::scenes::SceneDefinition;
use fsonos_api::surface::schedules::{Ran, ScheduleRequest, Scheduler};
use fsonos_api::{DjSteerRequest, ErrorCode, Failure, NoteCode, OutcomeDto, Surface};
use fsonos_core::HouseholdState;
use fsonos_core::clock::{Clock, FakeClock};
use fsonos_core::playback::PlayerPlayback;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{ActionFilter, DjSession, MemStore, SqliteStore, Store};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::{HouseholdId, PlayerId};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const START: &str = "2026-10-08T07:00:00+00:00";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    Steer(Box<DjSteer>),
    Start(Option<DjSession>, i64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReadyCall {
    coordinator: PlayerId,
    steer: Option<DjSteer>,
    session: Option<DjSession>,
}

#[derive(Clone, Default)]
struct Probe {
    calls: Arc<Mutex<Vec<Call>>>,
    feeding: Arc<AtomicBool>,
    status_fails: Arc<AtomicBool>,
    readiness: Arc<Mutex<Vec<ReadyCall>>>,
    refuse_start: Arc<Mutex<Option<PlayerId>>>,
}

impl Probe {
    fn take(&self) -> Vec<Call> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }
}

fn outcome() -> OutcomeDto {
    OutcomeDto {
        done: "recorded DJ operation".into(),
        changed: true,
        volume: None,
        notes: Vec::new(),
    }
}

fn steering(at: Option<DjSpeakers<'_>>, store: &dyn Store, clock: &dyn Clock) -> DjSteeringDto {
    let now = clock.now().timestamp();
    let session = at
        .and_then(|at| store.dj_session(&at.coordinator.0).unwrap())
        .filter(|row| row.expires > now);
    let added = session
        .as_ref()
        .and_then(|row| row.constraints.as_deref())
        .map_or_else(SteerConstraints::default, |json| {
            serde_json::from_str(json).unwrap()
        });
    let mut effective = added.clone();
    effective.exclude_keywords.push("program-only".into());
    let expires_at = session
        .as_ref()
        .map(|row| row.expires)
        .filter(|expires| *expires != i64::MAX);
    DjSteeringDto {
        source: if session.is_some() {
            "session"
        } else {
            "program"
        }
        .into(),
        mood: session
            .as_ref()
            .map_or_else(|| Some("calm".into()), |row| row.mood.clone()),
        constraints: effective,
        added,
        expires_at,
        expires_in_secs: expires_at.map(|expires| u64::try_from(expires - now).unwrap()),
    }
}

impl DjEngine for Probe {
    fn start_ready(
        &self,
        at: DjSpeakers<'_>,
        store: &dyn Store,
        steer: Option<&DjSteer>,
        _: &dyn Clock,
    ) -> Result<(), Failure> {
        self.readiness.lock().unwrap().push(ReadyCall {
            coordinator: at.coordinator.clone(),
            steer: steer.cloned(),
            session: store.dj_session(&at.coordinator.0).unwrap(),
        });
        if self.refuse_start.lock().unwrap().as_ref() == Some(at.coordinator) {
            return Err(Failure::new(
                ErrorCode::NotReady,
                "the DJ library is still loading",
            ));
        }
        Ok(())
    }

    fn act(
        &self,
        at: DjSpeakers<'_>,
        store: &mut dyn Store,
        action: DjAction,
        clock: &dyn Clock,
    ) -> Result<OutcomeDto, Failure> {
        assert_eq!(action, DjAction::Start);
        self.calls.lock().unwrap().push(Call::Start(
            store.dj_session(&at.coordinator.0).unwrap(),
            clock.now().timestamp(),
        ));
        self.feeding.store(true, Ordering::SeqCst);
        Ok(outcome())
    }

    fn steer(
        &self,
        at: DjSpeakers<'_>,
        store: &mut dyn Store,
        steer: &DjSteer,
        clock: &dyn Clock,
    ) -> Result<OutcomeDto, Failure> {
        self.calls
            .lock()
            .unwrap()
            .push(Call::Steer(Box::new(steer.clone())));
        match steer {
            DjSteer::Clear => store.delete_dj_session(&at.coordinator.0).unwrap(),
            DjSteer::Set {
                mood,
                constraints,
                for_secs,
            } => {
                store
                    .save_dj_session(&DjSession {
                        coordinator: at.coordinator.0.clone(),
                        mood: mood.clone(),
                        constraints: (!constraints.is_empty())
                            .then(|| serde_json::to_string(constraints).unwrap()),
                        expires: for_secs.map_or(i64::MAX, |seconds| {
                            clock.now().timestamp() + i64::try_from(seconds).unwrap()
                        }),
                    })
                    .unwrap();
            }
        }
        Ok(outcome())
    }

    fn status(
        &self,
        at: DjSpeakers<'_>,
        store: &dyn Store,
        _: Option<u32>,
        clock: &dyn Clock,
    ) -> Result<DjStatusDto, Failure> {
        if self.status_fails.load(Ordering::SeqCst) {
            return Err(Failure::new(
                ErrorCode::Internal,
                "cannot read the active DJ direction",
            ));
        }
        Ok(DjStatusDto {
            zone: "Alpha".into(),
            running: self.feeds(at.coordinator),
            now: None,
            next: Vec::new(),
            steering: steering(Some(at), store, clock),
            problem: None,
        })
    }

    fn moods(
        &self,
        at: Option<DjSpeakers<'_>>,
        store: &dyn Store,
        clock: &dyn Clock,
    ) -> Result<DjMoodsDto, Failure> {
        Ok(DjMoodsDto {
            moods: ["calm", "bright"]
                .into_iter()
                .map(|name| DjMoodDto {
                    name: name.into(),
                    constraints: SteerConstraints::default(),
                })
                .collect(),
            programs: Vec::new(),
            now: steering(at, store, clock),
        })
    }

    fn feeds(&self, _: &PlayerId) -> bool {
        self.feeding.load(Ordering::SeqCst)
    }

    fn on_playback(&self, _: DjSpeakers<'_>, _: &mut dyn Store, _: &PlayerPlayback, _: &dyn Clock) {
    }

    fn until_program_changes(&self, _: &dyn Clock) -> Option<u64> {
        Some(300)
    }
}

struct SharedClock(Arc<FakeClock>);

impl Clock for SharedClock {
    fn now(&self) -> DateTime<FixedOffset> {
        self.0.now()
    }

    fn elapsed_secs(&self) -> i64 {
        self.0.elapsed_secs()
    }
}

struct Fixture {
    sim: SimHandle,
    clock: Arc<FakeClock>,
    probe: Probe,
}

impl Fixture {
    fn new() -> Self {
        Self::with_rooms(&["Alpha"])
    }

    fn with_rooms(rooms: &[&str]) -> Self {
        let sim = SimHousehold::builder()
            .s2(rooms
                .iter()
                .map(|name| SimPlayerSpec::new(*name, SimModel::One)))
            .spawn()
            .unwrap();
        let lan = sim.lan();
        let host = sim.player("Alpha").unwrap().ip;
        soap::set_av_transport_uri(
            &lan,
            host,
            "x-rincon-mp3radio://example.invalid/current.mp3",
            "",
        )
        .unwrap();
        soap::play(&lan, host).unwrap();
        Self {
            sim,
            clock: Arc::new(FakeClock::new(DateTime::parse_from_rfc3339(START).unwrap())),
            probe: Probe::default(),
        }
    }

    fn surface(&self, store: Box<dyn Store + Send>, policy: Policy) -> Arc<Surface> {
        let player = self.sim.player("Alpha").unwrap();
        let mut house = HouseholdState {
            id: Some(HouseholdId(player.household.clone())),
            ..HouseholdState::default()
        };
        house.apply_topology(&get_zone_group_state(&self.sim.lan(), player.ip).unwrap());
        Arc::new(
            Surface::new(
                Box::new(self.sim.lan()),
                Box::new(move |_| Ok(vec![house.clone()])),
                policy,
                Box::new(SharedClock(Arc::clone(&self.clock))),
            )
            .with_action_log(store, "test")
            .with_dj(Box::new(self.probe.clone())),
        )
    }

    fn memory_surface(&self) -> Arc<Surface> {
        self.surface(Box::new(MemStore::default()), Policy::default())
    }
}

fn constraints() -> SteerConstraints {
    SteerConstraints {
        include_composers: vec!["brahms".into()],
        exclude_composers: vec!["mahler".into()],
        include_artists: vec!["chamber orchestra".into()],
        exclude_artists: vec!["brass ensemble".into()],
        include_genres: vec!["classical".into()],
        exclude_genres: vec!["jazz".into()],
        periods: vec!["romantic".into()],
        exclude_periods: vec!["baroque".into()],
        decades: vec![1970],
        exclude_decades: vec![1980],
        include_keywords: vec!["piano".into()],
        exclude_keywords: vec!["opera".into()],
        min_work_minutes: Some(4),
        max_work_minutes: Some(90),
        energy_bias: -1,
        allow_long_works: true,
    }
}

fn steer(surface: &Surface, mood: &str, filters: SteerConstraints, for_secs: Option<u64>) {
    let request = DjSteerRequest {
        zone: "Alpha".into(),
        mood: Some(mood.into()),
        constraints: filters,
        for_secs,
        ..DjSteerRequest::default()
    };
    surface
        .control(&Client::Cli, "dj_steer", |houses| {
            plan_dj_steer(houses, &request)
        })
        .unwrap();
}

#[allow(clippy::needless_pass_by_value)]
fn scene(source: Value) -> SceneDefinition {
    serde_json::from_value(json!({
        "groups": [{"coordinator": "Alpha", "source": source, "playing": true}]
    }))
    .unwrap()
}

fn scheduled(when: &str) -> ScheduleRequest {
    serde_json::from_value(json!({
        "when": when, "action": "dj_start", "zone": "Alpha", "mood": "calm",
        "direction": {"constraints": constraints(), "for_secs": 900}
    }))
    .unwrap()
}

fn assert_set_then_start(calls: &[Call], filters: &SteerConstraints, seconds: u64) {
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(
        calls[0],
        Call::Steer(Box::new(DjSteer::Set {
            mood: Some("calm".into()),
            constraints: filters.clone(),
            for_secs: Some(seconds),
        }))
    );
    let Call::Start(Some(session), at) = &calls[1] else {
        panic!("the first selection did not see its steering: {calls:?}");
    };
    assert_eq!(session.mood.as_deref(), Some("calm"));
    let actual: SteerConstraints = session
        .constraints
        .as_deref()
        .map_or_else(SteerConstraints::default, |json| {
            serde_json::from_str(json).unwrap()
        });
    assert_eq!(actual, *filters);
    assert_eq!(session.expires, at + i64::try_from(seconds).unwrap());
}

fn capture_and_recall(remaining: u64) {
    let f = Fixture::new();
    let surface = f.memory_surface();
    steer(&surface, "calm", constraints(), Some(900));
    f.clock
        .advance(TimeDelta::seconds(900 - i64::try_from(remaining).unwrap()));
    f.probe.feeding.store(true, Ordering::SeqCst);
    let saved = surface
        .save_scene(&Client::Cli, "Directed evening")
        .unwrap();
    let source = &saved.groups[0].source;
    assert_eq!(source.kind, "dj");
    assert_eq!(source.mood.as_deref(), Some("calm"));
    let direction = source.direction.as_ref().expect("captured full direction");
    assert_eq!(
        direction.constraints,
        constraints(),
        "effective program filters are not frozen"
    );
    assert_eq!(direction.for_secs, Some(remaining));
    assert_eq!(
        surface.scene(&Client::Cli, "Directed evening").unwrap(),
        saved
    );
    assert_eq!(surface.scenes(&Client::Cli).unwrap(), vec![saved]);

    steer(&surface, "bright", SteerConstraints::default(), None);
    f.probe.feeding.store(false, Ordering::SeqCst);
    f.clock.advance(TimeDelta::hours(6));
    drop(f.probe.take());
    let applied = surface
        .apply_scene(&Client::Cli, "Directed evening")
        .unwrap();
    assert!(applied.complete, "{applied:?}");
    assert_set_then_start(&f.probe.take(), &constraints(), remaining);
}

#[test]
fn captured_direction_preserves_added_filters_and_reapplies_before_first_selection() {
    for remaining in [780, 1] {
        capture_and_recall(remaining);
    }
}

#[test]
fn a_failed_capture_preserves_the_existing_saved_direction() {
    let f = Fixture::new();
    let surface = f.memory_surface();
    steer(&surface, "calm", constraints(), Some(900));
    f.probe.feeding.store(true, Ordering::SeqCst);
    let saved = surface
        .save_scene(&Client::Cli, "Directed evening")
        .unwrap();
    steer(&surface, "bright", SteerConstraints::default(), None);
    f.probe.status_fails.store(true, Ordering::SeqCst);
    drop(f.probe.take());

    let refused = surface
        .save_scene(&Client::Cli, "Directed evening")
        .unwrap_err();
    assert_eq!(refused.code, ErrorCode::Internal);
    assert!(
        refused
            .detail
            .contains("cannot read the active DJ direction")
    );
    assert_eq!(
        surface.scene(&Client::Cli, "Directed evening").unwrap(),
        saved
    );
    assert!(f.probe.take().is_empty());
}

#[test]
fn an_authored_direction_is_saved_and_applied_before_the_first_selection() {
    let f = Fixture::new();
    let surface = f.memory_surface();
    let saved = surface
        .save_scene_from(
            &Client::Cli,
            "Written direction",
            &scene(json!({
                "kind": "dj", "mood": "calm",
                "direction": {"constraints": constraints(), "for_secs": 900}
            })),
        )
        .unwrap();
    assert_eq!(
        saved.groups[0]
            .source
            .direction
            .as_ref()
            .unwrap()
            .constraints,
        constraints()
    );
    steer(&surface, "bright", SteerConstraints::default(), None);
    drop(f.probe.take());
    let applied = surface
        .apply_scene(&Client::Cli, "Written direction")
        .unwrap();
    assert!(applied.complete, "{applied:?}");
    assert_set_then_start(&f.probe.take(), &constraints(), 900);
}

#[test]
fn an_automatic_capture_clears_later_steering_before_selecting_new_music() {
    let f = Fixture::new();
    let surface = f.memory_surface();
    f.probe.feeding.store(true, Ordering::SeqCst);
    let saved = surface
        .save_scene(&Client::Cli, "Automatic evening")
        .unwrap();
    let source = &saved.groups[0].source;
    assert_eq!(source.kind, "dj");
    assert!(
        source.mood.is_none(),
        "the current program mood is not an override"
    );
    let direction = source
        .direction
        .as_ref()
        .expect("explicit automatic intent");
    assert!(direction.constraints.is_empty() && direction.for_secs.is_none());

    steer(&surface, "bright", constraints(), None);
    f.probe.feeding.store(false, Ordering::SeqCst);
    drop(f.probe.take());
    let applied = surface
        .apply_scene(&Client::Cli, "Automatic evening")
        .unwrap();
    assert!(applied.complete, "{applied:?}");
    assert_eq!(
        f.probe.take(),
        [
            Call::Steer(Box::new(DjSteer::Clear)),
            Call::Start(None, f.clock.now().timestamp()),
        ]
    );
}

#[test]
fn legacy_scenes_keep_existing_steering_and_mood_only_expiry_semantics() {
    let f = Fixture::new();
    let surface = f.memory_surface();
    for source in [
        json!({"kind": "dj"}),
        json!({"kind": "dj", "direction": null}),
    ] {
        steer(&surface, "bright", constraints(), None);
        let saved = surface
            .save_scene_from(&Client::Cli, "Legacy", &scene(source))
            .unwrap();
        assert!(saved.groups[0].source.direction.is_none());
        drop(f.probe.take());
        assert!(
            surface
                .apply_scene(&Client::Cli, "Legacy")
                .unwrap()
                .complete
        );
        let calls = f.probe.take();
        let [Call::Start(Some(session), _)] = calls.as_slice() else {
            panic!("legacy start unexpectedly replaces steering: {calls:?}");
        };
        assert_eq!(session.mood.as_deref(), Some("bright"));
        assert_eq!(session.expires, i64::MAX);
    }
    surface
        .save_scene_from(
            &Client::Cli,
            "Mood only",
            &scene(json!({
                "kind": "dj", "mood": "calm"
            })),
        )
        .unwrap();
    drop(f.probe.take());
    assert!(
        surface
            .apply_scene(&Client::Cli, "Mood only")
            .unwrap()
            .complete
    );
    assert_set_then_start(&f.probe.take(), &SteerConstraints::default(), 300);
}

#[test]
fn caller_and_scene_writer_must_both_be_allowed_to_apply_the_saved_direction() {
    let f = Fixture::new();
    let restricted = Client::Tailnet("tag:restricted".into());
    let cli = Client::Cli;
    for denied_writer in [false, true] {
        let policy =
            Policy::from_toml("[clients.\"tag:restricted\"]\ndeny=[\"dj_steer\"]\n").unwrap();
        let surface = f.surface(Box::new(MemStore::default()), policy);
        let (writer, caller) = if denied_writer {
            (&restricted, &cli)
        } else {
            (&cli, &restricted)
        };
        surface
            .save_scene_from(
                writer,
                "Restricted",
                &scene(json!({
                    "kind": "dj", "mood": "calm",
                    "direction": {"constraints": constraints(), "for_secs": 900}
                })),
            )
            .unwrap();
        drop(f.probe.take());
        let applied = surface.apply_scene(caller, "Restricted").unwrap();
        assert!(!applied.changed && applied.steps.is_empty(), "{applied:?}");
        assert!(
            applied
                .notes
                .iter()
                .any(|note| note.code == NoteCode::Skipped && note.detail.contains("dj_steer")),
            "{applied:?}"
        );
        assert!(
            f.probe.take().is_empty(),
            "denying steer must prevent start too"
        );
    }
}

#[test]
fn stored_schedules_keep_all_filters_and_restart_the_duration_on_every_run() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("direction.sqlite");
    let f = Fixture::new();
    let surface = f.surface(
        Box::new(SqliteStore::open(&path).unwrap()),
        Policy::default(),
    );
    let creator = Client::Tailnet("tag:routine".into());
    let saved = surface
        .add_schedule(&creator, &scheduled("daily 07:30"))
        .unwrap();
    let direction = saved.direction.as_ref().expect("saved direction");
    assert_eq!(direction.constraints, constraints());
    assert_eq!(direction.for_secs, Some(900));
    drop(surface);

    let store = SqliteStore::open(&path).unwrap();
    let rows = store.schedules().unwrap();
    let persisted: Value = serde_json::from_str(&rows[0].action).unwrap();
    assert_eq!(
        persisted["direction"],
        json!({"constraints": constraints(), "for_secs": 900})
    );
    let surface = f.surface(Box::new(store), Policy::default());
    assert_eq!(
        surface.schedules(&Client::Cli).unwrap()[0].direction,
        saved.direction
    );
    let scheduler = Scheduler::new(&surface);
    let first = DateTime::parse_from_rfc3339("2026-10-08T07:30:00+00:00").unwrap();
    for day in [0, 1] {
        let run = first + TimeDelta::days(day);
        f.clock.set(run - TimeDelta::seconds(60));
        steer(&surface, "bright", SteerConstraints::default(), None);
        f.clock.set(run);
        drop(f.probe.take());
        assert_eq!(
            scheduler.tick_at(run.to_utc(), &Utc),
            [Ran::Fired {
                id: saved.id,
                at: run.to_utc()
            }]
        );
        assert!(scheduler.wait(Duration::from_secs(5)));
        assert_set_then_start(&f.probe.take(), &constraints(), 900);
    }
    let actions = surface
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    let starts: Vec<_> = actions
        .iter()
        .filter(|row| row.action.intent.starts_with("dj_start"))
        .collect();
    assert_eq!(starts.len(), 2);
    assert!(starts.iter().all(|row| row.action.client == "tag:routine"));
}

#[test]
fn a_stored_schedule_rechecks_its_creators_steering_permission_before_start() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("permission.sqlite");
    let f = Fixture::new();
    let surface = f.surface(
        Box::new(SqliteStore::open(&path).unwrap()),
        Policy::default(),
    );
    let creator = Client::Tailnet("tag:routine".into());
    let saved = surface
        .add_schedule(&creator, &scheduled("daily 07:30"))
        .unwrap();
    drop(surface);
    let policy = Policy::from_toml("[clients.\"tag:routine\"]\ndeny=[\"dj_steer\"]\n").unwrap();
    let surface = f.surface(Box::new(SqliteStore::open(&path).unwrap()), policy);
    let refused = surface
        .add_schedule(&creator, &scheduled("in 1h"))
        .unwrap_err();
    assert_eq!(refused.code, ErrorCode::PolicyDenied);
    steer(&surface, "bright", constraints(), None);
    drop(f.probe.take());
    let at = DateTime::parse_from_rfc3339("2026-10-08T07:30:00+00:00").unwrap();
    f.clock.set(at);
    let scheduler = Scheduler::new(&surface);
    assert_eq!(
        scheduler.tick_at(at.to_utc(), &Utc),
        [Ran::Fired {
            id: saved.id,
            at: at.to_utc()
        }]
    );
    assert!(scheduler.wait(Duration::from_secs(5)));
    assert!(
        f.probe.take().is_empty(),
        "a refused steer must never fall through to Start"
    );
    let current = surface.dj_status(&Client::Cli, "Alpha").unwrap();
    assert_eq!(current.steering.mood.as_deref(), Some("bright"));
    assert_eq!(current.steering.added, constraints());
    assert!(
        surface.schedules(&Client::Cli).unwrap()[0]
            .last_fired
            .is_some()
    );
}

fn only_read_requests(sim: &SimHandle, from: usize) {
    let log = sim.soap_log();
    let writes: Vec<_> = log[from..]
        .iter()
        .filter(|request| !request.action.starts_with("Get") && request.action != "Browse")
        .map(|request| request.action.as_str())
        .collect();
    assert!(
        writes.is_empty(),
        "readiness sent speaker writes: {writes:?}"
    );
}

#[test]
fn a_capped_scene_checks_prospective_direction_before_lowering_any_volume() {
    let f = Fixture::new();
    let player = f.sim.player("Alpha").unwrap();
    let coordinator = PlayerId(player.uuid.clone());
    soap::set_volume(&f.sim.lan(), player.ip, 90).unwrap();
    let policy = Policy::from_toml("[clients.\"tag:capped\"]\nmax_volume = 30\n").unwrap();
    let surface = f.surface(Box::new(MemStore::default()), policy);
    steer(&surface, "bright", SteerConstraints::default(), None);
    let definition: SceneDefinition = serde_json::from_value(json!({
        "groups": [{"coordinator": "Alpha", "source": {
            "kind": "dj", "mood": "calm",
            "direction": {"constraints": constraints(), "for_secs": 60}
        }}],
        "mutes": {"Alpha": true}
    }))
    .unwrap();
    surface
        .save_scene_from(
            &Client::Tailnet("tag:capped".into()),
            "Ready first",
            &definition,
        )
        .unwrap();
    *f.probe.refuse_start.lock().unwrap() = Some(coordinator.clone());
    drop(f.probe.take());
    let before = f.sim.soap_log().len();
    let failure = surface
        .apply_scene(&Client::Cli, "Ready first")
        .unwrap_err();
    assert_eq!(failure.code, ErrorCode::NotReady);
    only_read_requests(&f.sim, before);
    assert_eq!(soap::get_volume(&f.sim.lan(), player.ip).unwrap(), 90);
    assert!(
        f.probe.take().is_empty(),
        "no steering or start before readiness"
    );
    let prepared = f.probe.readiness.lock().unwrap().clone();
    assert_eq!(prepared.len(), 1);
    assert_eq!(prepared[0].coordinator, coordinator);
    assert_eq!(
        prepared[0].session.as_ref().unwrap().mood.as_deref(),
        Some("bright")
    );
    assert_eq!(
        prepared[0].steer,
        Some(DjSteer::Set {
            mood: Some("calm".into()),
            constraints: constraints(),
            for_secs: Some(60)
        })
    );
    assert_eq!(
        surface
            .dj_status(&Client::Cli, "Alpha")
            .unwrap()
            .steering
            .mood
            .as_deref(),
        Some("bright")
    );

    // Once ready, execution sees exactly the direction checked above, with a
    // fresh relative lifetime and the writer's volume cap still enforced.
    *f.probe.refuse_start.lock().unwrap() = None;
    assert!(
        surface
            .apply_scene(&Client::Cli, "Ready first")
            .unwrap()
            .complete
    );
    assert_set_then_start(&f.probe.take(), &constraints(), 60);
    assert_eq!(soap::get_volume(&f.sim.lan(), player.ip).unwrap(), 30);
}

#[test]
fn every_scene_dj_is_ready_before_regrouping_to_a_future_coordinator() {
    let f = Fixture::with_rooms(&["Alpha", "Beta", "Gamma"]);
    let alpha = f.sim.player("Alpha").unwrap();
    let beta = f.sim.player("Beta").unwrap();
    let gamma = f.sim.player("Gamma").unwrap();
    soap::set_av_transport_uri(
        &f.sim.lan(),
        beta.ip,
        &format!("x-rincon:{}", alpha.uuid),
        "",
    )
    .unwrap();
    let before_topology = get_zone_group_state(&f.sim.lan(), alpha.ip).unwrap();
    let surface = f.memory_surface();
    let definition: SceneDefinition = serde_json::from_value(json!({
        "groups": [
            {"coordinator": "Gamma", "source": {"kind": "dj", "mood": "calm"}},
            {"coordinator": "Beta", "members": ["Alpha"],
                "source": {"kind": "dj", "mood": "bright"}}
        ],
        "volumes": {"Alpha": 20, "Beta": 22, "Gamma": 25}
    }))
    .unwrap();
    surface
        .save_scene_from(&Client::Cli, "Two DJs", &definition)
        .unwrap();
    *f.probe.refuse_start.lock().unwrap() = Some(PlayerId(beta.uuid.clone()));
    let before = f.sim.soap_log().len();
    let failure = surface.apply_scene(&Client::Cli, "Two DJs").unwrap_err();
    assert_eq!(failure.code, ErrorCode::NotReady);
    only_read_requests(&f.sim, before);
    assert!(
        f.probe.take().is_empty(),
        "even the first ready group stays untouched"
    );
    assert_eq!(
        get_zone_group_state(&f.sim.lan(), alpha.ip).unwrap(),
        before_topology
    );
    let prepared = f.probe.readiness.lock().unwrap();
    assert_eq!(
        prepared
            .iter()
            .map(|call| call.coordinator.clone())
            .collect::<Vec<_>>(),
        [PlayerId(gamma.uuid.clone()), PlayerId(beta.uuid.clone())]
    );
    assert!(prepared.iter().all(|call| call.session.is_none()));
}

#[test]
fn a_scheduled_scene_ignores_denied_dj_steps_and_applies_its_allowed_volumes() {
    for restricted_writer in [false, true] {
        let f = Fixture::new();
        let player = f.sim.player("Alpha").unwrap();
        soap::set_volume(&f.sim.lan(), player.ip, 45).unwrap();
        let policy =
            Policy::from_toml("[clients.\"tag:restricted\"]\ndeny = [\"dj_steer\"]\n").unwrap();
        let surface = f.surface(Box::new(MemStore::default()), policy);
        let restricted = Client::Tailnet("tag:restricted".into());
        let cli = Client::Cli;
        let (writer, caller) = if restricted_writer {
            (&restricted, &cli)
        } else {
            (&cli, &restricted)
        };
        let definition: SceneDefinition = serde_json::from_value(json!({
            "groups": [{"coordinator": "Alpha", "source": {
                "kind": "dj", "mood": "calm", "direction": {}
            }}],
            "volumes": {"Alpha": 20}
        }))
        .unwrap();
        surface
            .save_scene_from(writer, "Permitted steps", &definition)
            .unwrap();
        *f.probe.refuse_start.lock().unwrap() = Some(PlayerId(player.uuid.clone()));
        let request: ScheduleRequest = serde_json::from_value(json!({
            "when": "in 1m", "action": "apply_scene", "scene": "Permitted steps"
        }))
        .unwrap();
        let saved = surface.add_schedule(caller, &request).unwrap();
        f.clock.advance(TimeDelta::minutes(1));
        let at = f.clock.now().to_utc();
        let scheduler = Scheduler::new(&surface);
        assert_eq!(
            scheduler.tick_at(at, &Utc),
            [Ran::Fired { id: saved.id, at }]
        );
        assert!(scheduler.wait(Duration::from_secs(5)));
        assert_eq!(soap::get_volume(&f.sim.lan(), player.ip).unwrap(), 20);
        assert!(f.probe.readiness.lock().unwrap().is_empty());
        assert!(f.probe.take().is_empty());
        assert!(
            surface.schedules(&Client::Cli).unwrap()[0]
                .last_fired
                .is_some()
        );
        assert!(
            scheduler
                .tick_at(at + TimeDelta::seconds(1), &Utc)
                .is_empty()
        );
    }
}
