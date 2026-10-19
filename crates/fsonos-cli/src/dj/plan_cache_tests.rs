//! sp4f.5: a skip keeps the built pool while nothing it is built from
//! changed, and rebuilds it after a library sync or an edit to moods.toml or
//! preferences.toml. (A start always rebuilds.)

use super::State;
use fsonos_core::store::MemStore;
use fsonos_spotify::cache::apply_library_read;
use fsonos_spotify::library::{LibraryItem, Origin};
use fsonos_spotify::steer::Moods;
use std::path::PathBuf;

/// A song of `artist`'s, saved on its own album.
fn song(n: usize, artist: &str) -> LibraryItem {
    LibraryItem {
        source_uri: format!("spotify:track:plan-cache-{n}"),
        title: format!("Song {n}"),
        artists: vec![artist.into()],
        artist_id: None,
        album: Some(format!("Album {n}")),
        album_uri: Some(format!("spotify:album:plan-cache-{n}")),
        album_artists: vec![artist.into()],
        disc_number: Some(1),
        track_number: Some(1),
        added_at: None,
        genres: vec!["pop".into()],
        release_year: Some(2015),
        taste_pm: None,
        label: None,
        duration_secs: Some(200),
        explicit: false,
        origin: Origin::SavedAlbum,
    }
}

/// A fresh data directory's moods.toml path (the file itself absent).
fn data_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "fsonos-plan-cache-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("moods.toml")
}

#[test]
fn the_pool_is_rebuilt_only_after_a_sync_or_an_edit_to_moods_or_preferences() {
    let moods_file = data_dir();
    let moods = Some(&moods_file);
    let mut store = MemStore::default();
    let mut items = vec![song(1, "Artist One"), song(2, "Artist Two")];
    apply_library_read(&mut store, &items).unwrap();
    let mut state = State {
        moods: Moods::builtin(),
        ..State::default()
    };
    state.refresh(&store, moods, 0).unwrap();
    assert_eq!(state.pool.len(), 2, "the first refresh builds the pool");

    // The cache grows, but nothing says so: the next skip keeps the pool.
    items.push(song(3, "Artist Three"));
    apply_library_read(&mut store, &items).unwrap();
    state.refresh(&store, moods, 0).unwrap();
    assert_eq!(
        state.pool.len(),
        2,
        "kept: nothing it is built from changed"
    );

    // A library sync (its freshness notice) rebuilds it.
    state.planned = false;
    state.refresh(&store, moods, 0).unwrap();
    assert_eq!(state.pool.len(), 3, "rebuilt after a sync");

    // So does an edit to preferences.toml beside moods.toml...
    items.push(song(4, "Artist Four"));
    apply_library_read(&mut store, &items).unwrap();
    std::fs::write(
        moods_file.with_file_name("preferences.toml"),
        "explicit = true\n",
    )
    .unwrap();
    state.refresh(&store, moods, 0).unwrap();
    assert_eq!(state.pool.len(), 4, "rebuilt after a preferences edit");

    // ...and to moods.toml.
    items.push(song(5, "Artist Five"));
    apply_library_read(&mut store, &items).unwrap();
    state.refresh(&store, moods, 0).unwrap();
    assert_eq!(state.pool.len(), 4, "kept again");
    std::fs::write(&moods_file, "# the owner's moods\n").unwrap();
    state.refresh(&store, moods, 0).unwrap();
    assert_eq!(state.pool.len(), 5, "rebuilt after a moods edit");
}
