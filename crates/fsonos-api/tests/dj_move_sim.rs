//! A move holds the DJ in the group the music leaves while the music moves,
//! then hands it to the new coordinator; nothing to move holds nothing.
//! Exercised against `fsonos-sim` players over real loopback
//! sockets, with an engine that records what the surface tells it.

use fsonos_api::dj::{DjEngine, DjMoodsDto, DjSpeakers, DjStatusDto, DjSteer};
use fsonos_api::plan::{DjAction, plan_move, plan_play};
use fsonos_api::surface::{Surface, Survey};
use fsonos_api::{ErrorCode, Failure, MoveRequest, OutcomeDto, PlayRequest};
use fsonos_core::clock::{Clock, SystemClock};
use fsonos_core::playback::PlayerPlayback;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{MemStore, Store};
use fsonos_core::{HouseholdState, resolve_room};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

const STREAM: &str = "x-rincon-mp3radio://stream.example.invalid/moving.mp3";

fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .spawn()
        .unwrap()
}

/// The households as the Kitchen player sees them right now.
fn now(lan: &SimLan, kitchen: IpAddr) -> Vec<HouseholdState> {
    let mut st = HouseholdState::default();
    st.apply_topology(&get_zone_group_state(lan, kitchen).unwrap());
    vec![st]
}

fn coordinator(houses: &[HouseholdState], room: &str) -> PlayerId {
    resolve_room(houses, room).unwrap().coordinator.id.clone()
}

/// Records each hold and hand-over, and who led the Kitchen's group at the
/// moment of the hold (so the hold is known to come before the move).
struct Recorder {
    lan: SimLan,
    kitchen: IpAddr,
    calls: Arc<Mutex<Vec<String>>>,
}

impl DjEngine for Recorder {
    fn act(
        &self,
        _: DjSpeakers<'_>,
        _: &mut dyn Store,
        _: DjAction,
        _: &dyn Clock,
    ) -> Result<OutcomeDto, Failure> {
        Err(Failure::new(ErrorCode::NotImplemented, "not in this test"))
    }

    fn steer(
        &self,
        _: DjSpeakers<'_>,
        _: &mut dyn Store,
        _: &DjSteer,
        _: &dyn Clock,
    ) -> Result<OutcomeDto, Failure> {
        Err(Failure::new(ErrorCode::NotImplemented, "not in this test"))
    }

    fn status(
        &self,
        _: DjSpeakers<'_>,
        _: &dyn Store,
        _: Option<u32>,
        _: &dyn Clock,
    ) -> Result<DjStatusDto, Failure> {
        Err(Failure::new(ErrorCode::NotImplemented, "not in this test"))
    }

    fn moods(
        &self,
        _: Option<DjSpeakers<'_>>,
        _: &dyn Store,
        _: &dyn Clock,
    ) -> Result<DjMoodsDto, Failure> {
        Err(Failure::new(ErrorCode::NotImplemented, "not in this test"))
    }

    fn feeds(&self, _: &PlayerId) -> bool {
        true
    }

    fn on_playback(&self, _: DjSpeakers<'_>, _: &mut dyn Store, _: &PlayerPlayback, _: &dyn Clock) {
    }

    fn moving(&self, from: &PlayerId) {
        let leads = coordinator(&now(&self.lan, self.kitchen), "Kitchen");
        self.calls
            .lock()
            .unwrap()
            .push(format!("moving {} (kitchen led by {})", from.0, leads.0));
    }

    fn moved(&self, from: &PlayerId, to: &PlayerId) {
        self.calls
            .lock()
            .unwrap()
            .push(format!("moved {} -> {}", from.0, to.0));
    }
}

#[test]
fn a_move_holds_the_dj_then_hands_it_to_the_new_coordinator() {
    let sim = sim();
    let lan = sim.lan();
    let kitchen = sim.player("Kitchen").unwrap().ip;
    let calls = Arc::new(Mutex::new(Vec::new()));
    let survey: Survey = Box::new(move |t| {
        let mut st = HouseholdState::default();
        st.apply_topology(&get_zone_group_state(t, kitchen).map_err(fsonos_core::CoreError::from)?);
        Ok(vec![st])
    });
    let surface = Surface::new(
        Box::new(sim.lan()),
        survey,
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(MemStore::default()), "mcp")
    .with_dj(Box::new(Recorder {
        lan: lan.clone(),
        kitchen,
        calls: Arc::clone(&calls),
    }));
    let me = Client::LoopbackHttp;
    let before = now(&lan, kitchen);
    let (k, o) = (
        coordinator(&before, "Kitchen"),
        coordinator(&before, "Office"),
    );

    let play = PlayRequest {
        zone: "Kitchen".into(),
        source_uri: STREAM.into(),
        title: None,
    };
    surface
        .control(&me, "play", |rooms| plan_play(rooms, &play))
        .unwrap();
    assert!(calls.lock().unwrap().is_empty(), "only a move holds the DJ");

    let to_office = MoveRequest {
        zone: "Kitchen".into(),
        to: "Office".into(),
        copy: false,
    };
    surface
        .control(&me, "move_playback", |rooms| plan_move(rooms, &to_office))
        .unwrap();
    let after = now(&lan, kitchen);
    assert_eq!(coordinator(&after, "Office"), o, "the Office leads now");
    assert_eq!(
        *calls.lock().unwrap(),
        [
            format!("moving {} (kitchen led by {})", k.0, k.0),
            format!("moved {} -> {}", k.0, o.0),
        ],
        "held while the Kitchen still led, then handed to the Office"
    );

    // A move to where the music already is plans as nothing, and holds
    // nothing.
    calls.lock().unwrap().clear();
    let nowhere = MoveRequest {
        zone: "Office".into(),
        to: "Office".into(),
        copy: false,
    };
    surface
        .control(&me, "move_playback", |rooms| plan_move(rooms, &nowhere))
        .unwrap();
    assert!(
        calls.lock().unwrap().is_empty(),
        "nothing to move holds nothing"
    );
}
