//! Accepted genre enrichment must change an already running selection plan.

use super::*;
use fsonos_spotify::steer::{DjConstraints, admit, haystack};

const ARTIST: &str = "0000000000000000000001";

fn untagged() -> (ObservedStore, State) {
    let (mut store, _) = fixture();
    let mut track = song(1);
    track.artist_id = Some(ARTIST.into());
    track.genres.clear();
    apply_library_read(&mut store, &[track]).unwrap();
    let mut state = State::default();
    state.reload(&store, None, 0).unwrap();
    assert!(state.pool.works()[0].genres().is_empty());
    (store, state)
}

fn permits_jazz(state: &State) -> bool {
    let works = state.pool.works();
    let haystacks = works.iter().map(haystack).collect::<Vec<_>>();
    let (allowed, relaxed) = admit(
        works,
        &haystacks,
        &DjConstraints {
            include_genres: vec!["jazz".into()],
            strict: true,
            ..DjConstraints::default()
        },
        1,
    );
    assert!(relaxed.is_empty());
    !allowed.is_empty()
}

#[test]
fn external_genre_enrichment_changes_a_warmed_strict_selection_without_library_sync() {
    let (mut store, mut state) = untagged();
    let entries = store.library().unwrap();
    let owner = store.spotify_library_owner().unwrap();
    assert!(!permits_jazz(&state));

    store
        .save_artist_genres(ARTIST, &["jazz".into()], 1)
        .unwrap();
    assert!(!state.plan_is_current(&store, None).unwrap());
    state.refresh(&store, None, 1).unwrap();
    assert!(permits_jazz(&state));
    assert_eq!(state.pool.works()[0].genre(), Some("jazz"));
    assert_eq!(store.library().unwrap(), entries);
    assert_eq!(store.spotify_library_owner().unwrap(), owner);

    // A later successful read can retire only the overlay without rewriting
    // the library. The next pick must obey that evidence too.
    store.save_artist_genres(ARTIST, &[], 2).unwrap();
    state.refresh(&store, None, 2).unwrap();
    assert!(!permits_jazz(&state));
    assert!(state.pool.works()[0].genres().is_empty());
}

#[test]
fn genre_order_updates_variety_but_identical_enrichment_reuses_the_plan() {
    let (mut store, mut state) = untagged();
    store
        .save_artist_genres(ARTIST, &["jazz".into(), "pop".into()], 1)
        .unwrap();
    state.refresh(&store, None, 1).unwrap();
    assert_eq!(state.pool.works()[0].genre(), Some("jazz"));
    let reads = store.library_reads.get();

    store
        .save_artist_genres(ARTIST, &["jazz".into(), "pop".into(), "jazz".into()], 2)
        .unwrap();
    assert!(state.plan_is_current(&store, None).unwrap());
    state.refresh(&store, None, 2).unwrap();
    assert_eq!(store.library_reads.get(), reads);

    store
        .save_artist_genres(ARTIST, &["pop".into(), "jazz".into()], 3)
        .unwrap();
    state.refresh(&store, None, 3).unwrap();
    assert!(store.library_reads.get() > reads);
    assert_eq!(state.pool.works()[0].genre(), Some("pop"));
}

#[test]
fn genre_changes_during_rebuild_cannot_publish_or_retain_a_stale_plan() {
    let (store, mut state) = untagged();
    *store.genres_while_reading.borrow_mut() = Some((ARTIST.into(), vec!["jazz".into()]));
    let error = state.reload(&store, None, 1).unwrap_err();
    assert_eq!(error.code, fsonos_api::ErrorCode::NotReady);
    assert!(!state.planned);
    assert!(state.pool.is_empty(), "the old untagged plan was retired");

    state.refresh(&store, None, 2).unwrap();
    assert!(state.planned);
    assert!(permits_jazz(&state));
}

#[test]
fn failed_rebuild_after_genre_publication_does_not_keep_old_selection() {
    let (mut store, mut state) = untagged();
    store
        .save_artist_genres(ARTIST, &["jazz".into()], 1)
        .unwrap();
    store.fail_library.set(true);
    assert!(state.refresh(&store, None, 1).is_err());
    assert!(!state.planned);
    assert!(state.pool.is_empty());

    store.fail_library.set(false);
    state.refresh(&store, None, 2).unwrap();
    assert!(permits_jazz(&state));
}
