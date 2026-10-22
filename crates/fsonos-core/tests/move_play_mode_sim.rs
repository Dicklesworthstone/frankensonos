//! Replay relocations preserve queue order settings through the real S1/S2
//! SOAP simulator. Fault wrappers change replies or inject an owner action.

use fsonos_core::moving::{MoveError, MoveMethod, MoveReport, copy_playback, move_playback_within};
use fsonos_core::{HouseholdState, resolve_room};
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::cell::Cell;
use std::net::IpAddr;
use std::time::Duration;

const OTHER: &str = "x-rincon-mp3radio://example.invalid/new-radio";
const PAIRS: [(&str, &str); 2] = [("S1 Source", "S2 Target"), ("S2 Source", "S1 Target")];

struct Fixture {
    sim: SimHandle,
    from: String,
    to: String,
    houses: Vec<HouseholdState>,
    source: IpAddr,
    target: IpAddr,
}

impl Fixture {
    fn new(from: &str, to: &str, mode: PlayMode) -> Self {
        let sim = SimHousehold::builder()
            .s1([
                SimPlayerSpec::new("S1 Source", SimModel::Play5Gen1),
                SimPlayerSpec::new("S1 Target", SimModel::Play5Gen1),
            ])
            .s2([
                SimPlayerSpec::new("S2 Source", SimModel::One),
                SimPlayerSpec::new("S2 Target", SimModel::One),
            ])
            .spawn()
            .unwrap();
        let source = sim.player(from).unwrap().ip;
        let target = sim.player(to).unwrap().ip;
        let lan = sim.lan();
        let houses = ["S1 Source", "S2 Source"]
            .iter()
            .map(|room| {
                let mut h = HouseholdState::default();
                h.apply_topology(
                    &get_zone_group_state(&lan, sim.player(room).unwrap().ip).unwrap(),
                );
                h
            })
            .collect();
        for n in 1..=3 {
            let uri = format!("x-file-cifs://example.invalid/track{n}.flac");
            soap::add_uri_to_queue(&lan, source, &uri, "", false).unwrap();
        }
        let id = PlayerId(sim.player(from).unwrap().uuid.clone());
        soap::play_from_queue(&lan, source, &id).unwrap();
        soap::seek_track(&lan, source, 2).unwrap();
        soap::seek_position(&lan, source, 37).unwrap();
        soap::play(&lan, source).unwrap();
        set_play_mode(&lan, source, mode).unwrap();
        let other = if mode == PlayMode::Normal {
            PlayMode::Shuffle
        } else {
            PlayMode::Normal
        };
        set_play_mode(&lan, target, other).unwrap();
        Self {
            sim,
            from: from.into(),
            to: to.into(),
            houses,
            source,
            target,
        }
    }

    fn copy<T: Transport>(&self, t: &T) -> Result<MoveReport, MoveError> {
        let from = resolve_room(&self.houses, &self.from).unwrap();
        let to = resolve_room(&self.houses, &self.to).unwrap();
        copy_playback(t, &self.houses, &from, &to)
    }

    fn source_kept(&self, since: usize) {
        assert_eq!(
            soap::get_transport_info(&self.sim.lan(), self.source)
                .unwrap()
                .state,
            TransportState::Playing
        );
        assert!(
            self.sim.soap_log()[since..]
                .iter()
                .all(|e| e.room != self.from || e.action != "Stop")
        );
    }

    fn target_actions(&self, since: usize) -> Vec<String> {
        self.sim.soap_log()[since..]
            .iter()
            .filter(|e| e.room == self.to)
            .map(|e| e.action.clone())
            .collect()
    }

    fn copied(&self, mode: PlayMode) {
        let lan = self.sim.lan();
        assert_eq!(get_play_mode(&lan, self.target).unwrap(), mode);
        let position = soap::get_position_info(&lan, self.target).unwrap();
        assert_eq!((position.track, position.position_secs), (2, Some(37)));
        let queue = browse_snapshot(&lan, self.target, QUEUE).unwrap();
        let uris: Vec<_> = queue
            .objects
            .iter()
            .map(|o| o.res.as_ref().unwrap().uri.as_str())
            .collect();
        assert_eq!(
            uris,
            [
                "x-file-cifs://example.invalid/track1.flac",
                "x-file-cifs://example.invalid/track2.flac",
                "x-file-cifs://example.invalid/track3.flac"
            ]
        );
    }
}

#[derive(Clone, Copy)]
enum Fault {
    Lost,
    Malformed,
    NotSent,
    Unreadable,
    SwitchSource,
    EditQueue,
    ChangeModeAfterPlay,
    ChangeSourceMode,
    SwitchSourceAfterPlay,
    PauseSourceAfterPlay,
    EditSourceQueueAfterPlay,
    ChangeSourceModeAfterPlay,
}

struct Replies {
    inner: SimLan,
    source: IpAddr,
    target: IpAddr,
    fault: Fault,
    triggered: Cell<bool>,
    settings: Cell<usize>,
    source_settings: Cell<usize>,
    mode_writes: Cell<usize>,
}

impl Replies {
    fn new(f: &Fixture, fault: Fault) -> Self {
        Self {
            inner: f.sim.lan(),
            source: f.source,
            target: f.target,
            fault,
            triggered: Cell::new(false),
            settings: Cell::new(0),
            source_settings: Cell::new(0),
            mode_writes: Cell::new(0),
        }
    }

    fn lost(host: IpAddr) -> ProtoError {
        ProtoError::Network {
            target: host.to_string(),
            detail: "injected missing reply".into(),
        }
    }
}

impl Transport for Replies {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if host == self.source
            && name == "GetTransportSettings"
            && matches!(self.fault, Fault::ChangeSourceMode)
        {
            self.source_settings.set(self.source_settings.get() + 1);
            // Snapshot capture brackets playback with two settings reads.
            // Change only when Prepared::confirm_source begins its own proof,
            // so the destination must remain untouched.
            if self.source_settings.get() == 3 {
                let result = self.inner.soap_post(host, path, action, body)?;
                set_play_mode(&self.inner, host, PlayMode::RepeatAll)?;
                return Ok(result);
            }
        }
        if host != self.target {
            return self.inner.soap_post(host, path, action, body);
        }
        if name == "GetTransportSettings" {
            self.settings.set(self.settings.get() + 1);
            if matches!(self.fault, Fault::Unreadable) && self.triggered.get() {
                return Err(Self::lost(host));
            }
            // The first read is nonmutating preflight. The second follows the
            // copied queue install, just before deciding whether to set mode.
            if self.settings.get() == 2 {
                let result = self.inner.soap_post(host, path, action, body)?;
                match self.fault {
                    Fault::SwitchSource => {
                        soap::set_av_transport_uri(&self.inner, host, OTHER, "")?;
                    }
                    Fault::EditQueue => {
                        soap::add_uri_to_queue(
                            &self.inner,
                            host,
                            "x-file-cifs://example.invalid/owner.flac",
                            "",
                            false,
                        )?;
                    }
                    _ => {}
                }
                return Ok(result);
            }
        }
        if name == "SetPlayMode" {
            self.mode_writes.set(self.mode_writes.get() + 1);
            self.triggered.set(true);
            if matches!(self.fault, Fault::NotSent) {
                return Err(Self::lost(host));
            }
            let result = self.inner.soap_post(host, path, action, body)?;
            return match self.fault {
                Fault::Lost | Fault::Unreadable => Err(Self::lost(host)),
                Fault::Malformed => Ok("<broken-reply>".into()),
                _ => Ok(result),
            };
        }
        let result = self.inner.soap_post(host, path, action, body)?;
        if name == "Play" {
            match self.fault {
                Fault::ChangeModeAfterPlay => {
                    set_play_mode(&self.inner, host, PlayMode::RepeatAll)?;
                }
                Fault::SwitchSourceAfterPlay => {
                    soap::set_av_transport_uri(&self.inner, self.source, OTHER, "")?;
                }
                Fault::PauseSourceAfterPlay => {
                    soap::pause(&self.inner, self.source)?;
                }
                Fault::EditSourceQueueAfterPlay => {
                    soap::add_uri_to_queue(
                        &self.inner,
                        self.source,
                        "x-file-cifs://example.invalid/owner-added.flac",
                        "",
                        false,
                    )?;
                }
                Fault::ChangeSourceModeAfterPlay => {
                    set_play_mode(&self.inner, self.source, PlayMode::RepeatAll)?;
                }
                _ => {}
            }
        }
        Ok(result)
    }
}

#[test]
fn all_modes_follow_copies_in_both_directions_before_seek_and_play() {
    for (from, to) in PAIRS {
        for mode in PlayMode::ALL {
            let f = Fixture::new(from, to, mode);
            let since = f.sim.soap_log().len();
            assert_eq!(f.copy(&f.sim.lan()).unwrap().method, MoveMethod::Copied);
            f.copied(mode);
            let sent = f.target_actions(since);
            let setting = sent.iter().position(|a| a == "SetPlayMode").unwrap();
            let seek = sent.iter().position(|a| a == "Seek").unwrap();
            let play = sent.iter().position(|a| a == "Play").unwrap();
            assert!(setting < seek && seek < play, "{sent:?}");
            assert_eq!(sent.iter().filter(|a| *a == "SetPlayMode").count(), 1);
            assert_eq!(
                soap::get_transport_info(&f.sim.lan(), f.source)
                    .unwrap()
                    .state,
                TransportState::Stopped
            );
        }
    }
}

#[test]
fn paused_copies_preserve_repeat_without_an_audible_play_pause_cycle() {
    for (from, to) in PAIRS {
        let f = Fixture::new(from, to, PlayMode::RepeatOne);
        soap::pause(&f.sim.lan(), f.source).unwrap();
        let since = f.sim.soap_log().len();
        f.copy(&f.sim.lan()).unwrap();
        f.copied(PlayMode::RepeatOne);
        assert!(
            f.target_actions(since)
                .iter()
                .all(|a| a != "Play" && a != "Pause")
        );
    }
}

#[test]
fn unsupported_delegation_uses_the_same_mode_preserving_replay() {
    for (from, to) in [("S1 Source", "S1 Target"), ("S2 Source", "S2 Target")] {
        let f = Fixture::new(from, to, PlayMode::ShuffleNoRepeat);
        f.sim
            .upnp_fault(from, "DelegateGroupCoordinationTo", 401)
            .unwrap();
        let source = resolve_room(&f.houses, from).unwrap();
        let target = resolve_room(&f.houses, to).unwrap();
        let report =
            move_playback_within(&f.sim.lan(), &f.houses, &source, &target, Duration::ZERO)
                .unwrap();
        assert_eq!(report.method, MoveMethod::Copied);
        f.copied(PlayMode::ShuffleNoRepeat);
    }
}

#[test]
fn lost_or_malformed_mode_replies_are_confirmed_without_repetition() {
    for (from, to) in PAIRS {
        for fault in [Fault::Lost, Fault::Malformed] {
            let f = Fixture::new(from, to, PlayMode::ShuffleRepeatOne);
            let t = Replies::new(&f, fault);
            f.copy(&t).unwrap();
            assert_eq!(t.mode_writes.get(), 1);
            f.copied(PlayMode::ShuffleRepeatOne);
        }
    }
}

#[test]
fn unconfirmed_settings_never_seek_play_or_stop_the_source() {
    for (from, to) in PAIRS {
        for fault in [Fault::NotSent, Fault::Unreadable] {
            let f = Fixture::new(from, to, PlayMode::Shuffle);
            let t = Replies::new(&f, fault);
            let since = f.sim.soap_log().len();
            assert!(f.copy(&t).is_err());
            assert_eq!(t.mode_writes.get(), 1);
            f.source_kept(since);
            assert!(
                f.target_actions(since)
                    .iter()
                    .all(|a| a != "Seek" && a != "Play")
            );
        }
    }
}

#[test]
fn refused_and_ignored_mode_writes_do_not_release_the_source() {
    for (from, to) in PAIRS {
        for ignored in [false, true] {
            let f = Fixture::new(from, to, PlayMode::Shuffle);
            if ignored {
                f.sim.ignore_action(to, "SetPlayMode").unwrap();
            } else {
                f.sim.upnp_fault(to, "SetPlayMode", 402).unwrap();
            }
            let since = f.sim.soap_log().len();
            assert!(f.copy(&f.sim.lan()).is_err());
            f.source_kept(since);
            let sent = f.target_actions(since);
            assert!(sent.iter().all(|a| a != "Seek" && a != "Play"));
            assert_eq!(sent.iter().filter(|a| *a == "SetPlayMode").count(), 1);
        }
    }
}

#[test]
fn source_or_queue_changes_during_mode_reads_block_the_setting() {
    for (from, to) in PAIRS {
        for fault in [Fault::SwitchSource, Fault::EditQueue] {
            let f = Fixture::new(from, to, PlayMode::Shuffle);
            let t = Replies::new(&f, fault);
            let since = f.sim.soap_log().len();
            assert!(f.copy(&t).is_err());
            assert_eq!(t.mode_writes.get(), 0);
            f.source_kept(since);
            assert!(
                f.target_actions(since)
                    .iter()
                    .all(|a| a != "Seek" && a != "Play")
            );
        }
    }
}

#[test]
fn unreadable_destination_settings_fail_before_any_destination_mutation() {
    for (from, to) in PAIRS {
        let f = Fixture::new(from, to, PlayMode::RepeatAll);
        f.sim.upnp_fault(to, "GetTransportSettings", 401).unwrap();
        let since = f.sim.soap_log().len();
        assert!(f.copy(&f.sim.lan()).is_err());
        assert!(
            f.target_actions(since)
                .iter()
                .all(|a| a.starts_with("Get") || a == "Browse")
        );
        f.source_kept(since);
    }
}

#[test]
fn an_unavailable_source_setting_does_not_invent_normal_on_the_destination() {
    for (from, to) in PAIRS {
        let f = Fixture::new(from, to, PlayMode::Normal);
        f.sim.upnp_fault(from, "GetTransportSettings", 401).unwrap();
        let since = f.sim.soap_log().len();
        f.copy(&f.sim.lan()).unwrap();
        f.copied(PlayMode::Shuffle);
        assert!(f.target_actions(since).iter().all(|a| a != "SetPlayMode"));
    }
}

#[test]
fn direct_streams_do_not_read_or_change_queue_ordering() {
    for (from, to) in PAIRS {
        let f = Fixture::new(from, to, PlayMode::Normal);
        let lan = f.sim.lan();
        soap::set_av_transport_uri(&lan, f.source, OTHER, "").unwrap();
        soap::play(&lan, f.source).unwrap();
        f.sim.upnp_fault(to, "GetTransportSettings", 401).unwrap();
        let since = f.sim.soap_log().len();
        f.copy(&lan).unwrap();
        assert_eq!(soap::get_media_info(&lan, f.target).unwrap().uri, OTHER);
        assert!(
            f.target_actions(since)
                .iter()
                .all(|a| a != "SetPlayMode" && a != "GetTransportSettings")
        );
    }
}

#[test]
fn an_already_matching_mode_is_not_written_again() {
    for (from, to) in PAIRS {
        let f = Fixture::new(from, to, PlayMode::Shuffle);
        set_play_mode(&f.sim.lan(), f.target, PlayMode::Shuffle).unwrap();
        let since = f.sim.soap_log().len();
        f.copy(&f.sim.lan()).unwrap();
        f.copied(PlayMode::Shuffle);
        assert!(f.target_actions(since).iter().all(|a| a != "SetPlayMode"));
    }
}

#[test]
fn the_final_mode_check_does_not_release_the_source_after_an_owner_change() {
    for (from, to) in PAIRS {
        let f = Fixture::new(from, to, PlayMode::Shuffle);
        let t = Replies::new(&f, Fault::ChangeModeAfterPlay);
        let since = f.sim.soap_log().len();
        assert!(f.copy(&t).is_err());
        assert_eq!(t.mode_writes.get(), 1);
        assert_eq!(
            get_play_mode(&f.sim.lan(), f.target).unwrap(),
            PlayMode::RepeatAll
        );
        f.source_kept(since);
    }
}

#[test]
fn a_source_mode_changed_during_preparation_leaves_the_destination_untouched() {
    for (from, to) in PAIRS {
        let f = Fixture::new(from, to, PlayMode::Shuffle);
        let t = Replies::new(&f, Fault::ChangeSourceMode);
        let since = f.sim.soap_log().len();
        assert!(f.copy(&t).is_err());
        assert!(
            f.target_actions(since)
                .iter()
                .all(|a| a.starts_with("Get") || a == "Browse")
        );
        f.source_kept(since);
    }
}

#[test]
fn owner_changes_after_destination_start_are_not_overwritten_by_source_stop() {
    for (from, to) in PAIRS {
        for fault in [
            Fault::SwitchSourceAfterPlay,
            Fault::PauseSourceAfterPlay,
            Fault::EditSourceQueueAfterPlay,
            Fault::ChangeSourceModeAfterPlay,
        ] {
            let f = Fixture::new(from, to, PlayMode::Shuffle);
            let t = Replies::new(&f, fault);
            let since = f.sim.soap_log().len();
            assert!(f.copy(&t).is_err());
            assert!(
                f.sim.soap_log()[since..]
                    .iter()
                    .all(|entry| entry.room != f.from || entry.action != "Stop"),
                "a newer owner action must not be overwritten by copy cleanup"
            );
            match fault {
                Fault::SwitchSourceAfterPlay => {
                    assert_eq!(
                        soap::get_media_info(&f.sim.lan(), f.source).unwrap().uri,
                        OTHER
                    );
                }
                Fault::PauseSourceAfterPlay => {
                    assert_eq!(
                        soap::get_transport_info(&f.sim.lan(), f.source)
                            .unwrap()
                            .state,
                        TransportState::Paused
                    );
                }
                Fault::EditSourceQueueAfterPlay => {
                    assert_eq!(
                        browse_snapshot(&f.sim.lan(), f.source, QUEUE)
                            .unwrap()
                            .objects
                            .len(),
                        4
                    );
                }
                Fault::ChangeSourceModeAfterPlay => {
                    assert_eq!(
                        get_play_mode(&f.sim.lan(), f.source).unwrap(),
                        PlayMode::RepeatAll
                    );
                }
                _ => unreachable!(),
            }
        }
    }
}
