//! Library search and play history through `Surface`, against `fsonos-sim`
//! players over real sockets: the owner's saved tracks and a household's
//! favorites rank together, and plays started through the surface are
//! recorded for `recent_plays`, newest first, under the room that played
//! them.

use fsonos_api::plan::plan_play;
use fsonos_api::surface::{Surface, Survey};
use fsonos_api::{ErrorCode, PlayFavoriteRequest, PlayRequest, SearchRequest};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{LibraryEntry, LibraryOrigin, MemStore, Store};
use fsonos_sim::{SimHandle, SimHousehold};
use fsonos_types::Track;
use std::time::Duration;

const STREAM: &str = "x-rincon-mp3radio://stream.example.invalid/a.mp3";

fn surface(sim: &SimHandle, store: MemStore) -> Surface {
    let survey: Survey = Box::new(|t| {
        Ok(fsonos_core::inventory::survey(t, &[], Duration::from_millis(500))?.households)
    });
    Surface::new(
        Box::new(sim.lan()),
        survey,
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(store), "mcp")
}

fn library() -> MemStore {
    let mut store = MemStore::default();
    let entry = |title: &str, artist: &str, id: &str| LibraryEntry {
        track: Track {
            title: title.into(),
            artist: Some(artist.into()),
            album: Some("Goldberg Variations".into()),
            source_uri: format!("spotify:track:{id}"),
            uri: None,
            duration_secs: Some(180),
        },
        is_classical: true,
        added: 1_700_000_000,
        album_uri: None,
        album_artists: Some(artist.into()),
        origin: LibraryOrigin::SavedAlbum,
        disc_number: Some(1),
        track_number: Some(1),
        work_key: None,
        ..LibraryEntry::default()
    };
    store
        .upsert_library(&[
            entry(
                "Goldberg Variations, BWV 988: Aria",
                "Glenn Gould",
                "aria0000000000000000a",
            ),
            entry(
                "Symphony No. 9: Largo",
                "Antonin Dvorak",
                "largo000000000000000b",
            ),
        ])
        .unwrap();
    store
}

fn search(query: &str, zone: Option<&str>) -> SearchRequest {
    SearchRequest {
        query: query.into(),
        zone: zone.map(str::to_string),
        limit: None,
    }
}

#[test]
fn the_library_and_a_households_favorites_rank_together() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let surface = surface(&sim, library());
    let hits = surface
        .search_library(&Client::Cli, &search("bwv 988", None))
        .unwrap();
    assert_eq!(hits[0].kind, "track");
    assert_eq!(
        hits[0].source_uri.as_deref(),
        Some("spotify:track:aria0000000000000000a")
    );
    // Favorites need a room: they belong to its household.
    assert!(
        surface
            .search_library(&Client::Cli, &search("sim radio", None))
            .unwrap()
            .is_empty()
    );
    let hits = surface
        .search_library(&Client::Cli, &search("sim radio", Some("Living Room")))
        .unwrap();
    assert_eq!(hits[0].kind, "favorite", "{hits:?}");
    assert!(hits[0].favorite.as_deref().unwrap().starts_with("FV:2/"));
    // Bad requests are coded.
    let empty = surface
        .search_library(&Client::Cli, &search("  ", None))
        .unwrap_err();
    assert_eq!(empty.code, ErrorCode::InvalidArgument);
    let many = SearchRequest {
        limit: Some(500),
        ..search("x", None)
    };
    assert_eq!(
        surface
            .search_library(&Client::Cli, &many)
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
}

#[test]
fn plays_through_the_surface_are_history_newest_first() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let surface = surface(&sim, MemStore::default());
    assert!(
        surface
            .recent_plays(&Client::Cli, None, 20)
            .unwrap()
            .is_empty()
    );

    let req = PlayRequest {
        zone: "Kitchen".into(),
        source_uri: STREAM.into(),
        title: None,
    };
    surface
        .control(&Client::Cli, "play", |h| plan_play(h, &req))
        .unwrap();
    surface
        .play_favorite(
            &Client::Cli,
            &PlayFavoriteRequest {
                zone: "Living Room".into(),
                favorite: "sim radio".into(),
            },
        )
        .unwrap();

    let all = surface.recent_plays(&Client::Cli, None, 20).unwrap();
    assert_eq!(all.len(), 2, "{all:?}");
    assert_eq!(all[0].room.as_deref(), Some("Living Room"), "newest first");
    assert_eq!(all[1].source_uri, STREAM);
    assert_eq!(all[1].room.as_deref(), Some("Kitchen"));

    let kitchen = surface
        .recent_plays(&Client::Cli, Some("kitchen"), 20)
        .unwrap();
    assert_eq!(kitchen.len(), 1);
    assert_eq!(kitchen[0].source_uri, STREAM);
}
