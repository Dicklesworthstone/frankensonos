//! ContentDirectory events must invalidate queue ownership even while playback
//! is paused and an owner's edit leaves both length and current track unchanged.

use super::*;
use fsonos_proto::control as transport;

const OWNER: &str = "spotify:track:QueueInvalidationOwner";

struct Paused {
    rig: Rig,
    pool: WorkPool,
    store: MemStore,
    feed: QueueFeed,
    observation: PlayerPlayback,
}

impl Paused {
    fn new(s2: bool) -> Self {
        let rig = Rig::generation(s2);
        let pool = works_of(&song_items());
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
        feed.lookahead = 3;
        feed.start(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        transport::pause(&rig.lan, rig.host()).unwrap();
        let playing = control::playback(&rig.lan, &rig.houses, &rig.coordinator).unwrap();
        let mut observation = PlayerPlayback::default();
        observation.media_uri = Some(transport::get_media_info(&rig.lan, rig.host()).unwrap().uri);
        observation.track_uri = Some(playing.position.uri);
        observation.queue_position = Some(playing.position.track);
        observation.queue_length = feed.queue_len;
        observation.transport = Some(TransportState::Paused);
        feed.on_playback(
            &rig.speakers(&rig.lan),
            plan(&pool, MIDNIGHT),
            &mut store,
            &observation,
        )
        .unwrap();
        Self {
            rig,
            pool,
            store,
            feed,
            observation,
        }
    }

    fn observe(&mut self) -> Result<usize, FeedError> {
        self.feed.on_playback(
            &self.rig.speakers(&self.rig.lan),
            plan(&self.pool, MIDNIGHT + 1),
            &mut self.store,
            &self.observation,
        )
    }

    fn replace_last(&self) {
        let queue = self.rig.queue();
        self.rig.owner_removes(u32::try_from(queue.len()).unwrap());
        // `owner_adds` uses the S2 fixture; learn this household's actual
        // parameters so the same regression covers S1 too.
        control::queue_spotify_tracks(
            &self.rig.lan,
            &self.rig.houses,
            &self.rig.coordinator,
            &[(OWNER, "Owner's inserted song")],
        )
        .unwrap();
        assert_eq!(self.rig.queue().len(), queue.len());
    }

    fn writes_since(&self, marker: usize) -> Vec<String> {
        self.rig
            .sim
            .soap_log()
            .into_iter()
            .skip(marker)
            .filter(|entry| entry.action != "Browse" && !entry.action.starts_with("Get"))
            .map(|entry| entry.action)
            .collect()
    }
}

#[test]
fn invalidation_is_lazy_and_retains_the_feed_until_a_fresh_read() {
    let mut run = Paused::new(true);
    let snapshot = run.feed.snapshot();
    run.feed.dry_end = run.feed.queue_len;
    assert!(run.feed.accepted_playback());
    let marker = run.rig.sim.soap_log().len();
    run.feed.invalidate_queue();
    run.feed.invalidate_queue();
    assert_eq!(
        run.rig.sim.soap_log().len(),
        marker,
        "no polling or mutation"
    );
    assert_eq!(run.feed.snapshot(), snapshot);
    assert_eq!(run.feed.queue_len, None);
    assert_eq!(
        run.feed.dry_end, run.observation.queue_length,
        "a retry retains its original end, never its evidence"
    );
    assert!(!run.feed.accepted_playback());
    assert!(run.feed.is_active());
    assert_eq!(run.observe().unwrap(), 0);
    assert_eq!(run.feed.queue_len, run.observation.queue_length);
    assert!(run.writes_since(marker).is_empty());
}

#[test]
fn same_length_edits_are_reconciled_before_refilling_and_keep_owner_insertions() {
    for s2 in [false, true] {
        let mut run = Paused::new(s2);
        run.replace_last();
        let edited = run.rig.queue();
        let heard = plays(&run.store);
        let marker = run.rig.sim.soap_log().len();
        run.feed.invalidate_queue();
        assert_eq!(
            run.observe().unwrap(),
            1,
            "replace the removed lookahead work"
        );
        let after = run.rig.queue();
        assert_eq!(&after[..edited.len()], &edited);
        assert_eq!(after[edited.len() - 1], OWNER);
        assert!(
            run.feed
                .queued()
                .iter()
                .all(|work| { work.movements.iter().all(|movement| movement.uri != OWNER) })
        );
        assert_eq!(run.feed.queued().len(), 4);
        assert_eq!(run.feed.queued().last().unwrap().first(), Some(5));
        assert_eq!(
            transport::get_transport_info(&run.rig.lan, run.rig.host())
                .unwrap()
                .state,
            TransportState::Paused
        );
        assert_eq!(
            plays(&run.store),
            heard,
            "an edit does not become listening"
        );
        assert!(
            run.writes_since(marker)
                .iter()
                .all(|action| action == "AddURIToQueue")
        );
    }
}

#[test]
fn a_failed_invalidated_read_cannot_refill_or_publish_a_partial_model() {
    for s2 in [false, true] {
        let mut run = Paused::new(s2);
        run.replace_last();
        run.feed.invalidate_queue();
        let snapshot = run.feed.snapshot();
        let queue = run.rig.queue();
        run.rig.sim.upnp_fault(ROOM, "Browse", 701).unwrap();
        let marker = run.rig.sim.soap_log().len();
        assert!(run.observe().is_err());
        assert_eq!(run.feed.snapshot(), snapshot);
        assert_eq!(run.feed.queue_len, None);
        assert!(!run.feed.accepted_playback());
        assert!(run.writes_since(marker).is_empty());
        run.rig.sim.clear_faults(ROOM).unwrap();
        assert_eq!(run.rig.queue(), queue);
        assert_eq!(
            run.observe().unwrap(),
            1,
            "a later observation retries the read"
        );
        assert_eq!(&run.rig.queue()[..queue.len()], &queue);
    }
}

#[test]
fn an_entire_same_length_replacement_relinquishes_without_touching_owner_music() {
    for s2 in [false, true] {
        let mut run = Paused::new(s2);
        let length = run.rig.queue().len();
        remove_all_tracks_from_queue(&run.rig.lan, run.rig.host()).unwrap();
        let tracks = vec![(OWNER, "Owner's replacement"); length];
        control::queue_spotify_tracks(&run.rig.lan, &run.rig.houses, &run.rig.coordinator, &tracks)
            .unwrap();
        let queue = run.rig.queue();
        assert_eq!(queue.len(), length);
        let marker = run.rig.sim.soap_log().len();
        run.feed.invalidate_queue();
        assert_eq!(run.observe().unwrap(), 0);
        assert!(!run.feed.is_active());
        assert!(run.feed.snapshot().is_none());
        assert!(run.writes_since(marker).is_empty());
        assert_eq!(run.rig.queue(), queue);
        assert!(plays(&run.store).is_empty());
    }
}

#[test]
fn a_temporary_source_retains_invalidation_until_the_queue_returns() {
    let mut run = Paused::new(true);
    run.replace_last();
    run.feed.invalidate_queue();
    let snapshot = run.feed.snapshot();
    let queue_source = run.observation.media_uri.take();
    run.observation.media_uri = Some("http://example.invalid/announcement.wav".into());
    let marker = run.rig.sim.soap_log().len();
    assert_eq!(run.observe().unwrap(), 0);
    assert_eq!(run.rig.sim.soap_log().len(), marker);
    assert_eq!(run.feed.queue_len, None);
    assert_eq!(run.feed.snapshot(), snapshot);
    run.observation.media_uri = queue_source;
    assert_eq!(run.observe().unwrap(), 1);
}
