//! Temporary sources suspend retained DJ queues without polling or losing work.

use super::*;
use fsonos_core::playback::PlaybackObservation;
use fsonos_proto::control as soap;

fn sample(rig: &Rig) -> PlayerPlayback {
    let host = rig.host();
    let observation = PlaybackObservation {
        transport: soap::get_transport_info(&rig.lan, host).unwrap(),
        position: soap::get_position_info(&rig.lan, host).unwrap(),
        media: soap::get_media_info(&rig.lan, host).unwrap(),
    };
    let mut state = Playback::default();
    state.apply_observation(&rig.coordinator, &observation, Instant::now());
    state.of(&rig.coordinator).unwrap().clone()
}

struct NoIo;

impl Transport for NoIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("a non-queue source must not read or change the retained DJ queue")
    }
}

#[test]
fn another_source_cannot_complete_a_partial_work_but_returning_to_the_queue_can() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        let pool = multi_movement_pool();
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
        let flaky = Flaky::new(rig.sim.lan());
        flaky.fail_add_in(2);
        assert!(
            feed.start(&rig.speakers(&flaky), plan(&pool, MIDNIGHT), &mut store)
                .is_err()
        );
        assert!(feed.is_active());
        assert_eq!(rig.queue().len(), 1);
        assert!(!feed.queued()[0].is_whole());
        let expected = feed.queued()[0]
            .tracks()
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let queue = rig.queue();
        let retained = feed.snapshot();
        control::play_uri(
            &rig.lan,
            &rig.houses,
            &rig.coordinator,
            "http://example.invalid/announcement.wav",
            "",
        )
        .unwrap();
        let mut temporary = sample(&rig);
        temporary.play_mode = Some("NORMAL".into());
        assert!(!temporary.is_queue_of(&rig.coordinator));
        assert_eq!(
            feed.on_playback(
                &rig.speakers(&NoIo),
                plan(&pool, MIDNIGHT + 10),
                &mut store,
                &temporary
            )
            .unwrap(),
            0,
        );
        assert_eq!(feed.snapshot(), retained);
        assert_eq!(rig.queue(), queue);
        assert!(plays(&store).is_empty());

        control::play_queue_from(&rig.lan, &rig.houses, &rig.coordinator, 1).unwrap();
        let returned = sample(&rig);
        assert!(returned.is_queue_of(&rig.coordinator));
        feed.on_playback(
            &rig.speakers(&rig.lan),
            plan(&pool, MIDNIGHT + 20),
            &mut store,
            &returned,
        )
        .unwrap();
        assert!(feed.is_active());
        assert!(feed.queued()[0].is_whole());
        assert_eq!(&rig.queue()[..expected.len()], expected);
        assert_eq!(plays(&store), [expected[0].clone()]);
    }
}

#[test]
fn matching_spotify_track_and_position_are_not_permission_to_feed_another_source() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        let pool = multi_movement_pool();
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
        feed.start(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        let queue = rig.queue();
        let before = feed.snapshot();
        let mut observation = sample(&rig);
        for source in [
            None,
            Some("http://example.invalid/announcement.wav"),
            Some("x-sonos-vli:test-player:2,spotify:test-session"),
            Some("x-rincon-queue:another-player#0"),
        ] {
            observation.media_uri = source.map(str::to_owned);
            for mode in [
                None,
                Some("NORMAL"),
                Some("SHUFFLE"),
                Some("NOT_IMPLEMENTED"),
            ] {
                observation.play_mode = mode.map(str::to_owned);
                for transport in [
                    TransportState::Playing,
                    TransportState::Paused,
                    TransportState::Stopped,
                ] {
                    observation.transport = Some(transport);
                    assert_eq!(
                        feed.on_playback(
                            &rig.speakers(&NoIo),
                            plan(&pool, MIDNIGHT + 10),
                            &mut store,
                            &observation,
                        )
                        .unwrap(),
                        0
                    );
                    assert_eq!(feed.snapshot(), before);
                    assert!(plays(&store).is_empty());
                }
            }
        }
        assert_eq!(rig.queue(), queue);
        let returned = sample(&rig);
        feed.on_playback(
            &rig.speakers(&rig.lan),
            plan(&pool, MIDNIGHT + 20),
            &mut store,
            &returned,
        )
        .unwrap();
        assert_eq!(plays(&store).len(), 1, "the owned queue is observed again");
        assert_eq!(rig.queue(), queue);
    }
}

struct SourceOnly<'a>(&'a SimLan);

impl Transport for SourceOnly<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        assert!(
            action.contains("#GetMediaInfo"),
            "a delayed queue event must stop before queue reads or writes: {action}"
        );
        self.0.soap_post(host, path, action, body)
    }
}

#[test]
fn a_delayed_queue_event_cannot_refill_while_a_temporary_source_plays() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        let pool = works_of(&song_items());
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
        feed.start(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        let last = feed.queued().last().unwrap().first().unwrap();
        control::play_queue_from(&rig.lan, &rig.houses, &rig.coordinator, last).unwrap();
        let mut delayed = sample(&rig);
        let queue = rig.queue();
        let retained = feed.snapshot();
        let length = feed.queue_len;
        control::play_uri(
            &rig.lan,
            &rig.houses,
            &rig.coordinator,
            "http://example.invalid/radio.mp3",
            "",
        )
        .unwrap();

        // A changed event length must not invalidate the retained queue either.
        for reported_length in [length, length.map(|length| length + 1)] {
            delayed.queue_length = reported_length;
            assert_eq!(
                feed.on_playback(
                    &rig.speakers(&SourceOnly(&rig.lan)),
                    plan(&pool, MIDNIGHT + 10),
                    &mut store,
                    &delayed,
                )
                .unwrap(),
                0
            );
            assert_eq!(feed.snapshot(), retained);
            assert_eq!(feed.queue_len, length);
            assert!(!feed.accepted_playback);
            assert!(plays(&store).is_empty());
            assert_eq!(rig.queue(), queue);
        }
        let live = sample(&rig);
        assert_eq!(
            live.media_uri.as_deref(),
            Some("http://example.invalid/radio.mp3")
        );
        assert_eq!(live.transport, Some(TransportState::Playing));

        control::play_queue_from(&rig.lan, &rig.houses, &rig.coordinator, last).unwrap();
        assert_eq!(
            feed.on_playback(
                &rig.speakers(&rig.lan),
                plan(&pool, MIDNIGHT + 20),
                &mut store,
                &sample(&rig),
            )
            .unwrap(),
            1
        );
        assert_eq!(&rig.queue()[..queue.len()], queue);
        assert_eq!(
            plays(&store),
            [queue[usize::try_from(last - 1).unwrap()].clone()]
        );
        assert!(feed.accepted_playback);
    }
}

#[test]
fn a_delayed_queue_event_cannot_complete_a_partial_work_during_an_announcement() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        let pool = multi_movement_pool();
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
        let flaky = Flaky::new(rig.sim.lan());
        flaky.fail_add_in(2);
        assert!(
            feed.start(&rig.speakers(&flaky), plan(&pool, MIDNIGHT), &mut store)
                .is_err()
        );
        assert!(!feed.queued()[0].is_whole());
        control::play_queue_from(&rig.lan, &rig.houses, &rig.coordinator, 1).unwrap();
        let delayed = sample(&rig);
        let queue = rig.queue();
        let retained = feed.snapshot();
        let length = feed.queue_len;
        control::play_uri(
            &rig.lan,
            &rig.houses,
            &rig.coordinator,
            "http://example.invalid/announcement.wav",
            "",
        )
        .unwrap();

        assert_eq!(
            feed.on_playback(
                &rig.speakers(&SourceOnly(&rig.lan)),
                plan(&pool, MIDNIGHT + 10),
                &mut store,
                &delayed,
            )
            .unwrap(),
            0
        );
        assert_eq!(feed.snapshot(), retained);
        assert_eq!(feed.queue_len, length);
        assert!(!feed.accepted_playback);
        assert!(plays(&store).is_empty());
        assert_eq!(rig.queue(), queue);

        control::play_queue_from(&rig.lan, &rig.houses, &rig.coordinator, 1).unwrap();
        feed.on_playback(
            &rig.speakers(&rig.lan),
            plan(&pool, MIDNIGHT + 20),
            &mut store,
            &sample(&rig),
        )
        .unwrap();
        assert!(feed.queued()[0].is_whole());
        assert!(rig.queue().len() > queue.len());
        assert_eq!(plays(&store), queue);
    }
}

#[test]
fn an_explicit_dj_start_can_replace_a_temporary_source() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        control::play_uri(
            &rig.lan,
            &rig.houses,
            &rig.coordinator,
            "http://example.invalid/radio.mp3",
            "",
        )
        .unwrap();
        let pool = works_of(&song_items());
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
        feed.start(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        let live = sample(&rig);
        assert!(live.is_queue_of(&rig.coordinator));
        assert_eq!(live.transport, Some(TransportState::Playing));
        assert!(feed.is_active());
        assert_eq!(rig.queue().len(), 2);
        assert!(plays(&store).is_empty());
    }
}
