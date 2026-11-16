//! An identical song or complete work belongs to its occurrence, not its URI.
//! Reconciliation and explicit commands run against the real socket simulator.

use super::*;

fn observe(rig: &Rig, feed: &mut QueueFeed, pool: &WorkPool, store: &mut MemStore) {
    let live = control::playback(&rig.lan, &rig.houses, &rig.coordinator).unwrap();
    let mut playback = PlayerPlayback::default();
    playback.transport = Some(live.transport.state);
    playback.queue_position = Some(live.position.track);
    playback.track_uri = Some(live.position.uri);
    playback.queue_length = Some(u32::try_from(rig.queue().len()).unwrap());
    playback.media_uri = Some(
        fsonos_proto::control::get_media_info(&rig.lan, rig.host())
            .unwrap()
            .uri,
    );
    feed.on_playback(
        &rig.speakers(&rig.lan),
        plan(pool, MIDNIGHT),
        store,
        &playback,
    )
    .unwrap();
}

fn insert(rig: &Rig, s2: bool, uri: &str, position: u32) {
    let params = rig.sim.render_params(if s2 { 2 } else { 1 }).unwrap();
    soap::call(
        &rig.lan,
        rig.host(),
        &AV_TRANSPORT,
        "AddURIToQueue",
        &soap::args_xml(&[
            ("InstanceID", "0"),
            ("EnqueuedURI", &spotify_queue_uri(uri)),
            (
                "EnqueuedURIMetaData",
                &spotify_track_didl(uri, "Owner", &params),
            ),
            ("DesiredFirstTrackNumberEnqueued", &position.to_string()),
            ("EnqueueAsNext", "0"),
        ]),
    )
    .unwrap();
}

fn restored(rig: &Rig, played: Vec<QueuedWork>, queued: Vec<QueuedWork>) -> QueueFeed {
    QueueFeed::restore(
        FeedSnapshot {
            zone: rig.coordinator.0.clone(),
            earlier: Vec::new(),
            lookahead: 1,
            queued,
            playing: None,
            recorded: None,
            played,
            recovery: RecoveryBudget::default(),
            dry_end: None,
        },
        DjConfig {
            trim_queue_at: u32::MAX,
            ..DjConfig::default()
        },
        7,
    )
}

fn work(uris: &[&str], first: u32) -> QueuedWork {
    QueuedWork {
        title: "Synthetic work".into(),
        composer: "Synthetic artist".into(),
        work_key: uris.join("/"),
        movements: uris
            .iter()
            .enumerate()
            .map(|(index, uri)| Movement {
                uri: (*uri).into(),
                title: "Synthetic movement".into(),
                position: Some(first + u32::try_from(index).unwrap()),
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

const A: &str = "spotify:track:OwnershipA";
const B: &str = "spotify:track:OwnershipB";
const C: &str = "spotify:track:OwnershipC";
const D: &str = "spotify:track:OwnershipD";

#[test]
fn an_inserted_copy_of_a_waiting_song_cannot_capture_the_dj_skip() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        let pool = works_of(&song_items());
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
        feed.lookahead = 2;
        let started = feed
            .start(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap()
            .to_vec();
        assert_eq!(started.len(), 3);
        let waiting = &started[1];
        assert_eq!(waiting.movements.len(), 1);
        insert(
            &rig,
            s2,
            &waiting.movements[0].uri,
            waiting.first().unwrap(),
        );
        let before = rig.queue();
        let next = feed
            .skip(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        assert_eq!(next.work_key, started[2].work_key);
        assert_eq!(
            next.first(),
            started[2].first().map(|position| position + 1)
        );
        assert_eq!(rig.position(), next.first().unwrap());
        assert_eq!(
            rig.queue(),
            before,
            "neither identical copy is removed or re-enqueued"
        );
        assert!(!feed.queued().iter().any(|w| w.work_key == waiting.work_key));
    }
}

#[test]
fn an_inserted_complete_work_retires_only_the_ambiguous_work() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        for uri in [A, B, C, D] {
            insert(&rig, s2, uri, 0);
        }
        let unique = work(&[D], 4);
        let mut feed = restored(
            &rig,
            Vec::new(),
            vec![work(&[A], 1), work(&[B, C], 2), unique],
        );
        insert(&rig, s2, C, 2);
        insert(&rig, s2, B, 2);
        let before = rig.queue();
        feed.resync(&rig.speakers(&rig.lan)).unwrap();
        assert_eq!(feed.queued(), [work(&[A], 1), work(&[D], 6)]);
        assert_eq!(rig.queue(), before);
    }
}

#[test]
fn an_owners_intact_copy_cannot_capture_the_djs_split_work() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        // The original A/B remains at 1/3; an inserted owner item splits it,
        // and a second intact A/B also precedes our uniquely identified C.
        for uri in [A, D, B, A, B, C] {
            insert(&rig, s2, uri, 0);
        }
        let mut feed = restored(&rig, Vec::new(), vec![work(&[A, B], 1), work(&[C], 3)]);
        let before = rig.queue();
        feed.resync(&rig.speakers(&rig.lan)).unwrap();
        assert_eq!(feed.queued(), [work(&[C], 6)]);
        assert_eq!(rig.queue(), before);
    }
}

#[test]
fn partial_repeated_rotations_keep_only_provable_occurrences() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        for uri in [D, A, A, B, C] {
            insert(&rig, s2, uri, 0);
        }
        let mut feed = restored(
            &rig,
            Vec::new(),
            vec![
                work(&[D], 1),
                work(&[A], 2),
                work(&[B], 3),
                work(&[A], 4),
                work(&[C], 5),
            ],
        );
        let before = rig.queue();
        feed.resync(&rig.speakers(&rig.lan)).unwrap();
        assert_eq!(feed.queued(), [work(&[D], 1), work(&[C], 5)]);
        assert_eq!(rig.queue(), before);
    }
}

#[test]
fn restoring_after_an_owner_playlist_cannot_adopt_its_identical_prefix() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        // The first copy is the owner's playlist; the latter is the DJ's set.
        for uri in [A, B, A, B] {
            insert(&rig, s2, uri, 0);
        }
        let mut feed = restored(&rig, Vec::new(), vec![work(&[A], 3), work(&[B], 4)]);
        control::play_queue_from(&rig.lan, &rig.houses, &rig.coordinator, 3).unwrap();
        let before = rig.queue();
        let marker = rig.sim.soap_log().len();
        let mut store = MemStore::default();
        observe(&rig, &mut feed, &WorkPool::default(), &mut store);
        assert!(
            !feed.is_active(),
            "an ambiguous restored set is not authority to feed"
        );
        assert!(feed.snapshot().is_none());
        assert!(store.recent_plays(None, 10).unwrap().is_empty());
        assert_eq!(rig.position(), 3);
        assert_eq!(rig.queue(), before);
        assert!(
            rig.sim
                .soap_log()
                .iter()
                .skip(marker)
                .all(|entry| { entry.action == "Browse" || entry.action.starts_with("Get") })
        );
    }
}

#[test]
fn repeated_dj_rotations_keep_their_distinct_occurrences_across_restore() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        for uri in [A, B, A, B, C] {
            insert(&rig, s2, uri, 0);
        }
        let played = vec![work(&[A], 1), work(&[B], 2)];
        let queued = vec![work(&[A], 3), work(&[B], 4), work(&[C], 5)];
        let mut feed = restored(&rig, played.clone(), queued.clone());
        let before = rig.queue();
        feed.resync(&rig.speakers(&rig.lan)).unwrap();
        assert_eq!(feed.played, played);
        assert_eq!(feed.queued(), queued);
        // A following owner copy of an already-played song is not needed to
        // satisfy either owned occurrence, so neither migrates onto it.
        insert(&rig, s2, A, 0);
        feed.resync(&rig.speakers(&rig.lan)).unwrap();
        assert_eq!(feed.played, played);
        assert_eq!(feed.queued(), queued);
        assert_eq!(&rig.queue()[..before.len()], before);
    }
}

#[test]
fn identical_whole_work_rotations_remain_owned_in_order() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        for uri in [A, B, A, B, C, D] {
            insert(&rig, s2, uri, 0);
        }
        let queued = vec![work(&[A, B], 1), work(&[A, B], 3), work(&[C, D], 5)];
        let mut feed = restored(&rig, Vec::new(), queued.clone());
        feed.resync(&rig.speakers(&rig.lan)).unwrap();
        assert_eq!(feed.queued(), queued);
    }
}

#[test]
fn a_retired_movement_cannot_reclaim_a_later_owner_copy() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        for uri in [A, B, C] {
            insert(&rig, s2, uri, 0);
        }
        let mut feed = restored(&rig, Vec::new(), vec![work(&[A, B, C], 1)]);
        rig.owner_removes(2);
        feed.resync(&rig.speakers(&rig.lan)).unwrap();
        assert_eq!(feed.queued()[0].movements[1].position, None);
        insert(&rig, s2, B, 2);
        feed.resync(&rig.speakers(&rig.lan)).unwrap();
        let movements = &feed.queued()[0].movements;
        assert_eq!(movements[0].position, Some(1));
        assert_eq!(
            movements[1].position, None,
            "the owner supplied the new copy"
        );
        assert_eq!(movements[2].position, Some(3));
        assert_eq!(rig.queue(), [A, B, C]);
    }
}

#[test]
fn a_dry_retry_cannot_refill_after_its_current_and_waiting_claims_are_lost() {
    for s2 in [false, true] {
        for retained_history in [false, true] {
            let rig = Rig::generation(s2);
            let actual = if retained_history {
                vec![A, B, B]
            } else {
                vec![A, B, A, B]
            };
            for uri in actual {
                insert(&rig, s2, uri, 0);
            }
            let (played, queued) = if retained_history {
                (vec![work(&[A], 1)], vec![work(&[B], 2)])
            } else {
                (Vec::new(), vec![work(&[A, B], 1)])
            };
            let mut feed = restored(&rig, played, queued);
            feed.playing = Some(2);
            feed.dry_end = Some(2);
            let pool = works_of(&song_items());
            let mut store = MemStore::default();
            let before = rig.queue();
            let marker = rig.sim.soap_log().len();
            assert_eq!(
                feed.play_on_past_the_end(
                    &rig.speakers(&rig.lan),
                    plan(&pool, MIDNIGHT),
                    &mut store,
                    2,
                )
                .unwrap(),
                0
            );
            assert!(!feed.is_active());
            assert!(feed.snapshot().is_none());
            assert_eq!(rig.queue(), before);
            assert!(
                rig.sim
                    .soap_log()
                    .iter()
                    .skip(marker)
                    .all(|entry| entry.action == "Browse" || entry.action.starts_with("Get"))
            );
        }
    }
}

/// Supply a large queue at the final reconciliation read, without thousands
/// of setup writes. Every non-browse read still uses the real simulator; any
/// attempted mutation fails the test before it reaches the player.
struct RepeatedQueue<'a> {
    lan: &'a SimLan,
    total: usize,
    reads: Cell<usize>,
}

impl Transport for RepeatedQueue<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if action.contains("#Browse") && body.contains("<ObjectID>Q:0</ObjectID>") {
            self.reads.set(self.reads.get() + 1);
            let number = |name: &str| -> usize {
                let opening = format!("<{name}>");
                body.split_once(opening.as_str())
                    .unwrap()
                    .1
                    .split('<')
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap()
            };
            let start = number("StartingIndex");
            let end = (start + number("RequestedCount")).min(self.total);
            let uri = format!("x-sonos-spotify:{}", spotify_queue_uri(A));
            let mut didl = String::from(
                r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">"#,
            );
            for index in start..end {
                didl.push_str(&format!(
                    r#"<item id="Q:0/{}" parentID="Q:0" restricted="1"><dc:title>Repeated</dc:title><upnp:class>object.item.audioItem.musicTrack</upnp:class><res>{uri}</res></item>"#,
                    index + 1
                ));
            }
            didl.push_str("</DIDL-Lite>");
            return Ok(format!(
                concat!(
                    r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>"#,
                    r#"<u:BrowseResponse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">"#,
                    "<Result>{}</Result><NumberReturned>{}</NumberReturned>",
                    "<TotalMatches>{}</TotalMatches><UpdateID>1</UpdateID>",
                    "</u:BrowseResponse></s:Body></s:Envelope>"
                ),
                fsonos_proto::didl::xml_escape(&didl),
                end - start,
                self.total
            ));
        }
        assert!(
            action.contains("#Get") || action.contains("#Browse"),
            "retired ownership must not authorize {action}"
        );
        self.lan.soap_post(host, path, action, body)
    }
}

#[test]
fn exceeding_the_proof_bound_releases_a_dry_retry_without_writes() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        insert(&rig, s2, D, 0);
        let mut feed = restored(&rig, Vec::new(), vec![work(&vec![A; 2_500], 1)]);
        feed.playing = Some(2_500);
        feed.dry_end = Some(2_500);
        let read = RepeatedQueue {
            lan: &rig.lan,
            total: 2_499,
            reads: Cell::new(0),
        };
        let pool = works_of(&song_items());
        let mut store = MemStore::default();
        let before = rig.queue();
        assert_eq!(
            feed.play_on_past_the_end(
                &rig.speakers(&read),
                plan(&pool, MIDNIGHT),
                &mut store,
                2_500,
            )
            .unwrap(),
            0
        );
        assert!(read.reads.get() > 1, "the complete paged queue was read");
        assert!(!feed.is_active());
        assert!(feed.snapshot().is_none());
        assert_eq!(rig.queue(), before);

        // The owner can explicitly start a fresh session after retirement.
        feed.start(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        assert!(feed.is_active());
        assert_eq!(rig.position(), 2);
        assert_eq!(&rig.queue()[..before.len()], before);
    }
}

#[test]
fn an_explicit_start_reactivates_after_reconciling_an_ambiguous_saved_set() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        for uri in [A, B, A, B] {
            insert(&rig, s2, uri, 0);
        }
        let mut feed = restored(&rig, Vec::new(), vec![work(&[A, B], 1)]);
        let before = rig.queue();
        let pool = works_of(&song_items());
        let mut store = MemStore::default();
        feed.start(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        assert!(feed.is_active());
        assert_eq!(rig.position(), 5);
        assert!(feed.queued().iter().all(|work| work.first().unwrap() > 4));
        assert_eq!(&rig.queue()[..before.len()], before);
    }
}

#[test]
fn an_all_ambiguous_queue_is_released_even_when_the_triggering_event_is_stale() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        for uri in [A, B, A, B] {
            insert(&rig, s2, uri, 0);
        }
        control::play_queue_from(&rig.lan, &rig.houses, &rig.coordinator, 2).unwrap();
        let mut feed = restored(
            &rig,
            Vec::new(),
            vec![work(&[A], 1), work(&[B], 2), work(&[C], 3), work(&[D], 4)],
        );
        feed.playing = Some(2);
        feed.queue_len = Some(4);
        let mut stale = PlayerPlayback::default();
        stale.transport = Some(TransportState::Playing);
        stale.queue_position = Some(1);
        stale.track_uri = Some(format!("x-sonos-spotify:{}", spotify_queue_uri(A)));
        stale.queue_length = Some(4);
        stale.media_uri = Some(
            fsonos_proto::control::get_media_info(&rig.lan, rig.host())
                .unwrap()
                .uri,
        );
        let marker = rig.sim.soap_log().len();
        let mut store = MemStore::default();
        feed.on_playback(
            &rig.speakers(&rig.lan),
            plan(&WorkPool::default(), MIDNIGHT),
            &mut store,
            &stale,
        )
        .unwrap();
        assert!(!feed.is_active());
        assert_eq!(rig.position(), 2);
        assert!(
            rig.sim
                .soap_log()
                .iter()
                .skip(marker)
                .all(|entry| entry.action == "Browse" || entry.action.starts_with("Get"))
        );
        assert!(store.recent_plays(None, 10).unwrap().is_empty());
    }
}

#[test]
fn renderer_recovery_cannot_reuse_claims_from_a_discarded_stale_observation() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        for uri in [A, B, C, B, C, D] {
            insert(&rig, s2, uri, 0);
        }
        control::play_queue_from(&rig.lan, &rig.houses, &rig.coordinator, 2).unwrap();
        let mut feed = restored(
            &rig,
            vec![work(&[A], 1)],
            vec![work(&[B], 2), work(&[C], 3), work(&[D], 6)],
        );
        feed.playing = Some(2);
        feed.queue_len = Some(6);
        let before = feed.snapshot();
        let mut stale = PlayerPlayback::default();
        stale.transport = Some(TransportState::Playing);
        stale.queue_position = Some(1);
        stale.track_uri = Some(format!("x-sonos-spotify:{}", spotify_queue_uri(A)));
        stale.queue_length = Some(6);
        stale.media_uri = Some(
            fsonos_proto::control::get_media_info(&rig.lan, rig.host())
                .unwrap()
                .uri,
        );
        let mut store = MemStore::default();
        feed.on_playback(
            &rig.speakers(&rig.lan),
            plan(&WorkPool::default(), MIDNIGHT),
            &mut store,
            &stale,
        )
        .unwrap();
        assert_eq!(
            feed.snapshot(),
            before,
            "a stale event cannot move the cursor"
        );
        assert_eq!(
            feed.queue_len, None,
            "its discarded proof invalidates the old cache"
        );

        rig.sim.playback_error(ROOM).unwrap();
        let live = control::playback(&rig.lan, &rig.houses, &rig.coordinator).unwrap();
        let mut error = stale;
        error.transport = Some(live.transport.state);
        error.transport_status = Some("ERROR_OCCURRED".into());
        error.queue_position = Some(live.position.track);
        error.track_uri = Some(live.position.uri);
        let marker = rig.sim.soap_log().len();
        let result = feed.on_playback(
            &rig.speakers(&rig.lan),
            plan(&WorkPool::default(), MIDNIGHT),
            &mut store,
            &error,
        );
        assert!(matches!(result, Err(FeedError::RecoveryStopped(_))));
        assert!(!feed.is_active());
        assert_eq!(rig.position(), 2);
        assert!(
            rig.sim
                .soap_log()
                .iter()
                .skip(marker)
                .all(|entry| entry.action == "Browse" || entry.action.starts_with("Get"))
        );
        assert!(store.recent_plays(None, 10).unwrap().is_empty());
    }
}
