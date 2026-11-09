//! Renderer failures are recovered through real SOAP without replaying an
//! uncertain write or treating skipped music as listening history.

use super::*;
use fsonos_core::playback::PlaybackObservation;
use fsonos_proto::control as transport;
use std::cell::RefCell;

fn sample(rig: &Rig) -> PlayerPlayback {
    let observation = PlaybackObservation {
        transport: transport::get_transport_info(&rig.lan, rig.host()).unwrap(),
        position: transport::get_position_info(&rig.lan, rig.host()).unwrap(),
        media: transport::get_media_info(&rig.lan, rig.host()).unwrap(),
    };
    let mut playback = Playback::default();
    playback.apply_observation(&rig.coordinator, &observation, Instant::now());
    playback.of(&rig.coordinator).unwrap().clone()
}

fn deliver<T: Transport + ?Sized>(
    rig: &Rig,
    feed: &mut QueueFeed,
    pool: &WorkPool,
    store: &mut MemStore,
    via: &T,
    event: &PlayerPlayback,
    now: i64,
) -> Result<usize, FeedError> {
    feed.on_playback(&rig.speakers(via), plan(pool, now), store, event)
}

fn started(s2: bool, lookahead: usize) -> (Rig, WorkPool, MemStore, QueueFeed) {
    let rig = Rig::generation(s2);
    let pool = multi_movement_pool();
    let mut store = MemStore::default();
    let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
    feed.lookahead = lookahead;
    feed.start(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
        .unwrap();
    let event = sample(&rig);
    assert_eq!(event.transport, Some(TransportState::Playing));
    assert_eq!(event.transport_status.as_deref(), Some("OK"));
    deliver(
        &rig, &mut feed, &pool, &mut store, &rig.lan, &event, MIDNIGHT,
    )
    .unwrap();
    assert_eq!(plays(&store).len(), 1);
    (rig, pool, store, feed)
}

#[derive(Debug, Clone, Copy)]
enum Failure {
    Rejected,
    LostReply,
}

#[derive(Debug, Clone, Copy)]
enum OwnerSeek {
    Track(u32),
    Time(u32),
}

/// Record only the feed's mutations. An injected owner's seek travels through
/// the underlying real transport, outside that record of automatic commands.
struct Observed<'a> {
    inner: &'a SimLan,
    writes: RefCell<Vec<String>>,
    failure: Option<(&'static str, Failure)>,
    owner_seek: Option<OwnerSeek>,
    injected: Cell<bool>,
    sought: Cell<bool>,
}

impl<'a> Observed<'a> {
    fn new(inner: &'a SimLan) -> Self {
        Self {
            inner,
            writes: RefCell::new(Vec::new()),
            failure: None,
            owner_seek: None,
            injected: Cell::new(false),
            sought: Cell::new(false),
        }
    }

    fn writes(&self) -> Vec<String> {
        self.writes.borrow().clone()
    }
}

impl Transport for Observed<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if name != "Browse" && !name.starts_with("Get") {
            self.writes.borrow_mut().push(name.to_owned());
        }
        if let Some((target, failure)) = self.failure
            && name == target
            && !self.injected.replace(true)
        {
            return match failure {
                Failure::Rejected => Err(ProtoError::SoapFault {
                    code: 701,
                    reason: "recovery command refused by simulator test".into(),
                }),
                Failure::LostReply => {
                    self.inner.soap_post(host, path, action, body)?;
                    Err(ProtoError::Network {
                        target: "simulated recovery reply".into(),
                        detail: "the command succeeded but its reply was lost".into(),
                    })
                }
            };
        }
        let response = self.inner.soap_post(host, path, action, body)?;
        if name == "Seek" {
            self.sought.set(true);
        }
        if name == "GetTransportInfo"
            && self.sought.get()
            && let Some(seek) = self.owner_seek
            && !self.injected.replace(true)
        {
            match seek {
                OwnerSeek::Track(track) => transport::seek_track(self.inner, host, track)?,
                OwnerSeek::Time(seconds) => transport::seek_position(self.inner, host, seconds)?,
            }
        }
        Ok(response)
    }
}

#[test]
fn recovery_starts_the_next_whole_work_without_replacing_the_queue_or_inventing_listens() {
    for s2 in [false, true] {
        let (rig, pool, mut store, mut feed) = started(s2, 1);
        let failed = feed.queued()[0].clone();
        let next = feed.queued()[1].clone();
        assert!(failed.movements.len() > 1 && next.movements.len() > 1);
        let heard = plays(&store);
        // Select a later movement, then fail before any Playing observation
        // can count it. Recovery must skip the rest of this entire work.
        transport::seek_track(&rig.lan, rig.host(), failed.movements[1].position.unwrap()).unwrap();
        rig.sim.playback_error(ROOM).unwrap();
        let error = sample(&rig);
        assert_eq!(error.transport_status.as_deref(), Some("ERROR_OCCURRED"));
        let queue = rig.queue();
        let observed = Observed::new(&rig.lan);
        assert_eq!(
            deliver(
                &rig,
                &mut feed,
                &pool,
                &mut store,
                &observed,
                &error,
                MIDNIGHT + 1,
            )
            .unwrap(),
            0
        );
        assert_eq!(observed.writes(), ["Seek", "Play"]);
        assert_eq!(
            rig.queue(),
            queue,
            "recovery adds, removes and replaces nothing"
        );
        assert_eq!(rig.position(), next.first().unwrap());
        assert_eq!(plays(&store), heard, "recovery commands are not listens");
        let notice = feed.take_recovery_notice().unwrap();
        assert_eq!(notice.failed_work, failed.title);
        assert_eq!(notice.next_work, next.title);
        assert_eq!(notice.attempt, 1);
        assert!(notice.playing);
        assert!(feed.take_recovery_notice().is_none());
        assert!(feed.is_active());

        let playing = sample(&rig);
        assert_eq!(playing.transport, Some(TransportState::Playing));
        assert_eq!(playing.transport_status.as_deref(), Some("OK"));
        deliver(
            &rig,
            &mut feed,
            &pool,
            &mut store,
            &rig.lan,
            &playing,
            MIDNIGHT + 2,
        )
        .unwrap();
        assert_eq!(
            plays(&store),
            [heard[0].clone(), next.movements[0].uri.clone()]
        );
        for skipped in failed.movements.iter().skip(1) {
            assert!(!plays(&store).contains(&skipped.uri));
        }
    }
}

#[test]
fn stale_errors_and_unknown_statuses_never_authorize_recovery_writes() {
    let (rig, pool, mut store, mut feed) = started(true, 1);
    let heard = plays(&store);
    rig.sim.playback_error(ROOM).unwrap();
    let error = sample(&rig);
    // The owner has resumed before a buffered error reaches the feed.
    transport::play(&rig.lan, rig.host()).unwrap();
    let observed = Observed::new(&rig.lan);
    let before = feed.snapshot();
    assert_eq!(
        deliver(
            &rig,
            &mut feed,
            &pool,
            &mut store,
            &observed,
            &error,
            MIDNIGHT + 1,
        )
        .unwrap(),
        0
    );
    assert_eq!(feed.snapshot(), before);
    assert!(observed.writes().is_empty());

    rig.sim.playback_error(ROOM).unwrap();
    for status in [
        None,
        Some("vendor-waiting"),
        Some("error_occurred"),
        Some("OK"),
    ] {
        let mut event = sample(&rig);
        event.transport_status = status.map(str::to_owned);
        assert_eq!(
            deliver(
                &rig,
                &mut feed,
                &pool,
                &mut store,
                &observed,
                &event,
                MIDNIGHT + 2,
            )
            .unwrap(),
            0
        );
        assert!(observed.writes().is_empty(), "{status:?}");
        assert_eq!(plays(&store), heard);
    }
    for missing_position in [false, true] {
        let mut event = sample(&rig);
        if missing_position {
            event.queue_position = None;
        } else {
            event.track_uri = None;
        }
        assert_eq!(
            deliver(
                &rig,
                &mut feed,
                &pool,
                &mut store,
                &observed,
                &event,
                MIDNIGHT + 3,
            )
            .unwrap(),
            0
        );
        assert!(observed.writes().is_empty());
    }
    assert!(feed.is_active());
    assert_eq!(plays(&store), heard);
}

#[test]
fn owner_insertions_including_an_identical_copy_prevent_automatic_skipping() {
    for cached_length in [false, true] {
        let (rig, pool, mut store, mut feed) = started(true, 1);
        let original_length = feed.left_at().unwrap();
        let failed_last = feed.queued()[0].last().unwrap();
        transport::seek_track(&rig.lan, rig.host(), failed_last).unwrap();
        let owner = if cached_length {
            // An appended duplicate leaves the recovery range itself intact.
            feed.queued()[0].movements[0].uri.clone()
        } else {
            "spotify:track:OwnerInsertedDuringFailure".into()
        };
        rig.owner_adds(&owner, !cached_length);
        rig.sim.playback_error(ROOM).unwrap();
        let mut event = sample(&rig);
        if cached_length {
            event.queue_length = Some(original_length);
        }
        let queue = rig.queue();
        let heard = plays(&store);
        let observed = Observed::new(&rig.lan);
        let result = deliver(
            &rig,
            &mut feed,
            &pool,
            &mut store,
            &observed,
            &event,
            MIDNIGHT + 1,
        );
        assert!(
            matches!(result, Ok(0) | Err(FeedError::RecoveryStopped(_))),
            "{result:?}"
        );
        assert!(observed.writes().is_empty());
        assert_eq!(rig.position(), failed_last);
        assert_eq!(rig.queue(), queue);
        assert!(queue.contains(&owner));
        assert_eq!(plays(&store), heard);
    }
}

#[test]
fn a_failure_without_a_waiting_whole_work_stops_management_without_queue_mutations() {
    let (rig, pool, mut store, mut feed) = started(true, 1);
    let last_work = feed.queued().last().unwrap().clone();
    transport::seek_track(&rig.lan, rig.host(), last_work.first().unwrap()).unwrap();
    rig.sim.playback_error(ROOM).unwrap();
    let event = sample(&rig);
    let queue = rig.queue();
    let heard = plays(&store);
    let observed = Observed::new(&rig.lan);
    let result = deliver(
        &rig,
        &mut feed,
        &pool,
        &mut store,
        &observed,
        &event,
        MIDNIGHT + 1,
    );
    assert!(
        matches!(result, Err(FeedError::RecoveryStopped(_))),
        "{result:?}"
    );
    assert!(!feed.is_active());
    assert!(feed.snapshot().is_none());
    assert!(observed.writes().is_empty());
    assert_eq!(rig.queue(), queue);
    assert_eq!(rig.position(), last_work.first().unwrap());
    assert_eq!(plays(&store), heard);
}

#[test]
fn refused_and_lost_seek_or_play_replies_are_never_automatically_replayed() {
    for action in ["Seek", "Play"] {
        for failure in [Failure::Rejected, Failure::LostReply] {
            let (rig, pool, mut store, mut feed) = started(true, 1);
            let first = feed.queued()[0].first().unwrap();
            let next = feed.queued()[1].first().unwrap();
            rig.sim.playback_error(ROOM).unwrap();
            let event = sample(&rig);
            let queue = rig.queue();
            let heard = plays(&store);
            let observed = Observed {
                failure: Some((action, failure)),
                ..Observed::new(&rig.lan)
            };
            let result = deliver(
                &rig,
                &mut feed,
                &pool,
                &mut store,
                &observed,
                &event,
                MIDNIGHT + 1,
            );
            assert!(
                matches!(result, Err(FeedError::RecoveryStopped(_))),
                "{action} {failure:?}: {result:?}"
            );
            assert!(observed.injected.get(), "{action} {failure:?}");
            assert!(!feed.is_active());
            assert!(feed.snapshot().is_none());
            let expected = if action == "Seek" {
                vec!["Seek"]
            } else {
                vec!["Seek", "Play"]
            };
            assert_eq!(observed.writes(), expected);
            let selected = if action == "Seek" && matches!(failure, Failure::Rejected) {
                first
            } else {
                next
            };
            assert_eq!(rig.position(), selected);
            let state = transport::get_transport_info(&rig.lan, rig.host()).unwrap();
            if action == "Play" && matches!(failure, Failure::LostReply) {
                assert_eq!(state.state, TransportState::Playing);
            } else {
                assert_eq!(state.state, TransportState::Stopped);
            }
            for second in 2..=4 {
                assert_eq!(
                    deliver(
                        &rig,
                        &mut feed,
                        &pool,
                        &mut store,
                        &observed,
                        &event,
                        MIDNIGHT + second,
                    )
                    .unwrap(),
                    0
                );
            }
            assert_eq!(observed.writes(), expected, "no automatic replay");
            assert_eq!(rig.queue(), queue);
            assert_eq!(plays(&store), heard);
        }
    }
}

#[test]
fn an_owner_seek_during_the_post_seek_status_read_prevents_play() {
    for relative_time in [false, true] {
        let (rig, pool, mut store, mut feed) = started(true, 1);
        let next = feed.queued()[1].clone();
        assert!(next.movements.len() > 1);
        let owner_seek = if relative_time {
            OwnerSeek::Time(37)
        } else {
            OwnerSeek::Track(next.movements[1].position.unwrap())
        };
        rig.sim.playback_error(ROOM).unwrap();
        let event = sample(&rig);
        let queue = rig.queue();
        let heard = plays(&store);
        let observed = Observed {
            owner_seek: Some(owner_seek),
            ..Observed::new(&rig.lan)
        };
        let result = deliver(
            &rig,
            &mut feed,
            &pool,
            &mut store,
            &observed,
            &event,
            MIDNIGHT + 1,
        );
        assert!(
            matches!(result, Err(FeedError::RecoveryStopped(_))),
            "{owner_seek:?}: {result:?}"
        );
        assert!(observed.injected.get());
        assert_eq!(
            observed.writes(),
            ["Seek"],
            "Play must not follow the owner's seek"
        );
        assert!(!feed.is_active());
        let actual = transport::get_position_info(&rig.lan, rig.host()).unwrap();
        match owner_seek {
            OwnerSeek::Track(track) => assert_eq!(actual.track, track),
            OwnerSeek::Time(seconds) => {
                assert_eq!(actual.track, next.first().unwrap());
                assert_eq!(actual.position_secs, Some(seconds));
            }
        }
        assert_eq!(
            transport::get_transport_info(&rig.lan, rig.host())
                .unwrap()
                .state,
            TransportState::Stopped
        );
        assert_eq!(rig.queue(), queue);
        assert_eq!(plays(&store), heard);
    }
}

#[test]
fn recovery_budget_survives_feed_snapshot_restarts_and_exhaustion_sends_no_commands() {
    let (rig, pool, mut store, mut feed) = started(true, 4);
    assert_eq!(feed.queued().len(), 5);
    let queue = rig.queue();
    let heard = plays(&store);
    let observed = Observed::new(&rig.lan);
    for attempt in 1_u8..=3 {
        rig.sim.playback_error(ROOM).unwrap();
        let event = sample(&rig);
        assert_eq!(
            deliver(
                &rig,
                &mut feed,
                &pool,
                &mut store,
                &observed,
                &event,
                MIDNIGHT + i64::from(attempt),
            )
            .unwrap(),
            0
        );
        let notice = feed.take_recovery_notice().unwrap();
        assert_eq!(notice.attempt, attempt);
        assert!(notice.playing);
        let snapshot = feed.snapshot().unwrap();
        let encoded = serde_json::to_string(&snapshot).unwrap();
        let restored: FeedSnapshot = serde_json::from_str(&encoded).unwrap();
        assert_eq!(restored, snapshot);
        feed = QueueFeed::restore(restored, DjConfig::default(), 7);
    }
    assert_eq!(
        observed.writes(),
        ["Seek", "Play", "Seek", "Play", "Seek", "Play"]
    );
    rig.sim.playback_error(ROOM).unwrap();
    let event = sample(&rig);
    let stopped_at = rig.position();
    let before = observed.writes();
    let result = deliver(
        &rig,
        &mut feed,
        &pool,
        &mut store,
        &observed,
        &event,
        MIDNIGHT + 4,
    );
    assert!(
        matches!(result, Err(FeedError::RecoveryStopped(_))),
        "{result:?}"
    );
    assert!(!feed.is_active());
    assert!(feed.snapshot().is_none());
    assert_eq!(observed.writes(), before);
    for second in 5..=7 {
        assert_eq!(
            deliver(
                &rig,
                &mut feed,
                &pool,
                &mut store,
                &observed,
                &event,
                MIDNIGHT + second,
            )
            .unwrap(),
            0
        );
    }
    assert_eq!(observed.writes(), before);
    assert_eq!(rig.position(), stopped_at);
    assert_eq!(rig.queue(), queue);
    assert_eq!(plays(&store), heard);
}

#[test]
fn a_cached_healthy_queue_end_cannot_restart_a_fresh_renderer_error() {
    for s2 in [false, true] {
        let (rig, pool, mut store, mut feed) = started(s2, 1);
        let last = feed.left_at().unwrap();
        transport::seek_track(&rig.lan, rig.host(), last).unwrap();
        let final_movement = sample(&rig);
        // A failed refill leaves the genuinely observed final movement at
        // the queue's end. Let that movement finish on the simulator's clock.
        let flaky = Flaky::new(rig.lan.clone());
        flaky.fail_add_in(1);
        assert!(
            deliver(
                &rig,
                &mut feed,
                &pool,
                &mut store,
                &flaky,
                &final_movement,
                MIDNIGHT + 1,
            )
            .is_err()
        );
        rig.sim.advance(Duration::from_secs(3600));
        let cached = sample(&rig);
        assert_eq!(cached.transport, Some(TransportState::Stopped));
        assert_eq!(cached.transport_status.as_deref(), Some("OK"));
        assert_eq!(cached.queue_position, Some(1));
        assert!(feed.ran_dry(&cached));
        rig.sim.playback_error(ROOM).unwrap();
        let fresh = sample(&rig);
        assert_eq!(fresh.transport_status.as_deref(), Some("ERROR_OCCURRED"));
        assert_eq!(fresh.queue_position, cached.queue_position);
        let queue = rig.queue();
        let before = feed.snapshot();
        let heard = plays(&store);
        let observed = Observed::new(&rig.lan);
        assert_eq!(
            deliver(
                &rig,
                &mut feed,
                &pool,
                &mut store,
                &observed,
                &cached,
                MIDNIGHT + 2,
            )
            .unwrap(),
            0
        );
        assert!(observed.writes().is_empty());
        assert_eq!(feed.snapshot(), before);
        assert_eq!(rig.queue(), queue);
        assert_eq!(rig.position(), 1);
        assert_eq!(plays(&store), heard);
    }
}
