//! Sources can be prepared for resume without ever sending Play. Exercise
//! the same household-account and queue-replacement paths as ordinary starts.

use fsonos_core::{HouseholdState, control};
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::didl::{SpotifyContainer, spotify_uri_from_renderer_uri};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::cell::{Cell, RefCell};
use std::net::IpAddr;

const OLD: &str = "x-file-cifs://example.invalid/old.flac";
const NEXT: &str = "x-file-cifs://example.invalid/next.flac";
const NEW: &str = "x-rincon-mp3radio://example.invalid/new.mp3";

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
        for uri in [OLD, NEXT] {
            soap::add_uri_to_queue(&lan, host, uri, "", false).unwrap();
        }
        soap::set_volume(&lan, host, 31).unwrap();
        soap::set_mute(&lan, host, true).unwrap();
        control::play_queue_from(&lan, &[house.clone()], &id, 2).unwrap();
        Self {
            sim,
            houses: vec![house],
            id,
            host,
        }
    }

    fn assert_silent(&self) {
        let lan = self.sim.lan();
        assert!(matches!(
            soap::get_transport_info(&lan, self.host).unwrap().state,
            TransportState::Paused | TransportState::Stopped
        ));
        assert_eq!(soap::get_volume(&lan, self.host).unwrap(), 31);
        assert!(soap::get_mute(&lan, self.host).unwrap());
    }
}

/// Drop one reply after the actual operation. Every other call is real
/// loopback traffic; record even operations that fail before an acknowledgement.
struct Replies {
    inner: SimLan,
    lose: Option<&'static str>,
    lost: Cell<bool>,
    malformed: bool,
    unreadable: bool,
    calls: RefCell<Vec<String>>,
}

impl Replies {
    fn new(sim: &SimHandle) -> Self {
        Self {
            inner: sim.lan(),
            lose: None,
            lost: Cell::new(false),
            malformed: false,
            unreadable: false,
            calls: RefCell::new(Vec::new()),
        }
    }

    fn count(&self, action: &str) -> usize {
        self.calls
            .borrow()
            .iter()
            .filter(|name| *name == action)
            .count()
    }

    fn assert_no_play(&self) {
        assert_eq!(self.count("Play"), 0);
        for action in [
            "SetVolume",
            "SetMute",
            "SetGroupVolume",
            "RemoveAllTracksFromQueue",
        ] {
            assert_eq!(self.count(action), 0, "unexpected {action}");
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
        let name = action
            .trim_matches('"')
            .rsplit('#')
            .next()
            .unwrap_or(action);
        self.calls.borrow_mut().push(name.into());
        if self.unreadable && self.lost.get() && name.starts_with("Get") {
            return Err(ProtoError::Malformed("injected unreadable state".into()));
        }
        let answer = self.inner.soap_post(host, path, action, body)?;
        if self.lose == Some(name) && !self.lost.replace(true) {
            if self.malformed {
                return Ok("<incomplete>".into());
            }
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "injected lost load reply".into(),
            });
        }
        Ok(answer)
    }
}

#[test]
fn native_and_spotify_tracks_load_silently_without_touching_the_queue_or_levels() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        for spotify in [false, true] {
            let replies = Replies::new(&f.sim);
            let before = browse_snapshot(&f.sim.lan(), f.host, QUEUE).unwrap();
            if spotify {
                control::load_spotify_track(
                    &replies,
                    &f.houses,
                    &f.id,
                    "spotify:track:0SimPreparedTrack00001",
                    "Prepared",
                )
                .unwrap();
            } else {
                control::load_uri(&replies, &f.houses, &f.id, NEW, "").unwrap();
            }
            f.assert_silent();
            replies.assert_no_play();
            assert_eq!(replies.count("SetAVTransportURI"), 1);
            assert_eq!(
                browse_snapshot(&f.sim.lan(), f.host, QUEUE).unwrap(),
                before
            );
            let media = soap::get_media_info(&f.sim.lan(), f.host).unwrap();
            if spotify {
                assert_eq!(
                    spotify_uri_from_renderer_uri(&media.uri).as_deref(),
                    Some("spotify:track:0SimPreparedTrack00001")
                );
            } else {
                assert_eq!(media.uri, NEW);
            }
            control::resume(&f.sim.lan(), &f.houses, &f.id).unwrap();
            assert_eq!(
                soap::get_transport_info(&f.sim.lan(), f.host)
                    .unwrap()
                    .state,
                TransportState::Playing
            );
        }
    }
}

#[test]
fn every_source_and_seek_reply_can_be_lost_without_playing_or_repeating() {
    for generation in [1, 2] {
        for stage in ["SetAVTransportURI", "Seek"] {
            for malformed in [false, true] {
                let f = Fixture::new(generation);
                let mut replies = Replies::new(&f.sim);
                replies.lose = Some(stage);
                replies.malformed = malformed;
                control::load_queue_from(&replies, &f.houses, &f.id, 1).unwrap();
                f.assert_silent();
                replies.assert_no_play();
                assert_eq!(replies.count(stage), 1);
                assert_eq!(
                    soap::get_position_info(&f.sim.lan(), f.host).unwrap().track,
                    1
                );
            }
        }
    }
}

#[test]
fn ignored_source_selection_never_seeks_the_old_playing_queue() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        f.sim.ignore_action("Room", "SetAVTransportURI").unwrap();
        let replies = Replies::new(&f.sim);
        assert!(control::load_queue_from(&replies, &f.houses, &f.id, 1).is_err());
        assert_eq!(replies.count("Seek"), 0);
        replies.assert_no_play();
        assert_eq!(
            soap::get_position_info(&f.sim.lan(), f.host).unwrap().track,
            2
        );
    }
}

#[test]
fn ordinary_loading_keeps_the_mode_and_ordered_loading_changes_it_before_seeking() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let lan = f.sim.lan();
        fsonos_proto::play_mode::set_play_mode(&lan, f.host, PlayMode::Shuffle).unwrap();
        let replies = Replies::new(&f.sim);
        control::load_queue_from(&replies, &f.houses, &f.id, 1).unwrap();
        assert_eq!(
            fsonos_proto::play_mode::get_play_mode(&lan, f.host).unwrap(),
            PlayMode::Shuffle
        );
        assert_eq!(replies.count("SetPlayMode"), 0);
        control::load_queue_from_in_order(&replies, &f.houses, &f.id, 2).unwrap();
        assert_eq!(
            fsonos_proto::play_mode::get_play_mode(&lan, f.host).unwrap(),
            PlayMode::Normal
        );
        f.assert_silent();
        replies.assert_no_play();
    }
}

#[test]
fn ignored_ordering_is_not_a_successful_preparation() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let lan = f.sim.lan();
        fsonos_proto::play_mode::set_play_mode(&lan, f.host, PlayMode::Shuffle).unwrap();
        f.sim.ignore_action("Room", "SetPlayMode").unwrap();
        let replies = Replies::new(&f.sim);
        assert!(control::load_queue_from_in_order(&replies, &f.houses, &f.id, 2).is_err());
        assert_eq!(replies.count("Seek"), 0);
        replies.assert_no_play();
    }
}

#[test]
fn album_and_playlist_preparation_pauses_before_any_new_track_can_enter_the_queue() {
    for generation in [1, 2] {
        for kind in [SpotifyContainer::Album, SpotifyContainer::Playlist] {
            let f = Fixture::new(generation);
            let replies = Replies::new(&f.sim);
            let uri = if kind == SpotifyContainer::Album {
                "spotify:album:0SimPreparedAlbum00001"
            } else {
                "spotify:playlist:0SimPreparedList000001"
            };
            control::load_spotify_container(&replies, &f.houses, &f.id, kind, uri, "Prepared")
                .unwrap();
            f.assert_silent();
            replies.assert_no_play();
            assert_eq!(replies.count("AddURIToQueue"), 1);
            assert_eq!(replies.count("RemoveTrackRangeFromQueue"), 1);
            let calls = replies.calls.borrow();
            let pause = calls.iter().position(|name| name == "Pause").unwrap();
            let append = calls
                .iter()
                .position(|name| name == "AddURIToQueue")
                .unwrap();
            assert!(pause < append);
            let queue = browse_snapshot(&f.sim.lan(), f.host, QUEUE).unwrap();
            assert_eq!(queue.objects.len(), 3);
            assert_eq!(
                soap::get_position_info(&f.sim.lan(), f.host).unwrap().track,
                1
            );
        }
    }
}

#[test]
fn a_failed_or_ignored_pause_never_appends_or_removes_music() {
    for generation in [1, 2] {
        for ignored in [false, true] {
            let f = Fixture::new(generation);
            if ignored {
                f.sim.ignore_action("Room", "Pause").unwrap();
            } else {
                f.sim.upnp_fault("Room", "Pause", 800).unwrap();
            }
            let replies = Replies::new(&f.sim);
            assert!(
                control::load_spotify_container(
                    &replies,
                    &f.houses,
                    &f.id,
                    SpotifyContainer::Album,
                    "spotify:album:0SimPreparedAlbum00001",
                    "Prepared",
                )
                .is_err()
            );
            assert_eq!(replies.count("Pause"), 1);
            assert_eq!(replies.count("AddURIToQueue"), 0);
            assert_eq!(replies.count("RemoveTrackRangeFromQueue"), 0);
            replies.assert_no_play();
        }
    }
}

#[test]
fn lost_pause_and_removal_replies_are_confirmed_without_replaying_the_replacement() {
    for generation in [1, 2] {
        for stage in ["Pause", "RemoveTrackRangeFromQueue", "SetAVTransportURI"] {
            let f = Fixture::new(generation);
            let mut replies = Replies::new(&f.sim);
            replies.lose = Some(stage);
            control::load_spotify_container(
                &replies,
                &f.houses,
                &f.id,
                SpotifyContainer::Album,
                "spotify:album:0SimPreparedAlbum00001",
                "Prepared",
            )
            .unwrap();
            f.assert_silent();
            replies.assert_no_play();
            assert_eq!(replies.count(stage), 1);
            assert_eq!(replies.count("AddURIToQueue"), 1);
        }
    }
}

#[test]
fn missing_slots_empty_sources_and_former_coordinators_do_not_change_playback() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let replies = Replies::new(&f.sim);
        assert!(control::load_uri(&replies, &f.houses, &f.id, " ", "").is_err());
        assert!(control::load_uri(&replies, &f.houses, &f.id, "x-rincon:Peer", "").is_err());
        assert!(control::load_queue_from(&replies, &f.houses, &f.id, 0).is_err());
        assert!(control::load_queue_from(&replies, &f.houses, &f.id, 99).is_err());
        let peer = PlayerId(f.sim.player("Peer").unwrap().uuid.clone());
        soap::join_group(&f.sim.lan(), f.host, &peer).unwrap();
        assert!(control::load_uri(&replies, &f.houses, &f.id, NEW, "").is_err());
        assert_eq!(replies.count("SetAVTransportURI"), 0);
        assert_eq!(replies.count("Pause"), 0);
        replies.assert_no_play();
    }
}

#[test]
fn unconfirmed_loading_stops_at_the_failed_stage_and_never_resumes() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let mut replies = Replies::new(&f.sim);
        replies.lose = Some("SetAVTransportURI");
        replies.unreadable = true;
        assert!(control::load_queue_from(&replies, &f.houses, &f.id, 1).is_err());
        assert_eq!(replies.count("SetAVTransportURI"), 1);
        assert_eq!(replies.count("Seek"), 0);
        replies.assert_no_play();
    }
}
