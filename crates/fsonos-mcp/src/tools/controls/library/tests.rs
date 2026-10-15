use super::*;
use crate::tools::Daemon;
use fsonos_api::surface::dj_sync::library::BrowsePage;
use fsonos_api::{ErrorCode, Failure, Surface};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{LibraryEntry, MemStore, Store};
use fsonos_proto::{ProtoError, Transport};
use serde_json::{Value, json};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

struct NoLan;

impl Transport for NoLan {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("browsing cached music must not contact speakers")
    }
}

fn local(with_store: bool) -> Backend {
    let mut surface = Surface::new(
        Box::new(NoLan),
        Box::new(|_| panic!("browsing cached music must not discover speakers")),
        Policy::default(),
        Box::new(SystemClock),
    );
    if with_store {
        let mut store = MemStore::default();
        let entries = (0..53)
            .map(|n| {
                let mut entry = LibraryEntry::default();
                entry.track.source_uri = format!("spotify:track:sim{n:03}");
                entry.track.title = format!("Movement {n:03}");
                entry.track.artist = Some("Björk & Guests".into());
                entry.track.album = Some("A recording".into());
                entry.album_uri = Some("spotify:album:simAlbum".into());
                entry.disc_number = Some(1);
                // Deliberately reverse title order: album browsing uses track numbers.
                entry.track_number = Some(53 - n);
                entry.explicit = n == 0;
                entry
            })
            .collect::<Vec<_>>();
        store.upsert_library(&entries).unwrap();
        store
            .save_playlists(&[("spotify:playlist:simList".into(), "A list".into())])
            .unwrap();
        surface = surface.with_action_log(Box::new(store), "mcp-test");
    }
    Backend::shared(Arc::new(surface), Client::McpStdio)
}

fn data(result: FinalCallToolResult) -> BrowsePage {
    serde_json::from_value(result.structured_content.unwrap()).unwrap()
}

fn query(kind: &str) -> BrowseQuery {
    BrowseQuery {
        kind: Some(kind.into()),
        ..BrowseQuery::default()
    }
}

struct Remote {
    answer: Option<Result<Value, Failure>>,
    calls: Mutex<Vec<Value>>,
}

impl Daemon for Remote {
    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Option<Result<Value, Failure>> {
        self.calls.lock().unwrap().push(json!([method, path, body]));
        self.answer.clone()
    }
}

fn remote(answer: Option<Result<Value, Failure>>) -> Arc<Remote> {
    Arc::new(Remote {
        answer,
        calls: Mutex::default(),
    })
}

#[test]
fn agents_can_page_the_real_cached_surface_without_speaker_io() {
    let backend = local(true);
    let first = data(
        backend
            .browse_library(&BrowseQuery::default(), None)
            .unwrap(),
    );
    assert_eq!(
        (first.items.len(), first.total, first.next_offset),
        (50, 53, Some(50))
    );
    assert!(
        first.items[0].explicit,
        "browsing does not apply DJ exclusions"
    );
    let second = data(
        backend
            .browse_library(
                &BrowseQuery {
                    offset: first.next_offset,
                    ..BrowseQuery::default()
                },
                Some(&first.revision),
            )
            .unwrap(),
    );
    assert_eq!((second.items.len(), second.next_offset), (3, None));
    assert_eq!(second.items[0].source_uri, "spotify:track:sim050");
    assert_eq!(second.revision, first.revision);
}

#[test]
fn albums_open_cached_disc_order_and_playlists_keep_their_own_uris() {
    let backend = local(true);
    let albums = data(backend.browse_library(&query("albums"), None).unwrap());
    assert_eq!(albums.items[0].cached_tracks, Some(53));
    let tracks = data(
        backend
            .browse_library(
                &BrowseQuery {
                    album_uri: Some(albums.items[0].source_uri.clone()),
                    limit: Some(2),
                    ..BrowseQuery::default()
                },
                None,
            )
            .unwrap(),
    );
    assert_eq!(tracks.items[0].track_number, Some(1));
    assert_eq!(tracks.items[0].source_uri, "spotify:track:sim052");
    let lists = data(backend.browse_library(&query("playlists"), None).unwrap());
    assert_eq!(lists.items[0].source_uri, "spotify:playlist:simList");
    assert_eq!(
        lists.items[0].cached_tracks, None,
        "membership is not invented"
    );
}

#[test]
fn filters_and_invalid_requests_use_shared_surface_validation() {
    let backend = local(true);
    let filtered = data(
        backend
            .browse_library(
                &BrowseQuery {
                    q: Some("bjork 052".into()),
                    ..BrowseQuery::default()
                },
                None,
            )
            .unwrap(),
    );
    assert_eq!(filtered.total, 1);
    for invalid in [
        query("unknown"),
        BrowseQuery {
            limit: Some(51),
            ..BrowseQuery::default()
        },
        BrowseQuery {
            limit: Some(0),
            ..BrowseQuery::default()
        },
        BrowseQuery {
            album_uri: Some("spotify:track:wrong".into()),
            ..BrowseQuery::default()
        },
    ] {
        let error = backend.browse_library(&invalid, None).unwrap_err();
        assert_eq!(error.data.unwrap()["code"], "INVALID_ARGUMENT");
    }
    let error = local(false)
        .browse_library(&BrowseQuery::default(), None)
        .unwrap_err();
    assert_eq!(error.data.unwrap()["code"], "NOT_READY");
}

#[test]
fn daemon_routing_encodes_literal_filters_and_retains_the_page_contract() {
    let backend = local(true);
    let query = BrowseQuery {
        q: Some("Björk & 100% + ?/#".into()),
        album_uri: Some("spotify:album:simAlbum".into()),
        limit: Some(2),
        ..BrowseQuery::default()
    };
    let page = backend
        .surface
        .browse_library(&Client::McpStdio, &query)
        .unwrap();
    let daemon = remote(Some(Ok(serde_json::to_value(&page).unwrap())));
    let backend = backend.with_daemon(daemon.clone());
    let result = data(
        backend
            .browse_library(&query, Some(&page.revision))
            .unwrap(),
    );
    assert_eq!(result, page);
    assert_eq!(
        *daemon.calls.lock().unwrap(),
        [json!([
            "GET",
            "/library/browse?kind=tracks&offset=0&limit=2&q=Bj%C3%B6rk%20%26%20100%25%20%2B%20%3F%2F%23&album_uri=spotify%3Aalbum%3AsimAlbum",
            null
        ])]
    );
}

#[test]
fn stale_revisions_are_errors_not_silently_restarted_pages() {
    let page = local(true)
        .surface
        .browse_library(&Client::McpStdio, &BrowseQuery::default())
        .unwrap();
    let daemon = remote(Some(Ok(serde_json::to_value(page).unwrap())));
    let backend = local(true).with_daemon(daemon.clone());
    let error = backend
        .browse_library(&BrowseQuery::default(), Some("previous-listing"))
        .unwrap_err();
    assert_eq!(error.data.unwrap()["code"], "INVALID_ARGUMENT");
    assert_eq!(daemon.calls.lock().unwrap().len(), 1);
}

#[test]
fn daemon_failures_never_fall_back_to_a_different_nonempty_cache() {
    for answer in [
        Err(Failure::new(ErrorCode::PolicyDenied, "not allowed")),
        Err(Failure::new(ErrorCode::NotReady, "cache unavailable")),
        Ok(json!({"items": []})),
    ] {
        let daemon = remote(Some(answer));
        let backend = local(true).with_daemon(daemon.clone());
        assert!(
            backend
                .browse_library(&BrowseQuery::default(), None)
                .is_err()
        );
        assert_eq!(daemon.calls.lock().unwrap().len(), 1);
    }
}

#[test]
fn no_selected_daemon_preserves_standalone_browsing() {
    let daemon = remote(None);
    let backend = local(true).with_daemon(daemon.clone());
    let page = data(
        backend
            .browse_library(&BrowseQuery::default(), None)
            .unwrap(),
    );
    assert_eq!(page.total, 53);
    assert_eq!(daemon.calls.lock().unwrap().len(), 1);
}

#[test]
fn contradictory_pages_are_rejected_before_exposing_playable_selections() {
    let query = BrowseQuery {
        limit: Some(2),
        ..BrowseQuery::default()
    };
    let valid = local(true)
        .surface
        .browse_library(&Client::McpStdio, &query)
        .unwrap();
    let mut invalid = Vec::new();
    let mut page = valid.clone();
    page.offset = 1;
    invalid.push(page);
    let mut page = valid.clone();
    page.limit = 51;
    invalid.push(page);
    let mut page = valid.clone();
    page.kind = "albums".into();
    invalid.push(page);
    let mut page = valid.clone();
    page.next_offset = Some(0);
    invalid.push(page);
    let mut page = valid.clone();
    page.items.pop();
    invalid.push(page);
    let mut page = valid.clone();
    page.items[1] = page.items[0].clone();
    invalid.push(page);
    let mut page = valid.clone();
    page.items[0].source_uri = "spotify:album:wrong".into();
    invalid.push(page);
    let mut page = valid.clone();
    page.coverage = "complete account".into();
    invalid.push(page);
    let mut page = valid.clone();
    page.revision.clear();
    invalid.push(page);
    assert!(valid.validate_for(&query, None).is_ok());
    for page in invalid {
        assert!(page.validate_for(&query, None).is_err());
    }
}

#[test]
fn text_is_actionable_without_fabricated_coverage_or_terminal_control_codes() {
    let backend = local(true);
    let mut page = backend
        .surface
        .browse_library(&Client::McpStdio, &query("albums"))
        .unwrap();
    page.items[0].title = "Title\n\u{1b}[2J".into();
    let text = page.text();
    assert!(text.contains("53 cached track(s); the album may contain more"));
    assert!(text.contains("source_uri: spotify:album:simAlbum"));
    assert!(!text.contains('\u{1b}'));
    assert!(text.contains("Title\\n"));
    assert_eq!(
        page.items[0].title, "Title\n\u{1b}[2J",
        "structured data is unchanged"
    );
}
