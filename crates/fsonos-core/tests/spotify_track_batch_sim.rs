//! Bounded catalog playback over real S1/S2 SOAP sockets. Inject only lost
//! replies, explicit refusals and independent changes at the transport edge.

use fsonos_core::{CoreError, HouseholdState, control};
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::cell::{Cell, RefCell};
use std::net::IpAddr;
use std::time::Duration;

const TRACKS: [(&str, &str); 4] = [
    ("spotify:track:0000000000000000000101", "First & live"),
    ("spotify:track:0000000000000000000102", "Second"),
    ("spotify:track:0000000000000000000103", "Third"),
    ("spotify:track:0000000000000000000101", "First again"),
];
const OLD: [&str; 3] = [
    "http://example.invalid/first.mp3",
    "http://example.invalid/second.mp3",
    "http://example.invalid/third.mp3",
];
const RADIO: &str = "http://example.invalid/radio.mp3";

struct Rig {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
    id: PlayerId,
    host: IpAddr,
    peer: PlayerId,
    peer_host: IpAddr,
}

impl Rig {
    fn new(s2: bool, populated: bool) -> Self {
        let model = if s2 {
            SimModel::One
        } else {
            SimModel::Play5Gen1
        };
        let players = [
            SimPlayerSpec::new("Batch", model),
            SimPlayerSpec::new("Peer", model),
        ];
        let builder = SimHousehold::builder();
        let sim = if s2 {
            builder.s2(players)
        } else {
            builder.s1(players)
        }
        .spawn()
        .unwrap();
        let host = sim.player("Batch").unwrap().ip;
        let id = PlayerId(sim.player("Batch").unwrap().uuid.clone());
        let peer_host = sim.player("Peer").unwrap().ip;
        let peer = PlayerId(sim.player("Peer").unwrap().uuid.clone());
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&sim.lan(), host).unwrap());
        let houses = vec![house];
        if populated {
            for uri in OLD {
                soap::enqueue_uri(&sim.lan(), host, uri, "", 0, false).unwrap();
            }
            control::play_queue_from(&sim.lan(), &houses, &id, 1).unwrap();
        }
        Self {
            sim,
            houses,
            id,
            host,
            peer,
            peer_host,
        }
    }

    fn queue(&self) -> Vec<String> {
        browse_snapshot(&self.sim.lan(), self.host, QUEUE)
            .unwrap()
            .objects
            .into_iter()
            .map(|item| {
                let uri = item.res.unwrap().uri;
                spotify_uri_from_renderer_uri(&uri).unwrap_or(uri)
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    None,
    LostApplied,
    RefuseSecond,
    LostSecondBeforeApply,
    SourceBeforeFirstRead,
    MembershipBeforeFirstRead,
    SourceAfterLearning,
    MembershipAfterFirst,
    RebootAfterFirst,
    AdvanceAfterFirst,
}

struct Replies<'a> {
    rig: &'a Rig,
    lan: SimLan,
    fault: Fault,
    adds: Cell<usize>,
    edited: Cell<bool>,
    writes: RefCell<Vec<String>>,
}

impl<'a> Replies<'a> {
    fn new(rig: &'a Rig, fault: Fault) -> Self {
        Self {
            rig,
            lan: rig.sim.lan(),
            fault,
            adds: Cell::new(0),
            edited: Cell::new(false),
            writes: RefCell::new(Vec::new()),
        }
    }

    fn lost(&self) -> ProtoError {
        ProtoError::Network {
            target: self.rig.host.to_string(),
            detail: "injected missing reply".into(),
        }
    }

    fn only_adds(&self, count: usize) {
        assert_eq!(*self.writes.borrow(), vec!["AddURIToQueue"; count]);
    }
}

impl Transport for Replies<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if name == "GetZoneGroupState"
            && matches!(
                self.fault,
                Fault::SourceBeforeFirstRead | Fault::MembershipBeforeFirstRead
            )
            && !self.edited.replace(true)
        {
            if self.fault == Fault::SourceBeforeFirstRead {
                soap::set_av_transport_uri(&self.lan, host, RADIO, "")?;
            } else {
                soap::join_group(&self.lan, self.rig.peer_host, &self.rig.id)?;
            }
        }
        if !name.starts_with("Get") && name != "Browse" {
            self.writes.borrow_mut().push(name.into());
        }
        if name == "AddURIToQueue" {
            self.adds.set(self.adds.get() + 1);
            if self.adds.get() == 2 {
                if self.fault == Fault::RefuseSecond {
                    return Err(ProtoError::SoapFault {
                        code: 800,
                        reason: "injected second-track refusal".into(),
                    });
                }
                if self.fault == Fault::LostSecondBeforeApply {
                    return Err(self.lost());
                }
            }
        }
        let result = self.lan.soap_post(host, path, action, body)?;
        if self.fault == Fault::SourceAfterLearning
            && name == "Browse"
            && body.contains("<ObjectID>FV:2</ObjectID>")
            && !self.edited.replace(true)
        {
            soap::set_av_transport_uri(&self.lan, host, RADIO, "")?;
        }
        if name == "AddURIToQueue" && self.adds.get() == 1 {
            match self.fault {
                Fault::MembershipAfterFirst => {
                    soap::join_group(&self.lan, self.rig.peer_host, &self.rig.id)?
                }
                Fault::RebootAfterFirst => self.rig.sim.reboot("Batch", Duration::ZERO).unwrap(),
                Fault::AdvanceAfterFirst => soap::seek_track(&self.lan, host, 2)?,
                _ => {}
            }
        }
        if name == "AddURIToQueue" && self.fault == Fault::LostApplied {
            return Err(self.lost());
        }
        Ok(result)
    }
}

#[test]
fn replacement_plays_the_exact_order_after_complete_staging_on_both_generations() {
    for s2 in [false, true] {
        for populated in [false, true] {
            let rig = Rig::new(s2, populated);
            if populated {
                set_play_mode(&rig.sim.lan(), rig.host, PlayMode::ShuffleNoRepeat).unwrap();
            }
            let t = Replies::new(&rig, Fault::None);
            control::play_spotify_tracks(&t, &rig.houses, &rig.id, &TRACKS).unwrap();
            assert_eq!(rig.queue(), TRACKS.map(|(uri, _)| uri));
            assert_eq!(
                get_play_mode(&rig.sim.lan(), rig.host).unwrap(),
                PlayMode::Normal
            );
            let playback = control::playback(&rig.sim.lan(), &rig.houses, &rig.id).unwrap();
            assert_eq!(playback.transport.state, TransportState::Playing);
            assert_eq!(playback.position.track, 1);
            assert_eq!(t.adds.get(), TRACKS.len());
            let writes = t.writes.borrow();
            assert!(!writes.iter().any(|name| name == "RemoveAllTracksFromQueue"));
            assert_eq!(
                writes.iter().filter(|name| name.as_str() == "Play").count(),
                1
            );
            if populated {
                assert_eq!(
                    writes
                        .iter()
                        .position(|name| name == "RemoveTrackRangeFromQueue"),
                    Some(TRACKS.len())
                );
            }
        }
    }
}

#[test]
fn append_and_next_keep_order_and_never_start_or_pause_music() {
    for s2 in [false, true] {
        for next in [false, true] {
            for radio in [false, true] {
                let rig = Rig::new(s2, true);
                if radio {
                    control::play_uri(&rig.sim.lan(), &rig.houses, &rig.id, RADIO, "").unwrap();
                }
                let before = control::playback(&rig.sim.lan(), &rig.houses, &rig.id).unwrap();
                let source = soap::get_media_info(&rig.sim.lan(), rig.host).unwrap();
                let t = Replies::new(&rig, Fault::None);
                let added =
                    control::enqueue_spotify_tracks(&t, &rig.houses, &rig.id, &TRACKS, next)
                        .unwrap();
                let first = if next && !radio { 2 } else { 4 };
                assert_eq!(added.first, first);
                assert_eq!(added.playing_queue, !radio);
                let mut expected: Vec<String> = OLD.iter().map(|uri| (*uri).into()).collect();
                let index = usize::try_from(first - 1).unwrap();
                expected.splice(index..index, TRACKS.iter().map(|(uri, _)| (*uri).into()));
                assert_eq!(rig.queue(), expected);
                t.only_adds(TRACKS.len());
                let after = control::playback(&rig.sim.lan(), &rig.houses, &rig.id).unwrap();
                assert_eq!(after.transport.state, before.transport.state);
                assert_eq!(
                    soap::get_media_info(&rig.sim.lan(), rig.host).unwrap().uri,
                    source.uri
                );
                assert_eq!(after.position.track, before.position.track);
                assert!(
                    control::unqueue(
                        &rig.sim.lan(),
                        &rig.houses,
                        &rig.id,
                        added.first,
                        &added.uris
                    )
                    .unwrap()
                );
                assert_eq!(rig.queue(), OLD);
            }
        }
    }
}

#[test]
fn loading_a_batch_pauses_the_old_source_and_never_sends_play() {
    for s2 in [false, true] {
        let rig = Rig::new(s2, true);
        let t = Replies::new(&rig, Fault::None);
        control::load_spotify_tracks(&t, &rig.houses, &rig.id, &TRACKS).unwrap();
        assert_eq!(rig.queue(), TRACKS.map(|(uri, _)| uri));
        assert!(!t.writes.borrow().iter().any(|name| name == "Play"));
        let after = control::playback(&rig.sim.lan(), &rig.houses, &rig.id).unwrap();
        assert!(matches!(
            after.transport.state,
            TransportState::Paused | TransportState::Stopped
        ));
        assert_eq!(after.position.track, 1);
    }
}

#[test]
fn partial_refusal_or_missing_write_retains_the_original_queue_and_stops() {
    for s2 in [false, true] {
        for fault in [Fault::RefuseSecond, Fault::LostSecondBeforeApply] {
            let rig = Rig::new(s2, true);
            let t = Replies::new(&rig, fault);
            assert!(matches!(
                control::play_spotify_tracks(&t, &rig.houses, &rig.id, &TRACKS),
                Err(CoreError::Uncertain(_))
            ));
            t.only_adds(2);
            assert_eq!(&rig.queue()[..OLD.len()], OLD);
            assert_eq!(rig.queue().len(), OLD.len() + 1);
            assert_eq!(
                control::playback(&rig.sim.lan(), &rig.houses, &rig.id)
                    .unwrap()
                    .position
                    .track,
                1
            );
        }
    }
}

#[test]
fn lost_applied_replies_are_confirmed_without_duplicate_tracks() {
    for s2 in [false, true] {
        for next in [false, true] {
            let rig = Rig::new(s2, true);
            let t = Replies::new(&rig, Fault::LostApplied);
            let added =
                control::enqueue_spotify_tracks(&t, &rig.houses, &rig.id, &TRACKS, next).unwrap();
            t.only_adds(TRACKS.len());
            assert_eq!(added.uris.len(), TRACKS.len());
            assert_eq!(rig.queue().len(), OLD.len() + TRACKS.len());
        }
    }
}

#[test]
fn changed_source_or_stale_coordinator_prevents_any_batch_write() {
    for s2 in [false, true] {
        let rig = Rig::new(s2, true);
        let t = Replies::new(&rig, Fault::SourceAfterLearning);
        assert!(control::play_spotify_tracks(&t, &rig.houses, &rig.id, &TRACKS).is_err());
        t.only_adds(0);
        assert_eq!(rig.queue(), OLD);
        assert_eq!(
            soap::get_media_info(&rig.sim.lan(), rig.host).unwrap().uri,
            RADIO
        );
        soap::join_group(&rig.sim.lan(), rig.host, &rig.peer).unwrap();
        let t = Replies::new(&rig, Fault::None);
        assert!(control::enqueue_spotify_tracks(&t, &rig.houses, &rig.id, &TRACKS, false).is_err());
        t.only_adds(0);
    }
}

#[test]
fn group_boot_and_current_occurrence_changes_stop_the_remaining_batch() {
    for s2 in [false, true] {
        for fault in [
            Fault::MembershipAfterFirst,
            Fault::RebootAfterFirst,
            Fault::AdvanceAfterFirst,
        ] {
            let rig = Rig::new(s2, true);
            let t = Replies::new(&rig, fault);
            assert!(matches!(
                control::enqueue_spotify_tracks(&t, &rig.houses, &rig.id, &TRACKS, true),
                Err(CoreError::Uncertain(_))
            ));
            t.only_adds(1);
        }
    }
}

#[test]
fn retained_destinations_cannot_adopt_a_change_at_the_first_core_read() {
    for s2 in [false, true] {
        for fault in [
            Fault::SourceBeforeFirstRead,
            Fault::MembershipBeforeFirstRead,
        ] {
            for operation in 0..3 {
                let rig = Rig::new(s2, true);
                let destination =
                    control::SourceDestination::read(&rig.sim.lan(), rig.host, &rig.id).unwrap();
                let t = Replies::new(&rig, fault);
                let result = match operation {
                    0 => control::play_spotify_tracks_bound(
                        &t,
                        &rig.houses,
                        &rig.id,
                        &TRACKS,
                        &destination,
                    ),
                    1 => control::load_spotify_tracks_bound(
                        &t,
                        &rig.houses,
                        &rig.id,
                        &TRACKS,
                        &destination,
                    ),
                    _ => control::enqueue_spotify_tracks_bound(
                        &t,
                        &rig.houses,
                        &rig.id,
                        &TRACKS,
                        true,
                        &destination,
                    )
                    .map(drop),
                };
                assert!(
                    result.is_err(),
                    "{fault:?} operation {operation} adopted a changed target"
                );
                t.only_adds(0);
                assert_eq!(rig.queue(), OLD);
            }
        }
    }
}

#[test]
fn invalid_batches_are_rejected_before_any_transport_read_or_write() {
    struct NoIo;
    impl Transport for NoIo {
        fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
            panic!("invalid batch performed I/O");
        }
    }
    let too_many = vec![TRACKS[0]; control::MAX_SPOTIFY_TRACK_BATCH + 1];
    for tracks in [
        Vec::new(),
        too_many,
        vec![("spotify:artist:0000000000000000000101", "Wrong kind")],
        vec![("spotify:track:short", "Bad id")],
        vec![("spotify:track:00000000000000000001?1", "Bad id")],
    ] {
        let id = PlayerId("fixture".into());
        assert!(control::play_spotify_tracks(&NoIo, &[], &id, &tracks).is_err());
        assert!(control::load_spotify_tracks(&NoIo, &[], &id, &tracks).is_err());
        assert!(control::enqueue_spotify_tracks(&NoIo, &[], &id, &tracks, true).is_err());
    }
}
