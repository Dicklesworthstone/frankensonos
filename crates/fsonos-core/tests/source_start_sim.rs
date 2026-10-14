//! Direct stream and Spotify starts over real S1/S2 simulator sockets. Lost
//! replies may advance a confirmed stage, but never repeat a playback write.

use fsonos_core::{CoreError, HouseholdState, control};
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::cell::{Cell, RefCell};
use std::net::IpAddr;

const OLD: &str = "x-rincon-mp3radio://example.invalid/old.mp3";
const NEW: &str = "x-rincon-mp3radio://example.invalid/new.mp3";
const OTHER: &str = "x-rincon-mp3radio://example.invalid/owner.mp3";
const WRITES: [&str; 2] = ["SetAVTransportURI", "Play"];

#[derive(Clone, Copy)]
enum Fault {
    None,
    Lost,
    Malformed,
    Before,
    Rejected,
    Unreadable,
    Takeover,
    DuringRead,
    Regroup,
    Metadata,
    Paused,
}

struct Replies {
    inner: SimLan,
    peer: PlayerId,
    fault: Fault,
    stage: Option<&'static str>,
    calls: RefCell<Vec<String>>,
    failed: Cell<bool>,
    switched: Cell<bool>,
}

impl Replies {
    fn new(sim: &SimHandle, fault: Fault, stage: Option<&'static str>) -> Self {
        Self {
            inner: sim.lan(),
            peer: PlayerId(sim.player("Peer").unwrap().uuid.clone()),
            fault,
            stage,
            calls: RefCell::new(Vec::new()),
            failed: Cell::new(false),
            switched: Cell::new(false),
        }
    }

    fn lost(host: IpAddr) -> ProtoError {
        ProtoError::Network {
            target: host.to_string(),
            detail: "injected lost source-start reply".into(),
        }
    }

    fn writes(&self) -> Vec<String> {
        self.calls
            .borrow()
            .iter()
            .filter(|name| WRITES.contains(&name.as_str()))
            .cloned()
            .collect()
    }

    fn takeover(&self, host: IpAddr) -> Result<(), ProtoError> {
        soap::set_av_transport_uri(&self.inner, host, OTHER, "")?;
        soap::play(&self.inner, host)
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
        let name = action
            .trim_matches('"')
            .rsplit('#')
            .next()
            .unwrap_or(action);
        self.calls.borrow_mut().push(name.into());
        if self.failed.get() && matches!(self.fault, Fault::Unreadable) {
            return Err(ProtoError::Malformed("read-back unavailable".into()));
        }
        let selected = WRITES.contains(&name) && self.stage.is_none_or(|stage| stage == name);
        if selected && matches!(self.fault, Fault::Before) {
            return Err(Self::lost(host));
        }
        if selected && matches!(self.fault, Fault::Rejected) {
            return Err(ProtoError::SoapFault {
                code: 701,
                reason: "injected definite refusal".into(),
            });
        }
        let mut response = self.inner.soap_post(host, path, action, body)?;
        if self.failed.get() && name == "GetMediaInfo" {
            if matches!(self.fault, Fault::DuringRead) && !self.switched.replace(true) {
                // Return the old read after an independent owner's switch.
                self.takeover(host)?;
            }
            if matches!(self.fault, Fault::Metadata) {
                let open = "<CurrentURIMetaData>";
                let start = response.find(open).unwrap() + open.len();
                let end = response.find("</CurrentURIMetaData>").unwrap();
                response.replace_range(start..end, "");
            }
        }
        if !selected || matches!(self.fault, Fault::None) {
            return Ok(response);
        }
        self.failed.set(true);
        match self.fault {
            Fault::Malformed => return Ok("<incomplete-reply>".into()),
            Fault::Takeover => self.takeover(host)?,
            Fault::Regroup => soap::join_group(&self.inner, host, &self.peer)?,
            Fault::Paused => soap::pause(&self.inner, host)?,
            _ => {}
        }
        Err(Self::lost(host))
    }
}

struct Fixture {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
    id: PlayerId,
    host: IpAddr,
}

impl Fixture {
    fn new(generation: u8) -> Self {
        let builder = SimHousehold::builder();
        let builder = if generation == 1 {
            builder.s1([
                SimPlayerSpec::new("Room", SimModel::Play5Gen1),
                SimPlayerSpec::new("Peer", SimModel::Play5Gen1),
            ])
        } else {
            builder.s2([
                SimPlayerSpec::new("Room", SimModel::One),
                SimPlayerSpec::new("Peer", SimModel::One),
            ])
        };
        let sim = builder.spawn().unwrap();
        let player = sim.player("Room").unwrap();
        let host = player.ip;
        let id = PlayerId(player.uuid.clone());
        let lan = sim.lan();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        soap::add_uri_to_queue(&lan, host, OLD, "", false).unwrap();
        soap::set_av_transport_uri(&lan, host, OLD, "").unwrap();
        soap::play(&lan, host).unwrap();
        Self {
            sim,
            houses: vec![house],
            id,
            host,
        }
    }

    fn start(&self, replies: &Replies, uri: &str, metadata: &str) -> Result<(), CoreError> {
        control::play_uri(replies, &self.houses, &self.id, uri, metadata)
    }

    fn spotify(&self) -> (String, String) {
        control::spotify_track_source(
            &self.sim.lan(),
            &self.houses,
            &self.id,
            "spotify:track:0SimNewTrack0000000001",
            "Test track",
        )
        .unwrap()
        .unwrap()
    }

    fn assert_queue_preserved(&self) {
        let queue = browse_all(&self.sim.lan(), self.host, QUEUE).unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].res.as_ref().unwrap().uri, OLD);
    }
}

#[test]
fn lost_and_malformed_source_and_play_replies_complete_in_both_generations() {
    for generation in [1, 2] {
        for fault in [Fault::Lost, Fault::Malformed] {
            for spotify in [false, true] {
                let f = Fixture::new(generation);
                let (uri, metadata) = if spotify {
                    f.spotify()
                } else {
                    (NEW.into(), String::new())
                };
                let replies = Replies::new(&f.sim, fault, None);
                f.start(&replies, &uri, &metadata).unwrap();
                assert_eq!(replies.writes(), WRITES);
                let media = soap::get_media_info(&f.sim.lan(), f.host).unwrap();
                assert_eq!(media.uri, uri);
                assert_eq!(media.uri_metadata, metadata);
                assert_eq!(
                    soap::get_transport_info(&f.sim.lan(), f.host)
                        .unwrap()
                        .state,
                    TransportState::Playing
                );
                f.assert_queue_preserved();
            }
        }
    }
}

#[test]
fn successful_replies_keep_the_two_write_path_without_extra_reads() {
    let f = Fixture::new(2);
    let replies = Replies::new(&f.sim, Fault::None, None);
    f.start(&replies, NEW, "").unwrap();
    assert_eq!(*replies.calls.borrow(), WRITES);
}

#[test]
fn unapplied_requests_and_definite_refusals_never_advance_or_repeat() {
    for fault in [Fault::Before, Fault::Rejected] {
        for (index, stage) in WRITES.iter().enumerate() {
            let f = Fixture::new(2);
            let replies = Replies::new(&f.sim, fault, Some(stage));
            let error = f.start(&replies, NEW, "").unwrap_err();
            assert_eq!(replies.writes(), WRITES[..=index]);
            if matches!(fault, Fault::Rejected) {
                assert!(matches!(
                    error,
                    CoreError::Proto(ProtoError::SoapFault { code: 701, .. })
                ));
                assert_eq!(*replies.calls.borrow(), WRITES[..=index]);
            } else {
                assert!(matches!(
                    error,
                    CoreError::Proto(ProtoError::Network { .. })
                ));
            }
            f.assert_queue_preserved();
        }
    }
}

#[test]
fn failed_readback_retains_the_original_error_and_sends_no_play() {
    let f = Fixture::new(2);
    let replies = Replies::new(&f.sim, Fault::Unreadable, Some("SetAVTransportURI"));
    let error = f.start(&replies, NEW, "").unwrap_err().to_string();
    assert!(error.contains("injected lost source-start reply"));
    assert!(!error.contains("read-back unavailable"));
    assert_eq!(replies.writes(), WRITES[..1]);
}

#[test]
fn owner_source_changes_before_or_during_readback_are_not_overwritten() {
    for generation in [1, 2] {
        for fault in [Fault::Takeover, Fault::DuringRead] {
            for (index, stage) in WRITES.iter().enumerate() {
                let f = Fixture::new(generation);
                let replies = Replies::new(&f.sim, fault, Some(stage));
                assert!(f.start(&replies, NEW, "").is_err());
                assert_eq!(replies.writes(), WRITES[..=index]);
                let media = soap::get_media_info(&f.sim.lan(), f.host).unwrap();
                assert_eq!(media.uri, OTHER);
                assert_eq!(
                    soap::get_transport_info(&f.sim.lan(), f.host)
                        .unwrap()
                        .state,
                    TransportState::Playing
                );
                f.assert_queue_preserved();
            }
        }
    }
}

#[test]
fn regrouping_after_source_selection_blocks_the_later_play() {
    let f = Fixture::new(2);
    let replies = Replies::new(&f.sim, Fault::Regroup, Some("SetAVTransportURI"));
    assert!(f.start(&replies, NEW, "").is_err());
    assert_eq!(replies.writes(), WRITES[..1]);
    let groups = get_zone_group_state(&f.sim.lan(), f.host).unwrap();
    assert!(groups.groups.iter().all(|group| group.coordinator != f.id));
}

#[test]
fn matching_uri_without_the_requested_metadata_is_not_confirmation() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let (uri, metadata) = f.spotify();
        assert!(!metadata.is_empty());
        let replies = Replies::new(&f.sim, Fault::Metadata, Some("SetAVTransportURI"));
        let error = f.start(&replies, &uri, &metadata).unwrap_err();
        assert!(matches!(
            error,
            CoreError::Proto(ProtoError::Network { .. })
        ));
        assert_eq!(replies.writes(), WRITES[..1]);
    }
}

#[test]
fn an_owner_pause_after_a_lost_play_reply_is_not_reported_as_playing() {
    let f = Fixture::new(2);
    let replies = Replies::new(&f.sim, Fault::Paused, Some("Play"));
    assert!(f.start(&replies, NEW, "").is_err());
    assert_eq!(replies.writes(), WRITES);
    assert_eq!(
        soap::get_transport_info(&f.sim.lan(), f.host)
            .unwrap()
            .state,
        TransportState::Paused
    );
}

#[test]
fn empty_sources_are_rejected_before_interrupting_existing_music() {
    let f = Fixture::new(2);
    let replies = Replies::new(&f.sim, Fault::None, None);
    assert!(f.start(&replies, " \n", "").is_err());
    assert!(replies.calls.borrow().is_empty());
    assert_eq!(soap::get_media_info(&f.sim.lan(), f.host).unwrap().uri, OLD);
}
