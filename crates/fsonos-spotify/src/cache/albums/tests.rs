//! A status observation may inherit progress; an actual read owns its result.

use super::*;

fn status(pending: usize) -> AlbumContinuation {
    AlbumContinuation {
        owner: Some("test-owner".into()),
        generation: Some(3),
        pending,
        ..AlbumContinuation::default()
    }
}

fn progressed(offset: u32) -> AlbumContinuation {
    let uri = "spotify:album:TestAlbum".to_owned();
    AlbumContinuation {
        cursor: Some(uri.clone()),
        partial: Some(PartialAlbum {
            uri,
            offset,
            total: Some(50),
            pages: 1,
            tracks: (0..offset)
                .map(|number| PartialTrack {
                    disc_number: 1,
                    track_number: number + 1,
                    source_uri: format!("spotify:track:Partial{number}"),
                    title: format!("Movement {number}"),
                    duration_secs: Some(180),
                    explicit: Some(false),
                })
                .collect(),
        }),
        advanced: usize::try_from(offset).unwrap(),
        ..status(2)
    }
}

#[test]
fn status_only_observations_keep_the_previous_offset_and_rotation() {
    let previous = progressed(8);
    let mut observed = status(3);
    observed.inherit_progress(&previous);
    assert_eq!(observed.partial, previous.partial);
    assert_eq!(observed.cursor, previous.cursor);
    assert_eq!(observed.pending, 3);
    assert_eq!(observed.advanced, 0);
}

#[test]
fn new_pages_cannot_be_rolled_back_by_an_older_scheduler_checkpoint() {
    let previous = progressed(8);
    let mut current = progressed(16);
    let expected = current.clone();
    current.inherit_progress(&previous);
    assert_eq!(current, expected);
}

#[test]
fn completed_or_failed_attempts_cannot_resurrect_an_older_partial() {
    let previous = progressed(8);
    for (pending, completed) in [(0, 1), (1, 1), (1, 0)] {
        let mut current = AlbumContinuation {
            completed,
            cursor: Some("spotify:album:LaterAlbum".into()),
            ..status(pending)
        };
        let expected = current.clone();
        current.inherit_progress(&previous);
        assert_eq!(current, expected);
    }
    let mut complete = status(0);
    complete.inherit_progress(&previous);
    assert!(complete.partial.is_none());
    assert!(complete.cursor.is_none());
}

#[test]
fn invalidated_or_other_owner_progress_is_never_inherited() {
    let previous = progressed(8);
    let mut old_generation = previous.clone();
    old_generation.generation = Some(2);
    let mut another_owner = previous.clone();
    another_owner.owner = Some("another-owner".into());
    let mut invalidated = previous;
    invalidated.requires_refresh = true;
    for previous in [old_generation, another_owner, invalidated] {
        let mut current = status(2);
        let expected = current.clone();
        current.inherit_progress(&previous);
        assert_eq!(current, expected);
    }
}
