//! Silent DJ scene preparation against real S1/S2 simulator sockets.

use super::*;
use fsonos_core::playback::PlaybackObservation;
use fsonos_proto::control as transport;

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

fn writes(rig: &Rig, from: usize) -> Vec<String> {
    rig.sim.soap_log()[from..]
        .iter()
        .filter(|call| call.action != "Browse" && !call.action.starts_with("Get"))
        .map(|call| call.action.clone())
        .collect()
}

fn inactive(rig: &Rig) {
    assert!(matches!(
        sample(rig).transport,
        Some(TransportState::Paused | TransportState::Stopped)
    ));
}

#[test]
fn silent_load_pauses_the_old_source_before_adding_and_selecting_whole_works() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        transport::set_av_transport_uri(
            &rig.lan,
            rig.host(),
            "http://example.invalid/old-radio.mp3",
            "",
        )
        .unwrap();
        transport::play(&rig.lan, rig.host()).unwrap();
        let pool = multi_movement_pool();
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
        let before = rig.sim.soap_log().len();
        let queued = feed
            .load(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap()
            .to_vec();
        let sent = writes(&rig, before);
        assert_eq!(sent.first().map(String::as_str), Some("Pause"));
        assert!(!sent.iter().any(|action| action == "Play"), "{sent:?}");
        assert!(sent.iter().any(|action| action == "AddURIToQueue"));
        assert_eq!(queued.len(), 2);
        assert!(queued.iter().all(QueuedWork::is_whole));
        assert_eq!(rig.position(), queued[0].first().unwrap());
        assert_whole_works(&rig, &pool, 1);
        inactive(&rig);
        assert!(feed.is_active() && feed.is_suspended());
        assert!(plays(&store).is_empty(), "preparing music is not listening");
    }
}

#[test]
fn silent_load_keeps_owned_playing_paused_and_stopped_occurrences_in_place() {
    for s2 in [false, true] {
        for state in [
            TransportState::Playing,
            TransportState::Paused,
            TransportState::Stopped,
        ] {
            let rig = Rig::generation(s2);
            let pool = multi_movement_pool();
            let mut store = MemStore::default();
            let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
            feed.start(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
                .unwrap();
            let current = feed.queued()[0].movements[1].position.unwrap();
            transport::seek_track(&rig.lan, rig.host(), current).unwrap();
            transport::seek_position(&rig.lan, rig.host(), 37).unwrap();
            match state {
                TransportState::Paused => transport::pause(&rig.lan, rig.host()).unwrap(),
                TransportState::Stopped => transport::stop(&rig.lan, rig.host()).unwrap(),
                _ => {}
            }
            let original = transport::get_position_info(&rig.lan, rig.host()).unwrap();
            let queue = rig.queue();
            let before = rig.sim.soap_log().len();
            feed.load(
                &rig.speakers(&rig.lan),
                plan(&pool, MIDNIGHT + 40),
                &mut store,
            )
            .unwrap();
            let position = transport::get_position_info(&rig.lan, rig.host()).unwrap();
            assert_eq!(position.track, current);
            assert_eq!(position.position_secs, original.position_secs);
            assert_eq!(rig.queue(), queue);
            inactive(&rig);
            let expected = if state == TransportState::Playing {
                vec!["Pause", "SetPlayMode"]
            } else {
                vec!["SetPlayMode"]
            };
            assert_eq!(writes(&rig, before), expected);
            assert!(feed.is_suspended());
            assert!(plays(&store).is_empty());
        }
    }
}

#[test]
fn repeated_load_and_silent_readiness_reuse_the_prepared_set_without_a_library() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        let pool = multi_movement_pool();
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
        feed.load(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        let queue = rig.queue();
        let retained = feed.snapshot();
        let empty = WorkPool::default();
        let before = rig.sim.soap_log().len();
        feed.load_ready(&rig.speakers(&rig.lan), plan(&empty, MIDNIGHT + 10), &store)
            .unwrap();
        assert_eq!(feed.snapshot(), retained);
        assert!(writes(&rig, before).is_empty());
        let before = rig.sim.soap_log().len();
        feed.load(
            &rig.speakers(&rig.lan),
            plan(&empty, MIDNIGHT + 10),
            &mut store,
        )
        .unwrap();
        assert_eq!(rig.queue(), queue);
        assert_eq!(rig.position(), feed.queued()[0].first().unwrap());
        assert_eq!(writes(&rig, before), ["SetPlayMode"]);
        inactive(&rig);
    }
}

#[test]
fn prepared_feeds_ignore_stale_playing_exhaustion_and_error_events_after_restart() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        let pool = multi_movement_pool();
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
        feed.load(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        // A pending continuation cannot survive the explicit silent intent.
        feed.dry_end = Some(u32::try_from(rig.queue().len()).unwrap());
        feed.suspend();
        let snapshot = feed.snapshot().unwrap();
        assert!(snapshot.suspended);
        assert_eq!(snapshot.dry_end, None);
        let encoded = serde_json::to_string(&snapshot).unwrap();
        let snapshot = serde_json::from_str(&encoded).unwrap();
        let mut feed = QueueFeed::restore(snapshot, DjConfig::default(), 17);
        let stopped = sample(&rig);
        let mut playing = stopped.clone();
        playing.transport = Some(TransportState::Playing);
        let mut error = stopped.clone();
        error.transport_status = Some("ERROR_OCCURRED".into());
        let queue = rig.queue();
        let before = rig.sim.soap_log().len();
        for event in [&stopped, &playing, &error] {
            assert_eq!(
                feed.on_playback(
                    &rig.speakers(&rig.lan),
                    plan(&pool, MIDNIGHT + 10),
                    &mut store,
                    event,
                )
                .unwrap(),
                0
            );
            assert!(feed.is_suspended());
            assert!(!feed.accepted_playback());
        }
        assert!(writes(&rig, before).is_empty());
        assert_eq!(rig.queue(), queue);
        assert!(plays(&store).is_empty());
        inactive(&rig);
    }
}

#[test]
fn a_fresh_owned_playing_observation_resumes_maintenance_and_records_one_listen() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        let pool = multi_movement_pool();
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
        feed.load(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        feed = QueueFeed::restore(feed.snapshot().unwrap(), DjConfig::default(), 17);
        transport::play(&rig.lan, rig.host()).unwrap();
        let event = sample(&rig);
        let before = rig.sim.soap_log().len();
        for _ in 0..2 {
            assert_eq!(
                feed.on_playback(
                    &rig.speakers(&rig.lan),
                    plan(&pool, MIDNIGHT + 1),
                    &mut store,
                    &event,
                )
                .unwrap(),
                0
            );
        }
        assert!(!feed.is_suspended());
        assert!(feed.accepted_playback());
        assert_eq!(plays(&store).len(), 1);
        assert!(
            writes(&rig, before).is_empty(),
            "observing resume sends no Play"
        );

        let next = feed.queued()[1].first().unwrap();
        transport::seek_track(&rig.lan, rig.host(), next).unwrap();
        let event = sample(&rig);
        assert_eq!(
            feed.on_playback(
                &rig.speakers(&rig.lan),
                plan(&pool, MIDNIGHT + 100),
                &mut store,
                &event,
            )
            .unwrap(),
            1,
            "normal lookahead resumes after the listener starts the prepared set"
        );
    }
}

#[test]
fn explicit_start_and_skip_can_play_a_silently_prepared_set() {
    for s2 in [false, true] {
        for skip in [false, true] {
            let rig = Rig::generation(s2);
            let pool = multi_movement_pool();
            let mut store = MemStore::default();
            let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
            feed.load(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
                .unwrap();
            let queue = rig.queue();
            let selected = feed.queued()[usize::from(skip)].first().unwrap();
            if skip {
                feed.skip(
                    &rig.speakers(&rig.lan),
                    plan(&pool, MIDNIGHT + 1),
                    &mut store,
                )
                .unwrap();
            } else {
                feed.start(
                    &rig.speakers(&rig.lan),
                    plan(&pool, MIDNIGHT + 1),
                    &mut store,
                )
                .unwrap();
            }
            assert!(!feed.is_suspended());
            assert_eq!(rig.queue(), queue);
            assert_eq!(rig.position(), selected);
            assert_eq!(sample(&rig).transport, Some(TransportState::Playing));
            assert!(
                plays(&store).is_empty(),
                "wait for an actual Playing observation"
            );
        }
    }
}

#[test]
fn start_resumes_a_stopped_prepared_middle_movement_after_restore() {
    for s2 in [false, true] {
        for restore in [false, true] {
            let rig = Rig::generation(s2);
            let pool = multi_movement_pool();
            let mut store = MemStore::default();
            let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
            feed.start(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
                .unwrap();
            let current = feed.queued()[0].movements[1].position.unwrap();
            transport::seek_track(&rig.lan, rig.host(), current).unwrap();
            transport::seek_position(&rig.lan, rig.host(), 37).unwrap();
            transport::stop(&rig.lan, rig.host()).unwrap();
            feed.load(
                &rig.speakers(&rig.lan),
                plan(&pool, MIDNIGHT + 40),
                &mut store,
            )
            .unwrap();
            let selected = transport::get_position_info(&rig.lan, rig.host()).unwrap();
            assert_eq!(selected.track, current);
            let queue = rig.queue();
            if restore {
                feed = QueueFeed::restore(feed.snapshot().unwrap(), DjConfig::default(), 17);
            }
            let before = rig.sim.soap_log().len();
            feed.start(
                &rig.speakers(&rig.lan),
                plan(&pool, MIDNIGHT + 50),
                &mut store,
            )
            .unwrap();
            let resumed = transport::get_position_info(&rig.lan, rig.host()).unwrap();
            assert_eq!(resumed.track, selected.track);
            assert_eq!(resumed.uri, selected.uri);
            assert_eq!(resumed.position_secs, selected.position_secs);
            assert_eq!(rig.queue(), queue);
            assert_eq!(writes(&rig, before), ["SetPlayMode", "Play"]);
            assert_eq!(sample(&rig).transport, Some(TransportState::Playing));
            assert!(!feed.is_suspended());
        }
    }
}

#[test]
fn an_empty_library_does_not_pause_music_or_suspend_an_unprepared_feed() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        transport::set_av_transport_uri(
            &rig.lan,
            rig.host(),
            "http://example.invalid/old-radio.mp3",
            "",
        )
        .unwrap();
        transport::play(&rig.lan, rig.host()).unwrap();
        let pool = WorkPool::default();
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
        let before = rig.sim.soap_log().len();
        assert!(matches!(
            feed.load(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store),
            Err(FeedError::EmptyPool)
        ));
        assert!(writes(&rig, before).is_empty());
        assert!(!feed.is_active());
        assert!(!feed.is_suspended());
        assert!(rig.queue().is_empty());
        assert_eq!(
            transport::get_media_info(&rig.lan, rig.host()).unwrap().uri,
            "http://example.invalid/old-radio.mp3"
        );
        assert_eq!(sample(&rig).transport, Some(TransportState::Playing));
    }
}

struct RefusePause<'a>(&'a SimLan);

impl Transport for RefusePause<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if action.trim_matches('"').ends_with("#Pause") {
            return Err(ProtoError::SoapFault {
                code: 501,
                reason: "synthetic pause refusal".into(),
            });
        }
        self.0.soap_post(host, path, action, body)
    }
}

#[test]
fn a_refused_pause_prevents_silent_load_queue_and_source_writes() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        transport::set_av_transport_uri(
            &rig.lan,
            rig.host(),
            "http://example.invalid/old-radio.mp3",
            "",
        )
        .unwrap();
        transport::play(&rig.lan, rig.host()).unwrap();
        let pool = multi_movement_pool();
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
        let before = rig.sim.soap_log().len();
        assert!(
            feed.load(
                &rig.speakers(&RefusePause(&rig.lan)),
                plan(&pool, MIDNIGHT),
                &mut store,
            )
            .is_err()
        );
        assert!(writes(&rig, before).is_empty());
        assert!(!feed.is_active(), "no queue entries were accepted");
        assert!(rig.queue().is_empty());
        assert_eq!(
            transport::get_media_info(&rig.lan, rig.host()).unwrap().uri,
            "http://example.invalid/old-radio.mp3"
        );
        assert_eq!(sample(&rig).transport, Some(TransportState::Playing));
    }
}

#[test]
fn legacy_feed_snapshots_remain_unsuspended() {
    let snapshot: FeedSnapshot = serde_json::from_value(serde_json::json!({
        "zone": "SyntheticCoordinator",
        "earlier": [],
        "lookahead": 1,
        "queued": [],
        "playing": null,
        "recorded": null
    }))
    .unwrap();
    assert!(!snapshot.suspended);
}

/// Real players may retain native source metadata while omitting the track's
/// metadata or reporting it as unsupported.
struct SourceMetadataOnly<'a> {
    inner: &'a SimLan,
    missing: &'a str,
}

impl Transport for SourceMetadataOnly<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let reply = self.inner.soap_post(host, path, action, body)?;
        if action.contains("#GetPositionInfo") {
            let (before, rest) = reply.split_once("<TrackMetaData>").unwrap();
            let (_, after) = rest.split_once("</TrackMetaData>").unwrap();
            return Ok(format!(
                "{before}<TrackMetaData>{}</TrackMetaData>{after}",
                self.missing,
            ));
        }
        Ok(reply)
    }
}

#[test]
fn direct_source_metadata_prepares_and_resumes_silent_dj_without_favorites_or_track_metadata() {
    for s2 in [false, true] {
        for missing in ["", "NOT_IMPLEMENTED"] {
            let rig = Rig::generation(s2);
            let generation = if s2 { 2 } else { 1 };
            let params = rig.sim.render_params(generation).unwrap();
            let uri = "spotify:track:MnOpQr0123456789StUvWx";
            transport::set_av_transport_uri(
                &rig.lan,
                rig.host(),
                &fsonos_proto::didl::spotify_track_uri(uri, &params),
                &spotify_track_didl(uri, "Selected native music", &params),
            )
            .unwrap();
            transport::play(&rig.lan, rig.host()).unwrap();
            transport::seek_position(&rig.lan, rig.host(), 37).unwrap();
            transport::pause(&rig.lan, rig.host()).unwrap();
            rig.sim.retain_favorites(generation, |_| false).unwrap();
            let source_only = SourceMetadataOnly {
                inner: &rig.lan,
                missing,
            };
            let speakers = rig.speakers(&source_only);
            let household = household_of(&speakers).unwrap();
            let pool = multi_movement_pool();
            let mut store = MemStore::default();
            let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
            let before = rig.sim.soap_log().len();

            feed.load_ready(&speakers, plan(&pool, MIDNIGHT), &store)
                .unwrap();
            assert!(writes(&rig, before).is_empty());
            assert!(store.render_params(household).unwrap().is_none());
            assert!(!feed.is_active());
            let selected = transport::get_position_info(&rig.lan, rig.host()).unwrap();
            assert_eq!(selected.position_secs, Some(37));
            assert_eq!(
                spotify_uri_from_renderer_uri(&selected.uri).as_deref(),
                Some(uri)
            );

            let before = rig.sim.soap_log().len();
            feed.load(&speakers, plan(&pool, MIDNIGHT), &mut store)
                .unwrap();
            assert!(!writes(&rig, before).iter().any(|action| action == "Play"));
            inactive(&rig);
            assert!(feed.is_active() && feed.is_suspended());
            assert!(feed.queued().iter().all(QueuedWork::is_whole));
            assert_eq!(store.render_params(household).unwrap().unwrap().0, params);
            assert!(plays(&store).is_empty());
            let queue = rig.queue();
            let position = rig.position();

            let mut restored =
                QueueFeed::restore(feed.snapshot().unwrap(), DjConfig::default(), 11);
            let before = rig.sim.soap_log().len();
            restored
                .start(&speakers, plan(&pool, MIDNIGHT + 10), &mut store)
                .unwrap();
            assert_eq!(rig.queue(), queue);
            assert_eq!(rig.position(), position);
            assert_eq!(writes(&rig, before), ["SetPlayMode", "Play"]);
            assert_eq!(sample(&rig).transport, Some(TransportState::Playing));
            assert!(!restored.is_suspended());

            // The original direct source is gone and TrackMetaData is still
            // absent. The accepted household cache must sustain later refills.
            let next = restored.queued()[1].first().unwrap();
            transport::seek_track(&rig.lan, rig.host(), next).unwrap();
            let event = sample(&rig);
            assert_eq!(
                restored
                    .on_playback(&speakers, plan(&pool, MIDNIGHT + 20), &mut store, &event)
                    .unwrap(),
                1
            );
            assert!(restored.queued().iter().all(QueuedWork::is_whole));
        }
    }
}
