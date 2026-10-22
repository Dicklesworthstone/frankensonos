use super::*;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::Policy;
use fsonos_core::store::{ActionFilter, MemStore, Store};

const URI: &str = "spotify:playlist:Observed";

fn query() -> PlaylistQuery {
    PlaylistQuery {
        playlist_uri: URI.into(),
        ..PlaylistQuery::default()
    }
}

fn track(position: u32, id: &str, title: &str) -> PlaylistTrack {
    PlaylistTrack {
        position,
        source_uri: format!("spotify:track:{id}"),
        title: title.into(),
        artist: Some("Élodie & the Quartet".into()),
        album: Some("Silver sessions".into()),
        album_uri: Some("spotify:album:Silver".into()),
        disc_number: Some(2),
        track_number: Some(7),
        duration_secs: Some(181),
        explicit: id == "Repeated",
    }
}

fn observation() -> CachedPlaylist {
    CachedPlaylist::new(
        URI.into(),
        1_790_000_000,
        Some(12),
        9,
        false,
        vec![
            track(1, "Repeated", "Zinc & lights"),
            track(4, "Repeated", "Zinc & lights"),
            track(6, "Middle", "Copper interlude"),
            track(9, "Last", "Amber finale"),
        ],
    )
}

fn projected(query: &PlaylistQuery) -> PlaylistPage {
    page(query, "Ordered listening".into(), Some(observation())).unwrap()
}

#[test]
fn pages_preserve_occurrences_and_guard_the_observation() {
    let mut query = query();
    query.limit = Some(2);
    let first = projected(&query);
    assert_eq!(first.items[0].source_uri, first.items[1].source_uri);
    assert_eq!(
        first
            .items
            .iter()
            .map(|item| item.position)
            .collect::<Vec<_>>(),
        [1, 4]
    );
    assert_eq!(
        (first.cached_total, first.total, first.next_offset),
        (4, 4, Some(2))
    );
    assert_eq!(
        (first.coverage.as_str(), first.source_total, first.examined),
        ("partial", Some(12), 9)
    );

    query.offset = first.next_offset;
    query.expected_revision.clone_from(&first.revision);
    let last = projected(&query);
    assert_eq!(
        last.items
            .iter()
            .map(|item| item.position)
            .collect::<Vec<_>>(),
        [6, 9]
    );
    assert_eq!(last.next_offset, None);
    assert_eq!(last.revision, first.revision);
    query.offset = Some(99);
    assert!(projected(&query).items.is_empty());

    let old = observation();
    let refreshed = CachedPlaylist::new(
        URI.into(),
        old.fetched_at + 1,
        old.source_total,
        old.examined,
        old.complete,
        old.tracks,
    );
    let failure = page(&query, "Ordered listening".into(), Some(refreshed)).unwrap_err();
    assert_eq!(failure.code, ErrorCode::InvalidArgument);
    assert!(failure.hint.contains("offset 0"));
}

#[test]
fn filters_match_all_normalized_words_without_changing_positions_or_coverage() {
    let mut query = query();
    query.q = Some("  ELODIE lights SILVER  ".into());
    query.limit = Some(1);
    let first = projected(&query);
    assert_eq!(first.q, "ELODIE lights SILVER");
    assert_eq!(
        (first.cached_total, first.total, first.next_offset),
        (4, 2, Some(1))
    );
    assert_eq!(first.items[0].position, 1);
    query.offset = first.next_offset;
    query.expected_revision.clone_from(&first.revision);
    assert_eq!(projected(&query).items[0].position, 4);

    query.offset = None;
    query.q = Some("spotify track Repeated".into());
    let absent = projected(&query);
    assert!(absent.items.is_empty());
    assert_eq!(
        (absent.cached_total, absent.total, absent.examined),
        (4, 0, 9)
    );
    assert_eq!(absent.coverage, "partial");
    assert_eq!(absent.revision, first.revision);
}

struct NoLan;

impl fsonos_proto::Transport for NoLan {
    fn soap_post(
        &self,
        _: std::net::IpAddr,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<String, fsonos_proto::ProtoError> {
        panic!("cached playlists must not contact a speaker");
    }
}

fn surface(store: MemStore, policy: Policy) -> Surface {
    Surface::new(
        Box::new(NoLan),
        Box::new(|_| panic!("cached playlists must not discover speakers")),
        policy,
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(store), "playlist-test")
}

#[test]
fn local_reads_distinguish_unread_empty_retained_and_unknown_playlists() {
    let mut store = MemStore::default();
    store
        .save_playlists(&[(URI.into(), "Listed but unread".into())])
        .unwrap();
    store
        .save_playlist_contents(&CachedPlaylist::new(
            "spotify:playlist:Empty".into(),
            10,
            Some(0),
            0,
            true,
            Vec::new(),
        ))
        .unwrap();
    let surface = surface(store, Policy::default());
    let unread = surface.playlist_contents(&Client::Cli, &query()).unwrap();
    assert_eq!(unread.coverage, "not_cached");
    assert_eq!(unread.name, "Listed but unread");
    assert!(unread.fetched_at.is_none());
    assert!(unread.revision.is_none());

    let mut request = query();
    request.playlist_uri = "spotify:playlist:Empty".into();
    let empty = surface.playlist_contents(&Client::Cli, &request).unwrap();
    assert_eq!(empty.name, request.playlist_uri);
    assert_eq!(
        (empty.coverage.as_str(), empty.source_total, empty.examined),
        ("complete", Some(0), 0)
    );
    assert!(empty.revision.is_some());
    assert!(empty.items.is_empty());

    request.playlist_uri = "spotify:playlist:Unknown".into();
    assert_eq!(
        surface
            .playlist_contents(&Client::Cli, &request)
            .unwrap_err()
            .code,
        ErrorCode::NoMatch
    );
    assert!(
        surface
            .log
            .as_ref()
            .unwrap()
            .store
            .lock()
            .unwrap()
            .recent_actions(&ActionFilter::default())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn permission_uses_the_existing_library_read_boundary() {
    let mut store = MemStore::default();
    store.save_playlist_contents(&observation()).unwrap();
    let policy = Policy::from_toml("[clients.mcp-stdio]\nallow = ['playlist_contents']").unwrap();
    let denied = surface(store, policy);
    assert_eq!(
        denied
            .playlist_contents(&Client::McpStdio, &query())
            .unwrap_err()
            .code,
        ErrorCode::PolicyDenied
    );
    let policy = Policy::from_toml("[clients.mcp-stdio]\nallow = ['browse_library']").unwrap();
    let mut store = MemStore::default();
    store.save_playlist_contents(&observation()).unwrap();
    assert_eq!(
        surface(store, policy)
            .playlist_contents(&Client::McpStdio, &query())
            .unwrap()
            .total,
        4
    );
}

#[test]
fn query_validation_and_encoding_keep_input_out_of_url_syntax() {
    assert_eq!(
        query().path().unwrap(),
        "/library/playlist?playlist_uri=spotify%3Aplaylist%3AObserved&offset=0&limit=50"
    );
    let mut request = query();
    request.q = Some("Élodie & a+b?".into());
    request.offset = Some(2);
    request.limit = Some(1);
    request.expected_revision = Some("rev&value".into());
    assert_eq!(
        request.path().unwrap(),
        "/library/playlist?playlist_uri=spotify%3Aplaylist%3AObserved&offset=2&limit=1&q=%C3%89lodie%20%26%20a%2Bb%3F&expected_revision=rev%26value"
    );
    let mut invalid = vec![PlaylistQuery::default()];
    for uri in [
        "spotify:album:Observed",
        "spotify:playlist:",
        "spotify:playlist:bad/id",
        " spotify:playlist:Observed",
    ] {
        invalid.push(PlaylistQuery {
            playlist_uri: uri.into(),
            ..query()
        });
    }
    for limit in [0, 51] {
        invalid.push(PlaylistQuery {
            limit: Some(limit),
            ..query()
        });
    }
    invalid.push(PlaylistQuery {
        offset: Some(1),
        ..query()
    });
    invalid.push(PlaylistQuery {
        q: Some("é".repeat(513)),
        ..query()
    });
    for revision in [" ".into(), "bad\nrevision".into(), "x".repeat(257)] {
        invalid.push(PlaylistQuery {
            expected_revision: Some(revision),
            ..query()
        });
    }
    for request in invalid {
        assert_eq!(request.path().unwrap_err().code, ErrorCode::InvalidArgument);
    }
    let boundary = PlaylistQuery {
        q: Some("é".repeat(512)),
        ..query()
    };
    boundary.validate().unwrap();
    let decoded: PlaylistQuery =
        serde_json::from_value(serde_json::json!({"playlist_uri": URI})).unwrap();
    assert_eq!(decoded.path().unwrap(), query().path().unwrap());
    assert!(
        serde_json::from_value::<PlaylistQuery>(
            serde_json::json!({"playlist_uri": URI, "extra": 1})
        )
        .is_err()
    );
}

#[test]
fn remote_validation_rejects_mismatched_identity_counts_positions_and_coverage() {
    let request = PlaylistQuery {
        limit: Some(2),
        ..query()
    };
    let valid = projected(&request);
    valid.validate_for(&request).unwrap();
    let mutations: [fn(&mut PlaylistPage); 18] = [
        |page| page.playlist_uri = "spotify:playlist:Other".into(),
        |page| page.q = "other".into(),
        |page| page.offset = 1,
        |page| page.limit = 50,
        |page| page.total = 5,
        |page| page.cached_total = 13,
        |page| page.items.truncate(1),
        |page| page.next_offset = None,
        |page| page.coverage = "live".into(),
        |page| page.coverage = "complete".into(),
        |page| page.coverage = "not_cached".into(),
        |page| page.fetched_at = None,
        |page| page.revision = None,
        |page| page.source_total = Some(8),
        |page| page.items[1].position = 1,
        |page| page.items[1].position = 10,
        |page| page.items[0].source_uri = "https://example.com/music".into(),
        |page| page.items[0].album_uri = Some("spotify:track:Wrong".into()),
    ];
    for mutate in mutations {
        let mut malformed = valid.clone();
        mutate(&mut malformed);
        assert_eq!(
            malformed.validate_for(&request).unwrap_err().code,
            ErrorCode::Internal,
            "{malformed:?}"
        );
    }
    let guarded = PlaylistQuery {
        expected_revision: Some("old-observation".into()),
        ..request
    };
    assert_eq!(
        valid.validate_for(&guarded).unwrap_err().code,
        ErrorCode::InvalidArgument
    );
}

#[test]
fn text_exposes_exact_sources_revision_coverage_and_continuation() {
    let request = PlaylistQuery {
        limit: Some(2),
        ..query()
    };
    let mut first = projected(&request);
    first.name = "Name\nforged heading".into();
    first.items[0].title = "Track\u{1b}[31m".into();
    let text = first.text();
    assert!(text.contains("Name\\nforged heading"));
    assert!(!text.contains('\u{1b}'));
    assert!(text.contains("spotify:track:Repeated"));
    assert!(text.contains(first.revision.as_deref().unwrap()));
    assert!(text.contains("offset=2"));
    assert!(text.contains("expected_revision"));
    assert!(text.contains("playlist, not a Sonos queue"));
    let last = projected(&query()).text();
    assert!(last.contains("End of the cached portion"));
    let unread = page(&query(), "Unread".into(), None).unwrap();
    assert!(unread.text().contains("No track contents have been cached"));
}
