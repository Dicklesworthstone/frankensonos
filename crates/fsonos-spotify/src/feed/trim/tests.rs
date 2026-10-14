//! Queue maintenance over real S1/S2 sockets, with faults at the write boundary.

use super::*;
use crate::dj::{DjConfig, PickReason, WorkPool};
use crate::feed::{Movement, Planning};
use fsonos_core::HouseholdState;
use fsonos_core::store::MemStore;
use fsonos_proto::didl::{spotify_queue_uri, spotify_track_didl};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::cell::Cell;

fn uri(n: u32) -> String {
    format!("spotify:track:{n:022}")
}

fn work(first: u32, uris: &[String]) -> QueuedWork {
    QueuedWork {
        title: "Test work".into(),
        composer: "Test artist".into(),
        work_key: format!("work-{first}"),
        movements: uris
            .iter()
            .enumerate()
            .map(|(i, uri)| Movement {
                uri: uri.clone(),
                title: "Movement".into(),
                position: first.checked_add(u32::try_from(i).unwrap()),
                added: true,
            })
            .collect(),
        reason: PickReason {
            factors: Vec::new(),
            relaxed: Vec::new(),
            summary: "Test selection".into(),
        },
    }
}

fn config() -> DjConfig {
    DjConfig {
        trim_queue_at: 0,
        keep_played: 1,
        ..DjConfig::default()
    }
}

struct Fixture {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
    coordinator: PlayerId,
    host: IpAddr,
    uris: Vec<String>,
    count: u32,
}

impl Fixture {
    fn new(generation: u8, count: u32) -> Self {
        let builder = SimHousehold::builder();
        let builder = if generation == 1 {
            builder.s1([SimPlayerSpec::new("Room", SimModel::Play5Gen1)])
        } else {
            builder.s2([SimPlayerSpec::new("Room", SimModel::One)])
        };
        let sim = builder.spawn().unwrap();
        let host = sim.player("Room").unwrap().ip;
        let coordinator = PlayerId(sim.player("Room").unwrap().uuid.clone());
        let lan = sim.lan();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        let params = sim.render_params(generation).unwrap();
        let mut uris = vec![uri(999)];
        uris.extend((1..=count).map(uri));
        // The identical second copy is the owner's, not the DJ's to remove.
        uris.extend((1..=count).map(uri));
        uris.extend([uri(count + 1), uri(count + 2)]);
        for uri in &uris {
            soap::add_uri_to_queue(
                &lan,
                host,
                &spotify_queue_uri(uri),
                &spotify_track_didl(uri, "Test track", &params),
                false,
            )
            .unwrap();
        }
        soap::play_from_queue(&lan, host, &coordinator).unwrap();
        soap::seek_track(&lan, host, 2 * count + 2).unwrap();
        soap::play(&lan, host).unwrap();
        Self {
            sim,
            houses: vec![house],
            coordinator,
            host,
            uris,
            count,
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
        let count = usize::try_from(self.count).unwrap();
        let playing = 2 * self.count + 2;
        let mut feed = QueueFeed::new(&self.coordinator, config(), 7);
        feed.active = true;
        feed.played = vec![work(2, &self.uris[1..1 + count])];
        feed.queued = vec![
            work(playing, &[uri(self.count + 1)]),
            work(playing + 1, &[uri(self.count + 2)]),
        ];
        feed.playing = Some(playing);
        feed.recorded = Some((playing, uri(self.count + 1)));
        feed.queue_len = Some(u32::try_from(self.uris.len()).unwrap());
        feed
    }

    fn queue(&self) -> Vec<String> {
        read(&self.sim.lan(), self.host)
            .unwrap()
            .uris
            .iter()
            .map(|uri| spotify_uri_from_renderer_uri(uri).unwrap_or_else(|| uri.clone()))
            .collect()
    }

    fn after_trim(&self) -> Vec<String> {
        let mut expected = self.uris.clone();
        expected.drain(1..1 + usize::try_from(self.count).unwrap());
        expected
    }
}

#[derive(Clone, Copy)]
enum Fault {
    None,
    ConcurrentEdit,
    BeforeDelete,
    LostAndUnreadable,
    LostReply,
    MalformedReply,
    EditedAfterRemoval,
    WrongRemoval,
    Preflight,
    ZeroRevision,
}

struct Replies {
    inner: SimLan,
    fault: Cell<Fault>,
    removes: Cell<usize>,
    unreadable: Cell<bool>,
}

impl Replies {
    fn new(sim: &SimHandle, fault: Fault) -> Self {
        Self {
            inner: sim.lan(),
            fault: Cell::new(fault),
            removes: Cell::new(0),
            unreadable: Cell::new(false),
        }
    }

    fn lost(host: IpAddr) -> ProtoError {
        ProtoError::Network {
            target: host.to_string(),
            detail: "injected queue removal failure".into(),
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
        if action.contains("#Browse") {
            if self.unreadable.get() || matches!(self.fault.get(), Fault::Preflight) {
                return Err(Self::lost(host));
            }
            let mut response = self.inner.soap_post(host, path, action, body)?;
            if matches!(self.fault.get(), Fault::ZeroRevision) {
                let start = response.find("<UpdateID>").unwrap() + "<UpdateID>".len();
                let end = response.find("</UpdateID>").unwrap();
                response.replace_range(start..end, "0");
            }
            return Ok(response);
        }
        if !action.contains("#RemoveTrackRangeFromQueue") {
            return self.inner.soap_post(host, path, action, body);
        }
        self.removes.set(self.removes.get() + 1);
        let fault = self.fault.replace(Fault::None);
        if matches!(fault, Fault::BeforeDelete) {
            return Err(Self::lost(host));
        }
        if matches!(fault, Fault::ConcurrentEdit) {
            soap::enqueue_uri(
                &self.inner,
                host,
                "x-file-cifs://example.invalid/owner.flac",
                "",
                1,
                false,
            )?;
        }
        if matches!(fault, Fault::WrongRemoval) {
            // An unrelated edit of the same length is not this operation's
            // result: this one removes the owner's prefix and one movement.
            let snapshot = read(&self.inner, host).unwrap();
            soap::remove_track_range_from_queue(&self.inner, host, snapshot.revision, 1, 2)?;
            return Err(Self::lost(host));
        }
        let response = self.inner.soap_post(host, path, action, body)?;
        if matches!(fault, Fault::EditedAfterRemoval) {
            soap::enqueue_uri(
                &self.inner,
                host,
                "x-file-cifs://example.invalid/new-owner.flac",
                "",
                1,
                false,
            )?;
            return Err(Self::lost(host));
        }
        if matches!(fault, Fault::LostReply) {
            return Err(Self::lost(host));
        }
        if matches!(fault, Fault::MalformedReply) {
            return Ok("<broken-removal-response>".into());
        }
        if matches!(fault, Fault::LostAndUnreadable) {
            self.unreadable.set(true);
            return Err(Self::lost(host));
        }
        Ok(response)
    }
}

#[test]
fn cleanup_uses_the_observed_revision_and_preserves_the_owners_copy() {
    for generation in [1, 2] {
        let f = Fixture::new(generation, 2);
        let replies = Replies::new(&f.sim, Fault::None);
        let revision = read(&f.sim.lan(), f.host).unwrap().revision.to_string();
        let mut feed = f.feed();
        feed.trim(&f.speakers(&replies)).unwrap();
        assert_eq!(f.queue(), f.after_trim());
        assert_eq!(replies.removes.get(), 1);
        assert!(feed.played.is_empty());
        assert_eq!(feed.playing, Some(4));
        assert_eq!(feed.recorded, Some((4, uri(3))));
        assert_eq!(feed.current().unwrap().track_at(4), Some(uri(3).as_str()));
        let log = f.sim.soap_log();
        let removal = log
            .iter()
            .find(|entry| entry.action == "RemoveTrackRangeFromQueue")
            .unwrap();
        assert!(removal.args.contains(&("UpdateID".into(), revision)));
        assert_eq!(
            soap::get_transport_info(&f.sim.lan(), f.host)
                .unwrap()
                .state,
            TransportState::Playing
        );
    }
}

#[test]
fn concurrent_insertion_is_rejected_instead_of_removing_shifted_positions() {
    for generation in [1, 2] {
        let f = Fixture::new(generation, 2);
        let replies = Replies::new(&f.sim, Fault::ConcurrentEdit);
        let mut feed = f.feed();
        assert!(feed.trim(&f.speakers(&replies)).is_err());
        let queue = f.queue();
        assert_eq!(&queue[1..], f.uris.as_slice());
        assert_eq!(replies.removes.get(), 1);
        assert!(feed.is_active());
        assert!(feed.queue_len.is_some(), "the changed queue was reconciled");
    }
}

#[test]
fn a_work_larger_than_a_browse_page_is_trimmed_whole() {
    for generation in [1, 2] {
        let f = Fixture::new(generation, 110);
        let replies = Replies::new(&f.sim, Fault::None);
        f.feed().trim(&f.speakers(&replies)).unwrap();
        assert_eq!(f.queue(), f.after_trim());
        assert_eq!(replies.removes.get(), 1);
    }
}

#[test]
fn a_backward_seek_or_another_source_prevents_stale_event_cleanup() {
    for generation in [1, 2] {
        for another_source in [false, true] {
            let f = Fixture::new(generation, 2);
            let replies = Replies::new(&f.sim, Fault::None);
            let mut feed = f.feed();
            if another_source {
                soap::set_av_transport_uri(
                    &f.sim.lan(),
                    f.host,
                    "x-rincon-mp3radio://example.invalid/radio.mp3",
                    "",
                )
                .unwrap();
            } else {
                soap::seek_track(&f.sim.lan(), f.host, 2).unwrap();
            }
            feed.trim(&f.speakers(&replies)).unwrap();
            assert_eq!(replies.removes.get(), 0);
            assert_eq!(f.queue(), f.uris);
        }
    }
}

#[test]
fn uncertain_deletions_drop_old_claims_even_across_a_restart() {
    for generation in [1, 2] {
        for fault in [Fault::BeforeDelete, Fault::LostAndUnreadable] {
            let f = Fixture::new(generation, 2);
            let replies = Replies::new(&f.sim, fault);
            let mut feed = f.feed();
            assert!(feed.trim(&f.speakers(&replies)).is_err());
            assert!(feed.played.is_empty());
            let snapshot = feed.snapshot().unwrap();
            assert!(snapshot.played.is_empty());
            let saved = serde_json::to_string(&snapshot).unwrap();
            let mut restored =
                QueueFeed::restore(serde_json::from_str(&saved).unwrap(), config(), 8);
            replies.unreadable.set(false);
            restored.resync(&f.speakers(&replies)).unwrap();
            restored.trim(&f.speakers(&replies)).unwrap();
            assert_eq!(replies.removes.get(), 1, "never repeat the removal");
            let expected = if matches!(fault, Fault::BeforeDelete) {
                f.uris.clone()
            } else {
                f.after_trim()
            };
            assert_eq!(f.queue(), expected);
        }
    }
}

#[test]
fn failed_readback_defers_top_up_until_positions_can_be_reconciled() {
    let f = Fixture::new(2, 2);
    let replies = Replies::new(&f.sim, Fault::Preflight);
    let mut feed = f.feed();
    assert!(feed.trim(&f.speakers(&replies)).is_err());
    assert_eq!(feed.queue_len, None);
    let pool = WorkPool::default();
    let plan = Planning {
        pool: &pool,
        steer: None,
        feedback: None,
        now: 0,
        local_hour: None,
    };
    assert_eq!(
        feed.top_up(&f.speakers(&replies), plan, &MemStore::default())
            .unwrap(),
        0
    );
    assert_eq!(replies.removes.get(), 0);
    assert_eq!(f.queue(), f.uris);
}

#[test]
fn revision_zero_never_disables_the_speakers_compare_check() {
    let f = Fixture::new(2, 2);
    let replies = Replies::new(&f.sim, Fault::ZeroRevision);
    f.feed().trim(&f.speakers(&replies)).unwrap();
    assert_eq!(replies.removes.get(), 0);
    assert_eq!(f.queue(), f.uris);
}

#[test]
fn only_complete_contiguous_nonzero_ranges_are_removable() {
    let complete = work(2, &[uri(1), uri(2)]);
    assert_eq!(range(&complete, 4), Some((2, 2)));
    assert_eq!(range(&complete, 3), None);
    for positions in [
        vec![Some(0), Some(1)],
        vec![Some(2), Some(4)],
        vec![Some(3), Some(2)],
        vec![Some(2), None],
    ] {
        let mut invalid = complete.clone();
        for (movement, position) in invalid.movements.iter_mut().zip(positions) {
            movement.position = position;
        }
        assert_eq!(range(&invalid, 10), None);
    }
    let mut partial = complete.clone();
    partial.movements[1].added = false;
    assert_eq!(range(&partial, 10), None);
    assert_eq!(range(&work(u32::MAX, &[uri(1), uri(2)]), u32::MAX), None);
    assert_eq!(range(&work(1, &[]), 10), None);
}

fn queue(revision: u32, uris: &[&str]) -> Queue {
    Queue {
        revision,
        uris: uris.iter().map(|uri| (*uri).into()).collect(),
    }
}

#[test]
fn exact_removals_preserve_every_other_uri_and_require_a_changed_revision() {
    let before = queue(7, &["owner", "a", "b", "a", "b", "playing"]);
    let after = queue(8, &["owner", "a", "b", "playing"]);
    assert!(removed_range(&before, &after, 1, 2));
    assert!(!removed_range(
        &before,
        &queue(7, &["owner", "a", "b", "playing"]),
        1,
        2,
    ));
    assert!(removed_range(&queue(7, &["a", "b"]), &queue(8, &[]), 0, 2));
    assert!(removed_range(
        &queue(u32::MAX, &["a", "b"]),
        &queue(0, &["b"]),
        0,
        1,
    ));
}

#[test]
fn wrong_removals_or_changed_neighbors_cannot_confirm_a_lost_reply() {
    let before = queue(7, &["owner", "a", "b", "next"]);
    for after in [
        queue(8, &["b", "next"]),
        queue(8, &["owner", "a"]),
        queue(8, &["changed-owner", "next"]),
        queue(8, &["next", "owner"]),
        queue(8, &["owner", "a", "b", "next"]),
        queue(8, &["owner"]),
    ] {
        assert!(!removed_range(&before, &after, 1, 2));
    }
    // Do not collapse renderer/account parameters while comparing neighbors.
    assert!(!removed_range(
        &queue(7, &["x-sonos-spotify:track?sn=1", "a"]),
        &queue(8, &["x-sonos-spotify:track?sn=2"]),
        1,
        1,
    ));
    for (start, count) in [(0, 0), (5, 1), (usize::MAX, 1), (1, usize::MAX)] {
        assert!(!removed_range(&before, &queue(8, &[]), start, count));
    }
}

#[test]
fn lost_or_malformed_removal_replies_complete_without_replaying_the_delete() {
    for generation in [1, 2] {
        for fault in [Fault::LostReply, Fault::MalformedReply] {
            let f = Fixture::new(generation, 2);
            let replies = Replies::new(&f.sim, fault);
            let mut feed = f.feed();
            feed.trim(&f.speakers(&replies)).unwrap();
            assert_eq!(replies.removes.get(), 1);
            assert_eq!(f.queue(), f.after_trim());
            assert_eq!(feed.playing, Some(4));
            assert_eq!(feed.recorded, Some((4, uri(3))));
            assert!(feed.played.is_empty());
            assert!(feed.is_active());
            let saved = serde_json::to_string(&feed.snapshot().unwrap()).unwrap();
            let mut restored =
                QueueFeed::restore(serde_json::from_str(&saved).unwrap(), config(), 8);
            restored.resync(&f.speakers(&replies)).unwrap();
            restored.trim(&f.speakers(&replies)).unwrap();
            assert_eq!(replies.removes.get(), 1);
            assert_eq!(
                f.queue(),
                f.after_trim(),
                "the owner's copy stays after restart"
            );
        }
    }
}

#[test]
fn an_unrelated_edit_after_a_lost_reply_cannot_retain_old_cleanup_claims() {
    for generation in [1, 2] {
        for fault in [Fault::EditedAfterRemoval, Fault::WrongRemoval] {
            let f = Fixture::new(generation, 2);
            let replies = Replies::new(&f.sim, fault);
            let mut feed = f.feed();
            assert!(feed.trim(&f.speakers(&replies)).is_err());
            assert_eq!(
                feed.playing, None,
                "do not invent a position after an uncertain edit"
            );
            assert!(feed.played.is_empty());
            let left = f.queue();
            feed.resync(&f.speakers(&replies)).unwrap();
            feed.playing = Some(soap::get_position_info(&f.sim.lan(), f.host).unwrap().track);
            feed.trim(&f.speakers(&replies)).unwrap();
            assert_eq!(replies.removes.get(), 1);
            assert_eq!(f.queue(), left);
        }
    }
}

#[test]
fn a_confirmed_lost_reply_allows_other_old_works_to_be_retired_once() {
    for generation in [1, 2] {
        let f = Fixture::new(generation, 2);
        let replies = Replies::new(&f.sim, Fault::LostReply);
        let mut feed = f.feed();
        feed.played = vec![work(2, &[uri(1)]), work(3, &[uri(2)])];
        feed.trim(&f.speakers(&replies)).unwrap();
        assert_eq!(
            replies.removes.get(),
            2,
            "one removal per work, not a replay"
        );
        assert_eq!(f.queue(), f.after_trim());
        assert_eq!(feed.playing, Some(4));
        assert!(feed.played.is_empty());
    }
}

#[test]
fn playback_keeps_feeding_and_recording_once_after_a_confirmed_lost_trim_reply() {
    use crate::test_shelf::{MIDNIGHT, song_items, works_of};
    use fsonos_core::playback::PlayerPlayback;
    use fsonos_core::store::Store;

    for generation in [1, 2] {
        let f = Fixture::new(generation, 2);
        let replies = Replies::new(&f.sim, Fault::LostReply);
        let mut feed = f.feed();
        let mut store = MemStore::default();
        store
            .record_play(&f.coordinator.0, &uri(3), MIDNIGHT)
            .unwrap();
        let pool = works_of(&song_items());
        let plan = Planning {
            pool: &pool,
            steer: None,
            feedback: None,
            now: MIDNIGHT,
            local_hour: Some(20),
        };
        let state = || {
            let position = soap::get_position_info(&f.sim.lan(), f.host).unwrap();
            let mut state = PlayerPlayback::default();
            state.transport = Some(TransportState::Playing);
            state.track_uri = Some(position.uri);
            state.queue_position = Some(position.track);
            state.queue_length = Some(u32::try_from(f.queue().len()).unwrap());
            state
        };
        let old_event = state();
        assert_eq!(
            feed.on_playback(&f.speakers(&replies), plan, &mut store, &old_event)
                .unwrap(),
            0
        );
        assert!(feed.take_trim_error().is_none());
        // The old event can still be buffered after the queue renumbered.
        assert_eq!(
            feed.on_playback(&f.speakers(&replies), plan, &mut store, &old_event)
                .unwrap(),
            0
        );
        assert_eq!(store.recent_plays(None, 20).unwrap().len(), 1);
        soap::next(&f.sim.lan(), f.host).unwrap();
        assert_eq!(
            feed.on_playback(&f.speakers(&replies), plan, &mut store, &state())
                .unwrap(),
            1
        );
        assert_eq!(store.recent_plays(None, 20).unwrap().len(), 2);
        assert_eq!(replies.removes.get(), 1);
        assert!(feed.is_active());
        let queue = f.queue();
        assert_eq!(&queue[..3], &[uri(999), uri(1), uri(2)]);
        let current = soap::get_position_info(&f.sim.lan(), f.host).unwrap().track;
        assert!(
            feed.queued()
                .iter()
                .any(|work| work.first().is_some_and(|at| at > current))
        );
    }
}
