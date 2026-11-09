//! Queue selection is not evidence that the selected music started playing.

use super::*;
use fsonos_proto::control as transport;

#[test]
fn a_refused_play_waits_for_playing_before_recording_history() {
    for s2 in [false, true] {
        let mut rig = Rig::generation(s2);
        let lan = rig.lan.clone();
        let pool = multi_movement_pool();
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
        rig.sim.upnp_fault(ROOM, "Play", 701).unwrap();
        assert!(
            feed.start(&rig.speakers(&lan), plan(&pool, MIDNIGHT), &mut store)
                .is_err()
        );
        assert!(feed.is_active());
        let first = feed.queued()[0].movements[0].uri.clone();
        rig.pump(&lan, &mut feed, plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        assert_eq!(
            rig.playback.of(&rig.coordinator).unwrap().transport,
            Some(TransportState::Stopped)
        );
        assert!(
            plays(&store).is_empty(),
            "queue selection followed by refused Play is unheard"
        );
        assert!(feed.recorded.is_none());

        rig.sim.clear_faults(ROOM).unwrap();
        transport::play(&lan, rig.host()).unwrap();
        rig.pump(&lan, &mut feed, plan(&pool, MIDNIGHT + 10), &mut store)
            .unwrap();
        assert_eq!(
            plays(&store),
            [first.clone()],
            "the same position records when it starts"
        );
        let playing = rig.playback.of(&rig.coordinator).unwrap().clone();
        feed.on_playback(
            &rig.speakers(&lan),
            plan(&pool, MIDNIGHT + 11),
            &mut store,
            &playing,
        )
        .unwrap();
        rig.nudge();
        rig.pump(&lan, &mut feed, plan(&pool, MIDNIGHT + 12), &mut store)
            .unwrap();
        assert_eq!(
            plays(&store),
            [first],
            "repeated observations and pause/resume count once"
        );
    }
}

#[test]
fn seeking_while_paused_does_not_count_the_next_movement_until_resume() {
    let mut rig = Rig::new();
    let lan = rig.lan.clone();
    let pool = multi_movement_pool();
    let mut store = MemStore::default();
    let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
    feed.start(&rig.speakers(&lan), plan(&pool, MIDNIGHT), &mut store)
        .unwrap();
    let first = feed.queued()[0].movements[0].uri.clone();
    let second = feed.queued()[0].movements[1].clone();
    rig.pump(&lan, &mut feed, plan(&pool, MIDNIGHT), &mut store)
        .unwrap();
    assert_eq!(plays(&store), [first.clone()]);

    transport::pause(&lan, rig.host()).unwrap();
    transport::seek_track(&lan, rig.host(), second.position.unwrap()).unwrap();
    rig.pump(&lan, &mut feed, plan(&pool, MIDNIGHT + 10), &mut store)
        .unwrap();
    assert_eq!(
        rig.playback.of(&rig.coordinator).unwrap().transport,
        Some(TransportState::Paused)
    );
    assert_eq!(
        plays(&store),
        [first.clone()],
        "a paused seek must not add listening history"
    );

    transport::play(&lan, rig.host()).unwrap();
    rig.pump(&lan, &mut feed, plan(&pool, MIDNIGHT + 20), &mut store)
        .unwrap();
    assert_eq!(plays(&store), [first, second.uri]);
}

#[test]
fn unknown_and_buffering_observations_do_not_claim_listening() {
    let rig = Rig::new();
    let pool = multi_movement_pool();
    let mut store = MemStore::default();
    let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
    feed.start(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
        .unwrap();
    let first = &feed.queued()[0].movements[0];
    let mut observation = PlayerPlayback::default();
    observation.media_uri = Some(format!("x-rincon-queue:{}#0", rig.coordinator.0));
    observation.queue_position = first.position;
    observation.track_uri = Some(spotify_queue_uri(&first.uri));
    observation.queue_length = feed.queue_len;
    for state in [
        None,
        Some(TransportState::Transitioning),
        Some(TransportState::Stopped),
        Some(TransportState::Paused),
    ] {
        observation.transport = state;
        feed.on_playback(
            &rig.speakers(&rig.lan),
            plan(&pool, MIDNIGHT + 1),
            &mut store,
            &observation,
        )
        .unwrap();
        assert!(plays(&store).is_empty(), "{state:?}");
        assert!(feed.recorded.is_none());
    }
    observation.transport = Some(TransportState::Playing);
    feed.on_playback(
        &rig.speakers(&rig.lan),
        plan(&pool, MIDNIGHT + 2),
        &mut store,
        &observation,
    )
    .unwrap();
    assert_eq!(plays(&store).len(), 1);
}
