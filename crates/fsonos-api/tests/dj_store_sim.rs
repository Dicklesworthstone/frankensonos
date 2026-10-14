//! A DJ command that takes long (a start waiting on Spotify, a top-up
//! talking to the speakers) never holds up the daemon's other calls on the
//! store: the engine gets the store locked per call. Against `fsonos-sim`
//! over real loopback sockets, with an engine that takes its time.

use fsonos_api::dj::{DjEngine, DjMoodsDto, DjSpeakers, DjStatusDto, DjSteer};
use fsonos_api::plan::{DjAction, plan_dj};
use fsonos_api::surface::{Surface, Survey};
use fsonos_api::{ErrorCode, Failure, OutcomeDto, ZoneRequest};
use fsonos_core::clock::{Clock, SystemClock};
use fsonos_core::playback::PlayerPlayback;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{MemStore, Store};
use fsonos_sim::SimHousehold;
use fsonos_types::PlayerId;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const PLAYED: &str = "spotify:track:0SimSlowDjTrack00000001";

fn not_implemented() -> Failure {
    Failure::new(ErrorCode::NotImplemented, "not in this test")
}

/// Starts in two seconds, as a DJ waiting on Spotify might, using the
/// store before and after; `waiting` once it waits.
struct Slow {
    waiting: Arc<AtomicBool>,
}

impl DjEngine for Slow {
    fn act(
        &self,
        at: DjSpeakers<'_>,
        store: &mut dyn Store,
        _: DjAction,
        _: &dyn Clock,
    ) -> Result<OutcomeDto, Failure> {
        let backend =
            |e: fsonos_core::store::StoreError| Failure::new(ErrorCode::Internal, e.to_string());
        store.recent_plays(None, 1).map_err(backend)?;
        self.waiting.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_secs(2));
        store
            .record_play(&at.coordinator.0, PLAYED, 1)
            .map_err(backend)?;
        Ok(OutcomeDto {
            done: "the DJ runs".into(),
            changed: true,
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
        Err(not_implemented())
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
        false
    }

    fn on_playback(&self, _: DjSpeakers<'_>, _: &mut dyn Store, _: &PlayerPlayback, _: &dyn Clock) {
    }
}

#[test]
fn a_slow_dj_start_holds_up_no_other_call() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let survey: Survey = Box::new(|t| {
        Ok(fsonos_core::inventory::survey(t, &[], Duration::from_millis(500))?.households)
    });
    let waiting = Arc::new(AtomicBool::new(false));
    let surface = Arc::new(
        Surface::new(
            Box::new(sim.lan()),
            survey,
            Policy::default(),
            Box::new(SystemClock),
        )
        .with_action_log(Box::new(MemStore::default()), "cli")
        .with_dj(Box::new(Slow {
            waiting: Arc::clone(&waiting),
        })),
    );
    let starting = Arc::clone(&surface);
    let start = std::thread::spawn(move || {
        starting.control(&Client::Cli, "dj_start", |h| {
            plan_dj(
                h,
                &ZoneRequest {
                    zone: "Kitchen".into(),
                },
                DjAction::Start,
            )
        })
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !waiting.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(waiting.load(Ordering::SeqCst), "the DJ never started");

    // While the DJ waits, the history answers at once.
    let asked = Instant::now();
    surface.recent_plays(&Client::Cli, None, 10).unwrap();
    assert!(
        asked.elapsed() < Duration::from_secs(1),
        "{:?}",
        asked.elapsed()
    );

    // And the DJ's own store calls still land.
    start.join().unwrap().unwrap();
    let plays = surface.recent_plays(&Client::Cli, None, 10).unwrap();
    assert!(plays.iter().any(|p| p.source_uri == PLAYED), "{plays:?}");
}
