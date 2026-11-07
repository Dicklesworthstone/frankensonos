use super::super::{browse, browse_catalog};
use super::*;
use fsonos_core::store::AlbumTrack;

fn track(id: &str, title: &str, album: &str, disc: u32, number: u32) -> LibraryEntry {
    let mut entry = LibraryEntry::default();
    entry.track.source_uri = format!("spotify:track:{id}");
    entry.track.title = title.into();
    entry.track.artist = Some("Björk".into());
    entry.track.album = Some("Same title".into());
    entry.track.duration_secs = Some(180);
    entry.album_uri = Some(format!("spotify:album:{album}"));
    entry.disc_number = Some(disc);
    entry.track_number = Some(number);
    entry
}

fn library() -> Vec<LibraryEntry> {
    let mut rows = (0_u32..117)
        .map(|n| {
            let mut row = track(
                &format!("T{n:03}"),
                if n % 3 == 0 { "Jóga" } else { "Other" },
                if n % 2 == 0 { "A" } else { "B" },
                1 + n / 70,
                1 + n % 70,
            );
            if n % 5 == 0 {
                row.track.artist = None;
            }
            if n % 7 == 0 {
                row.track.album = None;
            }
            if n % 11 == 0 {
                row.album_artists = Some("Album performer".into());
            }
            row.explicit = n % 13 == 0;
            row.candidate = Some(false);
            row
        })
        .collect::<Vec<_>>();
    let mut invalid = track("Invalid", "Hidden", "A", 1, 1);
    invalid.track.source_uri = "file:///not-a-track".into();
    rows.push(invalid);
    let mut duplicate = rows[1].clone();
    duplicate.track.title = "Later duplicate must not replace the first".into();
    rows.push(duplicate);
    rows
}

fn catalog() -> CachedAlbum {
    CachedAlbum {
        fetched_at: 1234,
        tracks: [
            (2, 1, "T001", "Finale", Some(false)),
            (1, 3, "Missing", "Jóga", Some(true)),
            (1, 2, "Repeated", "Second", None),
            (1, 1, "Repeated", "First", Some(false)),
            (0, 1, "Invalid", "Zero disc", Some(false)),
        ]
        .into_iter()
        .map(
            |(disc_number, track_number, id, title, explicit)| AlbumTrack {
                source_uri: format!("spotify:track:{id}"),
                title: title.into(),
                disc_number,
                track_number,
                duration_secs: Some(120),
                explicit,
            },
        )
        .collect(),
    }
}

fn indexed(
    library: &[LibraryEntry],
    lists: &[(String, String)],
    query: &BrowseQuery,
    album: Option<&CachedAlbum>,
) -> BrowsePage {
    let index = Library::new(library);
    View::new(Key::of(query), Some(&index), lists, album).page(Some(&index), query)
}

#[test]
fn every_category_filter_and_page_matches_the_original_projection() {
    let rows = library();
    let lists = vec![
        ("spotify:playlist:Z".into(), "Jóga favorites".into()),
        ("spotify:playlist:A".into(), "Another".into()),
        ("spotify:playlist:A".into(), "Duplicate ignored".into()),
        ("https://invalid.example/".into(), "Hidden".into()),
    ];
    for kind in ["tracks", "albums", "playlists"] {
        for album_uri in [None, Some("spotify:album:A"), Some("spotify:album:Absent")] {
            if kind != "tracks" && album_uri.is_some() {
                continue;
            }
            for q in ["", "  ", "joga", "bjork same", "OTHER", "no matches"] {
                for offset in [0, 1, 50, 999] {
                    for limit in [1, 7, MAX_PAGE] {
                        let query = BrowseQuery {
                            kind: Some(kind.into()),
                            album_uri: album_uri.map(str::to_owned),
                            q: Some(q.into()),
                            offset: Some(offset),
                            limit: Some(limit),
                        };
                        assert_eq!(
                            indexed(&rows, &lists, &query, None),
                            browse(&rows, &lists, &query).unwrap(),
                            "{query:?}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn completed_album_occurrences_and_wire_metadata_match_the_original_projection() {
    let rows = library();
    let album = catalog();
    for q in ["", "joga", "bjork", "first", "not present"] {
        for offset in [0, 1, 3, 99] {
            let query = BrowseQuery {
                album_uri: Some("spotify:album:A".into()),
                q: Some(q.into()),
                offset: Some(offset),
                limit: Some(2),
                ..BrowseQuery::default()
            };
            let old = browse_catalog(&rows, &[], &query, Some(&album)).unwrap();
            let new = indexed(&rows, &[], &query, Some(&album));
            assert_eq!(new, old);
            assert_eq!(
                serde_json::to_vec(&new).unwrap(),
                serde_json::to_vec(&old).unwrap()
            );
        }
    }
}

#[test]
fn missing_first_album_title_and_cross_album_saved_metadata_are_preserved() {
    let mut first = track("A", "First saved", "A", 1, 1);
    first.track.album = None;
    let mut later = track("B", "Second saved", "A", 1, 2);
    later.track.album = Some("Later known title".into());
    let mut elsewhere = track("T001", "Known elsewhere", "B", 9, 9);
    elsewhere.track.artist = Some("Exact URI performer".into());
    elsewhere.explicit = true;
    let rows = [elsewhere, later, first];
    let query = BrowseQuery {
        album_uri: Some("spotify:album:A".into()),
        ..BrowseQuery::default()
    };
    let album = catalog();
    let page = indexed(&rows, &[], &query, Some(&album));
    assert_eq!(
        page,
        browse_catalog(&rows, &[], &query, Some(&album)).unwrap()
    );
    assert_eq!(page.items[0].album.as_deref(), Some("Later known title"));
    let known = page.items.last().unwrap();
    assert_eq!(known.subtitle, "Exact URI performer");
    assert!(known.explicit && known.explicit_known);
}

#[test]
fn an_unfiltered_catalog_change_invalidates_even_a_hidden_occurrence() {
    let rows = library();
    let query = BrowseQuery {
        album_uri: Some("spotify:album:A".into()),
        q: Some("first".into()),
        limit: Some(1),
        ..BrowseQuery::default()
    };
    let mut album = catalog();
    let before = indexed(&rows, &[], &query, Some(&album));
    album.tracks[0].duration_secs = Some(999);
    let after = indexed(&rows, &[], &query, Some(&album));
    assert_eq!(before.items, after.items);
    assert_ne!(before.revision, after.revision);
    assert_eq!(
        after,
        browse_catalog(&rows, &[], &query, Some(&album)).unwrap()
    );
}

#[test]
fn equal_position_occurrences_keep_the_legacy_sequence_hash_and_display_ties() {
    let rows = library();
    let mut album = catalog();
    let mut repeated = album.tracks[1].clone();
    repeated.track_number = 1;
    repeated.disc_number = 1;
    repeated.title = "First".into();
    album.tracks.push(repeated);
    album.tracks.reverse();
    let query = BrowseQuery {
        album_uri: Some("spotify:album:A".into()),
        ..BrowseQuery::default()
    };
    assert_eq!(
        indexed(&rows, &[], &query, Some(&album)),
        browse_catalog(&rows, &[], &query, Some(&album)).unwrap()
    );
}

#[test]
fn equivalent_normalized_filters_share_revision_across_offsets() {
    let rows = library();
    let index = Library::new(&rows);
    let mut view = View::new(Key::Tracks, Some(&index), &[], None);
    let first = view.page(
        Some(&index),
        &BrowseQuery {
            q: Some("JÓGA".into()),
            limit: Some(1),
            ..BrowseQuery::default()
        },
    );
    let later = view.page(
        Some(&index),
        &BrowseQuery {
            q: Some("joga".into()),
            offset: Some(1),
            ..BrowseQuery::default()
        },
    );
    assert_eq!(first.revision, later.revision);
    assert_eq!(first.total, later.total);
    assert_eq!(first.next_offset, Some(1));
}

#[test]
fn owned_string_capacity_is_counted_in_the_memory_allowance() {
    let mut row = track("A", "Title", "A", 1, 1);
    row.track.title = String::with_capacity(8192);
    row.track.title.push_str("Large capacity");
    let mut item = item("track", "spotify:track:A", "", "");
    item.title = row.track.title;
    let rows = Rows::new(vec![item], false);
    assert!(rows.bytes() >= 8192 + size_of::<BrowseItem>());
    let mut uri = String::with_capacity(4096);
    uri.push_str("spotify:album:A");
    assert!(Key::Album(uri).heap_bytes() >= 4096);
}
