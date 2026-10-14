//! Queue ordering survives snapshot/undo round trips over real S1/S2 sockets.
//! Fault wrappers change only the chosen reply; all other requests reach the
//! simulator's stateful SOAP implementation.

use fsonos_core::actions;
use fsonos_core::announce::{Announcer, Clip};
use fsonos_core::policy::Client;
use fsonos_core::snapshot::{self, Aspect, RestoreOp, ZoneSnapshot};
use fsonos_core::store::{Action, MemStore};
use fsonos_core::{HouseholdState, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::cell::Cell;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const STREAM: &str = "x-rincon-mp3radio://example.invalid/other.mp3";
const CLIP: &str = "http://192.0.2.250:3400/media/mode-test.wav";

struct Fixture {
    sim: SimHandle,
    lan: SimLan,
    houses: Vec<HouseholdState>,
    coordinator: PlayerId,
    host: IpAddr,
}

impl Fixture {
    fn new(generation: u8) -> Self {
        let builder = SimHousehold::builder();
        let builder = if generation == 1 {
            builder.s1([SimPlayerSpec::new("Room", SimModel::Play5Gen1)])
        } else {
            builder.s2([SimPlayerSpec::new("Room", SimModel::One)])
        };
        let sim = builder.spawn().unwrap();
        let lan = sim.lan();
        let player = sim.player("Room").unwrap();
        let host = player.ip;
        let coordinator = PlayerId(player.uuid.clone());
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        for uri in [
            "x-file-cifs://example.invalid/first.flac",
            "x-file-cifs://example.invalid/second.flac",
        ] {
            soap::add_uri_to_queue(&lan, host, uri, "", false).unwrap();
        }
        soap::play_from_queue(&lan, host, &coordinator).unwrap();
        soap::seek_track(&lan, host, 2).unwrap();
        soap::seek_position(&lan, host, 37).unwrap();
        soap::play(&lan, host).unwrap();
        soap::set_volume(&lan, host, 27).unwrap();
        Self {
            sim,
            lan,
            houses: vec![house],
            coordinator,
            host,
        }
    }

    fn capture(&self, mode: PlayMode) -> ZoneSnapshot {
        set_play_mode(&self.lan, self.host, mode).unwrap();
        let snap = snapshot::capture(&self.lan, &self.houses, &self.coordinator, 1).unwrap();
        assert_eq!(snap.play_mode, Some(mode));
        snap
    }

    fn disturb(&self, mode: PlayMode) {
        soap::set_av_transport_uri(&self.lan, self.host, STREAM, "").unwrap();
        soap::play(&self.lan, self.host).unwrap();
        set_play_mode(&self.lan, self.host, mode).unwrap();
        soap::set_volume(&self.lan, self.host, 5).unwrap();
    }

    fn assert_restored(&self, mode: PlayMode, playing: bool) {
        let position = soap::get_position_info(&self.lan, self.host).unwrap();
        assert_eq!((position.track, position.position_secs), (2, Some(37)));
        assert_eq!(get_play_mode(&self.lan, self.host).unwrap(), mode);
        assert_eq!(soap::get_volume(&self.lan, self.host).unwrap(), 27);
        let state = soap::get_transport_info(&self.lan, self.host)
            .unwrap()
            .state;
        assert_eq!(state == TransportState::Playing, playing);
    }

    fn actions_since(&self, start: usize) -> Vec<String> {
        self.sim.soap_log()[start..]
            .iter()
            .map(|entry| entry.action.clone())
            .collect()
    }
}

#[test]
fn all_six_modes_round_trip_with_the_queue_before_resuming() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        for mode in PlayMode::ALL {
            let snap = f.capture(mode);
            let encoded = serde_json::to_value(&snap).unwrap();
            assert_eq!(encoded["play_mode"], mode.as_str());
            let snap: ZoneSnapshot = serde_json::from_value(encoded).unwrap();
            let other = if mode == PlayMode::Normal {
                PlayMode::Shuffle
            } else {
                PlayMode::Normal
            };
            f.disturb(other);
            let start = f.sim.soap_log().len();
            let report = snapshot::restore(&f.lan, &f.houses, &snap).unwrap();
            assert!(report.skipped.is_empty(), "{mode:?}: {report:?}");
            f.assert_restored(mode, true);
            let sent = f.actions_since(start);
            let setting = sent.iter().position(|a| a == "SetPlayMode").unwrap();
            let play = sent.iter().position(|a| a == "Play").unwrap();
            assert!(setting < play, "{sent:?}");
            assert_eq!(sent.iter().filter(|a| *a == "SetPlayMode").count(), 1);
        }
    }
}

#[test]
fn action_log_undo_restores_the_saved_queue_mode() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let snap = f.capture(PlayMode::ShuffleRepeatOne);
        let mut store = MemStore::default();
        actions::record(
            &mut store,
            &Action {
                at: 1,
                client: "cli".into(),
                surface: "test".into(),
                intent: "play other source".into(),
                decision: "allow".into(),
                result: "played".into(),
                before_state: actions::before_state(&[snap]),
                undo_of: None,
            },
        )
        .unwrap();
        f.disturb(PlayMode::Normal);
        let report = actions::undo_last(
            &f.lan,
            &f.houses,
            &mut store,
            None,
            &Client::Cli,
            "test",
            2,
        )
        .unwrap()
        .unwrap();
        assert!(report.failures.is_empty(), "{report:?}");
        assert!(report.zones.iter().all(|(_, r)| r.skipped.is_empty()));
        f.assert_restored(PlayMode::ShuffleRepeatOne, true);
    }
}

#[test]
fn paused_queues_restore_ordering_without_an_audible_play_pause_cycle() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        soap::pause(&f.lan, f.host).unwrap();
        let snap = f.capture(PlayMode::RepeatAll);
        f.disturb(PlayMode::Normal);
        let start = f.sim.soap_log().len();
        let report = snapshot::restore(&f.lan, &f.houses, &snap).unwrap();
        assert!(report.skipped.is_empty(), "{report:?}");
        f.assert_restored(PlayMode::RepeatAll, false);
        assert!(!f.actions_since(start).iter().any(|a| a == "Play"));
    }
}

#[test]
fn old_snapshots_leave_unknown_ordering_alone_instead_of_assuming_normal() {
    let f = Fixture::new(2);
    let mut encoded = serde_json::to_value(f.capture(PlayMode::RepeatOne)).unwrap();
    encoded.as_object_mut().unwrap().remove("play_mode");
    let old: ZoneSnapshot = serde_json::from_value(encoded).unwrap();
    assert_eq!(old.play_mode, None);
    assert!(serde_json::to_value(&old).unwrap().get("play_mode").is_none());
    f.disturb(PlayMode::ShuffleNoRepeat);
    let start = f.sim.soap_log().len();
    let report = snapshot::restore(&f.lan, &f.houses, &old).unwrap();
    assert!(report.skipped.is_empty(), "{report:?}");
    f.assert_restored(PlayMode::ShuffleNoRepeat, true);
    assert!(!f.actions_since(start).iter().any(|a| a == "SetPlayMode"));
}

#[test]
fn unsupported_capture_does_not_invent_a_mode_or_break_other_restoration() {
    let f = Fixture::new(1);
    f.sim.upnp_fault("Room", "GetTransportSettings", 401).unwrap();
    let snap = snapshot::capture(&f.lan, &f.houses, &f.coordinator, 1).unwrap();
    assert_eq!(snap.play_mode, None);
    f.disturb(PlayMode::Shuffle);
    let start = f.sim.soap_log().len();
    let report = snapshot::restore(&f.lan, &f.houses, &snap).unwrap();
    assert!(report.skipped.is_empty(), "{report:?}");
    assert!(!f.actions_since(start).iter().any(|a| a == "SetPlayMode"));
}

#[derive(Clone, Copy)]
enum Fault {
    Lost,
    Malformed,
    Ignored,
    Refused,
    ReadFailed,
}

struct Reply<'a> {
    inner: &'a SimLan,
    fault: Fault,
    writes: Cell<usize>,
}

impl Transport for Reply<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if action.contains("#GetTransportSettings")
            && self.writes.get() > 0
            && matches!(self.fault, Fault::ReadFailed)
        {
            return Err(ProtoError::Malformed("mode read-back unavailable".into()));
        }
        if !action.contains("#SetPlayMode") {
            return self.inner.soap_post(host, path, action, body);
        }
        self.writes.set(self.writes.get() + 1);
        if matches!(self.fault, Fault::Refused) {
            return Err(ProtoError::SoapFault {
                code: 701,
                reason: "mode refused".into(),
            });
        }
        if matches!(self.fault, Fault::Ignored) {
            return Ok(concat!(
                "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">",
                "<s:Body><u:SetPlayModeResponse xmlns:u=\"urn:x\"></u:SetPlayModeResponse>",
                "</s:Body></s:Envelope>"
            )
            .into());
        }
        let reply = self.inner.soap_post(host, path, action, body)?;
        match self.fault {
            Fault::Lost => Err(ProtoError::Network {
                target: host.to_string(),
                detail: "lost mode reply".into(),
            }),
            Fault::Malformed => Ok("<broken-reply>".into()),
            _ => Ok(reply),
        }
    }
}

#[test]
fn lost_and_malformed_mode_replies_are_confirmed_without_a_second_write() {
    for generation in [1, 2] {
        for fault in [Fault::Lost, Fault::Malformed] {
            let f = Fixture::new(generation);
            let snap = f.capture(PlayMode::RepeatOne);
            f.disturb(PlayMode::Normal);
            let reply = Reply {
                inner: &f.lan,
                fault,
                writes: Cell::new(0),
            };
            let report = snapshot::restore(&reply, &f.houses, &snap).unwrap();
            assert!(report.skipped.is_empty(), "{report:?}");
            assert_eq!(reply.writes.get(), 1);
            f.assert_restored(PlayMode::RepeatOne, true);
        }
    }
}

#[test]
fn refused_ignored_or_unverified_modes_block_resume_but_not_room_levels() {
    for generation in [1, 2] {
        for fault in [Fault::Refused, Fault::Ignored, Fault::ReadFailed] {
            let f = Fixture::new(generation);
            let snap = f.capture(PlayMode::RepeatOne);
            f.disturb(PlayMode::Normal);
            let start = f.sim.soap_log().len();
            let reply = Reply {
                inner: &f.lan,
                fault,
                writes: Cell::new(0),
            };
            let report = snapshot::restore(&reply, &f.houses, &snap).unwrap();
            assert!(report.skipped.iter().any(|(a, _)| *a == Aspect::Transport));
            assert!(!report.restored.contains(&Aspect::Transport));
            assert!(report.restored.contains(&Aspect::Volume));
            assert_eq!(soap::get_volume(&f.lan, f.host).unwrap(), 27);
            assert_eq!(reply.writes.get(), 1);
            assert!(!f.actions_since(start).iter().any(|a| a == "Play"));
        }
    }
}

#[test]
fn a_policy_veto_of_queue_restore_prevents_ordering_changes() {
    let f = Fixture::new(2);
    let snap = f.capture(PlayMode::Shuffle);
    f.disturb(PlayMode::Normal);
    let start = f.sim.soap_log().len();
    let report = snapshot::restore_vetted(&f.lan, &f.houses, &snap, &Aspect::ALL, &|op| {
        if matches!(op, RestoreOp::PlayQueue) {
            Err("queue restore denied by policy".into())
        } else {
            Ok(op.clone())
        }
    })
    .unwrap();
    assert!(
        report
            .skipped
            .iter()
            .any(|(_, why)| why.contains("queue restore denied"))
    );
    assert_eq!(soap::get_volume(&f.lan, f.host).unwrap(), 27);
    assert!(
        f.actions_since(start)
            .iter()
            .all(|a| a != "SetPlayMode" && a != "Play")
    );
}

#[test]
fn level_only_and_changed_queue_restores_never_change_ordering() {
    for changed_queue in [false, true] {
        let f = Fixture::new(2);
        let snap = f.capture(PlayMode::RepeatAll);
        f.disturb(PlayMode::Shuffle);
        if changed_queue {
            soap::add_uri_to_queue(&f.lan, f.host, STREAM, "", false).unwrap();
        }
        let aspects = if changed_queue {
            &Aspect::ALL[..]
        } else {
            &[Aspect::Volume, Aspect::Mute][..]
        };
        let start = f.sim.soap_log().len();
        let report = snapshot::restore_only(&f.lan, &f.houses, &snap, aspects).unwrap();
        assert_eq!(report.skipped.is_empty(), !changed_queue);
        assert_eq!(get_play_mode(&f.lan, f.host).unwrap(), PlayMode::Shuffle);
        assert_eq!(soap::get_volume(&f.lan, f.host).unwrap(), 27);
        assert!(
            f.actions_since(start)
                .iter()
                .all(|a| a != "SetPlayMode" && a != "Play")
        );
    }
}

/// An independent controller changes ordering just after the clip is loaded.
struct OwnerMode<'a> {
    inner: &'a SimLan,
    changed: AtomicBool,
}

impl Transport for OwnerMode<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let reply = self.inner.soap_post(host, path, action, body)?;
        if action.contains("#SetAVTransportURI")
            && body.contains(CLIP)
            && !self.changed.swap(true, Ordering::SeqCst)
        {
            set_play_mode(self.inner, host, PlayMode::RepeatOne)?;
        }
        Ok(reply)
    }
}

#[test]
fn announcement_cleanup_preserves_a_newer_mode_the_announcement_never_changed() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        f.capture(PlayMode::Shuffle);
        let owner = OwnerMode {
            inner: &f.lan,
            changed: AtomicBool::new(false),
        };
        let targets = [resolve_room(&f.houses, "Room").unwrap()];
        let clip = Clip {
            url: CLIP.into(),
            title: "Ordering test".into(),
            duration: Duration::ZERO,
        };
        let start = f.sim.soap_log().len();
        let report = Announcer::with_timing(Duration::from_millis(1), Duration::ZERO)
            .announce(&owner, &targets, &clip, 35, &|_| 100)
            .unwrap();
        assert!(owner.changed.load(Ordering::SeqCst));
        assert!(report.households[0].restore_failed.is_empty(), "{report:?}");
        assert!(
            report.households[0]
                .restored
                .iter()
                .all(|(_, r)| r.skipped.is_empty())
        );
        f.assert_restored(PlayMode::RepeatOne, true);
        assert_eq!(
            f.actions_since(start)
                .iter()
                .filter(|a| *a == "SetPlayMode")
                .count(),
            1,
            "only the owner's change, never stale mode cleanup"
        );
    }
}

/// The settings response belongs to the old queue, but an owner selects a
/// stream before that response reaches the snapshot executor.
struct SourceDuringModeRead<'a> {
    inner: &'a SimLan,
    changed: Cell<bool>,
}

impl Transport for SourceDuringModeRead<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let reply = self.inner.soap_post(host, path, action, body)?;
        if action.contains("#GetTransportSettings") && !self.changed.replace(true) {
            soap::set_av_transport_uri(self.inner, host, STREAM, "")?;
            soap::play(self.inner, host)?;
        }
        Ok(reply)
    }
}

#[test]
fn a_source_changed_during_the_mode_read_is_not_sought_or_resumed_as_the_queue() {
    for current in [PlayMode::Normal, PlayMode::RepeatOne] {
        let f = Fixture::new(2);
        let snap = f.capture(PlayMode::RepeatOne);
        f.disturb(current);
        let reply = SourceDuringModeRead {
            inner: &f.lan,
            changed: Cell::new(false),
        };
        let start = f.sim.soap_log().len();
        let report = snapshot::restore(&reply, &f.houses, &snap).unwrap();
        assert!(reply.changed.get());
        assert!(report.skipped.iter().any(|(a, _)| *a == Aspect::Source));
        assert_eq!(soap::get_media_info(&f.lan, f.host).unwrap().uri, STREAM);
        let sent = f.actions_since(start);
        assert!(sent.iter().all(|a| a != "SetPlayMode" && a != "Seek"));
        assert_eq!(
            sent.iter().filter(|a| *a == "Play").count(),
            1,
            "only the owner"
        );
    }
}

#[test]
fn source_only_restoration_recovers_ordering_without_starting_playback() {
    let f = Fixture::new(2);
    let snap = f.capture(PlayMode::ShuffleNoRepeat);
    f.disturb(PlayMode::RepeatAll);
    let start = f.sim.soap_log().len();
    let report = snapshot::restore_only(&f.lan, &f.houses, &snap, &[Aspect::Source]).unwrap();
    assert!(report.skipped.is_empty(), "{report:?}");
    assert_eq!(report.restored, [Aspect::Source]);
    assert_eq!(
        get_play_mode(&f.lan, f.host).unwrap(),
        PlayMode::ShuffleNoRepeat
    );
    assert!(!f.actions_since(start).iter().any(|a| a == "Play"));
}
