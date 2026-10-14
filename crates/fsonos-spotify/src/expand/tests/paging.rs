//! Whole-work reads must finish before an album can enter the durable cache.

use super::*;
use serde_json::{Value, json};

fn first_page() -> Value {
    let mut page: Value = serde_json::from_slice(include_bytes!(
        "../../../tests/fixtures/album_tracks_page1.json"
    ))
    .unwrap();
    page["next"] = Value::Null;
    page
}

fn last_page() -> Value {
    let mut page: Value = serde_json::from_slice(include_bytes!(
        "../../../tests/fixtures/album_tracks_page2.json"
    ))
    .unwrap();
    page.as_object_mut().unwrap().remove("total");
    page
}

fn target(spotify: &FakeSpotify, offset: u32) -> String {
    let endpoints = spotify.endpoints();
    let url = endpoints.album_tracks(album_id(ALBUM).unwrap(), offset);
    format!("/v1{}", url.strip_prefix(endpoints.api.as_str()).unwrap())
}

fn serve(spotify: &FakeSpotify, offset: u32, body: &Value) {
    spotify
        .state
        .lock()
        .unwrap()
        .library_pages
        .insert(target(spotify, offset), body.to_string());
}

#[test]
fn missing_album_links_recover_raw_positions_before_filtering_unavailable_tracks() {
    let (spotify, cache, _) = authorized("expand-coverage-positions");
    serve(&spotify, 3, &last_page());
    runtime().block_on(async {
        let cx = Cx::current().expect("ambient Cx");
        let mut session = Session::open(config(), cache, Client::default_for_runtime(&cx))
            .unwrap()
            .with_endpoints(spotify.endpoints());
        for unavailable in [false, true] {
            let mut first = first_page();
            first["items"][1]["is_playable"] = (!unavailable).into();
            serve(&spotify, 0, &first);
            let tracks = fetch_album_tracks(&mut session, &cx, ALBUM).await.unwrap();
            let expected: Vec<_> = fixture_album()
                .into_iter()
                .filter(|track| !unavailable || track.track_number != 2)
                .map(|track| track.source_uri)
                .collect();
            let actual: Vec<_> = tracks
                .iter()
                .map(|track| track.source_uri.clone())
                .collect();
            assert_eq!(actual, expected);
        }
    });
    let last = target(&spotify, 3);
    let fake = spotify.stop();
    assert_eq!(fake.log.len(), 4, "two pages per complete album read");
    assert_eq!(
        fake.log
            .iter()
            .filter(|line| line.ends_with(last.as_str()))
            .count(),
        2
    );
}

#[test]
fn incomplete_albums_neither_poison_the_cache_nor_wait_a_week_for_a_corrected_retry() {
    let (spotify, cache, _) = authorized("expand-coverage-cache");
    serve(&spotify, 0, &first_page());
    runtime().block_on(async {
        let cx = Cx::current().expect("ambient Cx");
        let mut session = Session::open(config(), cache, Client::default_for_runtime(&cx))
            .unwrap()
            .with_endpoints(spotify.endpoints());
        let mut store = MemStore::default();
        let items = [liked(ADAGIETTO, ALBUM)];
        let config = DjConfig::default();
        let mut wrong_offset = last_page();
        wrong_offset["offset"] = 2.into();
        let mut wrong_total = last_page();
        wrong_total["total"] = 7.into();
        for bad in [
            json!({ "items": [], "offset": 3, "next": null }),
            json!({ "items": false }),
            wrong_offset,
            wrong_total,
        ] {
            serve(&spotify, 3, &bad);
            // Reload the backoff from storage as the daemon does on restart.
            let mut misses = AlbumMisses::load(&store, NOW).unwrap();
            let (works, report) = complete_works(
                &mut session,
                &cx,
                &mut store,
                works_of(&items),
                &config,
                NOW,
                &mut misses,
            )
            .await;
            assert_eq!(
                (report.completed, report.fetched, report.skipped),
                (0, 0, 0)
            );
            assert_eq!(report.failed.len(), 1);
            assert!(report.halted.is_none());
            assert_eq!(works[0].movements.len(), 1, "keep the held movement");
            assert!(store.album_tracks(ALBUM).unwrap().is_none());
            assert!(misses.is_empty(), "a bad page is not an unavailable album");
            misses.save(&mut store, NOW).unwrap();
            assert!(store.album_misses().unwrap().is_empty());
        }
        serve(&spotify, 3, &last_page());
        let mut misses = AlbumMisses::load(&store, NOW + 60).unwrap();
        let (works, report) = complete_works(
            &mut session,
            &cx,
            &mut store,
            works_of(&items),
            &config,
            NOW + 60,
            &mut misses,
        )
        .await;
        assert_eq!(
            (report.completed, report.fetched, report.skipped),
            (1, 1, 0)
        );
        assert!(report.failed.is_empty());
        assert_eq!(works[0].movements.len(), 5);
        assert_eq!(works[0].completeness, Completeness::Complete);
        assert_eq!(store.album_tracks(ALBUM).unwrap().unwrap().tracks.len(), 6);
        let before = requests(&spotify.state, "/v1/albums/");
        let restarted = complete_from_cache(&store, works_of(&items)).unwrap();
        assert_eq!(restarted[0].movements.len(), 5);
        assert_eq!(restarted[0].completeness, Completeness::Complete);
        assert_eq!(requests(&spotify.state, "/v1/albums/"), before);
    });
    spotify.stop();
}

#[test]
fn album_continuations_cannot_skip_repeat_or_switch_collections() {
    let (spotify, cache, _) = authorized("expand-coverage-links");
    let endpoints = spotify.endpoints();
    let id = album_id(ALBUM).unwrap();
    let links = [
        endpoints.album_tracks(id, 0),
        endpoints.album_tracks(id, 2),
        endpoints.album_tracks(id, 4),
        endpoints.album_tracks("UnrelatedAlbum", 3),
        endpoints.saved_tracks(3),
        "https://example.invalid/v1/albums/UnrelatedAlbum/tracks?offset=3".into(),
    ];
    runtime().block_on(async {
        let cx = Cx::current().expect("ambient Cx");
        let mut session = Session::open(config(), cache, Client::default_for_runtime(&cx))
            .unwrap()
            .with_endpoints(endpoints.clone());
        for link in links {
            let mut first = first_page();
            first["next"] = link.into();
            serve(&spotify, 0, &first);
            let before = requests(&spotify.state, "/v1/");
            assert!(matches!(
                fetch_album_tracks(&mut session, &cx, ALBUM).await,
                Err(SpotifyError::Decode(_))
            ));
            assert_eq!(requests(&spotify.state, "/v1/"), before + 1);
        }
    });
    let fake = spotify.stop();
    assert_eq!(fake.log.len(), 6, "no invalid continuation was fetched");
    assert!(!fake.log.iter().any(|line| line.contains("UnrelatedAlbum")));
}

#[test]
fn complete_and_legacy_album_pages_do_not_trigger_speculative_requests() {
    let (spotify, cache, _) = authorized("expand-coverage-complete");
    let mut first = first_page();
    let last = last_page();
    first["items"]
        .as_array_mut()
        .unwrap()
        .extend(last["items"].as_array().unwrap().iter().cloned());
    assert_eq!(first["items"].as_array().unwrap().len(), 6);
    runtime().block_on(async {
        let cx = Cx::current().expect("ambient Cx");
        let mut session = Session::open(config(), cache, Client::default_for_runtime(&cx))
            .unwrap()
            .with_endpoints(spotify.endpoints());
        for counts_known in [true, false] {
            let mut page = first.clone();
            if !counts_known {
                page.as_object_mut().unwrap().remove("total");
                page.as_object_mut().unwrap().remove("offset");
            }
            serve(&spotify, 0, &page);
            let tracks = fetch_album_tracks(&mut session, &cx, ALBUM).await.unwrap();
            assert_eq!(tracks.len(), 6);
        }
    });
    assert_eq!(spotify.stop().log.len(), 2);
}
