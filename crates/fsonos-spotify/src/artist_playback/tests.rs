use super::*;
use crate::client::{CachedToken, TokenCache};
use crate::fake_spotify::{FakeSpotify, config, runtime};
use asupersync::http::Client;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

const ARTIST: &str = "Artist0000000000000001";
const OTHER: &str = "Artist0000000000000002";
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct Server {
    fake: Option<FakeSpotify>,
    data: PathBuf,
}

impl Server {
    fn new() -> Self {
        let fake = FakeSpotify::start();
        let data = std::env::temp_dir().join(format!(
            "fsonos-artist-playback-{}-{}-{}",
            std::process::id(),
            fake.addr.port(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        Self {
            fake: Some(fake),
            data,
        }
    }

    fn fake(&self) -> &FakeSpotify {
        self.fake.as_ref().unwrap()
    }

    fn session(&self, cx: &Cx, authorized: bool) -> Session {
        let cache = TokenCache::in_data_dir(&self.data);
        if authorized {
            cache
                .store(&CachedToken {
                    access_token: "artist-test-access".into(),
                    refresh_token: "artist-test-refresh".into(),
                    expires_at: i64::MAX / 2,
                    scope: "user-library-read".into(),
                })
                .unwrap();
            "artist-test-access".clone_into(&mut self.fake().state.lock().unwrap().access);
        }
        Session::open(config(), cache, Client::default_for_runtime(cx))
            .unwrap()
            .with_endpoints(self.fake().endpoints())
    }

    fn set(&self, url: &str, value: &Value) {
        let prefix = format!("http://{}", self.fake().addr);
        let route = url.strip_prefix(&prefix).unwrap().to_owned();
        self.fake()
            .state
            .lock()
            .unwrap()
            .library_pages
            .insert(route, value.to_string());
    }

    fn top(&self, tracks: &[Value]) {
        self.set(
            &self.fake().endpoints().artist_top_tracks(ARTIST),
            &json!({ "tracks": tracks }),
        );
    }

    fn albums(&self, albums: &[Value]) {
        self.set(
            &self
                .fake()
                .endpoints()
                .artist_albums(ARTIST, CATALOG_ALBUM_LIMIT),
            &page(albums, CATALOG_ALBUM_LIMIT),
        );
    }

    fn tracks(&self, album: usize, tracks: &[Value]) {
        self.set(
            &self.fake().endpoints().album_tracks(&album_id(album), 0),
            &page(tracks, PAGE_LIMIT),
        );
    }

    fn refused_top(&self, status: u16) {
        self.fake().state.lock().unwrap().top_tracks_refused = Some(status);
    }

    fn calls(&self) -> Vec<String> {
        self.fake().state.lock().unwrap().log.clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(fake) = self.fake.take() {
            fake.stop();
        }
    }
}

fn album_id(number: usize) -> String {
    format!("Album{number:017}")
}

fn track_id(number: usize) -> String {
    format!("Track{number:017}")
}

fn artist(id: &str) -> Value {
    json!({ "id": id, "name": "Lantern Coast" })
}

fn album(number: usize) -> Value {
    json!({
        "id": album_id(number), "uri": format!("spotify:album:{}", album_id(number)),
        "name": format!("Catalog album {number}"), "album_type": "album",
        "artists": [artist(ARTIST)], "release_date": "1994-05-17", "total_tracks": 100,
    })
}

fn track(number: usize) -> Value {
    json!({
        "id": track_id(number), "uri": format!("spotify:track:{}", track_id(number)),
        "name": format!("Song {number}"), "artists": [artist(ARTIST)], "album": album(1),
        "duration_ms": 210_000, "disc_number": 1, "track_number": number,
        "is_playable": true, "explicit": false,
    })
}

fn page(items: &[Value], limit: u32) -> Value {
    json!({
        "total": items.len() + 100, "offset": 0, "limit": limit, "items": items,
        "next": "https://example.invalid/never-follow-this-page",
    })
}

#[test]
fn top_tracks_preserve_provider_order_metadata_and_explicit_content() {
    let server = Server::new();
    let mut explicit = track(3);
    explicit["explicit"] = true.into();
    let mut unavailable = track(4);
    unavailable["is_playable"] = false.into();
    unavailable["uri"] = "an unavailable row needs no valid identity".into();
    let mut local = track(5);
    local["is_local"] = true.into();
    let mut uncredited = track(6);
    uncredited["artists"] = json!([artist(OTHER)]);
    server.top(&[
        explicit.clone(),
        track(1),
        explicit,
        unavailable,
        local,
        uncredited,
        track(2),
    ]);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, true);
        let selected = resolve(&mut session, &cx, &format!("spotify:artist:{ARTIST}"))
            .await
            .unwrap();
        assert_eq!(selected.source, SelectionSource::TopTracks);
        assert_eq!(selected.albums_read, 0);
        assert_eq!(selected.artist_uri, format!("spotify:artist:{ARTIST}"));
        assert_eq!(
            selected
                .tracks
                .iter()
                .map(|track| track.track_number)
                .collect::<Vec<_>>(),
            [Some(3), Some(1), Some(2)]
        );
        let first = &selected.tracks[0];
        assert!(
            first.explicit,
            "direct catalog playback does not apply DJ preferences"
        );
        assert_eq!(first.artist_id.as_deref(), Some(ARTIST));
        assert_eq!(first.album.as_deref(), Some("Catalog album 1"));
        assert_eq!(first.release_year, Some(1994));
        assert_eq!(first.duration_secs, Some(210));
        assert_eq!(first.origin, Origin::Taste);
        assert_eq!(first.taste_pm, None);
    });
    assert_eq!(server.calls().len(), 1);
}

#[test]
fn top_tracks_stop_after_ten_distinct_playable_tracks() {
    let server = Server::new();
    server.top(&(1..=20).map(track).collect::<Vec<_>>());
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, true);
        let selected = resolve(&mut session, &cx, ARTIST).await.unwrap();
        assert_eq!(selected.tracks.len(), MAX_TRACKS);
        assert_eq!(selected.tracks.last().unwrap().track_number, Some(10));
    });
    assert_eq!(server.calls().len(), 1);
}

#[test]
fn only_a_forbidden_or_missing_top_endpoint_uses_the_catalog_fallback() {
    for status in [403, 404] {
        let server = Server::new();
        server.refused_top(status);
        server.albums(&[album(1)]);
        server.tracks(1, &(1..=12).map(track).collect::<Vec<_>>());
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut session = server.session(&cx, true);
            let selected = resolve(&mut session, &cx, ARTIST).await.unwrap();
            assert_eq!(selected.source, SelectionSource::CatalogSample);
            assert_eq!(selected.albums_read, 1);
            assert_eq!(selected.tracks.len(), MAX_TRACKS);
            assert_eq!(
                selected.tracks[0].album_uri,
                Some(format!("spotify:album:{}", album_id(1)))
            );
        });
        let calls = server.calls();
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert!(calls[1].contains("include_groups=album,single&limit=10&offset=0"));
        assert!(calls[2].contains("limit=50&offset=0"));
        assert!(calls.iter().all(|call| !call.contains("never-follow")));
    }
}

#[test]
fn catalog_verifies_artist_credits_deduplicates_and_reads_at_most_three_albums() {
    let server = Server::new();
    server.refused_top(403);
    let mut other_album = album(7);
    other_album["artists"] = json!([artist(OTHER)]);
    let mut compilation = album(8);
    compilation["album_type"] = "compilation".into();
    server.albums(&[
        other_album,
        compilation,
        album(1),
        album(1),
        album(2),
        album(3),
        album(4),
    ]);
    let mut guest = track(8);
    guest["artists"] = json!([artist(OTHER)]);
    let mut unavailable = track(9);
    unavailable["is_playable"] = false.into();
    let mut explicit = track(1);
    explicit["explicit"] = true.into();
    server.tracks(1, &[guest, unavailable, explicit.clone(), explicit]);
    server.tracks(2, &[track(1), track(2)]);
    server.tracks(3, &[track(3)]);
    server.tracks(4, &[track(4)]);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, true);
        let selected = resolve(&mut session, &cx, ARTIST).await.unwrap();
        assert_eq!(selected.albums_read, MAX_CATALOG_ALBUMS);
        assert_eq!(
            selected
                .tracks
                .iter()
                .map(|track| track.track_number)
                .collect::<Vec<_>>(),
            [Some(1), Some(2), Some(3)]
        );
        assert!(selected.tracks[0].explicit);
        assert!(
            selected
                .tracks
                .iter()
                .all(|track| track.origin == Origin::Taste)
        );
    });
    let calls = server.calls();
    assert_eq!(
        calls.len(),
        5,
        "one top, one directory, three track pages: {calls:?}"
    );
    assert!(calls.iter().all(|call| !call.contains(&album_id(4))
        && !call.contains(&album_id(7))
        && !call.contains(&album_id(8))));
}

#[test]
fn empty_top_tracks_do_not_change_to_an_unrequested_selection_policy() {
    for tracks in [
        vec![],
        vec![{
            let mut unavailable = track(1);
            unavailable["is_playable"] = false.into();
            unavailable
        }],
    ] {
        let server = Server::new();
        server.top(&tracks);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut session = server.session(&cx, true);
            let error = resolve(&mut session, &cx, ARTIST).await.unwrap_err();
            assert!(matches!(error, SpotifyError::Decode(_)), "{error}");
            assert!(error.to_string().contains("no playable tracks"));
        });
        assert_eq!(server.calls().len(), 1);
    }
}

#[test]
fn empty_catalog_is_an_error_before_any_playback_can_begin() {
    let server = Server::new();
    server.refused_top(404);
    server.albums(&[]);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, true);
        assert!(matches!(
            resolve(&mut session, &cx, ARTIST).await,
            Err(SpotifyError::Decode(_))
        ));
    });
    assert_eq!(server.calls().len(), 2);
}

#[test]
fn malformed_artist_ids_are_rejected_before_network_or_authentication() {
    let server = Server::new();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, false);
        for input in [
            "",
            "short",
            "spotify:track:Artist0000000000000001",
            " Artist0000000000000001",
            "Artist000000000000000/",
            "Artist000000000000000é",
        ] {
            assert!(
                matches!(
                    resolve(&mut session, &cx, input).await,
                    Err(SpotifyError::Config(_))
                ),
                "{input}"
            );
        }
    });
    assert!(server.calls().is_empty());
}

#[test]
fn malformed_playable_track_and_album_identities_never_publish_a_partial_selection() {
    for variant in 0..5 {
        let server = Server::new();
        let mut malformed = track(2);
        match variant {
            0 => malformed["id"] = "short".into(),
            1 => malformed["uri"] = format!("spotify:track:{}", track_id(3)).into(),
            2 => malformed["album"]["id"] = "short".into(),
            3 => malformed["album"]["uri"] = "https://example.invalid/album".into(),
            _ => {
                malformed.as_object_mut().unwrap().remove("explicit");
            }
        }
        server.top(&[track(1), malformed]);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut session = server.session(&cx, true);
            assert!(
                matches!(
                    resolve(&mut session, &cx, ARTIST).await,
                    Err(SpotifyError::Decode(_))
                ),
                "variant {variant}"
            );
        });
        assert_eq!(server.calls().len(), 1);
    }
}

#[test]
fn malformed_catalog_identities_and_page_counters_fail_without_following_links() {
    for variant in 0..4 {
        let server = Server::new();
        server.refused_top(403);
        let mut albums = page(&[album(1)], CATALOG_ALBUM_LIMIT);
        match variant {
            0 => albums["items"][0]["id"] = "invalid".into(),
            1 => albums["offset"] = 10.into(),
            2 => albums["total"] = 0.into(),
            _ => albums["items"] = (0..11).map(album).collect::<Vec<_>>().into(),
        }
        server.set(
            &server
                .fake()
                .endpoints()
                .artist_albums(ARTIST, CATALOG_ALBUM_LIMIT),
            &albums,
        );
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut session = server.session(&cx, true);
            assert!(
                matches!(
                    resolve(&mut session, &cx, ARTIST).await,
                    Err(SpotifyError::Decode(_))
                ),
                "variant {variant}"
            );
        });
        assert_eq!(server.calls().len(), 2);
    }
}

#[test]
fn authentication_and_nonfallback_api_errors_are_preserved() {
    let server = Server::new();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, false);
        assert!(matches!(
            resolve(&mut session, &cx, ARTIST).await,
            Err(SpotifyError::Auth(_))
        ));
    });
    assert!(server.calls().is_empty());
    server.refused_top(400);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, true);
        assert!(matches!(
            resolve(&mut session, &cx, ARTIST).await,
            Err(SpotifyError::Api { status: 400, .. })
        ));
    });
    assert_eq!(server.calls().len(), 1);
}

#[test]
fn a_long_rate_limit_is_preserved_and_prevents_an_immediate_retry() {
    let server = Server::new();
    {
        let mut state = server.fake().state.lock().unwrap();
        state.rate_limit_artists = 1;
        state.artist_retry_after = Some(3600);
    }
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, true);
        for _ in 0..2 {
            assert!(matches!(
                resolve(&mut session, &cx, ARTIST).await,
                Err(SpotifyError::Api { status: 429, .. })
            ));
        }
    });
    assert_eq!(
        server.calls().len(),
        1,
        "the session retains Spotify's cooldown"
    );
}

#[test]
fn a_transport_error_is_preserved_without_entering_catalog_fallback() {
    let server = Server::new();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut endpoints = server.fake().endpoints();
        // An invalid authority fails in the real HTTP client without a
        // network dependency, a retry delay or any possibility of token use.
        endpoints.api = "http://[".into();
        let mut session = server.session(&cx, true).with_endpoints(endpoints);
        assert!(matches!(
            resolve(&mut session, &cx, ARTIST).await,
            Err(SpotifyError::Http(_))
        ));
    });
    assert!(server.calls().is_empty());
}

#[test]
fn a_malformed_later_catalog_track_cannot_return_the_earlier_valid_sample() {
    let server = Server::new();
    server.refused_top(403);
    server.albums(&[album(1), album(2)]);
    server.tracks(1, &[track(1)]);
    let mut malformed = track(2);
    malformed["uri"] = format!("spotify:track:{}", track_id(3)).into();
    server.tracks(2, &[malformed]);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, true);
        assert!(matches!(
            resolve(&mut session, &cx, ARTIST).await,
            Err(SpotifyError::Decode(_))
        ));
    });
    assert_eq!(server.calls().len(), 4);
}
