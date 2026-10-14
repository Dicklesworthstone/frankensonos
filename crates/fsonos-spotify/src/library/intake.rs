//! Commit one decoded library page at a time, never half a page. Continuation
//! links must stay on their own collection and move forwards; finite budgets
//! also stop servers that keep inventing new page URLs indefinitely.

use super::{Fetch, LibraryRead};
use crate::SpotifyError;
use crate::client::{Endpoints, parse_query};
use std::collections::{HashSet, VecDeque};

const PAGE_BUDGET: usize = 20_000;
const ITEM_BUDGET: usize = 1_000_000;

pub(super) fn invalid(detail: &str) -> SpotifyError {
    SpotifyError::Decode(detail.into())
}

fn fits(current: usize, added: usize, limit: usize) -> bool {
    current
        .checked_add(added)
        .is_some_and(|total| total <= limit)
}

pub(super) fn path(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

pub(super) fn offset(url: &str) -> Result<u32, SpotifyError> {
    if url.contains('#') {
        return Err(invalid("library paging links must not contain fragments"));
    }
    let query = url.split_once('?').map_or("", |(_, query)| query);
    let pairs = parse_query(query)?;
    let mut offsets = pairs.iter().filter(|(key, _)| key == "offset");
    let Some((_, value)) = offsets.next() else {
        return Ok(0);
    };
    if offsets.next().is_some() {
        return Err(invalid("library paging link repeats its offset"));
    }
    value
        .parse()
        .map_err(|_| invalid("invalid library paging offset"))
}

fn check_route(endpoints: &Endpoints, fetch: &Fetch) -> Result<(), SpotifyError> {
    let expected = match fetch {
        Fetch::SavedAlbums(_, _) => endpoints.saved_albums(0),
        Fetch::SavedTracks(_, _) => endpoints.saved_tracks(0),
        Fetch::AlbumTracks(_, album, _, _) => endpoints.album_tracks(&album.id, 0),
    };
    if !endpoints.is_api_url(fetch.url()) || path(fetch.url()) != path(&expected) {
        return Err(invalid("library paging link changed its collection"));
    }
    offset(fetch.url())?;
    Ok(())
}

impl LibraryRead {
    pub(super) fn ingest_atomic(&mut self, body: &[u8]) -> Result<(), SpotifyError> {
        if self.pages >= PAGE_BUDGET {
            return Err(invalid("library read exceeded its page budget"));
        }
        let fetch = self
            .pending
            .front()
            .cloned()
            .ok_or_else(|| invalid("library read already complete"))?;
        let current = fetch.url().to_owned();
        // Stage only this page, not a clone of all tracks read so far. An
        // error leaves the outstanding URL available for a corrected retry.
        let mut staged = Self {
            endpoints: self.endpoints.clone(),
            pending: VecDeque::new(),
            requested: HashSet::new(),
            items: Vec::new(),
            pages: 0,
        };
        staged.ingest_page(fetch, body)?;
        if !fits(self.requested.len(), staged.pending.len(), PAGE_BUDGET) {
            return Err(invalid("library read exceeded its page budget"));
        }
        if !fits(self.items.len(), staged.items.len(), ITEM_BUDGET) {
            return Err(invalid("library read exceeded its item budget"));
        }
        for next in &staged.pending {
            check_route(&self.endpoints, next)?;
            if self.requested.contains(next.url()) {
                return Err(invalid("library paging loop"));
            }
            // Album hydration is a different collection and may start at
            // zero. A continuation of this collection must move forwards,
            // even when its query parameters have been reordered or changed.
            if path(next.url()) == path(&current) && offset(next.url())? <= offset(&current)? {
                return Err(invalid("library paging made no forward progress"));
            }
        }
        self.pending.pop_front();
        for next in staged.pending {
            self.queue(next);
        }
        self.items.extend(staged.items);
        self.pages += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::LibraryItem;
    use serde_json::{Value, json};

    fn track(number: u32) -> Value {
        json!({
            "uri": format!("spotify:track:TestRead{number}"),
            "name": format!("Song {number}"),
            "artists": [{ "name": "Test Artist" }],
            "album": { "name": "Test Album" },
            "is_playable": true
        })
    }

    fn liked(number: u32, next: Option<String>) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "items": [{ "track": track(number) }], "next": serde_json::Value::from(next)
        }))
        .unwrap()
    }

    fn tracks_read() -> LibraryRead {
        let mut read = LibraryRead::default();
        read.ingest(br#"{"items":[],"next":null}"#).unwrap();
        read
    }

    #[test]
    fn malformed_pages_keep_the_outstanding_request_and_success_count() {
        let mut read = tracks_read();
        let url = read.next_url().unwrap().to_owned();
        for bad in [b"{".as_slice(), b"{}", b"{\"items\":false}"] {
            assert!(read.ingest(bad).is_err());
            assert_eq!(read.next_url(), Some(url.as_str()));
            assert_eq!(read.pages_read(), 1);
            assert!(read.items.is_empty());
        }
        read.ingest(&liked(1, None)).unwrap();
        assert!(read.next_url().is_none());
        assert_eq!(read.into_items().len(), 1);
    }

    #[test]
    fn a_late_link_error_does_not_publish_items_or_consume_the_last_request() {
        let mut read = tracks_read();
        let next = Endpoints::default().saved_tracks(1);
        read.ingest(&liked(1, Some(next.clone()))).unwrap();
        let before = read.items.clone();
        let wrong = Endpoints::default().saved_albums(2);
        assert!(read.ingest(&liked(2, Some(wrong))).is_err());
        assert_eq!(read.items, before);
        assert_eq!(read.next_url(), Some(next.as_str()));
        assert_eq!(read.pages_read(), 2);
        read.ingest(&liked(2, None)).unwrap();
        let items = read.into_items();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].source_uri, "spotify:track:TestRead2");
    }

    #[test]
    fn renamed_queries_cannot_revisit_or_reverse_an_offset() {
        for query in ["offset=0&limit=1", "limit=20&offset=0", "offset=1&offset=2"] {
            let mut read = tracks_read();
            let next = format!("https://api.spotify.com/v1/me/tracks?{query}");
            assert!(read.ingest(&liked(1, Some(next))).is_err());
            assert_eq!(read.pages_read(), 1);
            assert!(read.items.is_empty());
        }
        let mut read = tracks_read();
        read.ingest(&liked(1, Some(Endpoints::default().saved_tracks(1))))
            .unwrap();
        let backwards = Endpoints::default().saved_tracks(0);
        assert!(read.ingest(&liked(2, Some(backwards))).is_err());
        assert_eq!(read.items.len(), 1);
    }

    #[test]
    fn an_album_link_cannot_assign_another_albums_tracks_to_its_metadata() {
        let mut read = LibraryRead::default();
        let body = json!({ "items": [{ "album": {
            "id": "AlbumA", "uri": "spotify:album:AlbumA", "name": "Album A",
            "tracks": {
                "items": [track(1)], "offset": 0, "total": 2,
                "next": Endpoints::default().album_tracks("AlbumB", 1)
            }
        }}], "next": null });
        assert!(read.ingest(&serde_json::to_vec(&body).unwrap()).is_err());
        assert_eq!(read.pages_read(), 0);
        assert!(read.items.is_empty());
        assert_eq!(read.pending.len(), 2);
        assert_eq!(read.requested.len(), 2);
    }

    #[test]
    fn budgets_are_checked_without_overflow_or_losing_the_pending_page() {
        assert!(fits(ITEM_BUDGET - 1, 1, ITEM_BUDGET));
        assert!(!fits(ITEM_BUDGET, 1, ITEM_BUDGET));
        assert!(!fits(usize::MAX, 1, usize::MAX));
        let mut read = tracks_read();
        let url = read.next_url().unwrap().to_owned();
        read.pages = PAGE_BUDGET;
        assert!(read.ingest(&liked(1, None)).is_err());
        assert_eq!(read.next_url(), Some(url.as_str()));
        assert!(read.items.is_empty());
    }

    #[test]
    fn a_page_with_two_album_follows_is_all_or_nothing() {
        let mut read = LibraryRead::default();
        let body = json!({ "items": [
            { "album": {
                "id": "AlbumA", "uri": "spotify:album:AlbumA", "name": "Album A"
            }},
            { "album": {
                "id": "AlbumB", "uri": "spotify:album:AlbumB", "name": "Album B",
                "tracks": { "items": [], "next": "https://example.invalid/tracks" }
            }}
        ], "next": null });
        assert!(read.ingest(&serde_json::to_vec(&body).unwrap()).is_err());
        assert_eq!(read.pending.len(), 2);
        assert_eq!(read.requested.len(), 2);
        assert!(read.items.is_empty());
        assert_eq!(read.pages_read(), 0);
    }

    #[test]
    fn failed_network_reads_preserve_cached_candidates_until_a_complete_retry() {
        use crate::cache::{apply_library_read, pool_from_store, sync_library};
        use crate::client::{CachedToken, SCOPE, TokenCache};
        use crate::fake_spotify::{FakeSpotify, config, runtime, scratch_dir};
        use crate::session::Session;
        use asupersync::Cx;
        use asupersync::http::Client;
        use fsonos_core::store::{MemStore, Store};

        let spotify = FakeSpotify::start();
        let data = scratch_dir("library-page-failure");
        let cache = TokenCache::in_data_dir(&data);
        cache
            .store(&CachedToken {
                access_token: "access-1".into(),
                refresh_token: "refresh-1".into(),
                expires_at: i64::MAX / 2,
                scope: SCOPE.into(),
            })
            .unwrap();
        {
            let mut fake = spotify.state.lock().unwrap();
            fake.access = "access-1".into();
            fake.refresh = "refresh-1".into();
        }
        let endpoints = spotify.endpoints();
        runtime().block_on(async {
            let cx = Cx::current().expect("ambient Cx");
            let mut session = Session::open(config(), cache, Client::default_for_runtime(&cx))
                .unwrap()
                .with_endpoints(endpoints.clone());
            let mut store = MemStore::default();
            let previous = LibraryItem {
                source_uri: "spotify:track:PreviouslySaved".into(),
                title: "Previously saved song".into(),
                artists: vec!["Test Artist".into()],
                ..LibraryItem::default()
            };
            apply_library_read(&mut store, &[previous]).unwrap();
            let before = store.library().unwrap();
            assert_eq!(pool_from_store(&store).unwrap().len(), 1);
            let wrong = liked(1, Some(endpoints.saved_albums(1)));
            let same = format!("{}/me/tracks?offset=0&limit=1", endpoints.api);
            let repeated = liked(1, Some(same));
            let failures = [
                "{".to_owned(),
                String::from_utf8(wrong).unwrap(),
                String::from_utf8(repeated).unwrap(),
            ];
            for body in failures {
                spotify.state.lock().unwrap().liked_tracks = Some(body);
                assert!(sync_library(&mut session, &cx, &mut store).await.is_err());
                assert_eq!(store.library().unwrap(), before);
                assert_eq!(pool_from_store(&store).unwrap().len(), 1);
            }
            spotify.state.lock().unwrap().liked_tracks = None;
            let report = sync_library(&mut session, &cx, &mut store).await.unwrap();
            assert_eq!(report.retired, 1);
            assert_eq!(pool_from_store(&store).unwrap().len(), 6);
        });
        spotify.stop();
    }
}
