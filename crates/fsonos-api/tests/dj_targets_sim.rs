//! A DJ target set selects independent groups once, prepares every start
//! before writes, and retains every result across execution failures.
//! SOAP uses loopback S1/S2 simulators; the probe supplies only DJ selection.

use chrono::DateTime;
use fsonos_api::dj::{
    DjEngine, DjMoodsDto, DjSpeakers, DjStatusDto, DjSteer, DjSteeringDto, SteerConstraints,
};
use fsonos_api::plan::{DjAction, plan_dj_set, plan_dj_start_set, plan_dj_steer_set};
use fsonos_api::{
    DjStartRequest, DjSteerRequest, ErrorCode, Failure, NoteCode, OutcomeDto, Surface, ZoneRequest,
};
use fsonos_core::actions::BeforeState;
use fsonos_core::clock::{Clock, FakeClock};
use fsonos_core::playback::PlayerPlayback;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::rooms::ControlTarget;
use fsonos_core::store::{ActionFilter, DjSession, LoggedAction, MemStore, Store};
use fsonos_core::{CoreError, HouseholdState, inventory, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::net::IpAddr;
use std::time::Duration;

const GROUPS: [&str; 4] = ["Lead@S1", "Guest@S1", "Lead@S2", "Guest@S2"];
const SELECTED: [&str; 2] = ["Lead@S1", "Lead@S2"];
const CAP: u8 = 25;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
    Ready(String, Option<DjSteer>),
    Steer(String, DjSteer),
    Act(String, DjAction, Option<DjSession>),
}

#[derive(Clone, Default)]
struct Probe {
    events: Arc<Mutex<Vec<Event>>>,
    refuse_ready: Arc<Mutex<Option<PlayerId>>>,
    running: Arc<Mutex<BTreeSet<String>>>,
    // Another controller adds this room during the first readiness check.
    regroup: Arc<Mutex<Option<(PlayerId, PlayerId)>>>,
    before_gate: Arc<Mutex<Option<(PlayerId, PlayerId)>>>,
    before_write: Arc<Mutex<Option<(PlayerId, PlayerId)>>>,
    external: Arc<Mutex<Option<SimLan>>>,
}

fn done(action: &str) -> OutcomeDto {
    OutcomeDto {
        done: action.into(),
        changed: true,
        volume: None,
        notes: Vec::new(),
    }
}

impl Probe {
    fn change_selected(&self, at: DjSpeakers<'_>, pending: &Mutex<Option<(PlayerId, PlayerId)>>) {
        let mut pending = pending.lock().unwrap();
        if pending.as_ref().is_none_or(|(_, leader)| leader != at.coordinator) {
            return;
        }
        let (member, leader) = pending.take().unwrap();
        let player = at.households.iter().find_map(|h| h.player(&member)).unwrap();
        // Another controller uses the underlying simulator, bypassing the
        // operation-scoped transport belonging to this DJ command.
        soap::join_group(self.external.lock().unwrap().as_ref().unwrap(), player.ip, &leader).unwrap();
    }

    fn ready(&self, at: DjSpeakers<'_>, steer: Option<&DjSteer>) -> Result<(), Failure> {
        self.events.lock().unwrap().push(Event::Ready(
            at.coordinator.0.clone(),
            steer.cloned(),
        ));
        if let Some((member, leader)) = self.regroup.lock().unwrap().take() {
            let player = at.households.iter().find_map(|h| h.player(&member)).unwrap();
            soap::join_group(at.transport, player.ip, &leader).unwrap();
        }
        if self.refuse_ready.lock().unwrap().as_ref() == Some(at.coordinator) {
            return Err(Failure::new(ErrorCode::NotReady, "the library is still loading"));
        }
        Ok(())
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
        self.ready(at, steer)
    }

    fn steer_ready(
        &self,
        at: DjSpeakers<'_>,
        _: &dyn Store,
        steer: &DjSteer,
        _: &dyn Clock,
    ) -> Result<(), Failure> {
        self.ready(at, Some(steer))
    }

    fn act(
        &self,
        at: DjSpeakers<'_>,
        store: &mut dyn Store,
        action: DjAction,
        _: &dyn Clock,
    ) -> Result<OutcomeDto, Failure> {
        self.events.lock().unwrap().push(Event::Act(
            at.coordinator.0.clone(),
            action,
            store.dj_session(&at.coordinator.0).unwrap(),
        ));
        let player = at.households.iter().find_map(|h| h.player(at.coordinator)).unwrap();
        self.change_selected(at, &self.before_write);
        let mut running = self.running.lock().unwrap();
        if action != DjAction::Start && !running.contains(&at.coordinator.0) {
            return Err(Failure::new(ErrorCode::NoDjSession, "the DJ is idle"));
        }
        match action {
            DjAction::Start => {
                soap::set_av_transport_uri(
                    at.transport, player.ip, "x-rincon-mp3radio://example.invalid/dj.mp3", "",
                ).map_err(CoreError::from)?;
                soap::play(at.transport, player.ip).map_err(CoreError::from)?;
                running.insert(at.coordinator.0.clone());
            }
            DjAction::Stop => {
                soap::pause(at.transport, player.ip).map_err(CoreError::from)?;
                running.remove(&at.coordinator.0);
            }
            DjAction::Skip => {}
        }
        Ok(done("DJ command completed"))
    }

    fn act_as_checked(
        &self,
        at: DjSpeakers<'_>,
        store: &mut dyn Store,
        action: DjAction,
        clock: &dyn Clock,
        _: &Client,
        before: &dyn Fn() -> Result<(), Failure>,
    ) -> Result<OutcomeDto, Failure> {
        self.change_selected(at, &self.before_gate);
        before()?;
        self.act(at, store, action, clock)
    }

    fn steer_checked(
        &self,
        at: DjSpeakers<'_>,
        store: &mut dyn Store,
        steer: &DjSteer,
        clock: &dyn Clock,
        before: &dyn Fn() -> Result<(), Failure>,
    ) -> Result<OutcomeDto, Failure> {
        self.change_selected(at, &self.before_gate);
        before()?;
        self.steer(at, store, steer, clock)
    }

    fn steer(
        &self,
        at: DjSpeakers<'_>,
        store: &mut dyn Store,
        steer: &DjSteer,
        clock: &dyn Clock,
    ) -> Result<OutcomeDto, Failure> {
        self.events.lock().unwrap().push(Event::Steer(at.coordinator.0.clone(), steer.clone()));
        match steer {
            DjSteer::Clear => store.delete_dj_session(&at.coordinator.0).unwrap(),
            DjSteer::Set { mood, constraints, for_secs } => store.save_dj_session(&DjSession {
                coordinator: at.coordinator.0.clone(),
                mood: mood.clone(),
                constraints: (!constraints.is_empty()).then(|| serde_json::to_string(constraints).unwrap()),
                expires: for_secs.map_or(i64::MAX, |secs| clock.now().timestamp() + i64::try_from(secs).unwrap()),
            }).unwrap(),
        }
        Ok(done("DJ direction saved"))
    }

    fn status(
        &self,
        at: DjSpeakers<'_>,
        store: &dyn Store,
        _: Option<u32>,
        _: &dyn Clock,
    ) -> Result<DjStatusDto, Failure> {
        let session = store.dj_session(&at.coordinator.0).unwrap();
        let added = session.as_ref().and_then(|row| row.constraints.as_deref())
            .map_or_else(SteerConstraints::default, |text| serde_json::from_str(text).unwrap());
        Ok(DjStatusDto {
            zone: at.coordinator.0.clone(),
            running: self.feeds(at.coordinator),
            now: None,
            next: Vec::new(),
            steering: DjSteeringDto {
                source: if session.is_some() { "session" } else { "program" }.into(),
                mood: session.as_ref().and_then(|row| row.mood.clone()),
                session_mood: session.as_ref().and_then(|row| row.mood.clone()),
                constraints: added.clone(),
                added,
                expires_at: session.as_ref().map(|row| row.expires),
                expires_in_secs: None,
            },
            problem: None,
        })
    }

    fn moods(
        &self,
        _: Option<DjSpeakers<'_>>,
        _: &dyn Store,
        _: &dyn Clock,
    ) -> Result<DjMoodsDto, Failure> {
        Err(Failure::new(ErrorCode::NotImplemented, "not used in this fixture"))
    }

    fn feeds(&self, coordinator: &PlayerId) -> bool {
        self.running.lock().unwrap().contains(&coordinator.0)
    }

    fn on_playback(&self, _: DjSpeakers<'_>, _: &mut dyn Store, _: &PlayerPlayback, _: &dyn Clock) {}
}

struct House {
    sim: SimHandle,
    households: Vec<HouseholdState>,
    directory: tempfile::TempDir,
    probe: Probe,
}

impl House {
    fn new() -> Self {
        let sim = SimHousehold::builder()
            .s1(["Lead", "Member", "Guest"].map(|name| SimPlayerSpec::new(name, SimModel::Play5Gen1)))
            .s2(["Lead", "Member", "Guest"].map(|name| SimPlayerSpec::new(name, SimModel::One)))
            .spawn().unwrap();
        for generation in [1, 2] {
            let player = |name| sim.players().iter()
                .find(|p| p.generation == generation && p.room == name).unwrap();
            soap::join_group(&sim.lan(), player("Member").ip, &PlayerId(player("Lead").uuid.clone())).unwrap();
        }
        let households = inventory::survey(&sim.lan(), &[], Duration::from_millis(500)).unwrap().households;
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("aliases.toml"),
            "[aliases]\nselected = [\"Member@S1\", \"Lead@S1\", \"Member@S2\", \"Lead@S2\"]\n",
        ).unwrap();
        for player in sim.players() {
            soap::set_volume(&sim.lan(), player.ip, 80).unwrap();
        }
        let probe = Probe::default();
        *probe.external.lock().unwrap() = Some(sim.lan());
        Self { sim, households, directory, probe }
    }

    fn target(&self, zone: &str) -> ControlTarget<'_> {
        resolve_room(&self.households, zone).unwrap()
    }

    fn surface(&self) -> Surface {
        self.surface_over(Box::new(self.sim.lan()))
    }

    fn surface_over(&self, transport: Box<dyn Transport + Send + Sync>) -> Surface {
        let known = self.households.clone();
        let mut policy = Policy::default();
        policy.defaults.max_volume = CAP;
        let mut store = MemStore::default();
        for zone in GROUPS {
            store.save_dj_session(&DjSession {
                coordinator: self.target(zone).coordinator.id.0.clone(),
                mood: Some("bright".into()),
                constraints: None,
                expires: i64::MAX,
            }).unwrap();
        }
        Surface::new(
            transport, Box::new(move |_| Ok(known.clone())), policy,
            Box::new(FakeClock::new(DateTime::parse_from_rfc3339("2026-10-12T07:30:00Z").unwrap())),
        )
        .with_aliases_file(self.directory.path().join("aliases.toml"))
        .with_action_log(Box::new(store), "test")
        .with_dj(Box::new(self.probe.clone()))
    }

    fn events(&self) -> Vec<Event> {
        self.probe.events.lock().unwrap().clone()
    }

    fn writes_since(&self, start: usize) -> usize {
        self.sim.soap_log().iter().skip(start)
            .filter(|call| call.action != "Browse" && !call.action.starts_with("Get")).count()
    }
}

fn direction() -> SteerConstraints {
    SteerConstraints {
        include_genres: vec!["classical".into()],
        exclude_composers: vec!["mahler".into()],
        strict: true,
        ..SteerConstraints::default()
    }
}

fn request(zone: &str) -> DjStartRequest {
    DjStartRequest {
        zone: zone.into(), mood: Some("calm".into()),
        constraints: direction(), for_secs: Some(900),
    }
}

fn one_action(surface: &Surface) -> LoggedAction {
    let mut actions = surface.recent_actions(&Client::Cli, &ActionFilter::default()).unwrap();
    assert_eq!(actions.len(), 1, "a DJ set has one action boundary");
    actions.pop().unwrap()
}

#[test]
fn a_directed_alias_deduplicates_members_and_caps_only_the_selected_groups() {
    let house = House::new();
    let surface = house.surface();
    let result = surface.control_set(&Client::McpStdio, "dj_start", |rooms| {
        plan_dj_start_set(rooms, &request("selected"))
    }).unwrap();
    let events = house.events();
    assert_eq!(events.iter().filter(|e| matches!(e, Event::Ready(..))).count(), 2);
    assert!(events[..2].iter().all(|e| matches!(e, Event::Ready(_, Some(_)))), "{events:?}");
    for zone in SELECTED {
        let coordinator = &house.target(zone).coordinator.id.0;
        let starts: Vec<_> = events.iter().filter_map(|event| match event {
            Event::Act(id, DjAction::Start, session) if id == coordinator => Some(session),
            _ => None,
        }).collect();
        assert_eq!(starts.len(), 1, "{zone} is started once");
        let session = starts[0].as_ref().unwrap();
        assert_eq!(session.mood.as_deref(), Some("calm"));
        assert_eq!(serde_json::from_str::<SteerConstraints>(session.constraints.as_deref().unwrap()).unwrap(), direction());
        assert_eq!(session.expires, DateTime::parse_from_rfc3339("2026-10-12T07:45:00Z").unwrap().timestamp());
        assert!(result.done.contains(zone), "{result:?}");
    }
    for generation in [1, 2] {
        for name in ["Lead", "Member", "Guest"] {
            let zone = format!("{name}@S{generation}");
            assert_eq!(soap::get_volume(&house.sim.lan(), house.target(&zone).player.ip).unwrap(),
                if name == "Guest" { 80 } else { CAP }, "{zone}");
        }
    }
    let action = one_action(&surface);
    let before = BeforeState::parse(action.action.before_state.as_deref().unwrap()).unwrap();
    assert_eq!(before.zones.len(), 2);
    assert_eq!(before.dj_sessions.len(), 2);
}

#[test]
fn one_unready_group_reports_every_preflight_and_leaves_all_groups_untouched() {
    let house = House::new();
    *house.probe.refuse_ready.lock().unwrap() = Some(house.target("Lead@S1").coordinator.id.clone());
    let surface = house.surface();
    let start = house.sim.soap_log().len();
    let refused = surface.control_set(&Client::McpStdio, "dj_start", |rooms| {
        plan_dj_start_set(rooms, &request("everywhere"))
    }).unwrap_err();
    assert_eq!(refused.code, ErrorCode::NotReady);
    assert_eq!(house.events().len(), GROUPS.len());
    assert!(house.events().iter().all(|event| matches!(event, Event::Ready(..))));
    assert_eq!(house.writes_since(start), 0, "no caps, sessions or playback before readiness");
    for zone in GROUPS {
        assert!(refused.detail.contains(zone), "{refused:?}");
        assert_eq!(surface.dj_status(&Client::Cli, zone).unwrap().steering.session_mood.as_deref(), Some("bright"));
    }
    assert!(surface.recent_actions(&Client::Cli, &ActionFilter::default()).unwrap().is_empty());
}

#[test]
fn steer_and_clear_all_groups_share_one_undo_boundary() {
    let house = House::new();
    let surface = house.surface();
    let request = DjSteerRequest {
        zone: "everywhere".into(), mood: Some("calm".into()), constraints: direction(),
        for_secs: Some(900), ..DjSteerRequest::default()
    };
    surface.control_set(&Client::Cli, "dj_steer", |rooms| plan_dj_steer_set(rooms, &request)).unwrap();
    let action = one_action(&surface);
    let before = BeforeState::parse(action.action.before_state.as_deref().unwrap()).unwrap();
    assert!(before.zones.is_empty(), "steering changes sessions, not queued music");
    assert_eq!(before.dj_sessions.len(), GROUPS.len());
    let undone = surface.undo_checked(&Client::Cli, action.id).unwrap();
    assert_eq!(undone.undone, Some(action.id));
    assert!(undone.failures.is_empty(), "{undone:?}");
    for zone in GROUPS {
        assert_eq!(surface.dj_status(&Client::Cli, zone).unwrap().steering.session_mood.as_deref(), Some("bright"));
    }
    let clear = DjSteerRequest { zone: "all@S2".into(), clear: true, ..DjSteerRequest::default() };
    surface.control_set(&Client::Cli, "dj_steer", |rooms| plan_dj_steer_set(rooms, &clear)).unwrap();
    for zone in ["Lead@S2", "Guest@S2"] {
        assert!(surface.dj_status(&Client::Cli, zone).unwrap().steering.session_mood.is_none());
    }
    assert_eq!(surface.dj_status(&Client::Cli, "Lead@S1").unwrap().steering.session_mood.as_deref(), Some("bright"));
}

#[test]
fn idle_groups_do_not_hide_other_skip_results_and_the_log_keeps_every_outcome() {
    for action in [DjAction::Skip, DjAction::Stop] {
        let house = House::new();
        let surface = house.surface();
        let running = house.target("Lead@S2").coordinator;
        soap::set_av_transport_uri(
            &house.sim.lan(), running.ip, "x-rincon-mp3radio://example.invalid/running.mp3", "",
        ).unwrap();
        soap::play(&house.sim.lan(), running.ip).unwrap();
        house.probe.running.lock().unwrap().insert(running.id.0.clone());
        *house.probe.refuse_ready.lock().unwrap() = Some(running.id.clone());
        let result = surface.control_set(&Client::Cli, if action == DjAction::Skip { "dj_skip" } else { "dj_stop" }, |rooms| {
            plan_dj_set(rooms, &ZoneRequest { zone: "all".into() }, action)
        }).unwrap();
        assert_eq!(house.events().len(), GROUPS.len());
        assert!(house.events().iter().all(|event| matches!(event, Event::Act(..))), "skip and stop do not require a library");
        assert!(result.done.contains("Lead@S2"));
        assert_eq!(result.notes.iter().filter(|note| note.code == NoteCode::Skipped).count(), 3);
        let logged = one_action(&surface);
        for zone in GROUPS {
            assert!(logged.action.result.contains(zone), "{logged:?}");
        }
        let before = BeforeState::parse(logged.action.before_state.as_deref().unwrap()).unwrap();
        assert_eq!(before.not_undoable.as_deref(), Some("incomplete multi-group DJ command"));
    }
}

#[test]
fn an_all_failed_execution_retains_every_group_and_cannot_be_replayed_as_a_read_failure() {
    let house = House::new();
    let surface = house.surface();
    let failure = surface.control_set(&Client::Cli, "dj_skip", |rooms| {
        plan_dj_set(rooms, &ZoneRequest { zone: "everywhere".into() }, DjAction::Skip)
    }).unwrap_err();
    assert_eq!(failure.code, ErrorCode::OutcomeUncertain);
    assert_eq!(house.events().len(), GROUPS.len());
    for zone in GROUPS {
        assert!(failure.detail.contains(zone), "{failure:?}");
    }
    assert!(one_action(&surface).action.before_state.is_some());
}

#[test]
fn a_group_changed_during_readiness_is_not_started_or_redirected() {
    let house = House::new();
    *house.probe.regroup.lock().unwrap() = Some((
        house.target("Guest@S1").player.id.clone(), house.target("Lead@S1").coordinator.id.clone(),
    ));
    let surface = house.surface();
    let result = surface.control_set(&Client::McpStdio, "dj_start", |rooms| {
        plan_dj_start_set(rooms, &request("selected"))
    }).unwrap();
    assert!(result.done.contains("Lead@S2"));
    assert!(result.notes.iter().any(|note| note.code == NoteCode::Skipped && note.detail.contains("Lead@S1")));
    let first = &house.target("Lead@S1").coordinator.id.0;
    assert!(!house.events().iter().any(|event| matches!(event, Event::Act(id, _, _) if id == first)));
    for zone in ["Lead@S1", "Member@S1", "Guest@S1"] {
        assert_eq!(soap::get_volume(&house.sim.lan(), house.target(zone).player.ip).unwrap(), 80);
    }
}

#[test]
fn policy_denial_precedes_every_readiness_check_and_write() {
    let house = House::new();
    let surface = house.surface();
    let start = house.sim.soap_log().len();
    let failure = surface.control_set(&Client::Unknown, "dj_start", |rooms| {
        plan_dj_start_set(rooms, &request("all"))
    }).unwrap_err();
    assert_eq!(failure.code, ErrorCode::PolicyDenied);
    assert!(house.events().is_empty());
    assert_eq!(house.writes_since(start), 0);
}

#[test]
fn an_unreadable_capped_room_blocks_the_entire_start_before_any_reduction() {
    let house = House::new();
    house.sim.upnp_fault(&house.target("Guest@S2").player.id.0, "GetVolume", 501).unwrap();
    let surface = house.surface();
    let start = house.sim.soap_log().len();
    let failure = surface.control_set(&Client::McpStdio, "dj_start", |rooms| {
        plan_dj_start_set(rooms, &request("all"))
    }).unwrap_err();
    assert_eq!(failure.code, ErrorCode::PlayerUnreachable);
    assert!(failure.detail.contains("volume") && failure.detail.contains("cap of 25"));
    for zone in GROUPS {
        assert!(failure.detail.contains(zone), "{failure:?}");
    }
    assert_eq!(house.writes_since(start), 0, "all caps are preflighted before reductions");
    assert!(house.events().iter().all(|event| matches!(event, Event::Ready(..))));
}

#[test]
fn a_group_expanding_after_engine_admission_is_blocked_at_the_actual_speaker_write() {
    let house = House::new();
    *house.probe.before_write.lock().unwrap() = Some((
        house.target("Guest@S1").player.id.clone(), house.target("Lead@S1").coordinator.id.clone(),
    ));
    let surface = house.surface();
    let start = house.sim.soap_log().len();
    let result = surface.control_set(&Client::Cli, "dj_start", |rooms| {
        plan_dj_start_set(rooms, &DjStartRequest { zone: "selected".into(), ..DjStartRequest::default() })
    }).unwrap();
    assert!(result.done.contains("Lead@S2"));
    assert!(result.notes.iter().any(|note| note.code == NoteCode::Skipped && note.detail.contains("Lead@S1")));
    let leader = &house.target("Lead@S1").coordinator.id.0;
    assert!(!house.sim.soap_log().iter().skip(start).any(|call|
        call.player == *leader && matches!(call.action.as_str(), "SetAVTransportURI" | "Play")));
    assert!(!house.probe.feeds(&house.target("Lead@S1").coordinator.id));
    assert!(house.probe.feeds(&house.target("Lead@S2").coordinator.id));
}

#[test]
fn a_checked_engine_rejects_late_regrouping_before_store_only_steering_or_start() {
    for start in [false, true] {
        let house = House::new();
        *house.probe.before_gate.lock().unwrap() = Some((
            house.target("Guest@S1").player.id.clone(), house.target("Lead@S1").coordinator.id.clone(),
        ));
        let surface = house.surface();
        let result = if start {
            surface.control_set(&Client::Cli, "dj_start", |rooms| {
                plan_dj_start_set(rooms, &request("selected"))
            })
        } else {
            surface.control_set(&Client::Cli, "dj_steer", |rooms| {
                plan_dj_steer_set(rooms, &DjSteerRequest {
                    zone: "selected".into(), mood: Some("calm".into()), ..DjSteerRequest::default()
                })
            })
        }.unwrap();
        assert!(result.done.contains("Lead@S2"));
        assert!(result.notes.iter().any(|note| note.code == NoteCode::Skipped && note.detail.contains("Lead@S1")));
        assert_eq!(surface.dj_status(&Client::Cli, "Lead@S1").unwrap().steering.session_mood.as_deref(), Some("bright"));
        let coordinator = &house.target("Lead@S1").coordinator.id.0;
        assert!(!house.events().iter().any(|event| matches!(event,
            Event::Steer(id, _) | Event::Act(id, _, _) if id == coordinator)));
    }
}

struct ChangeDuringCaps {
    lan: SimLan,
    leader: PlayerId,
    host: IpAddr,
    guest: IpAddr,
    reads: AtomicUsize,
}

impl Transport for ChangeDuringCaps {
    fn soap_post(&self, host: IpAddr, path: &str, action: &str, body: &str) -> Result<String, ProtoError> {
        let result = self.lan.soap_post(host, path, action, body);
        if host == self.host && action.trim_matches('"').ends_with("#GetVolume")
            // Readiness, the before-snapshot, then the execution's cap read.
            && self.reads.fetch_add(1, Ordering::SeqCst) == 2
        {
            soap::join_group(&self.lan, self.guest, &self.leader).unwrap();
        }
        result
    }
}

#[test]
fn a_group_expanding_during_cap_reads_gets_no_volume_or_playback_write() {
    let house = House::new();
    let leader = house.target("Lead@S1").coordinator;
    let surface = house.surface_over(Box::new(ChangeDuringCaps {
        lan: house.sim.lan(),
        leader: leader.id.clone(),
        host: leader.ip,
        guest: house.target("Guest@S1").player.ip,
        reads: AtomicUsize::new(0),
    }));
    let start = house.sim.soap_log().len();
    let result = surface.control_set(&Client::McpStdio, "dj_start", |rooms| {
        plan_dj_start_set(rooms, &request("selected"))
    }).unwrap();
    assert!(result.done.contains("Lead@S2"));
    assert!(result.notes.iter().any(|note| note.code == NoteCode::Skipped && note.detail.contains("Lead@S1")));
    for zone in ["Lead@S1", "Member@S1", "Guest@S1"] {
        let player = house.target(zone).player;
        assert_eq!(soap::get_volume(&house.sim.lan(), player.ip).unwrap(), 80);
        assert!(!house.sim.soap_log().iter().skip(start).any(|call|
            call.player == player.id.0
                && (matches!(call.action.as_str(), "SetVolume" | "Play")
                    || (zone != "Guest@S1" && call.action == "SetAVTransportURI"))),
            "only the simulated external controller may join Guest via SetAVTransportURI");
    }
}
