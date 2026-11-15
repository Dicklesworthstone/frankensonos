//! Navigation regressions over the existing real-socket Sonos simulator.
//! Most tests deliver explicit observations to make delayed-event ordering
//! deterministic; the GENA case exercises the subscription path as well.

use super::*;

struct Running {
    rig: Rig,
    pool: WorkPool,
    store: MemStore,
    feed: QueueFeed,
    started: Vec<QueuedWork>,
}

impl Running {
    fn new(pool: WorkPool) -> Self {
        let rig = Rig::new();
        let mut store = MemStore::default();
        let config = DjConfig {
            trim_queue_at: u32::MAX,
            ..DjConfig::default()
        };
        let mut feed = QueueFeed::new(&rig.coordinator, config, 7);
        feed.lookahead = 3;
        let started = feed
            .start(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap()
            .to_vec();
        assert_eq!(started.len(), 4);
        let mut running = Self {
            rig,
            pool,
            store,
            feed,
            started,
        };
        running.observe(MIDNIGHT);
        running.seek(running.started[2].first().unwrap());
        running.observe(MIDNIGHT + 600);
        assert_eq!(running.feed.played.len(), 2);
        running
    }

    fn classical() -> Self {
        Self::new(multi_movement_pool())
    }

    fn seek(&self, position: u32) {
        control::play_queue_from(
            &self.rig.lan,
            &self.rig.houses,
            &self.rig.coordinator,
            position,
        )
        .unwrap();
    }

    fn sample(&self) -> PlayerPlayback {
        let live =
            control::playback(&self.rig.lan, &self.rig.houses, &self.rig.coordinator).unwrap();
        let mut sample = PlayerPlayback::default();
        sample.queue_position = Some(live.position.track);
        sample.track_uri = Some(live.position.uri);
        sample.transport = Some(live.transport.state);
        sample.media_uri = Some(
            fsonos_proto::control::get_media_info(&self.rig.lan, self.rig.host())
                .unwrap()
                .uri,
        );
        sample.queue_length = Some(u32::try_from(self.rig.queue().len()).unwrap());
        sample
    }

    fn observe(&mut self, now: i64) -> usize {
        let sample = self.sample();
        self.feed
            .on_playback(
                &self.rig.speakers(&self.rig.lan),
                plan(&self.pool, now),
                &mut self.store,
                &sample,
            )
            .unwrap()
    }

    fn skip(&mut self) -> Result<QueuedWork, FeedError> {
        self.feed
            .skip(
                &self.rig.speakers(&self.rig.lan),
                plan(&self.pool, MIDNIGHT + 900),
                &mut self.store,
            )
            .cloned()
    }

    fn delayed_first_movement(&self) -> PlayerPlayback {
        let mut sample = self.sample();
        let movement = &self.started[0].movements[0];
        sample.queue_position = movement.position;
        sample.track_uri = Some(spotify_queue_uri(&movement.uri));
        sample
    }
}

#[test]
fn rewinding_into_a_work_restores_its_whole_shape_and_records_the_replay_once() {
    let mut run = Running::classical();
    let work = run.started[0].clone();
    assert!(work.movements.len() > 1);
    let position = work.movements[1].position.unwrap();
    let before = run.rig.queue();
    let heard = plays(&run.store).len();

    run.seek(position);
    assert_eq!(run.observe(MIDNIGHT + 900), 0);
    assert_eq!(run.feed.current(), Some(&work));
    assert_eq!(&run.feed.queued()[..2], &run.started[..2]);
    assert!(run.feed.played.is_empty());
    assert_eq!(run.rig.queue(), before, "rewinding does not enqueue again");
    assert_eq!(plays(&run.store).len(), heard + 1);
    assert_eq!(plays(&run.store).last(), Some(&work.movements[1].uri));

    assert_eq!(run.observe(MIDNIGHT + 901), 0);
    assert_eq!(plays(&run.store).len(), heard + 1, "not one play per event");
}

#[test]
fn an_immediate_skip_after_a_rewind_uses_the_retained_next_work() {
    let mut run = Running::classical();
    let before = run.rig.queue();
    run.seek(run.started[0].movements[1].position.unwrap());
    // No playback event delivered between the owner's seek and the skip.
    let next = run.skip().unwrap();
    assert_eq!(next, run.started[1]);
    assert_eq!(run.rig.position(), next.first().unwrap());
    assert_eq!(run.feed.current(), Some(&next));
    assert_eq!(run.rig.queue(), before);
}

#[test]
fn a_rewind_while_the_daemon_is_down_is_recovered_from_the_snapshot() {
    let mut run = Running::classical();
    let saved = serde_json::to_string(&run.feed.snapshot().unwrap()).unwrap();
    run.seek(run.started[0].first().unwrap());
    run.feed = QueueFeed::restore(
        serde_json::from_str(&saved).unwrap(),
        DjConfig {
            trim_queue_at: u32::MAX,
            ..DjConfig::default()
        },
        19,
    );
    let before = run.rig.queue();
    assert_eq!(run.observe(MIDNIGHT + 900), 0);
    assert_eq!(run.feed.current(), Some(&run.started[0]));
    let restored = reread(&run.feed, 20);
    assert_eq!(restored.current(), run.feed.current());
    assert_eq!(restored.snapshot(), run.feed.snapshot());
    assert_eq!(run.rig.queue(), before);
}

#[test]
fn previous_also_restores_single_track_songs() {
    let mut run = Running::new(works_of(&song_items()));
    let previous = run.started[1].clone();
    assert_eq!(previous.movements.len(), 1);
    run.seek(previous.first().unwrap());
    assert_eq!(run.observe(MIDNIGHT + 900), 0);
    assert_eq!(run.feed.current(), Some(&previous));
    assert_eq!(run.feed.played, [run.started[0].clone()]);
    assert_eq!(run.skip().unwrap(), run.started[2]);
}

#[test]
fn delayed_backward_events_do_not_rewind_the_model_or_publish_read_back() {
    let mut run = Running::classical();
    let stale = run.delayed_first_movement();
    let snapshot = run.feed.snapshot();
    let queue_len = run.feed.queue_len;
    let before = run.rig.queue();
    let history = plays(&run.store);
    assert_eq!(
        run.feed
            .on_playback(
                &run.rig.speakers(&run.rig.lan),
                plan(&run.pool, MIDNIGHT + 900),
                &mut run.store,
                &stale,
            )
            .unwrap(),
        0
    );
    assert_eq!(run.feed.snapshot(), snapshot);
    assert_eq!(run.feed.queue_len, queue_len);
    assert_eq!(run.rig.queue(), before);
    assert_eq!(plays(&run.store), history);
}

#[test]
fn the_same_uri_at_another_position_is_not_a_confirmed_rewind() {
    let mut run = Running::classical();
    let uri = run.started[0].movements[0].uri.clone();
    run.rig.owner_adds(&uri, false);
    run.seek(u32::try_from(run.rig.queue().len()).unwrap());
    let stale = run.delayed_first_movement();
    let before = run.rig.queue();
    let snapshot = run.feed.snapshot();
    assert_eq!(
        run.feed
            .on_playback(
                &run.rig.speakers(&run.rig.lan),
                plan(&run.pool, MIDNIGHT + 900),
                &mut run.store,
                &stale,
            )
            .unwrap(),
        0
    );
    assert_eq!(run.feed.snapshot(), snapshot);
    assert_eq!(run.rig.queue(), before, "the owner's copy is untouched");
}

#[test]
fn a_failed_rewind_read_keeps_the_old_model_and_can_be_retried() {
    let mut run = Running::classical();
    run.seek(run.started[0].first().unwrap());
    let sample = run.sample();
    let snapshot = run.feed.snapshot();
    let queue_len = run.feed.queue_len;
    let heard = plays(&run.store);
    run.rig
        .sim
        .upnp_fault(ROOM, "GetPositionInfo", 701)
        .unwrap();
    let result = run.feed.on_playback(
        &run.rig.speakers(&run.rig.lan),
        plan(&run.pool, MIDNIGHT + 900),
        &mut run.store,
        &sample,
    );
    assert!(matches!(
        result,
        Err(FeedError::Proto(ProtoError::SoapFault { code: 701, .. }))
    ));
    assert_eq!(run.feed.snapshot(), snapshot);
    assert_eq!(run.feed.queue_len, queue_len);
    assert_eq!(plays(&run.store), heard);
    run.rig.sim.clear_faults(ROOM).unwrap();
    run.observe(MIDNIGHT + 901);
    assert_eq!(run.feed.current(), Some(&run.started[0]));
}

#[test]
fn rewound_works_are_not_trim_candidates_and_still_count_as_queued_history() {
    let mut run = Running::classical();
    let before = run.rig.queue();
    run.feed.config.trim_queue_at = 0;
    run.feed.config.keep_played = 0;
    run.seek(run.started[0].movements[1].position.unwrap());
    run.observe(MIDNIGHT + 900);
    assert!(run.feed.played.is_empty());
    assert_eq!(run.rig.queue(), before);
    let history = run.feed.history(&run.store, MIDNIGHT + 900).unwrap();
    for movement in &run.started[1].movements {
        assert!(history.iter().any(|play| play.source_uri == movement.uri));
    }
}

#[test]
fn an_ordinary_forward_event_adds_no_navigation_reads() {
    struct NoIo;
    impl Transport for NoIo {
        fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
            panic!("an unchanged queue with sufficient lookahead needs no navigation I/O")
        }
    }
    let mut run = Running::classical();
    let position = run.started[2].movements[1].position.unwrap();
    run.seek(position);
    let sample = run.sample();
    assert_eq!(
        run.feed
            .on_playback(
                &run.rig.speakers(&NoIo),
                plan(&run.pool, MIDNIGHT + 900),
                &mut run.store,
                &sample,
            )
            .unwrap(),
        0
    );
    assert_eq!(run.feed.current(), Some(&run.started[2]));
}

#[test]
fn a_gena_rewind_event_restores_the_current_work() {
    let mut run = Running::classical();
    run.rig.notifies(); // discard events deliberately superseded by direct observations
    run.seek(run.started[0].movements[1].position.unwrap());
    let lan = run.rig.lan.clone();
    run.rig
        .pump(
            &lan,
            &mut run.feed,
            plan(&run.pool, MIDNIGHT + 900),
            &mut run.store,
        )
        .unwrap();
    assert_eq!(run.feed.current(), Some(&run.started[0]));
}

#[test]
fn removing_only_future_works_does_not_end_a_session_rewound_into_retained_music() {
    let mut run = Running::classical();
    let keep = run.started[1].last().unwrap();
    run.seek(run.started[0].movements[1].position.unwrap());
    for position in (keep + 1..=u32::try_from(run.rig.queue().len()).unwrap()).rev() {
        run.rig.owner_removes(position);
    }
    assert!(
        run.observe(MIDNIGHT + 900) > 0,
        "the DJ replenishes its lookahead"
    );
    assert!(run.feed.is_active());
    assert_eq!(run.feed.current(), Some(&run.started[0]));
    assert_eq!(run.skip().unwrap(), run.started[1]);
}

#[test]
fn replacing_the_queue_then_skipping_before_its_event_does_not_restart_the_dj() {
    let mut run = Running::classical();
    remove_all_tracks_from_queue(&run.rig.lan, run.rig.host()).unwrap();
    run.rig.owner_adds(OWNER, false);
    run.seek(1);
    let history = plays(&run.store);
    assert!(matches!(run.skip(), Err(FeedError::Inactive)));
    assert!(!run.feed.is_active());
    assert!(run.feed.snapshot().is_none());
    assert_eq!(run.rig.queue(), [OWNER]);
    assert_eq!(run.rig.position(), 1);
    assert_eq!(plays(&run.store), history);
}

#[test]
fn clearing_the_queue_then_skipping_before_its_event_does_not_fill_it_again() {
    let mut run = Running::classical();
    remove_all_tracks_from_queue(&run.rig.lan, run.rig.host()).unwrap();
    assert!(matches!(run.skip(), Err(FeedError::Inactive)));
    assert!(!run.feed.is_active());
    assert!(run.rig.queue().is_empty());
}

fn temporary_source<T: Transport + ?Sized>(transport: &T, host: IpAddr) {
    soap::call(
        transport,
        host,
        &AV_TRANSPORT,
        "SetAVTransportURI",
        &soap::args_xml(&[
            ("InstanceID", "0"),
            ("CurrentURI", "http://example.invalid/announcement.wav"),
            ("CurrentURIMetaData", ""),
        ]),
    )
    .unwrap();
    soap::call(
        transport,
        host,
        &AV_TRANSPORT,
        "Play",
        &soap::args_xml(&[("InstanceID", "0"), ("Speed", "1")]),
    )
    .unwrap();
}

#[test]
fn skip_does_not_interrupt_a_temporary_source_or_forget_the_retained_session() {
    let mut run = Running::classical();
    let queue = run.rig.queue();
    let snapshot = run.feed.snapshot();
    temporary_source(&run.rig.lan, run.rig.host());
    assert!(matches!(run.skip(), Err(FeedError::Inactive)));
    assert_eq!(run.feed.snapshot(), snapshot);
    assert_eq!(run.rig.queue(), queue);
    let media = fsonos_proto::control::get_media_info(&run.rig.lan, run.rig.host()).unwrap();
    assert_eq!(media.uri, "http://example.invalid/announcement.wav");
    run.seek(run.started[2].first().unwrap());
    run.observe(MIDNIGHT + 901);
    assert!(run.feed.is_active());
    assert_eq!(run.skip().unwrap(), run.started[3]);
}

#[test]
fn a_paused_owned_queue_can_still_skip_to_the_next_whole_work() {
    let mut run = Running::classical();
    control::pause(&run.rig.lan, &run.rig.houses, &run.rig.coordinator).unwrap();
    assert_eq!(run.skip().unwrap(), run.started[3]);
    assert_eq!(run.sample().transport, Some(TransportState::Playing));
}

#[derive(Clone, Copy)]
enum PartialWork {
    Current,
    Waiting,
}

fn partially_enqueued(s2: bool, partial: PartialWork) -> Running {
    let rig = Rig::generation(s2);
    let feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
    let mut run = Running {
        rig,
        feed,
        pool: multi_movement_pool(),
        store: MemStore::default(),
        started: Vec::new(),
    };
    let flaky = Flaky::new(run.rig.lan.clone());
    if matches!(partial, PartialWork::Waiting) {
        run.started = run
            .feed
            .start(
                &run.rig.speakers(&flaky),
                plan(&run.pool, MIDNIGHT),
                &mut run.store,
            )
            .unwrap()
            .to_vec();
        run.seek(run.started[1].first().unwrap());
    }
    flaky.fail_add_in(2);
    let failure = match partial {
        PartialWork::Current => run
            .feed
            .start(
                &run.rig.speakers(&flaky),
                plan(&run.pool, MIDNIGHT),
                &mut run.store,
            )
            .unwrap_err(),
        PartialWork::Waiting => {
            let playback = run.sample();
            run.feed
                .on_playback(
                    &run.rig.speakers(&flaky),
                    plan(&run.pool, MIDNIGHT + 600),
                    &mut run.store,
                    &playback,
                )
                .unwrap_err()
        }
    };
    assert!(matches!(
        failure,
        FeedError::Proto(ProtoError::SoapFault { code: 501, .. })
    ));
    let unfinished = run.feed.queued().last().unwrap();
    assert!(!unfinished.is_whole());
    assert_eq!(unfinished.movements.iter().filter(|m| m.added).count(), 1);
    if matches!(partial, PartialWork::Current) {
        run.seek(unfinished.first().unwrap());
    }
    run
}

/// One unavailable movement stays refused while all other traffic reaches
/// the real simulator. A skip must not require the music it is leaving.
struct RefusingMovement {
    inner: Flaky,
    uri: String,
    refused: Cell<usize>,
}

impl Transport for RefusingMovement {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let enqueue = format!(
            "<EnqueuedURI>{}</EnqueuedURI>",
            spotify_queue_uri(&self.uri)
        );
        if action.contains("#AddURIToQueue") && body.contains(&enqueue) {
            self.refused.set(self.refused.get() + 1);
            return Err(ProtoError::SoapFault {
                code: 501,
                reason: "this movement cannot be enqueued".into(),
            });
        }
        self.inner.soap_post(host, path, action, body)
    }
}

#[test]
fn skip_leaves_a_refused_current_movement_and_starts_the_next_whole_work() {
    for s2 in [false, true] {
        let mut run = partially_enqueued(s2, PartialWork::Current);
        let unfinished = run.feed.queued()[0].clone();
        let before = run.rig.queue();
        let refusing = RefusingMovement {
            inner: Flaky::new(run.rig.lan.clone()),
            uri: unfinished.movements[1].uri.clone(),
            refused: Cell::new(0),
        };
        let playback = run.sample();
        assert!(matches!(
            run.feed.on_playback(
                &run.rig.speakers(&refusing),
                plan(&run.pool, MIDNIGHT + 800),
                &mut run.store,
                &playback,
            ),
            Err(FeedError::Proto(ProtoError::SoapFault { code: 501, .. }))
        ));
        let heard = plays(&run.store);
        assert_eq!(heard, [unfinished.movements[0].uri.clone()]);
        assert_eq!(refusing.refused.get(), 1);
        let next = run
            .feed
            .skip(
                &run.rig.speakers(&refusing),
                plan(&run.pool, MIDNIGHT + 900),
                &mut run.store,
            )
            .unwrap()
            .clone();
        assert_eq!(
            refusing.refused.get(),
            1,
            "the skipped work is not completed"
        );
        assert!(next.is_whole());
        assert_ne!(next.work_key, unfinished.work_key);
        assert_eq!(next.first(), unfinished.last().map(|position| position + 1));
        assert_eq!(run.rig.position(), next.first().unwrap());
        assert_eq!(run.sample().transport, Some(TransportState::Playing));
        let queue = run.rig.queue();
        assert_eq!(&queue[..before.len()], before);
        assert_eq!(&queue[before.len()..], next.tracks());
        assert_eq!(plays(&run.store), heard, "skip alone is not listening");

        // A restart and rewind can leave the same partial current work with
        // an intact successor already waiting. Skip must use that successor
        // without retrying the old refusal or adding another copy.
        run.feed = reread(&run.feed, 19);
        run.seek(unfinished.first().unwrap());
        let again = run
            .feed
            .skip(
                &run.rig.speakers(&refusing),
                plan(&run.pool, MIDNIGHT + 901),
                &mut run.store,
            )
            .unwrap()
            .clone();
        assert_eq!(again, next);
        assert_eq!(refusing.refused.get(), 1);
        assert_eq!(run.rig.queue(), queue);
        assert_eq!(run.rig.position(), next.first().unwrap());

        let playback = run.sample();
        run.feed
            .on_playback(
                &run.rig.speakers(&refusing),
                plan(&run.pool, MIDNIGHT + 902),
                &mut run.store,
                &playback,
            )
            .unwrap();
        assert_eq!(
            refusing.refused.get(),
            1,
            "automatic feeding leaves the skipped fragment"
        );
        assert!(
            run.feed
                .queued()
                .iter()
                .all(|work| work.work_key != unfinished.work_key)
        );
        assert_eq!(&run.rig.queue()[..before.len()], before);
    }
}

#[test]
fn skip_requires_the_waiting_work_to_be_complete_before_leaving_the_current_one() {
    for s2 in [false, true] {
        let mut run = partially_enqueued(s2, PartialWork::Waiting);
        let unfinished = run.feed.queued().last().unwrap().clone();
        let position = run.rig.position();
        let queue = run.rig.queue();
        let heard = plays(&run.store);
        let writes = || {
            run.rig
                .sim
                .soap_log()
                .iter()
                .filter(|entry| {
                    matches!(entry.action.as_str(), "SetAVTransportURI" | "Seek" | "Play")
                })
                .count()
        };
        let before = writes();
        let refusing = RefusingMovement {
            inner: Flaky::new(run.rig.lan.clone()),
            uri: unfinished.movements[1].uri.clone(),
            refused: Cell::new(0),
        };
        let result = run.feed.skip(
            &run.rig.speakers(&refusing),
            plan(&run.pool, MIDNIGHT + 900),
            &mut run.store,
        );
        assert!(matches!(
            result,
            Err(FeedError::Proto(ProtoError::SoapFault { code: 501, .. }))
        ));
        assert_eq!(refusing.refused.get(), 1);
        assert_eq!(writes(), before, "an incomplete successor never starts");
        assert_eq!(run.rig.position(), position);
        assert_eq!(run.rig.queue(), queue);
        assert_eq!(plays(&run.store), heard);
        assert!(run.feed.is_active(), "a definite refusal remains retryable");

        let next = run.skip().unwrap();
        assert!(next.is_whole());
        assert_eq!(next.work_key, unfinished.work_key);
        assert_eq!(next.first(), unfinished.first());
        assert_eq!(run.rig.position(), next.first().unwrap());
        assert_eq!(run.sample().transport, Some(TransportState::Playing));
        assert_eq!(
            &run.rig.queue()[usize::try_from(next.first().unwrap() - 1).unwrap()..],
            next.tracks(),
        );
    }
}

#[test]
fn a_failed_new_successor_is_completed_on_explicit_retry_without_revisiting_the_skipped_work() {
    for s2 in [false, true] {
        let mut run = partially_enqueued(s2, PartialWork::Current);
        let current = run.feed.queued()[0].clone();
        let before = run.rig.queue();
        let refusing = RefusingMovement {
            inner: Flaky::new(run.rig.lan.clone()),
            uri: current.movements[1].uri.clone(),
            refused: Cell::new(0),
        };
        refusing.inner.fail_add_in(2);
        assert!(matches!(
            run.feed.skip(
                &run.rig.speakers(&refusing),
                plan(&run.pool, MIDNIGHT + 900),
                &mut run.store,
            ),
            Err(FeedError::Proto(ProtoError::SoapFault { code: 501, .. }))
        ));
        assert_eq!(refusing.refused.get(), 0);
        assert!(run.feed.is_active());
        assert_eq!(run.feed.queued().len(), 2);
        assert_eq!(run.feed.queued()[0], current);
        let successor = run.feed.queued()[1].clone();
        assert!(!successor.is_whole());
        assert_eq!(successor.movements.iter().filter(|m| m.added).count(), 1);
        assert_eq!(run.rig.position(), current.first().unwrap());
        assert_eq!(&run.rig.queue()[..before.len()], before);

        run.feed = reread(&run.feed, 19);
        let next = run
            .feed
            .skip(
                &run.rig.speakers(&refusing),
                plan(&run.pool, MIDNIGHT + 901),
                &mut run.store,
            )
            .unwrap()
            .clone();
        assert_eq!(
            refusing.refused.get(),
            0,
            "retry completes only the waiting work"
        );
        assert_eq!(next.work_key, successor.work_key);
        assert_eq!(next.first(), successor.first());
        assert!(next.is_whole());
        assert_eq!(run.feed.queued()[0], current);
        assert_eq!(
            run.feed.queued().len(),
            2,
            "retry never chooses another work"
        );
        assert_eq!(run.rig.position(), next.first().unwrap());
        assert_eq!(run.sample().transport, Some(TransportState::Playing));
        let queue = run.rig.queue();
        assert_eq!(&queue[..before.len()], before);
        assert_eq!(&queue[before.len()..], next.tracks());
    }
}

#[test]
fn playback_completes_the_partial_successor_after_a_failed_skip_before_adding_another_work() {
    for s2 in [false, true] {
        let mut run = partially_enqueued(s2, PartialWork::Current);
        let current = run.feed.queued()[0].clone();
        let before = run.rig.queue();
        let refusing = RefusingMovement {
            inner: Flaky::new(run.rig.lan.clone()),
            uri: current.movements[1].uri.clone(),
            refused: Cell::new(0),
        };
        let playback = run.sample();
        assert!(matches!(
            run.feed.on_playback(
                &run.rig.speakers(&refusing),
                plan(&run.pool, MIDNIGHT + 800),
                &mut run.store,
                &playback,
            ),
            Err(FeedError::Proto(ProtoError::SoapFault { code: 501, .. }))
        ));
        refusing.inner.fail_add_in(2);
        assert!(matches!(
            run.feed.skip(
                &run.rig.speakers(&refusing),
                plan(&run.pool, MIDNIGHT + 900),
                &mut run.store,
            ),
            Err(FeedError::Proto(ProtoError::SoapFault { code: 501, .. }))
        ));
        assert_eq!(run.feed.queued().len(), 2);
        let successor = run.feed.queued()[1].clone();
        assert!(!successor.is_whole());

        run.rig.next();
        assert_eq!(run.rig.position(), successor.first().unwrap());
        let playback = run.sample();
        let added = run
            .feed
            .on_playback(
                &run.rig.speakers(&refusing),
                plan(&run.pool, MIDNIGHT + 901),
                &mut run.store,
                &playback,
            )
            .unwrap();
        assert_eq!(
            refusing.refused.get(),
            1,
            "the old refusal is not revisited"
        );
        assert_eq!(added, 1);
        let completed = &run.feed.queued()[0];
        assert_eq!(completed.work_key, successor.work_key);
        assert_eq!(completed.first(), successor.first());
        assert!(completed.is_whole());
        let waiting = &run.feed.queued()[1];
        assert!(waiting.is_whole());
        assert_eq!(waiting.first(), completed.last().map(|last| last + 1));
        assert_eq!(run.rig.position(), completed.first().unwrap());
        let queue = run.rig.queue();
        assert_eq!(&queue[..before.len()], before);
        let end = before.len() + completed.movements.len();
        assert_eq!(&queue[before.len()..end], completed.tracks());
        assert_eq!(&queue[end..], waiting.tracks());
    }
}

#[test]
fn skip_on_an_owner_item_still_finds_the_shifted_waiting_work() {
    let mut run = Running::classical();
    run.rig.owner_adds(OWNER, true);
    run.rig.next();
    let next = run.skip().unwrap();
    assert_eq!(next.work_key, run.started[3].work_key);
    assert_eq!(
        next.first(),
        run.started[3].first().map(|position| position + 1)
    );
    assert!(run.rig.queue().iter().any(|uri| uri == OWNER));
    assert_eq!(run.rig.position(), next.first().unwrap());
}

#[test]
fn a_failed_skip_read_does_not_publish_resync_or_write_to_the_player() {
    let mut run = Running::classical();
    run.rig.owner_adds(OWNER, true);
    let snapshot = run.feed.snapshot();
    let queue_len = run.feed.queue_len;
    let queue = run.rig.queue();
    run.rig
        .sim
        .upnp_fault(ROOM, "GetPositionInfo", 701)
        .unwrap();
    assert!(matches!(
        run.skip(),
        Err(FeedError::Proto(ProtoError::SoapFault { code: 701, .. }))
    ));
    assert_eq!(run.feed.snapshot(), snapshot);
    assert_eq!(run.feed.queue_len, queue_len);
    assert_eq!(run.rig.queue(), queue);
    run.rig.sim.clear_faults(ROOM).unwrap();
    assert_eq!(run.skip().unwrap().work_key, run.started[3].work_key);
}

#[derive(Clone, Copy)]
enum ConcurrentEdit {
    Append,
    Source,
}

struct EditingDuringRead {
    inner: SimLan,
    edit: ConcurrentEdit,
    edited: Cell<bool>,
    writes: Cell<usize>,
}

impl Transport for EditingDuringRead {
    fn soap_post(
        &self,
        host: IpAddr,
        control_path: &str,
        soap_action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let action = soap_action.trim_matches('"').rsplit('#').next().unwrap();
        if matches!(
            action,
            "AddURIToQueue" | "SetAVTransportURI" | "Seek" | "Play"
        ) {
            self.writes.set(self.writes.get() + 1);
        }
        let response = self
            .inner
            .soap_post(host, control_path, soap_action, body)?;
        if action == "GetPositionInfo" && !self.edited.replace(true) {
            // This separate controller changes state after the response was
            // produced. Calls through inner are not writes made by the feed.
            match self.edit {
                ConcurrentEdit::Append => {
                    add_uri_to_queue(
                        &self.inner,
                        host,
                        "http://example.invalid/owners-track.wav",
                        "",
                        false,
                    )
                    .unwrap();
                }
                ConcurrentEdit::Source => temporary_source(&self.inner, host),
            }
        }
        Ok(response)
    }
}

#[test]
fn queue_or_source_changes_during_skip_reads_are_not_overwritten() {
    for edit in [ConcurrentEdit::Append, ConcurrentEdit::Source] {
        let mut run = Running::classical();
        let snapshot = run.feed.snapshot();
        let transport = EditingDuringRead {
            inner: run.rig.lan.clone(),
            edit,
            edited: Cell::new(false),
            writes: Cell::new(0),
        };
        let result = run.feed.skip(
            &run.rig.speakers(&transport),
            plan(&run.pool, MIDNIGHT + 900),
            &mut run.store,
        );
        assert!(matches!(result, Err(FeedError::Inactive)));
        assert!(
            transport.edited.get(),
            "the concurrent edit really occurred"
        );
        assert_eq!(transport.writes.get(), 0);
        assert_eq!(run.feed.snapshot(), snapshot);
        assert_eq!(run.feed.queue_len, None, "retry needs fresh queue evidence");
    }
}

#[test]
fn mode_repair_keeps_a_rewound_work_when_only_previously_played_works_remain() {
    let mut run = Running::classical();
    run.seek(run.started[0].movements[1].position.unwrap());
    let keep = run.started[1].last().unwrap();
    for position in (keep + 1..=u32::try_from(run.rig.queue().len()).unwrap()).rev() {
        run.rig.owner_removes(position);
    }
    soap::call(
        &run.rig.lan,
        run.rig.host(),
        &AV_TRANSPORT,
        "SetPlayMode",
        &soap::args_xml(&[("InstanceID", "0"), ("NewPlayMode", "SHUFFLE")]),
    )
    .unwrap();
    let mut sample = run.sample();
    sample.play_mode = Some("SHUFFLE".into());
    run.feed
        .on_playback(
            &run.rig.speakers(&run.rig.lan),
            plan(&run.pool, MIDNIGHT + 900),
            &mut run.store,
            &sample,
        )
        .unwrap();
    assert!(run.feed.is_active());
    assert_eq!(run.feed.current(), Some(&run.started[0]));
    let settings = soap::call(
        &run.rig.lan,
        run.rig.host(),
        &AV_TRANSPORT,
        "GetTransportSettings",
        &soap::args_xml(&[("InstanceID", "0")]),
    )
    .unwrap();
    assert_eq!(settings.get("PlayMode"), Some("NORMAL"));
}
