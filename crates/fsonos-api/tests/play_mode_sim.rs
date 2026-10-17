//! Shuffle/repeat through the shared surface and real virtual-player sockets.
//! Mode changes must neither restart playback nor edit the queue contents.

use fsonos_api::surface::queue::PlayModeRequest;
use fsonos_api::{ErrorCode, Surface};
use fsonos_core::HouseholdState;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_proto::control as soap;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayMode, PlayerId};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const RADIO: &str = "x-rincon-mp3radio://example.invalid/other.mp3";

#[derive(Clone, Copy)]
enum Fault {
    None,
    LostReply,
    MalformedReply,
    FailedReadBack,
    SourceChanged,
    ModeChangedBeforeWrite,
}

struct Wired {
    lan: SimLan,
    fault: Fault,
    writes: Arc<AtomicUsize>,
    reads: AtomicUsize,
}

impl Transport for Wired {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let setting = action.ends_with("#SetPlayMode\"");
        let reading = action.ends_with("#GetTransportSettings\"");
        if reading
            && matches!(self.fault, Fault::FailedReadBack)
            && self.writes.load(Ordering::Acquire) > 0
        {
            return Err(ProtoError::Network {
                target: "virtual mode reader".into(),
                detail: "injected read-back failure".into(),
            });
        }
        if setting {
            self.writes.fetch_add(1, Ordering::AcqRel);
        }
        let response = self.lan.soap_post(host, path, action, body)?;
        if reading
            && self.reads.fetch_add(1, Ordering::AcqRel) == 1
            && matches!(self.fault, Fault::ModeChangedBeforeWrite)
        {
            // An independent controller changes the mode during undo capture.
            set_play_mode(&self.lan, host, PlayMode::RepeatOne)?;
        }
        if setting {
            match self.fault {
                Fault::LostReply => {
                    return Err(ProtoError::Network {
                        target: "virtual mode writer".into(),
                        detail: "the successful reply was lost".into(),
                    });
                }
                Fault::MalformedReply => return Ok("not a SOAP response".into()),
                Fault::SourceChanged => soap::set_av_transport_uri(&self.lan, host, RADIO, "")?,
                _ => {}
            }
        }
        Ok(response)
    }
}

fn setup(generation: u8, fault: Fault) -> (SimHandle, Surface, Arc<AtomicUsize>) {
    let model = if generation == 1 {
        SimModel::Play5Gen1
    } else {
        SimModel::One
    };
    let players = [
        SimPlayerSpec::new("Alpha", model),
        SimPlayerSpec::new("Beta", model),
    ];
    let builder = SimHousehold::builder();
    let sim = if generation == 1 {
        builder.s1(players)
    } else {
        builder.s2(players)
    }
    .spawn()
    .unwrap();
    let lan = sim.lan();
    let alpha = sim.player("Alpha").unwrap();
    let coordinator = PlayerId(alpha.uuid.clone());
    let host = alpha.ip;
    for name in ["first", "second", "third"] {
        soap::enqueue_uri(
            &lan,
            host,
            &format!("x-file-cifs://example.invalid/{name}.flac"),
            "",
            0,
            false,
        )
        .unwrap();
    }
    soap::set_av_transport_uri(
        &lan,
        sim.player("Beta").unwrap().ip,
        &format!("x-rincon:{}", coordinator.0),
        "",
    )
    .unwrap();
    soap::play_from_queue(&lan, host, &coordinator).unwrap();
    soap::seek_track(&lan, host, 2).unwrap();
    soap::play(&lan, host).unwrap();
    sim.advance(Duration::from_secs(17));
    let mut household = HouseholdState::default();
    household.apply_topology(&get_zone_group_state(&lan, host).unwrap());
    let writes = Arc::new(AtomicUsize::new(0));
    let surface = Surface::new(
        Box::new(Wired {
            lan,
            fault,
            writes: Arc::clone(&writes),
            reads: AtomicUsize::new(0),
        }),
        Box::new(move |_| Ok(vec![household.clone()])),
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(MemStore::default()), "test");
    (sim, surface, writes)
}

fn ask(wanted: PlayMode) -> PlayModeRequest {
    PlayModeRequest {
        zone: "Beta".into(),
        mode: wanted.as_str().into(),
        expected_mode: None,
    }
}

#[test]
fn all_six_modes_preserve_track_time_queue_and_levels_on_both_generations() {
    for generation in [1, 2] {
        let (sim, surface, writes) = setup(generation, Fault::None);
        let lan = sim.lan();
        let host = sim.player("Alpha").unwrap().ip;
        let position = soap::get_position_info(&lan, host).unwrap();
        let source = soap::get_media_info(&lan, host).unwrap();
        let queue = fsonos_proto::content::browse_snapshot(&lan, host, "Q:0").unwrap();
        for wanted in PlayMode::ALL {
            let previous = if wanted == PlayMode::Normal {
                PlayMode::RepeatAll
            } else {
                PlayMode::Normal
            };
            set_play_mode(&lan, host, previous).unwrap();
            let start = sim.soap_log().len();
            let before_writes = writes.load(Ordering::Acquire);
            let outcome = surface.set_play_mode(&Client::Cli, &ask(wanted)).unwrap();
            assert!(outcome.changed);
            assert!(!outcome.recovered);
            assert_eq!(outcome.state.mode, wanted.as_str());
            assert_eq!(outcome.state.zone, "Alpha");
            assert_eq!(writes.load(Ordering::Acquire), before_writes + 1);
            assert_eq!(soap::get_position_info(&lan, host).unwrap(), position);
            assert_eq!(soap::get_media_info(&lan, host).unwrap(), source);
            assert_eq!(
                fsonos_proto::content::browse_snapshot(&lan, host, "Q:0").unwrap(),
                queue
            );
            assert!(
                sim.soap_log()[start..]
                    .iter()
                    .all(|call| call.action.starts_with("Get")
                        || call.action == "Browse"
                        || call.action == "SetPlayMode")
            );
        }
        sim.shutdown();
    }
}

#[test]
fn a_paused_queue_stays_paused_and_reading_is_allowed_for_unknown_clients() {
    let (sim, surface, _) = setup(2, Fault::None);
    let host = sim.player("Alpha").unwrap().ip;
    soap::pause(&sim.lan(), host).unwrap();
    let before = soap::get_transport_info(&sim.lan(), host).unwrap().state;
    surface
        .set_play_mode(&Client::Cli, &ask(PlayMode::ShuffleNoRepeat))
        .unwrap();
    assert_eq!(
        soap::get_transport_info(&sim.lan(), host).unwrap().state,
        before
    );
    let state = surface.get_play_mode(&Client::Unknown, "Beta").unwrap();
    assert!(state.queue_active);
    assert_eq!(state.mode, "SHUFFLE_NOREPEAT");
    sim.shutdown();
}

#[test]
fn a_noop_creates_no_write_and_no_undo_target() {
    let (sim, surface, writes) = setup(1, Fault::None);
    let outcome = surface
        .set_play_mode(&Client::Cli, &ask(PlayMode::Normal))
        .unwrap();
    assert!(!outcome.changed);
    assert_eq!(writes.load(Ordering::Acquire), 0);
    assert!(surface.undo(&Client::Cli, false).unwrap().is_none());
    sim.shutdown();
}

#[test]
fn stale_expected_modes_and_nonqueue_sources_never_change_playback() {
    let (sim, surface, writes) = setup(2, Fault::None);
    let mut req = ask(PlayMode::Shuffle);
    req.expected_mode = Some("REPEAT_ONE".into());
    assert_eq!(
        surface.set_play_mode(&Client::Cli, &req).unwrap_err().code,
        ErrorCode::InvalidArgument
    );
    let host = sim.player("Alpha").unwrap().ip;
    soap::set_av_transport_uri(&sim.lan(), host, RADIO, "").unwrap();
    assert!(
        !surface
            .get_play_mode(&Client::Cli, "Beta")
            .unwrap()
            .queue_active
    );
    assert!(
        surface
            .set_play_mode(&Client::Cli, &ask(PlayMode::Shuffle))
            .is_err()
    );
    assert_eq!(writes.load(Ordering::Acquire), 0);
    assert_eq!(soap::get_media_info(&sim.lan(), host).unwrap().uri, RADIO);
    assert!(surface.undo(&Client::Cli, false).unwrap().is_none());
    // Each refusal is logged as every failed call is: the policy allowed
    // it, and the result says it failed.
    let log = surface
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    let refused: Vec<_> = log
        .iter()
        .filter(|e| e.action.intent.starts_with("set_play_mode"))
        .collect();
    assert!(!refused.is_empty(), "{log:?}");
    for entry in refused {
        assert_eq!(entry.action.decision, "allow", "{entry:?}");
        assert!(entry.action.result.starts_with("failed: "), "{entry:?}");
    }
    sim.shutdown();
}

#[test]
fn mode_mutations_can_be_explicitly_undone_through_the_action_log() {
    for generation in [1, 2] {
        let (sim, surface, _) = setup(generation, Fault::None);
        let host = sim.player("Alpha").unwrap().ip;
        let mut req = ask(PlayMode::ShuffleRepeatOne);
        req.expected_mode = Some("NORMAL".into());
        surface.set_play_mode(&Client::Cli, &req).unwrap();
        let action = surface
            .recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap();
        assert!(action[0].action.before_state.is_some());
        let report = surface.undo(&Client::Cli, false).unwrap().unwrap();
        assert!(report.failures.is_empty(), "{report:?}");
        assert_eq!(get_play_mode(&sim.lan(), host).unwrap(), PlayMode::Normal);
        sim.shutdown();
    }
}

#[test]
fn successful_writes_with_lost_or_malformed_replies_are_confirmed_without_replay() {
    for fault in [Fault::LostReply, Fault::MalformedReply] {
        let (sim, surface, writes) = setup(2, fault);
        let outcome = surface
            .set_play_mode(&Client::Cli, &ask(PlayMode::RepeatAll))
            .unwrap();
        assert!(outcome.changed && outcome.recovered);
        assert_eq!(outcome.state.mode, "REPEAT_ALL");
        assert_eq!(writes.load(Ordering::Acquire), 1);
        sim.shutdown();
    }
}

#[test]
fn failed_confirmation_and_new_sources_leave_one_uncertain_write() {
    for fault in [Fault::FailedReadBack, Fault::SourceChanged] {
        let (sim, surface, writes) = setup(2, fault);
        let error = surface
            .set_play_mode(&Client::Cli, &ask(PlayMode::RepeatAll))
            .unwrap_err();
        assert!(!error.retryable());
        assert!(error.detail.contains("uncertain"));
        assert_eq!(writes.load(Ordering::Acquire), 1);
        sim.shutdown();
    }
}

#[test]
fn ignored_and_refused_writes_are_not_reported_as_success_or_retried() {
    for ignored in [false, true] {
        let (sim, surface, writes) = setup(1, Fault::None);
        if ignored {
            sim.ignore_action("Alpha", "SetPlayMode").unwrap();
        } else {
            sim.upnp_fault("Alpha", "SetPlayMode", 401).unwrap();
        }
        let failure = surface
            .set_play_mode(&Client::Cli, &ask(PlayMode::RepeatOne))
            .unwrap_err();
        assert!(!failure.retryable());
        assert_eq!(writes.load(Ordering::Acquire), 1);
        assert_eq!(
            get_play_mode(&sim.lan(), sim.player("Alpha").unwrap().ip).unwrap(),
            PlayMode::Normal
        );
        sim.shutdown();
    }
}

#[test]
fn a_mode_changed_during_capture_is_not_overwritten() {
    let (sim, surface, writes) = setup(2, Fault::ModeChangedBeforeWrite);
    assert!(
        surface
            .set_play_mode(&Client::Cli, &ask(PlayMode::Shuffle))
            .is_err()
    );
    assert_eq!(writes.load(Ordering::Acquire), 0);
    assert!(surface.undo(&Client::Cli, false).unwrap().is_none());
    assert_eq!(
        get_play_mode(&sim.lan(), sim.player("Alpha").unwrap().ip).unwrap(),
        PlayMode::RepeatOne
    );
    sim.shutdown();
}

#[test]
fn fresh_topology_targets_the_new_coordinator_not_the_cached_one() {
    let (sim, surface, writes) = setup(2, Fault::None);
    sim.reelect_coordinator("Alpha").unwrap();
    let mut req = ask(PlayMode::RepeatOne);
    req.zone = "Alpha".into();
    let result = surface.set_play_mode(&Client::Cli, &req).unwrap();
    assert_eq!(result.state.zone, "Beta");
    assert_eq!(writes.load(Ordering::Acquire), 1);
    let log = sim.soap_log();
    let last_write = log
        .iter()
        .rev()
        .find(|entry| entry.action == "SetPlayMode")
        .unwrap();
    assert_eq!(last_write.room, "Beta");
    sim.shutdown();
}

#[test]
fn unknown_clients_and_invalid_modes_never_reach_the_mutation() {
    let (sim, surface, writes) = setup(1, Fault::None);
    let calls = sim.soap_log().len();
    assert_eq!(
        surface
            .set_play_mode(&Client::Unknown, &ask(PlayMode::Shuffle))
            .unwrap_err()
            .code,
        ErrorCode::PolicyDenied
    );
    for value in ["", "NOT_IMPLEMENTED", "FUTURE_MODE"] {
        let req = PlayModeRequest {
            mode: value.into(),
            ..ask(PlayMode::Normal)
        };
        assert!(surface.set_play_mode(&Client::Cli, &req).is_err());
    }
    assert_eq!(sim.soap_log().len(), calls);
    assert_eq!(writes.load(Ordering::Acquire), 0);
    sim.shutdown();
}
