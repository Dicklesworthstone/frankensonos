//! Library search ranking over a small synthetic library and favorites.

use fsonos_core::favorites::{Favorite, FavoriteKind};
use fsonos_core::search::{HitSource, search, search_all};
use fsonos_core::store::{LibraryEntry, LibraryOrigin};
use fsonos_types::Track;

fn entry(id: &str, title: &str, artist: &str, album: &str) -> LibraryEntry {
    LibraryEntry {
        track: Track {
            title: title.into(),
            artist: Some(artist.into()),
            album: Some(album.into()),
            source_uri: format!("spotify:track:{id}"),
            uri: None,
            duration_secs: Some(200),
        },
        is_classical: true,
        added: 0,
        album_uri: None,
        album_artists: None,
        origin: LibraryOrigin::default(),
        disc_number: None,
        track_number: None,
        work_key: None,
        ..LibraryEntry::default()
    }
}

fn library() -> Vec<LibraryEntry> {
    vec![
        entry(
            "cello",
            "Cello Suite No. 1 in G Major, BWV 1007: I. Prélude",
            "Johann Sebastian Bach; Yo-Yo Ma",
            "Six Evolutions",
        ),
        entry(
            "aria",
            "Goldberg Variations, BWV 988: Aria",
            "Johann Sebastian Bach; Glenn Gould",
            "Bach: The Goldberg Variations (1981)",
        ),
        entry(
            "var1",
            "Goldberg Variations, BWV 988: Variatio 1. a 1 Clav.",
            "Johann Sebastian Bach; Glenn Gould",
            "Bach: The Goldberg Variations (1981)",
        ),
        entry(
            "dvorak9",
            "Symphony No. 9 in E Minor, Op. 95 \"From the New World\": II. Largo",
            "Antonín Dvořák; Berliner Philharmoniker",
            "Dvořák: Symphony No. 9",
        ),
        entry(
            "beet5",
            "Symphony No. 5 in C Minor, Op. 67: I. Allegro con brio",
            "Ludwig van Beethoven; Wiener Philharmoniker",
            "Beethoven: Symphonies 5 & 7",
        ),
        entry(
            "mahler5",
            "Symphony No. 5: IV. Adagietto",
            "Gustav Mahler; Berliner Philharmoniker",
            "Mahler: Symphony No. 5",
        ),
    ]
}

fn favorites() -> Vec<Favorite> {
    vec![Favorite {
        id: "FV:2/7".into(),
        title: "Evening Radio".into(),
        kind: FavoriteKind::Stream,
        uri: Some("x-rincon-mp3radio://stream.example.invalid/x.mp3".into()),
        metadata: String::new(),
        description: Some("Classical station".into()),
        art_uri: None,
    }]
}

fn top(query: &str) -> Vec<String> {
    search(&library(), &favorites(), query, 3)
        .into_iter()
        .map(|h| match h.source {
            HitSource::Library { source_uri } => {
                source_uri.trim_start_matches("spotify:track:").to_string()
            }
            HitSource::Favorite { id } => id,
            HitSource::Album { source_uri } | HitSource::Playlist { source_uri } => source_uri,
        })
        .collect()
}

#[test]
fn every_word_must_match_and_the_title_counts_most() {
    assert_eq!(top("goldberg gould")[..2], ["aria", "var1"]);
    assert_eq!(top("adagietto"), ["mahler5"]);
    assert!(top("bach mahler").is_empty(), "no result has both");
}

#[test]
fn a_catalog_number_picks_its_work() {
    // "bwv" alone matches every Bach title; the number decides.
    assert_eq!(top("bwv 988")[..2], ["aria", "var1"]);
    assert_eq!(top("bwv 1007"), ["cello"]);
    assert_eq!(top("op 67"), ["beet5"]);
}

#[test]
fn accents_prefixes_and_one_typo_still_match() {
    assert_eq!(top("dvorak 9"), ["dvorak9"]);
    assert_eq!(top("Dvořák largo"), ["dvorak9"]);
    assert_eq!(top("beethov"), ["beet5"], "prefix");
    assert_eq!(top("beethovn"), ["beet5"], "one letter missing");
    assert!(top("bthvn").is_empty(), "two edits is too far");
}

#[test]
fn favorites_are_searched_too() {
    assert_eq!(top("radio"), ["FV:2/7"]);
    assert_eq!(
        top("classical station"),
        ["FV:2/7"],
        "the description counts"
    );
}

#[test]
fn limits_and_empty_queries() {
    assert_eq!(search(&library(), &[], "symphony", 2).len(), 2);
    assert!(search(&library(), &[], "  ", 5).is_empty());
    assert!(search(&library(), &[], "symphony", 0).is_empty());
    let hits = search(&library(), &[], "berliner", 10);
    assert_eq!(hits.len(), 2, "artists are searched");
    assert!(
        hits.iter()
            .all(|h| h.subtitle.as_deref().unwrap().contains("Berliner"))
    );
}

/// A saved album's tracks (two of them), plus a liked track from another
/// album, whose album is not the owner's.
fn with_albums() -> Vec<LibraryEntry> {
    let track = |id: &str, title: &str, album: &str, origin: LibraryOrigin| LibraryEntry {
        album_uri: Some(format!("spotify:album:{album}")),
        album_artists: Some("The Lanterns".into()),
        origin,
        ..entry(id, title, "The Lanterns", &album.replace('-', " "))
    };
    let mut all = library();
    all.extend([
        track(
            "tide",
            "Low Tide",
            "harbor-lights",
            LibraryOrigin::SavedAlbum,
        ),
        track(
            "window",
            "Every Window",
            "harbor-lights",
            LibraryOrigin::Both,
        ),
        track(
            "kite",
            "Kite Season",
            "paper-moons",
            LibraryOrigin::LikedTrack,
        ),
    ]);
    all
}

#[test]
fn a_saved_album_is_a_result_of_its_own() {
    // Named by its title, the album comes first, then its tracks (once
    // each: one album hit for its two tracks).
    let hits = search(&with_albums(), &[], "harbor lights", 5);
    assert_eq!(hits.len(), 3, "{hits:?}");
    assert_eq!(
        hits[0].source,
        HitSource::Album {
            source_uri: "spotify:album:harbor-lights".into()
        }
    );
    assert_eq!(hits[0].title, "harbor lights");
    assert_eq!(hits[0].subtitle.as_deref(), Some("The Lanterns"));
    // A track's own title outranks the album it is on.
    let hits = search(&with_albums(), &[], "low tide", 5);
    assert_eq!(
        hits[0].source,
        HitSource::Library {
            source_uri: "spotify:track:tide".into()
        }
    );
    // The artist names both; the tracks come first in a tie.
    let hits = search(&with_albums(), &[], "lanterns", 10);
    assert_eq!(hits.len(), 4, "three tracks and one album: {hits:?}");
    assert!(matches!(hits[3].source, HitSource::Album { .. }));
    // A liked track's album is not the owner's.
    assert!(
        search(&with_albums(), &[], "paper moons", 5)
            .iter()
            .all(|h| !matches!(h.source, HitSource::Album { .. }))
    );
}

#[test]
fn an_un_saved_album_is_not_an_album_result() {
    // The owner un-saved "harbor lights": its rows are retired
    // (candidate == Some(false)) but keep the SavedAlbum origin. They must
    // not resurface as a saved-album hit; track hits are unaffected.
    let mut lib = with_albums();
    for e in lib
        .iter_mut()
        .filter(|e| e.album_uri.as_deref() == Some("spotify:album:harbor-lights"))
    {
        e.candidate = Some(false);
    }
    let hits = search(&lib, &[], "harbor lights", 5);
    assert!(
        hits.iter()
            .all(|h| !matches!(h.source, HitSource::Album { .. })),
        "retired album returned as album hit: {hits:?}"
    );
    // The tracks themselves still rank (track hits include retired rows).
    assert!(
        search(&lib, &[], "low tide", 5)
            .iter()
            .any(|h| matches!(h.source, HitSource::Library { .. })),
        "retired album's track lost from track hits"
    );
    // Legacy rows (candidate == None) keep the old behavior: album hit.
    let hits = search(&with_albums(), &[], "harbor lights", 5);
    assert!(matches!(hits[0].source, HitSource::Album { .. }));
}

#[test]
fn a_playlist_is_found_by_its_name() {
    let playlists = [
        (
            "spotify:playlist:sunday".to_owned(),
            "Sunday Morning".to_owned(),
        ),
        (
            "spotify:playlist:goldberg".to_owned(),
            "Goldberg Variations".to_owned(),
        ),
    ];
    let hits = search_all(&library(), &playlists, &[], "sunday morning", 5);
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(
        hits[0].source,
        HitSource::Playlist {
            source_uri: "spotify:playlist:sunday".into()
        }
    );
    assert_eq!(
        (hits[0].title.as_str(), hits[0].subtitle.as_deref()),
        ("Sunday Morning", None)
    );
    // Named like library tracks, a playlist comes after them in a tie.
    let hits = search_all(&library(), &playlists, &[], "goldberg", 5);
    assert!(
        matches!(
            hits.last().map(|h| &h.source),
            Some(HitSource::Playlist { .. })
        ),
        "{hits:?}"
    );
    assert!(hits.len() > 1);
    // Without playlists, search is as before.
    assert!(search(&library(), &[], "sunday morning", 5).is_empty());
}
