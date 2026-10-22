//! Lost mode replies are reconciled over real S1/S2 sockets, never replayed.

use fsonos_core::{CoreError, HouseholdState, control};
use fsonos_proto::control as soap;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::cell::{Cell, RefCell};
use std::net::IpAddr;

const FIRST: &str = "x-file-cifs://example.invalid/first.flac";
const SECOND: &str = "x-file-cifs://example.invalid/second.flac";
const OTHER_SOURCE: &str = "x-rincon-mp3radio://example.invalid/owner.mp3";
const START: [&str; 4] = ["SetAVTransportURI", "SetPlayMode", "Seek", "Play"];

struct Rig {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
    id: PlayerId,
    host: IpAddr,
    other: PlayerId,
}

impl Rig {
    fn new(s2: bool) -> Self {
        let model = if s2 {
            SimModel::One
        } else {
            SimModel::Play5Gen1
        };
        let players = [
            SimPlayerSpec::new("Alpha", model),
            SimPlayerSpec::new("Beta", model),
        ];
        let builder = SimHousehold::builder();
        let sim = if s2 {
            builder.s2(players)
        } else {
            builder.s1(players)
        }
        .spawn()
        .unwrap();
        let host = sim.player("Alpha").unwrap().ip;
        let id = PlayerId(sim.player("Alpha").unwrap().uuid.clone());
        let other = PlayerId(sim.player("Beta").unwrap().uuid.clone());
        let lan = sim.lan();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        for uri in [FIRST, SECOND] {
            soap::add_uri_to_queue(&lan, host, uri, "", false).unwrap();
        }
        soap::play_from_queue(&lan, host, &id).unwrap();
        set_play_mode(&lan, host, PlayMode::RepeatOne).unwrap();
        Self {
            sim,
            houses: vec![house],
            id,
            host,
            other,
        }
    }

    fn start(&self, t: &Intercept<'_>, position: u32) -> Result<(), CoreError> {
        control::play_queue_from_in_order(t, &self.houses, &self.id, position)
    }

    fn playing_second(&self) {
        let lan = self.sim.lan();
        let current = soap::get_position_info(&lan, self.host).unwrap();
        assert_eq!(current.track, 2);
        assert_eq!(current.uri, SECOND);
        assert_eq!(get_play_mode(&lan, self.host).unwrap(), PlayMode::Normal);
        assert_eq!(
            soap::get_transport_info(&lan, self.host).unwrap().state,
            TransportState::Playing
        );
    }
}

#[derive(Clone, Copy)]
enum Reply {
    Acknowledged,
    Lost,
    Malformed,
    NotApplied,
    Rejected,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Change {
    None,
    Source,
    Pause,
    Resume,
    SeekTrack,
    SeekPosition,
    Queue,
    Group,
    Mode,
    FailedRead,
    UnknownMode,
    MissingMode,
}

struct Intercept<'a> {
    rig: &'a Rig,
    inner: SimLan,
    reply: Reply,
    change: Change,
    settings_reads: Cell<usize>,
    calls: RefCell<Vec<String>>,
}

impl<'a> Intercept<'a> {
    fn new(rig: &'a Rig, reply: Reply, change: Change) -> Self {
        Self {
            rig,
            inner: rig.sim.lan(),
            reply,
            change,
            settings_reads: Cell::new(0),
            calls: RefCell::default(),
        }
    }

    fn writes(&self) -> Vec<String> {
        self.calls
            .borrow()
            .iter()
            .filter(|action| !action.starts_with("Get") && action.as_str() != "Browse")
            .cloned()
            .collect()
    }

    fn owner_changes(&self, host: IpAddr) -> Result<(), ProtoError> {
        match self.change {
            Change::Source => soap::set_av_transport_uri(&self.inner, host, OTHER_SOURCE, "")?,
            Change::Pause => {
                soap::play(&self.inner, host)?;
                soap::pause(&self.inner, host)?;
            }
            Change::Resume => soap::play(&self.inner, host)?,
            Change::SeekTrack => soap::seek_track(&self.inner, host, 2)?,
            Change::SeekPosition => soap::seek_position(&self.inner, host, 30)?,
            Change::Queue => {
                soap::remove_track_range_from_queue(&self.inner, host, 0, 2, 1)?;
                soap::add_uri_to_queue(&self.inner, host, SECOND, "", false)?;
            }
            Change::Group => soap::join_group(&self.inner, host, &self.rig.other)?,
            Change::Mode => set_play_mode(&self.inner, host, PlayMode::RepeatOne)?,
            _ => {}
        }
        Ok(())
    }
}

impl Transport for Intercept<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        self.calls.borrow_mut().push(name.into());
        if name == "SetPlayMode" {
            if matches!(self.reply, Reply::Rejected) {
                return Err(ProtoError::SoapFault {
                    code: 501,
                    reason: "explicit mode refusal".into(),
                });
            }
            if !matches!(self.reply, Reply::NotApplied) {
                let response = self.inner.soap_post(host, path, action, body)?;
                if matches!(self.reply, Reply::Acknowledged) {
                    return Ok(response);
                }
            }
            return if matches!(self.reply, Reply::Malformed) {
                Ok("<broken-mode-reply>".into())
            } else {
                Err(ProtoError::Network {
                    target: host.to_string(),
                    detail: "original lost SetPlayMode reply".into(),
                })
            };
        }
        if name == "GetTransportSettings" {
            self.settings_reads.set(self.settings_reads.get() + 1);
            if self.change == Change::FailedRead {
                return Err(ProtoError::Network {
                    target: host.to_string(),
                    detail: "secondary settings read failed".into(),
                });
            }
        }
        let mut response = self.inner.soap_post(host, path, action, body)?;
        if name == "GetTransportSettings" {
            if self.settings_reads.get() == 1 {
                self.owner_changes(host)?;
            }
            response = match self.change {
                Change::UnknownMode => response.replace("NORMAL", "FUTURE_MODE"),
                Change::MissingMode => response.replace("PlayMode", "AbsentMode"),
                _ => response,
            };
        }
        Ok(response)
    }
}

#[test]
fn ordinary_success_keeps_the_four_call_ordered_start_path() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let t = Intercept::new(&rig, Reply::Acknowledged, Change::None);
        rig.start(&t, 2).unwrap();
        assert_eq!(*t.calls.borrow(), START);
        rig.playing_second();
    }
}

#[test]
fn confirmation_reads_mode_before_the_requested_track_has_been_selected() {
    for s2 in [false, true] {
        for reply in [Reply::Lost, Reply::Malformed] {
            let rig = Rig::new(s2);
            let t = Intercept::new(&rig, reply, Change::None);
            rig.start(&t, 2).unwrap();
            assert_eq!(t.writes(), START);
            assert_eq!(t.settings_reads.get(), 2);
            rig.playing_second();
        }
    }
}

#[test]
fn an_unapplied_mode_request_is_not_retried_or_assumed_normal() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let t = Intercept::new(&rig, Reply::NotApplied, Change::None);
        let error = rig.start(&t, 2).unwrap_err();
        assert!(error.to_string().contains("original lost SetPlayMode"));
        assert_eq!(t.writes(), ["SetAVTransportURI", "SetPlayMode"]);
        assert_eq!(
            get_play_mode(&t.inner, rig.host).unwrap(),
            PlayMode::RepeatOne
        );
        assert_eq!(
            soap::get_position_info(&t.inner, rig.host).unwrap().track,
            1
        );
    }
}

#[test]
fn failed_unknown_or_missing_settings_keep_the_original_failure() {
    for s2 in [false, true] {
        for change in [Change::FailedRead, Change::UnknownMode, Change::MissingMode] {
            let rig = Rig::new(s2);
            let t = Intercept::new(&rig, Reply::Lost, change);
            let error = rig.start(&t, 2).unwrap_err();
            assert!(error.to_string().contains("original lost SetPlayMode"));
            assert_eq!(t.writes(), ["SetAVTransportURI", "SetPlayMode"]);
            assert_eq!(t.settings_reads.get(), 1);
            assert_eq!(
                soap::get_transport_info(&t.inner, rig.host).unwrap().state,
                TransportState::Stopped
            );
        }
    }
}

#[test]
fn a_definite_soap_refusal_is_not_converted_into_read_back_success() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        // Even a preexisting NORMAL setting does not excuse a definite refusal.
        set_play_mode(&rig.sim.lan(), rig.host, PlayMode::Normal).unwrap();
        let t = Intercept::new(&rig, Reply::Rejected, Change::None);
        let error = rig.start(&t, 2).unwrap_err();
        assert!(error.to_string().contains("explicit mode refusal"));
        assert_eq!(t.writes(), ["SetAVTransportURI", "SetPlayMode"]);
        assert_eq!(t.settings_reads.get(), 0);
    }
}

#[test]
fn owner_source_transport_and_seek_changes_block_dependent_writes() {
    for s2 in [false, true] {
        for change in [
            Change::Source,
            Change::Pause,
            Change::Resume,
            Change::SeekTrack,
            Change::SeekPosition,
        ] {
            let rig = Rig::new(s2);
            let t = Intercept::new(&rig, Reply::Lost, change);
            let error = rig.start(&t, 2).unwrap_err();
            assert!(error.to_string().contains("original lost SetPlayMode"));
            assert_eq!(t.writes(), ["SetAVTransportURI", "SetPlayMode"]);
            assert!(t.settings_reads.get() >= 1, "owner change was injected");
            if change == Change::Pause {
                assert_eq!(
                    soap::get_transport_info(&t.inner, rig.host).unwrap().state,
                    TransportState::Paused
                );
            }
            if change == Change::Source {
                assert_eq!(
                    soap::get_media_info(&t.inner, rig.host).unwrap().uri,
                    OTHER_SOURCE
                );
            }
        }
    }
}

#[test]
fn revision_group_and_mode_changes_block_recovery_even_with_matching_tracks() {
    for s2 in [false, true] {
        for change in [Change::Queue, Change::Group, Change::Mode] {
            let rig = Rig::new(s2);
            let t = Intercept::new(&rig, Reply::Lost, change);
            let error = rig.start(&t, 2).unwrap_err();
            assert!(error.to_string().contains("original lost SetPlayMode"));
            assert_eq!(t.writes(), ["SetAVTransportURI", "SetPlayMode"]);
            assert!(t.settings_reads.get() >= 1);
        }
    }
}

#[test]
fn a_missing_target_slot_cannot_be_created_by_recovery() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let t = Intercept::new(&rig, Reply::Lost, Change::None);
        let error = rig.start(&t, 3).unwrap_err();
        assert!(error.to_string().contains("original lost SetPlayMode"));
        assert_eq!(t.writes(), ["SetAVTransportURI", "SetPlayMode"]);
        assert_eq!(t.settings_reads.get(), 0);
    }
}

#[test]
fn recovery_of_ordered_preparation_never_sends_play() {
    for s2 in [false, true] {
        for reply in [Reply::Lost, Reply::Malformed] {
            let rig = Rig::new(s2);
            let t = Intercept::new(&rig, reply, Change::None);
            control::load_queue_from_in_order(&t, &rig.houses, &rig.id, 2).unwrap();
            assert_eq!(t.writes(), ["SetAVTransportURI", "SetPlayMode", "Seek"]);
            let current = soap::get_position_info(&t.inner, rig.host).unwrap();
            assert_eq!(current.track, 2);
            assert_eq!(current.uri, SECOND);
            assert_eq!(get_play_mode(&t.inner, rig.host).unwrap(), PlayMode::Normal);
            assert_eq!(
                soap::get_transport_info(&t.inner, rig.host).unwrap().state,
                TransportState::Stopped
            );
        }
    }
}
