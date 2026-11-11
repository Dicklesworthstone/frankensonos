//! An admitted continuation survives the process, but its observations do not.

use super::*;

fn round_trip(feed: &QueueFeed) -> QueueFeed {
    let bytes = serde_json::to_vec(&feed.snapshot().unwrap()).unwrap();
    let restored = QueueFeed::restore(
        serde_json::from_slice(&bytes).unwrap(),
        DjConfig::default(),
        11,
    );
    assert_eq!(
        restored.queue_len, None,
        "saved positions still need a read"
    );
    assert!(!restored.accepted_playback());
    restored
}

fn refilled_after_refusal(rig: &Rig) -> QueueFeed {
    let mut feed = rig.feed();
    rig.sim.upnp_fault(ROOM, "Play", 701).unwrap();
    let result = rig.tick(&rig.lan, &mut feed, &rig.event());
    assert!(
        matches!(
            result,
            Err(FeedError::Core(fsonos_core::CoreError::Proto(
                ProtoError::SoapFault { code: 701, .. }
            )))
        ),
        "{result:?}"
    );
    assert_eq!(feed.dry_end, Some(4));
    assert_eq!(feed.left_at(), Some(5), "the successor was actually added");
    assert_eq!(
        soap::get_transport_info(&rig.lan, rig.host).unwrap().state,
        TransportState::Stopped
    );
    rig.sim.clear_faults(ROOM).unwrap();
    round_trip(&feed)
}

#[test]
fn a_restored_exhausted_queue_starts_its_confirmed_successor_once() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let mut feed = refilled_after_refusal(&rig);
        assert_eq!(feed.dry_end, Some(4));
        let before = browse_snapshot(&rig.lan, rig.host, QUEUE).unwrap();
        let event = rig.event();
        // The old end no longer matches the refilled queue's length. The
        // durable cursor, rather than a newly invented exhaustion, is needed.
        assert!(!feed.ran_dry(&event));
        let start = rig.sim.soap_log().len();
        assert_eq!(rig.tick(&rig.lan, &mut feed, &event).unwrap(), 0);
        assert_eq!(
            rig.writes_since(start),
            ["SetAVTransportURI", "SetPlayMode", "Seek", "Play"]
        );
        assert_eq!(
            browse_snapshot(&rig.lan, rig.host, QUEUE).unwrap(),
            before,
            "the accepted append is not repeated"
        );
        assert_eq!(
            soap::get_position_info(&rig.lan, rig.host).unwrap().track,
            5
        );
        assert_eq!(
            soap::get_transport_info(&rig.lan, rig.host).unwrap().state,
            TransportState::Playing
        );
        assert!(feed.dry_end.is_none());
        assert!(round_trip(&feed).dry_end.is_none());
    }
}

#[test]
fn legacy_saved_feeds_have_no_pending_exhaustion() {
    let rig = Rig::new(false);
    let feed = refilled_after_refusal(&rig);
    let mut legacy = serde_json::to_value(feed.snapshot().unwrap()).unwrap();
    legacy.as_object_mut().unwrap().remove("dry_end");
    let snapshot: FeedSnapshot = serde_json::from_value(legacy).unwrap();
    assert_eq!(snapshot.dry_end, None);
    let feed = QueueFeed::restore(snapshot, DjConfig::default(), 11);
    assert!(feed.dry_end.is_none());
    let saved = serde_json::to_value(feed.snapshot().unwrap()).unwrap();
    assert!(saved.get("dry_end").is_none());
}

#[test]
fn a_consumed_continuation_cannot_rediscover_exhaustion_before_any_append() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let mut feed = rig.feed();
        let event = rig.event();
        rig.sim.upnp_fault(ROOM, "AddURIToQueue", 701).unwrap();
        assert!(rig.tick(&rig.lan, &mut feed, &event).is_err());
        assert_eq!(feed.dry_end, Some(4));
        assert_eq!(feed.left_at(), Some(4), "no successor was appended");
        let mut saved = feed.snapshot().unwrap();
        saved.dry_end = None;
        let cursor_only = QueueFeed::restore(saved.clone(), DjConfig::default(), 11);
        assert!(
            cursor_only.ran_dry(&event),
            "clearing only the cursor would permit rediscovery"
        );
        // The daemon consumes both fields on disk before a retry; its live
        // copy retains the admitted cursor until the actual outcome is known.
        saved.playing = None;
        let bytes = serde_json::to_vec(&saved).unwrap();
        let mut restored = QueueFeed::restore(
            serde_json::from_slice(&bytes).unwrap(),
            DjConfig::default(),
            11,
        );
        assert!(!restored.ran_dry(&event));
        rig.sim.clear_faults(ROOM).unwrap();
        let start = rig.sim.soap_log().len();
        for _ in 0..3 {
            rig.tick(&rig.lan, &mut restored, &rig.event()).unwrap();
        }
        assert!(
            rig.writes_since(start)
                .iter()
                .all(|action| action == "AddURIToQueue"),
            "the retained queue can be fed silently but never started"
        );
        assert_eq!(
            soap::get_transport_info(&rig.lan, rig.host).unwrap().state,
            TransportState::Stopped
        );
        assert!(restored.dry_end.is_none());
    }
}

#[test]
fn a_restored_continuation_never_resumes_a_user_paused_queue() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let mut feed = refilled_after_refusal(&rig);
        let stopped = rig.event();
        soap::play(&rig.lan, rig.host).unwrap();
        soap::pause(&rig.lan, rig.host).unwrap();
        let start = rig.sim.soap_log().len();
        assert_eq!(rig.tick(&rig.lan, &mut feed, &stopped).unwrap(), 0);
        assert!(
            rig.writes_since(start).is_empty(),
            "a stale stop sends nothing"
        );
        // A current paused observation cancels the pending start. Ordinary
        // silent feeding may continue, but it cannot start or seek the music.
        let mut paused = rig.event();
        paused.transport = Some(TransportState::Paused);
        rig.tick(&rig.lan, &mut feed, &paused).unwrap();
        assert!(feed.dry_end.is_none());
        assert!(round_trip(&feed).dry_end.is_none());
        assert!(
            rig.writes_since(start)
                .iter()
                .all(|action| action == "AddURIToQueue")
        );
        assert_eq!(
            soap::get_transport_info(&rig.lan, rig.host).unwrap().state,
            TransportState::Paused
        );
    }
}

#[test]
fn a_restored_continuation_preserves_a_source_switch_and_same_length_queue_edit() {
    for s2 in [false, true] {
        for source_switch in [false, true] {
            let rig = Rig::new(s2);
            let mut feed = refilled_after_refusal(&rig);
            if source_switch {
                control::play_uri(
                    &rig.lan,
                    &rig.houses,
                    &rig.id,
                    "http://example.invalid/owners-radio.mp3",
                    "",
                )
                .unwrap();
            } else {
                soap::remove_track_range_from_queue(&rig.lan, rig.host, 0, 5, 1).unwrap();
                soap::add_uri_to_queue(
                    &rig.lan,
                    rig.host,
                    "http://example.invalid/owners-track.mp3",
                    "",
                    false,
                )
                .unwrap();
            }
            let before = browse_snapshot(&rig.lan, rig.host, QUEUE).unwrap();
            assert_eq!(before.objects.len(), 5);
            let media = soap::get_media_info(&rig.lan, rig.host).unwrap();
            let state = soap::get_transport_info(&rig.lan, rig.host).unwrap();
            let start = rig.sim.soap_log().len();
            assert_eq!(rig.tick(&rig.lan, &mut feed, &rig.event()).unwrap(), 0);
            assert!(rig.writes_since(start).is_empty());
            assert_eq!(browse_snapshot(&rig.lan, rig.host, QUEUE).unwrap(), before);
            assert_eq!(
                soap::get_media_info(&rig.lan, rig.host).unwrap().uri,
                media.uri
            );
            assert_eq!(soap::get_transport_info(&rig.lan, rig.host).unwrap(), state);
        }
    }
}

#[test]
fn a_restored_continuation_rechecks_the_audience_and_boot_before_playing() {
    for s2 in [false, true] {
        for change in [AudienceChange::Join, AudienceChange::Reboot] {
            let mut rig = Rig::new(s2);
            let mut feed = refilled_after_refusal(&rig);
            let event = rig.event();
            let before = feed.snapshot();
            let observer = AudienceDuringRead {
                rig: &rig,
                change,
                during: ChangeDuring::Observation,
                added: Cell::new(false),
                changed: Cell::new(false),
            };
            let start = rig.sim.soap_log().len();
            assert_eq!(rig.tick(&observer, &mut feed, &event).unwrap(), 0);
            assert!(observer.changed.get());
            assert_eq!(rig.writes_since(start), change.writes());
            assert_eq!(feed.snapshot(), before);
            rig.houses[0].apply_topology(&get_zone_group_state(&rig.lan, rig.host).unwrap());
            let start = rig.sim.soap_log().len();
            assert_eq!(rig.tick(&rig.lan, &mut feed, &rig.event()).unwrap(), 0);
            assert!(
                rig.writes_since(start)
                    .iter()
                    .all(|action| action != "AddURIToQueue")
            );
            assert_eq!(
                soap::get_position_info(&rig.lan, rig.host).unwrap().track,
                5
            );
            assert_eq!(
                soap::get_transport_info(&rig.lan, rig.host).unwrap().state,
                TransportState::Playing
            );
        }
    }
}

struct LostRestartReply<'a> {
    lan: &'a SimLan,
    action: &'static str,
    block_readback: bool,
    lost: Cell<bool>,
}

fn missing_reply() -> ProtoError {
    ProtoError::Network {
        target: "simulated queue restart".into(),
        detail: "the command acted but its reply or confirming read was lost".into(),
    }
}

impl Transport for LostRestartReply<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if self.block_readback && self.lost.get() && action.contains("#GetMediaInfo") {
            return Err(missing_reply());
        }
        let response = self.lan.soap_post(host, path, action, body)?;
        if action.ends_with(&format!("#{}\"", self.action)) && !self.lost.replace(true) {
            return Err(missing_reply());
        }
        Ok(response)
    }
}

#[test]
fn an_unconfirmed_exhaustion_mutation_never_leaves_a_durable_replay() {
    for s2 in [false, true] {
        for action in ["SetAVTransportURI", "SetPlayMode", "Seek", "Play"] {
            let rig = Rig::new(s2);
            let mut feed = rig.feed();
            let event = rig.event();
            let observed = LostRestartReply {
                lan: &rig.lan,
                action,
                block_readback: true,
                lost: Cell::new(false),
            };
            let start = rig.sim.soap_log().len();
            let result = rig.tick(&observed, &mut feed, &event);
            assert!(
                matches!(result, Err(FeedError::RecoveryStopped(_))),
                "{action}: {result:?}"
            );
            assert!(observed.lost.get());
            assert!(!feed.is_active());
            assert!(
                feed.snapshot().is_none(),
                "no feed can replay after restart"
            );
            let writes = rig.writes_since(start);
            assert_eq!(
                writes.iter().filter(|name| name.as_str() == action).count(),
                1
            );
            let queue = browse_snapshot(&rig.lan, rig.host, QUEUE).unwrap();
            assert_eq!(queue.objects.len(), 5, "the confirmed append stays");
            for _ in 0..3 {
                assert_eq!(rig.tick(&observed, &mut feed, &event).unwrap(), 0);
            }
            assert_eq!(rig.writes_since(start), writes);
            assert_eq!(browse_snapshot(&rig.lan, rig.host, QUEUE).unwrap(), queue);
        }
    }
}

#[test]
fn an_exhaustion_play_confirmed_after_a_lost_reply_clears_the_pending_start() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let mut feed = rig.feed();
        let observed = LostRestartReply {
            lan: &rig.lan,
            action: "Play",
            block_readback: false,
            lost: Cell::new(false),
        };
        let start = rig.sim.soap_log().len();
        assert_eq!(rig.tick(&observed, &mut feed, &rig.event()).unwrap(), 1);
        assert!(observed.lost.get());
        assert!(feed.is_active());
        assert!(round_trip(&feed).dry_end.is_none());
        assert_eq!(
            rig.writes_since(start)
                .iter()
                .filter(|action| action.as_str() == "Play")
                .count(),
            1
        );
        assert_eq!(
            soap::get_position_info(&rig.lan, rig.host).unwrap().track,
            5
        );
        assert_eq!(
            soap::get_transport_info(&rig.lan, rig.host).unwrap().state,
            TransportState::Playing
        );
    }
}
