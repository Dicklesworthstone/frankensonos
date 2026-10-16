//! The long-running DJ uses the same deletion proof as manual edits and undo.

use super::*;
use crate::dj::{DjConfig, PickReason};
use crate::feed::Movement;
use fsonos_core::HouseholdState;
use fsonos_proto::didl::{spotify_queue_uri, spotify_track_didl};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::cell::Cell;

fn uri(index: u32) -> String {
    format!("spotify:track:{index:022}")
}

fn work(first: u32, tracks: &[u32]) -> QueuedWork {
    QueuedWork {
        title: "Synthetic work".into(),
        composer: "Synthetic artist".into(),
        work_key: format!("synthetic-{first}"),
        movements: tracks
            .iter()
            .enumerate()
            .map(|(offset, track)| Movement {
                uri: uri(*track),
                title: "Synthetic movement".into(),
                position: first.checked_add(u32::try_from(offset).unwrap()),
                added: true,
            })
            .collect(),
        reason: PickReason {
            factors: Vec::new(),
            relaxed: Vec::new(),
            summary: "Synthetic selection".into(),
        },
    }
}

struct Fixture {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
    coordinator: PlayerId,
    peer: PlayerId,
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
        let host = sim.player("Room").unwrap().ip;
        let coordinator = PlayerId(sim.player("Room").unwrap().uuid.clone());
        let peer = PlayerId(sim.player("Peer").unwrap().uuid.clone());
        let lan = sim.lan();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        let params = sim.render_params(generation).unwrap();
        // The third entry duplicates the first but belongs to the owner.
        for track in [1, 2, 1, 4, 5] {
            soap::add_uri_to_queue(
                &lan,
                host,
                &spotify_queue_uri(&uri(track)),
                &spotify_track_didl(&uri(track), "Synthetic track", &params),
                false,
            )
            .unwrap();
        }
        soap::play_from_queue(&lan, host, &coordinator).unwrap();
        soap::seek_track(&lan, host, 4).unwrap();
        soap::play(&lan, host).unwrap();
        Self {
            sim,
            houses: vec![house],
            coordinator,
            peer,
            host,
        }
    }

    fn speakers<'a, T: Transport + ?Sized>(&'a self, transport: &'a T) -> Speakers<'a, T> {
        Speakers {
            transport,
            households: &self.houses,
            coordinator: &self.coordinator,
        }
    }

    fn feed(&self) -> QueueFeed {
        let mut feed = QueueFeed::new(
            &self.coordinator,
            DjConfig {
                trim_queue_at: 0,
                keep_played: 1,
                ..DjConfig::default()
            },
            7,
        );
        feed.active = true;
        feed.played = vec![work(1, &[1, 2])];
        feed.queued = vec![work(4, &[4]), work(5, &[5])];
        feed.playing = Some(4);
        feed.recorded = Some((4, uri(4)));
        feed.queue_len = Some(5);
        feed
    }
}

#[derive(Clone, Copy)]
enum Fault {
    None,
    AcknowledgedNoChange,
    AcknowledgedExtraEdit,
    AcknowledgedUnreadable,
    RefusedAfterEdit,
    CoordinatorBefore,
    CoordinatorAfter,
}

struct Replies {
    inner: SimLan,
    peer: PlayerId,
    fault: Cell<Fault>,
    removes: Cell<usize>,
    unreadable: Cell<bool>,
}

impl Replies {
    fn new(fixture: &Fixture, fault: Fault) -> Self {
        Self {
            inner: fixture.sim.lan(),
            peer: fixture.peer.clone(),
            fault: Cell::new(fault),
            removes: Cell::new(0),
            unreadable: Cell::new(false),
        }
    }

    fn join_peer(&self, host: IpAddr) -> Result<(), ProtoError> {
        soap::set_av_transport_uri(&self.inner, host, &format!("x-rincon:{}", self.peer.0), "")
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
        // The SOAPACTION header arrives quoted: "urn:...:AVTransport:1#Name".
        let name = action.trim_matches('"');
        if name.ends_with("#Browse") && self.unreadable.get() {
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "synthetic read-back failure".into(),
            });
        }
        if name.ends_with("#GetZoneGroupState")
            && matches!(self.fault.get(), Fault::CoordinatorBefore)
        {
            self.fault.set(Fault::None);
            self.join_peer(host)?;
        }
        if !name.ends_with("#RemoveTrackRangeFromQueue") {
            return self.inner.soap_post(host, path, action, body);
        }
        self.removes.set(self.removes.get() + 1);
        let fault = self.fault.replace(Fault::None);
        if matches!(fault, Fault::AcknowledgedNoChange) {
            return Ok(concat!(
                "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>",
                "<u:RemoveTrackRangeFromQueueResponse xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\"/>",
                "</s:Body></s:Envelope>"
            )
            .into());
        }
        let response = self.inner.soap_post(host, path, action, body)?;
        match fault {
            Fault::AcknowledgedExtraEdit => {
                soap::enqueue_uri(
                    &self.inner,
                    host,
                    "x-file-cifs://example.invalid/owner.flac",
                    "",
                    1,
                    false,
                )?;
            }
            Fault::AcknowledgedUnreadable => self.unreadable.set(true),
            Fault::RefusedAfterEdit => {
                // The same edit by another actor cannot turn a refusal into
                // this request's acknowledged success.
                return Err(ProtoError::SoapFault {
                    code: 800,
                    reason: "synthetic refusal after an external edit".into(),
                });
            }
            Fault::CoordinatorAfter => self.join_peer(host)?,
            _ => {}
        }
        Ok(response)
    }
}

#[test]
fn acknowledged_cleanup_preserves_playback_and_owner_duplicates() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let replies = Replies::new(&f, Fault::None);
        let mut feed = f.feed();
        feed.trim(&f.speakers(&replies)).unwrap();
        let queue = read(&f.sim.lan(), f.host).unwrap();
        assert_eq!(queue.uris.len(), 3);
        assert!(same_track(&queue.uris[0], &uri(1)));
        assert_eq!(replies.removes.get(), 1);
        assert!(feed.played.is_empty());
        assert_eq!(feed.playing, Some(2));
        assert_eq!(feed.recorded, Some((2, uri(4))));
        assert_eq!(
            soap::get_transport_info(&f.sim.lan(), f.host)
                .unwrap()
                .state,
            TransportState::Playing
        );
    }
}

#[test]
fn success_reply_without_the_selected_change_is_uncertain_not_complete() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let replies = Replies::new(&f, Fault::AcknowledgedNoChange);
        let mut feed = f.feed();
        assert!(feed.trim(&f.speakers(&replies)).is_err());
        assert_eq!(read(&f.sim.lan(), f.host).unwrap().uris.len(), 5);
        assert!(feed.played.is_empty(), "unproven ownership is relinquished");
        assert_eq!(feed.playing, None);
        feed.resync(&f.speakers(&replies)).unwrap();
        feed.trim(&f.speakers(&replies)).unwrap();
        assert_eq!(replies.removes.get(), 1, "no delete replay");
    }
}

#[test]
fn success_followed_by_an_extra_edit_drops_cleanup_claims_before_restart() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let replies = Replies::new(&f, Fault::AcknowledgedExtraEdit);
        let mut feed = f.feed();
        assert!(feed.trim(&f.speakers(&replies)).is_err());
        assert!(feed.snapshot().unwrap().played.is_empty());
        assert_eq!(feed.playing, None);
        let queue = read(&f.sim.lan(), f.host).unwrap();
        assert_eq!(queue.uris.len(), 4);
        assert!(same_track(&queue.uris[1], &uri(1)));
        assert_eq!(replies.removes.get(), 1);
    }
}

#[test]
fn unreadable_acknowledged_delete_keeps_top_up_unreconciled() {
    let f = Fixture::new(2);
    let replies = Replies::new(&f, Fault::AcknowledgedUnreadable);
    let mut feed = f.feed();
    assert!(feed.trim(&f.speakers(&replies)).is_err());
    assert!(feed.played.is_empty());
    assert_eq!(feed.queue_len, None);
    replies.unreadable.set(false);
    feed.resync(&f.speakers(&replies)).unwrap();
    feed.trim(&f.speakers(&replies)).unwrap();
    assert_eq!(replies.removes.get(), 1);
}

#[test]
fn coordinator_change_is_not_followed_to_a_different_queue() {
    for generation in [1, 2] {
        for fault in [Fault::CoordinatorBefore, Fault::CoordinatorAfter] {
            let f = Fixture::new(generation);
            let replies = Replies::new(&f, fault);
            let mut feed = f.feed();
            assert!(feed.trim(&f.speakers(&replies)).is_err());
            let sent = usize::from(matches!(fault, Fault::CoordinatorAfter));
            assert_eq!(replies.removes.get(), sent);
            if sent > 0 {
                assert!(feed.played.is_empty());
                assert_eq!(feed.playing, None);
            }
        }
    }
}

#[test]
fn an_explicit_refusal_is_not_reclassified_from_someone_elses_edit() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let replies = Replies::new(&f, Fault::RefusedAfterEdit);
        let mut feed = f.feed();
        assert!(feed.trim(&f.speakers(&replies)).is_err());
        assert_eq!(replies.removes.get(), 1);
    }
}

#[test]
fn adjacent_song_claims_are_removed_in_one_verified_request() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let replies = Replies::new(&f, Fault::None);
        let mut feed = f.feed();
        feed.played = vec![work(1, &[1]), work(2, &[2])];
        feed.trim(&f.speakers(&replies)).unwrap();
        assert_eq!(replies.removes.get(), 1);
        assert_eq!(read(&f.sim.lan(), f.host).unwrap().uris.len(), 3);
        assert_eq!(feed.playing, Some(2));
        assert!(feed.played.is_empty());
    }
}

#[test]
fn separate_batches_preserve_unowned_music_between_claims() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let replies = Replies::new(&f, Fault::None);
        let mut feed = f.feed();
        feed.config.keep_played = 0;
        feed.played = vec![work(1, &[1]), work(3, &[1])];
        feed.trim(&f.speakers(&replies)).unwrap();
        let after = read(&f.sim.lan(), f.host).unwrap();
        assert_eq!(replies.removes.get(), 2);
        assert_eq!(after.uris.len(), 3);
        assert!(same_track(&after.uris[0], &uri(2)));
        assert_eq!(feed.recorded, Some((2, uri(4))));
    }
}

#[test]
fn an_incomplete_work_is_not_swept_into_its_neighbors_cleanup() {
    let f = Fixture::new(2);
    let replies = Replies::new(&f, Fault::None);
    let mut feed = f.feed();
    feed.config.keep_played = 0;
    let mut incomplete = work(2, &[2]);
    incomplete.movements[0].added = false;
    feed.played = vec![work(1, &[1]), incomplete, work(3, &[1])];
    feed.trim(&f.speakers(&replies)).unwrap();
    let after = read(&f.sim.lan(), f.host).unwrap();
    assert_eq!(after.uris.len(), 3);
    assert!(same_track(&after.uris[0], &uri(2)));
    assert_eq!(replies.removes.get(), 2);
}

fn append_until(f: &Fixture, generation: u8, end: u32) {
    let params = f.sim.render_params(generation).unwrap();
    for track in 6..=end {
        soap::add_uri_to_queue(
            &f.sim.lan(),
            f.host,
            &spotify_queue_uri(&uri(track)),
            &spotify_track_didl(&uri(track), "Synthetic track", &params),
            false,
        )
        .unwrap();
    }
}

fn playing_at(f: &Fixture, position: u32, length: u32) -> QueueFeed {
    soap::seek_track(&f.sim.lan(), f.host, position).unwrap();
    let mut feed = f.feed();
    feed.playing = Some(position);
    feed.recorded = Some((position, uri(position)));
    feed.queue_len = Some(length);
    feed.queued = vec![
        work(position, &[position]),
        work(position + 1, &[position + 1]),
    ];
    feed
}

#[test]
fn a_large_contiguous_history_is_not_rescanned_and_deleted_per_song() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        append_until(&f, generation, 128);
        let replies = Replies::new(&f, Fault::None);
        let mut feed = playing_at(&f, 127, 128);
        feed.played = (6..=125).map(|first| work(first, &[first])).collect();
        feed.trim(&f.speakers(&replies)).unwrap();
        assert_eq!(replies.removes.get(), 1, "120 songs share one removal");
        assert_eq!(read(&f.sim.lan(), f.host).unwrap().uris.len(), 8);
        assert_eq!(feed.playing, Some(7));
        assert_eq!(feed.recorded, Some((7, uri(127))));
        assert!(feed.played.is_empty());
        assert_eq!(
            soap::get_transport_info(&f.sim.lan(), f.host)
                .unwrap()
                .state,
            TransportState::Playing
        );
    }
}

#[test]
fn fragmented_cleanup_yields_after_eight_batches_and_continues_later() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        append_until(&f, generation, 48);
        let replies = Replies::new(&f, Fault::None);
        let mut feed = playing_at(&f, 47, 48);
        feed.played = (6..=44)
            .step_by(2)
            .map(|first| work(first, &[first]))
            .collect();
        assert_eq!(feed.played.len(), 20);
        for (removals, remaining) in [(8, 12), (16, 4), (20, 0)] {
            feed.trim(&f.speakers(&replies)).unwrap();
            assert_eq!(replies.removes.get(), removals);
            assert_eq!(feed.played.len(), remaining);
        }
        let after = read(&f.sim.lan(), f.host).unwrap();
        assert_eq!(after.uris.len(), 28);
        for track in (7..=45).step_by(2) {
            assert!(
                after
                    .uris
                    .iter()
                    .any(|candidate| same_track(candidate, &uri(track)))
            );
        }
        assert_eq!(feed.recorded, Some((27, uri(47))));
        assert!(feed.is_active());
    }
}

#[test]
fn a_changed_song_in_a_batch_does_not_authorize_partial_deletion() {
    let f = Fixture::new(2);
    let replies = Replies::new(&f, Fault::None);
    let mut feed = f.feed();
    feed.played = vec![work(1, &[1]), work(2, &[99])];
    feed.trim(&f.speakers(&replies)).unwrap();
    assert_eq!(replies.removes.get(), 0);
    assert_eq!(read(&f.sim.lan(), f.host).unwrap().uris.len(), 5);
}

#[test]
fn a_backwards_seek_keeps_a_merged_batch_that_touches_the_retained_work() {
    let f = Fixture::new(2);
    let replies = Replies::new(&f, Fault::None);
    let mut feed = f.feed();
    feed.played = vec![work(1, &[1]), work(2, &[2])];
    soap::seek_track(&f.sim.lan(), f.host, 2).unwrap();
    feed.trim(&f.speakers(&replies)).unwrap();
    assert_eq!(replies.removes.get(), 0);
    assert_eq!(read(&f.sim.lan(), f.host).unwrap().uris.len(), 5);
}

#[test]
fn the_no_change_fixture_is_a_successful_soap_acknowledgement() {
    let f = Fixture::new(2);
    let replies = Replies::new(&f, Fault::AcknowledgedNoChange);
    let before = read(&f.sim.lan(), f.host).unwrap();
    soap::remove_track_range_from_queue(&replies, f.host, before.revision, 1, 2).unwrap();
    assert_eq!(read(&f.sim.lan(), f.host).unwrap(), before);
    assert_eq!(replies.removes.get(), 1);
}
