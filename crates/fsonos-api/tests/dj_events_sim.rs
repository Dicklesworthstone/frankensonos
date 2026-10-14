//! The DJ's status goes out on the daemon's event stream as `dj.status`: a
//! DJ command publishes it, a command that changes nothing publishes
//! nothing, and a playback change the DJ picks on publishes the pick with
//! its reason. Against `fsonos-sim` over real loopback sockets, with the
//! daemon's live model and an engine that picks once the stream it waits
//! for plays.

use fsonos_api::dj::{
    DjEngine, DjMoodsDto, DjReasonDto, DjSpeakers, DjStatusDto, DjSteer, DjSteeringDto, DjWorkDto,
    SteerConstraints,
};
use fsonos_api::events::Event;
use fsonos_api::plan::{DjAction, plan_dj};
use fsonos_api::surface::{Surface, Survey, follow};
use fsonos_api::{ErrorCode, Failure, OutcomeDto, ZoneRequest};
use fsonos_core::clock::{Clock, SystemClock};
use fsonos_core::control;
use fsonos_core::live::{Live, LiveConfig};
use fsonos_core::playback::PlayerPlayback;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{MemStore, Store};
use fsonos_proto::net::Lan;
use fsonos_sim::{SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const STREAM: &str = "x-rincon-mp3radio://stream.example.invalid/dj.mp3";

/// Runs from a start; picks one work once [`STREAM`] plays in its group.
#[derive(Default)]
struct Picking {
    running: AtomicBool,
    picked: AtomicBool,
}

fn not_implemented() -> Failure {
    Failure::new(ErrorCode::NotImplemented, "not in this test")
}

impl DjEngine for Picking {
    fn act(
        &self,
        _: DjSpeakers<'_>,
        _: &mut dyn Store,
        action: DjAction,
        _: &dyn Clock,
    ) -> Result<OutcomeDto, Failure> {
        let start = action == DjAction::Start;
        let changed = self.running.swap(start, Ordering::SeqCst) != start;
        Ok(OutcomeDto {
            done: format!("the DJ {}", if start { "runs" } else { "stopped" }),
            changed,
            volume: None,
            notes: Vec::new(),
        })
    }

    fn steer(
        &self,
        _: DjSpeakers<'_>,
        _: &mut dyn Store,
        _: &DjSteer,
        _: &dyn Clock,
    ) -> Result<OutcomeDto, Failure> {
        Err(not_implemented())
    }

    fn status(
        &self,
        _: DjSpeakers<'_>,
        _: &dyn Store,
        _: Option<u32>,
        _: &dyn Clock,
    ) -> Result<DjStatusDto, Failure> {
        let pick = DjWorkDto {
            composer: "Clara Schumann".into(),
            title: "Romance".into(),
            performers: Vec::new(),
            album: None,
            movements: 1,
            movement: None,
            movement_title: None,
            minutes: None,
            reason: DjReasonDto {
                summary: "not heard lately".into(),
                factors: Vec::new(),
                relaxed: Vec::new(),
            },
        };
        Ok(DjStatusDto {
            zone: "Kitchen".into(),
            running: self.running.load(Ordering::SeqCst),
            now: None,
            next: if self.picked.load(Ordering::SeqCst) {
                vec![pick]
            } else {
                Vec::new()
            },
            steering: DjSteeringDto {
                source: "none".into(),
                mood: None,
                constraints: SteerConstraints::default(),
                added: SteerConstraints::default(),
                expires_at: None,
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
        Err(not_implemented())
    }

    fn feeds(&self, _: &PlayerId) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    fn on_playback(
        &self,
        _: DjSpeakers<'_>,
        _: &mut dyn Store,
        playback: &PlayerPlayback,
        _: &dyn Clock,
    ) {
        if playback.track_uri.as_deref() == Some(STREAM) {
            self.picked.store(true, Ordering::SeqCst);
        }
    }
}

#[test]
fn the_djs_status_streams_as_it_changes() {
    let sim = SimHousehold::builder()
        .s1([SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1)])
        .spawn()
        .unwrap();
    let lan = Arc::new(
        Lan::start()
            .unwrap()
            .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect())
            .with_ssdp_target(sim.ssdp_addr()),
    );
    let live = Arc::new(Live::start(Arc::clone(&lan), LiveConfig::new(Vec::new())));
    assert!(
        live.wait_ready(Duration::from_secs(10)),
        "{:?}",
        live.snapshot().last_error
    );
    let survey: Survey = Box::new(|_| Ok(Vec::new()));
    let surface = Arc::new(
        Surface::new(
            Box::new(sim.lan()),
            survey,
            Policy::default(),
            Box::new(SystemClock),
        )
        .with_live(&live)
        .with_action_log(Box::new(MemStore::default()), "cli")
        .with_dj(Box::new(Picking::default())),
    );
    follow(&surface);
    let me = Client::Cli;
    let bus = surface.events(&me).unwrap();
    let dj_events = |after: u64| -> Vec<Event> {
        bus.after(after)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "dj.status")
            .collect()
    };
    let start = || {
        surface
            .control(&me, "dj_start", |h| {
                plan_dj(
                    h,
                    &ZoneRequest {
                        zone: "Kitchen".into(),
                    },
                    DjAction::Start,
                )
            })
            .unwrap()
    };

    // A start publishes the status: running, nothing picked yet.
    start();
    let started = dj_events(0);
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(started[0].data["running"], true, "{started:?}");
    assert_eq!(started[0].data["zone"], "Kitchen");
    assert!(started[0].data["player"].is_string(), "{started:?}");
    let last = started[0].id;

    // Starting again changes nothing, so nothing goes out.
    start();
    assert!(dj_events(last).is_empty(), "{:?}", dj_events(last));

    // The stream plays in the DJ's group: it picks, and the pick goes out
    // with its reason.
    let kitchen = live
        .households()
        .iter()
        .flat_map(|h| &h.players)
        .find(|p| p.room_name == "Kitchen")
        .unwrap()
        .id
        .clone();
    control::play_uri(&*lan, &live.households(), &kitchen, STREAM, "").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let picked = loop {
        if let Some(e) = dj_events(last)
            .into_iter()
            .find(|e| e.data["next"][0]["title"] == "Romance")
        {
            break e;
        }
        assert!(Instant::now() < deadline, "no pick went out within 10 s");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(
        picked.data["next"][0]["reason"]["summary"],
        "not heard lately"
    );
}
