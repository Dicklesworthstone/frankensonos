//! Feedback follows an owned recording, not a mutable slot or a shared work key.

use super::super::attribution::movement as observed;
use super::*;
use fsonos_core::store::MemStore;
use fsonos_spotify::classical::CandidatePool;
use fsonos_spotify::dj::{DjConfig, PickReason};
use fsonos_spotify::feed::{FeedSnapshot, Movement as QueuedMovement, QueuedWork};
use fsonos_spotify::library::{LibraryItem, Origin};
use fsonos_spotify::works::{Completeness, group_works};

fn work(take: &str) -> Work {
    let items: Vec<_> = ["I. Prelude", "II. Allemande", "III. Courante"]
        .iter()
        .enumerate()
        .map(|(index, title)| LibraryItem {
            source_uri: format!("spotify:track:{take}Track{index}"),
            title: format!("Cello Suite No. 1 in G Major, BWV 1007: {title}"),
            artists: vec![
                "Johann Sebastian Bach".into(),
                format!("Test Ensemble {take}"),
            ],
            album: Some(format!("Test Album {take}")),
            album_uri: Some(format!("spotify:album:{take}")),
            disc_number: Some(1),
            track_number: Some(u32::try_from(index + 1).unwrap()),
            duration_secs: Some(180),
            genres: vec!["baroque".into()],
            origin: Origin::SavedAlbum,
            ..LibraryItem::default()
        })
        .collect();
    let works = group_works(CandidatePool::build(&items).tracks());
    assert_eq!(works.len(), 1);
    assert_eq!(works[0].completeness, Completeness::Complete);
    works[0].clone()
}

fn queued(work: &Work, start: u32) -> QueuedWork {
    QueuedWork {
        title: work.title.clone(),
        composer: work.composer.clone(),
        work_key: work.work_key.clone(),
        movements: work
            .movements
            .iter()
            .enumerate()
            .map(|(index, movement)| QueuedMovement {
                uri: movement.track.source_uri.clone(),
                title: movement.track.title.clone(),
                position: Some(start + u32::try_from(index).unwrap()),
                added: true,
            })
            .collect(),
        reason: PickReason {
            factors: Vec::new(),
            relaxed: Vec::new(),
            summary: "test".into(),
        },
    }
}

#[allow(clippy::field_reassign_with_default)] // The position clock is private to core.
fn playback(queued: &QueuedWork, index: usize) -> PlayerPlayback {
    let mut playback = PlayerPlayback::default();
    playback.transport = PLAYING;
    playback.queue_position = queued.movements[index].position;
    playback.track_uri = Some(format!(
        "x-sonos-spotify:{}?sid=9",
        queued.movements[index].uri.replace(':', "%3a")
    ));
    playback
}

fn sample(work: &Work, index: usize) -> Movement {
    let queued = queued(work, 1);
    let state = playback(&queued, index);
    let pool = WorkPool::from_works(vec![work.clone()]);
    let mut movement = observed(&[queued], &pool, &state, None).unwrap();
    movement.started = true;
    movement
}

fn feed(queued: QueuedWork) -> QueueFeed {
    QueueFeed::restore(
        FeedSnapshot {
            zone: "test-coordinator".into(),
            earlier: Vec::new(),
            lookahead: 1,
            queued: vec![queued],
            playing: Some(1),
            recorded: None,
            played: Vec::new(),
        },
        DjConfig::default(),
        1,
    )
}

#[test]
fn the_exact_recording_supplies_performer_and_duration_not_the_first_work_key_match() {
    let a = work("A");
    let b = work("B");
    assert_eq!(a.work_key, b.work_key);
    let queued = queued(&b, 10);
    let state = playback(&queued, 1);
    let pool = WorkPool::from_works(vec![a, b.clone()]);
    let seen = observed(&[queued], &pool, &state, None).unwrap();
    assert_eq!(
        seen.duration_secs,
        Some(180),
        "library duration fills missing live metadata"
    );
    assert_eq!(seen.index, 1);
    assert!(
        !seen.started,
        "an unknown initial position is not an observed track start"
    );
    assert_eq!(
        seen.evidence,
        FeedbackSignal::about(&b, Signal::FullListen, 0)
    );
}

#[test]
fn ownership_needs_both_the_uri_and_position_and_a_unique_recording() {
    let work = work("A");
    let queue = queued(&work, 1);
    let pool = WorkPool::from_works(vec![work.clone()]);
    let mut state = playback(&queue, 0);
    state.track_uri = Some("spotify:track:Unrelated".into());
    assert!(observed(std::slice::from_ref(&queue), &pool, &state, None).is_none());
    state = playback(&queue, 0);
    state.queue_position = Some(99);
    assert!(observed(std::slice::from_ref(&queue), &pool, &state, None).is_none());
    state = playback(&queue, 0);
    assert!(observed(&[queue.clone(), queue.clone()], &pool, &state, None).is_none());
    let duplicate = WorkPool::from_works(vec![work.clone(), work]);
    assert!(observed(std::slice::from_ref(&queue), &duplicate, &state, None).is_none());
    let mut reversed = queue.clone();
    reversed.movements.reverse();
    assert!(observed(&[reversed], &pool, &state, None).is_none());
    let mut duplicate_slot = queue.clone();
    duplicate_slot.movements.push(queue.movements[0].clone());
    assert!(observed(&[duplicate_slot], &pool, &state, None).is_none());
}

#[test]
fn all_movements_must_be_heard_in_order_before_one_completion_signal() {
    let work = work("A");
    let mut listening = Listening::default();
    for index in 0..3 {
        assert_eq!(
            listening.on_change(
                i64::try_from(index).unwrap() * 180,
                PLAYING,
                Some(sample(&work, index)),
            ),
            None
        );
    }
    assert_eq!(
        listening.on_change(540, Some(TransportState::Stopped), None),
        Some((work.work_key.clone(), Signal::FullListen))
    );
    assert_eq!(
        listening.on_change(541, Some(TransportState::Stopped), None),
        None
    );
}

#[test]
fn starting_at_the_finale_skipping_a_movement_or_changing_recordings_is_not_a_full_listen() {
    let a = work("A");
    let b = work("B");
    for route in [
        vec![(&a, 2)],
        vec![(&a, 0), (&a, 2)],
        vec![(&a, 0), (&b, 1), (&b, 2)],
    ] {
        let mut listening = Listening::default();
        let mut now = 0;
        for (work, index) in route {
            assert_eq!(
                listening.on_change(now, PLAYING, Some(sample(work, index))),
                None
            );
            now += 180;
        }
        assert_eq!(
            listening.on_change(now, Some(TransportState::Stopped), None),
            None
        );
    }
    let mut listening = Listening::default();
    listening.on_change(0, PLAYING, Some(sample(&a, 0)));
    // An early skipped first movement emits a negative signal, not eventual completion.
    assert_eq!(
        listening.on_change(10, PLAYING, Some(sample(&a, 1))),
        Some((a.work_key.clone(), Signal::EarlySkip))
    );
    listening.on_change(190, PLAYING, Some(sample(&a, 2)));
    assert_eq!(
        listening.on_change(370, Some(TransportState::Stopped), None),
        None
    );
}

#[test]
fn unproven_durations_and_partial_works_do_not_earn_whole_work_credit() {
    let mut work = work("A");
    for partial in [false, true] {
        let mut listening = Listening::default();
        if partial {
            work.completeness = Completeness::Partial;
        }
        for index in 0..3 {
            let mut movement = sample(&work, index);
            if !partial && index == 0 {
                movement.duration_secs = None;
            }
            assert_eq!(
                listening.on_change(i64::try_from(index).unwrap() * 180, PLAYING, Some(movement),),
                None
            );
        }
        assert_eq!(
            listening.on_change(540, Some(TransportState::Stopped), None),
            None
        );
    }
}

#[test]
fn queue_position_changes_and_mid_track_observations_cannot_invent_early_skips() {
    let work = work("A");
    let mut listening = Listening::default();
    let mut current = sample(&work, 0);
    current.started = false;
    listening.on_change(0, PLAYING, Some(current));
    assert_eq!(
        listening.on_change(10, Some(TransportState::Stopped), None),
        None
    );
    listening.on_change(20, PLAYING, Some(sample(&work, 0)));
    let mut shifted = sample(&work, 0);
    shifted.position = 50;
    assert_eq!(listening.on_change(30, PLAYING, Some(shifted)), None);
    assert_eq!(
        listening.on_change(40, Some(TransportState::Stopped), None),
        None
    );
}

#[test]
fn same_track_observations_retain_attribution_when_the_pool_is_reloaded() {
    let work = work("A");
    let previous = sample(&work, 0);
    let queue = queued(&work, 1);
    let state = playback(&queue, 0);
    let seen = observed(&[queue], &WorkPool::default(), &state, Some(&previous)).unwrap();
    assert_eq!(seen.evidence, previous.evidence);
    assert_eq!(seen.recording, previous.recording);
}

#[test]
fn completed_feedback_keeps_its_recording_metadata_through_reload_and_is_written_once() {
    for classical in [true, false] {
        let a = work("A");
        let mut b = work("B");
        for movement in &mut b.movements {
            movement.classical = classical;
        }
        let queue = queued(&b, 1);
        let mut state = playback(&queue, 2);
        state.transport = Some(TransportState::Stopped);
        let feed = feed(queue);
        let mut listening = Listening {
            current: Some((sample(&b, 2), 180)),
            prefix_complete: true,
            ..Listening::default()
        };
        // The recording that played was removed from the pool by a library refresh.
        let pool = WorkPool::from_works(vec![a]);
        let mut store = MemStore::default();
        assert!(implicit(
            &mut listening,
            &feed,
            &pool,
            &mut store,
            &state,
            0,
            1000
        ));
        let rows = store.feedback_between(0..2000).unwrap();
        assert_eq!(
            rows,
            [FeedbackSignal::about(&b, Signal::FullListen, 1000).to_store()]
        );
        assert!(!implicit(
            &mut listening,
            &feed,
            &pool,
            &mut store,
            &state,
            0,
            1001
        ));
        assert_eq!(store.feedback_between(0..2000).unwrap(), rows);
    }
}

#[test]
fn loss_of_ownership_and_non_dj_playback_are_not_dislikes() {
    let work = work("A");
    for active in [true, false] {
        let queue = queued(&work, 1);
        let mut state = playback(&queue, 0);
        state.track_uri = Some("https://stream.example.invalid/music".into());
        let feed = if active {
            feed(queue)
        } else {
            QueueFeed::new(
                &fsonos_types::PlayerId("test-coordinator".into()),
                DjConfig::default(),
                1,
            )
        };
        let mut listening = Listening::default();
        listening.on_change(0, PLAYING, Some(sample(&work, 0)));
        let mut store = MemStore::default();
        let pool = WorkPool::from_works(vec![work.clone()]);
        assert!(!implicit(
            &mut listening,
            &feed,
            &pool,
            &mut store,
            &state,
            0,
            10
        ));
        assert!(store.feedback_between(0..100).unwrap().is_empty());
        assert_eq!(
            listening.on_change(20, Some(TransportState::Stopped), None),
            None
        );
    }
}

#[test]
fn spotify_identity_is_a_whole_track_uri_not_a_substring_in_another_source() {
    for uri in [
        "https://example.invalid/spotify:track:AbC123",
        "spotify:track:AbC123/other",
        "spotify:track:AbC123#fragment",
        "spotify:track:",
        "spotify:album:AbC123",
    ] {
        assert_eq!(source_of(uri), None, "{uri}");
    }
    for uri in [
        "spotify:track:AbC123",
        "spotify%3atrack%3aAbC123",
        "x-sonos-spotify:spotify%3Atrack%3AAbC123?sid=9",
        "x-sonos-spotify:spotify:track:AbC123?sid=9",
    ] {
        assert_eq!(source_of(uri).as_deref(), Some("spotify:track:AbC123"));
    }
}

#[test]
fn repeated_track_uris_in_one_recording_still_have_distinct_movement_positions() {
    let mut work = work("A");
    work.movements[1].track.source_uri = work.movements[0].track.source_uri.clone();
    let mut listening = Listening::default();
    for index in 0..3 {
        let movement = sample(&work, index);
        assert_eq!(movement.index, index);
        assert_eq!(
            listening.on_change(i64::try_from(index).unwrap() * 180, PLAYING, Some(movement)),
            None
        );
    }
    assert_eq!(
        listening.on_change(540, Some(TransportState::Stopped), None),
        Some((work.work_key, Signal::FullListen))
    );
}
