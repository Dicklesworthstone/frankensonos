//! The plays the DJ starts are held to the caps of whoever asked for the
//! music (p7vq.13), through `Surface` and the daemon's live model against
//! `fsonos-sim` over real loopback sockets, with an engine that plays as the
//! real one does: on a skip, and on its own when the group's queue ran dry.
//! A capped agent's skip on a paused session comes down to its cap first,
//! and a session the agent started restarts within its caps even when the
//! owner exempted the DJ itself from them.

use fsonos_api::dj::{DjEngine, DjMoodsDto, DjSpeakers, DjStatusDto, DjSteer};
use fsonos_api::plan::{DjAction, plan_dj, plan_play};
use fsonos_api::surface::{Surface, Survey, follow};
use fsonos_api::{ErrorCode, Failure, OutcomeDto, PlayRequest, ZoneRequest};
use fsonos_core::clock::{Clock, SystemClock};
use fsonos_core::live::{Live, LiveConfig};
use fsonos_core::playback::PlayerPlayback;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{MemStore, Store};
use fsonos_proto::control::{get_transport_info, get_volume, pause, play, set_volume, stop};
use fsonos_proto::net::Lan;
use fsonos_sim::{SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const STREAM: &str = "x-rincon-mp3radio://stream.example.invalid/dj.mp3";

/// Agents are capped at 30. The owner exempts the DJ (client `dj`), and the
/// CLI is not capped.
const POLICY: &str =
    "[defaults]\nmax_volume = 30\nmax_step = 100\n\n[clients.dj]\ncapped = false\n";

/// Plays as the real DJ does: a skip plays the group, and so does a queue
/// that ran dry (its transport stopped) while it runs.
#[derive(Default)]
struct Restarting {
    running: AtomicBool,
    /// Set when it restarted a stopped group.
    restarted: Arc<AtomicBool>,
}

fn not_implemented() -> Failure {
    Failure::new(ErrorCode::NotImplemented, "not in this test")
}

/// The coordinator's address, as the DJ is handed the speakers.
fn coordinator(at: &DjSpeakers<'_>) -> Option<IpAddr> {
    at.households
        .iter()
        .find_map(|h| h.player(at.coordinator))
        .map(|p| p.ip)
}

impl DjEngine for Restarting {
    fn act(
        &self,
        at: DjSpeakers<'_>,
        _: &mut dyn Store,
        action: DjAction,
        _: &dyn Clock,
    ) -> Result<OutcomeDto, Failure> {
        match action {
            DjAction::Start => self.running.store(true, Ordering::SeqCst),
            DjAction::Stop => self.running.store(false, Ordering::SeqCst),
            DjAction::Skip => {
                let host = coordinator(&at).ok_or_else(not_implemented)?;
                play(at.transport, host)
                    .map_err(|e| Failure::new(ErrorCode::UpnpFault, e.to_string()))?;
            }
        }
        Ok(OutcomeDto {
            done: format!("the DJ: {action:?}"),
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
        self.running.load(Ordering::SeqCst)
    }

    fn on_playback(
        &self,
        at: DjSpeakers<'_>,
        _: &mut dyn Store,
        playback: &PlayerPlayback,
        _: &dyn Clock,
    ) {
        if playback.transport != Some(TransportState::Stopped)
            || self.restarted.load(Ordering::SeqCst)
        {
            return;
        }
        if let Some(host) = coordinator(&at)
            && play(at.transport, host).is_ok()
        {
            self.restarted.store(true, Ordering::SeqCst);
        }
    }
}

/// Whether `done` holds within 10 s.
fn eventually(mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

struct Rig {
    sim: fsonos_sim::SimHandle,
    surface: Arc<Surface>,
    /// The engine's [`Restarting::restarted`].
    restarted: Arc<AtomicBool>,
    kitchen: IpAddr,
    kitchen_id: PlayerId,
    live: Arc<Live>,
}

/// One S1 Kitchen, the daemon's live model, and the surface following it
/// with a [`Restarting`] DJ under [`POLICY`].
fn rig() -> Rig {
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
    let kitchen = sim.players()[0].ip;
    let kitchen_id = PlayerId(sim.players()[0].uuid.clone());
    let engine = Restarting::default();
    let restarted = Arc::clone(&engine.restarted);
    let survey: Survey = Box::new(|_| Ok(Vec::new()));
    let surface = Arc::new(
        Surface::new(
            Box::new(sim.lan()),
            survey,
            Policy::from_toml(POLICY).unwrap(),
            Box::new(SystemClock),
        )
        .with_live(&live)
        .with_action_log(Box::new(MemStore::default()), "cli")
        .with_dj(Box::new(engine)),
    );
    follow(&surface);
    Rig {
        sim,
        surface,
        restarted,
        kitchen,
        kitchen_id,
        live,
    }
}

impl Rig {
    fn dj(&self, client: &Client, action: DjAction) -> OutcomeDto {
        let tool = match action {
            DjAction::Start => "dj_start",
            DjAction::Skip => "dj_skip",
            DjAction::Stop => "dj_stop",
        };
        let zone = ZoneRequest {
            zone: "Kitchen".into(),
        };
        self.surface
            .control(client, tool, |h| plan_dj(h, &zone, action))
            .unwrap()
    }

    fn playing(&self) -> bool {
        get_transport_info(&self.sim.lan(), self.kitchen)
            .is_ok_and(|t| t.state == TransportState::Playing)
    }

    fn volume(&self) -> u8 {
        get_volume(&self.sim.lan(), self.kitchen).unwrap()
    }

    /// The queue runs dry with the room left at 80: the DJ restarts it.
    fn runs_dry_at_80(&self) {
        // The live model has heard it play, so no older Stopped is left to
        // reach the DJ after the flag is cleared.
        assert!(
            eventually(|| {
                self.live.player(&self.kitchen_id).and_then(|p| p.transport)
                    == Some(TransportState::Playing)
            }),
            "the live model never heard the room play"
        );
        set_volume(&self.sim.lan(), self.kitchen, 80).unwrap();
        self.restarted.store(false, Ordering::SeqCst);
        stop(&self.sim.lan(), self.kitchen).unwrap();
        assert!(
            eventually(|| self.restarted.load(Ordering::SeqCst) && self.playing()),
            "the DJ did not restart the stopped group"
        );
    }
}

/// The owner's music plays in Kitchen.
fn owner_plays(rig: &Rig) {
    let request = PlayRequest {
        zone: "Kitchen".into(),
        source_uri: STREAM.into(),
        title: None,
    };
    rig.surface
        .control(&Client::Cli, "play", |h| plan_play(h, &request))
        .unwrap();
    assert!(eventually(|| rig.playing()));
}

/// An agent capped at 30 starts the DJ, which the owner exempted from the
/// caps; hours later its queue runs dry with the room turned up to 80. The
/// DJ's restart keeps to the agent's 30, where it was held to the DJ's own
/// caps alone, none here. A session the owner (uncapped) started restarts
/// at the level the room was left at.
#[test]
fn a_dry_queue_restart_keeps_to_the_caps_of_whoever_started_the_dj() {
    let rig = rig();
    owner_plays(&rig);
    rig.dj(&Client::McpStdio, DjAction::Start);
    rig.runs_dry_at_80();
    assert_eq!(rig.volume(), 30, "restarted above the starter's cap");

    rig.dj(&Client::Cli, DjAction::Start);
    rig.runs_dry_at_80();
    assert_eq!(rig.volume(), 80, "neither the DJ nor the owner is capped");
    rig.sim.shutdown();
}

/// The owner's DJ session is paused in a room at 80; an agent capped at 30
/// skips. The skip starts the music, so the room comes down to 30 first.
#[test]
fn a_capped_skip_on_a_paused_session_starts_it_within_the_cap() {
    let rig = rig();
    owner_plays(&rig);
    rig.dj(&Client::Cli, DjAction::Start);
    set_volume(&rig.sim.lan(), rig.kitchen, 80).unwrap();
    pause(&rig.sim.lan(), rig.kitchen).unwrap();
    assert!(eventually(|| !rig.playing()));

    let out = rig.dj(&Client::McpStdio, DjAction::Skip);
    assert!(eventually(|| rig.playing()), "{out:?}");
    assert_eq!(rig.volume(), 30, "{out:?}");
    rig.sim.shutdown();
}
