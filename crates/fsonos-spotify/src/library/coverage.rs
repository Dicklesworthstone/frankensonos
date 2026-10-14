//! Contiguous library paging. A missing `next` is recoverable when the
//! advertised total proves there are more entries; a gap or an empty page
//! before that total is not a complete library read.

use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::intake::{invalid, offset, path};
use crate::SpotifyError;
use crate::client::Endpoints;

/// Unlike the general API paging DTO, retain absent counts as unknown, not
/// zero. Coverage counts raw entries, including unavailable tracks.
#[derive(Deserialize)]
pub(crate) struct Page<T> {
    pub(crate) items: Vec<T>,
    #[serde(default)]
    pub(crate) next: Option<String>,
    #[serde(flatten)]
    pub(crate) coverage: Coverage,
}

#[derive(Deserialize)]
pub(crate) struct Coverage {
    #[serde(default)]
    pub(crate) offset: Option<u32>,
    #[serde(default)]
    pub(crate) total: Option<u32>,
}

impl<T: DeserializeOwned> Page<T> {
    pub(crate) fn read(
        body: &[u8],
        current: &str,
        endpoints: &Endpoints,
        expected_total: Option<u32>,
        resume: impl FnOnce(u32) -> String,
    ) -> Result<Self, SpotifyError> {
        let mut page: Self = serde_json::from_slice(body)
            .map_err(|error| SpotifyError::Decode(format!("library paging object: {error}")))?;
        page.coverage.inherit(expected_total)?;
        page.next = page.coverage.next(
            page.items.len(),
            current,
            endpoints,
            page.next.take(),
            resume,
        )?;
        Ok(page)
    }
}

impl Coverage {
    /// Later pages may omit a total, but cannot erase or contradict one
    /// already advertised for this collection. A changing library is retried
    /// as a new read rather than published as a complete partial snapshot.
    pub(crate) fn inherit(&mut self, expected: Option<u32>) -> Result<(), SpotifyError> {
        if let (Some(expected), Some(reported)) = (expected, self.total)
            && expected != reported
        {
            return Err(invalid(
                "library total changed during the read; retry the library",
            ));
        }
        self.total = self.total.or(expected);
        Ok(())
    }

    pub(crate) fn next(
        &self,
        raw_count: usize,
        current: &str,
        endpoints: &Endpoints,
        next: Option<String>,
        resume: impl FnOnce(u32) -> String,
    ) -> Result<Option<String>, SpotifyError> {
        let start = offset(current)?;
        if self.offset.is_some_and(|reported| reported != start) {
            return Err(invalid("library response offset differs from its request"));
        }
        let count =
            u32::try_from(raw_count).map_err(|_| invalid("library page has too many entries"))?;
        let end = start
            .checked_add(count)
            .ok_or_else(|| invalid("library paging offset overflow"))?;
        if self.total.is_some_and(|total| end > total) {
            return Err(invalid("library page extends beyond its advertised total"));
        }
        if let Some(next) = &next {
            if !endpoints.is_api_url(next) {
                return Err(invalid("refusing to follow a paging link off the Web API"));
            }
            if path(next) != path(current) {
                return Err(invalid("library paging link changed its collection"));
            }
            if count == 0 || offset(next)? != end {
                return Err(invalid("library paging link skips or repeats entries"));
            }
        } else if self.total.is_some_and(|total| end < total) {
            if count == 0 {
                return Err(invalid("empty library page before its advertised total"));
            }
            // Rebuild from the known endpoint, never from an untrusted URL.
            // The reader's existing route checks and budgets apply as usual.
            return Ok(Some(resume(end)));
        }
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::LibraryRead;
    use serde_json::{Value, json};

    fn track(number: u32) -> Value {
        json!({ "track": {
            "uri": format!("spotify:track:Coverage{number}"),
            "name": format!("Song {number}"),
            "artists": [{ "name": "Test Artist" }],
            "album": { "name": "Test Album" },
            "is_playable": true
        } })
    }

    fn tracks_read() -> LibraryRead {
        let mut read = LibraryRead::default();
        read.ingest(br#"{"items":[],"next":null,"total":0}"#)
            .unwrap();
        read
    }

    fn ingest(read: &mut LibraryRead, body: &Value) -> Result<(), SpotifyError> {
        read.ingest(&serde_json::to_vec(body).unwrap())
    }

    #[test]
    fn missing_continuation_recovers_liked_tracks_after_unavailable_entries() {
        let mut read = tracks_read();
        ingest(
            &mut read,
            &json!({ "items": [track(1), { "track": null }],
                "offset": 0, "limit": 50, "total": 3, "next": null }),
        )
        .unwrap();
        let next = Endpoints::default().saved_tracks(2);
        assert_eq!(read.next_url(), Some(next.as_str()));
        ingest(
            &mut read,
            &json!({ "items": [track(3)], "offset": 2, "total": 3, "next": null }),
        )
        .unwrap();
        assert!(read.next_url().is_none());
        assert_eq!(read.pages_read(), 3);
        let items = read.into_items();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].source_uri, "spotify:track:Coverage1");
        assert_eq!(items[1].source_uri, "spotify:track:Coverage3");
    }

    #[test]
    fn absent_counts_remain_unknown_and_complete_pages_need_no_extra_fetch() {
        for metadata in [json!({}), json!({ "offset": 0, "total": 1 })] {
            let mut read = tracks_read();
            let mut body = metadata;
            body["items"] = json!([track(1)]);
            ingest(&mut read, &body).unwrap();
            assert!(read.next_url().is_none());
            assert_eq!(read.into_items().len(), 1);
        }
        let mut read = tracks_read();
        let next = Endpoints::default().saved_tracks(1);
        ingest(&mut read, &json!({ "items": [track(1)], "next": next })).unwrap();
        // Older API shapes can omit offset on a later page too.
        ingest(&mut read, &json!({ "items": [track(2)], "next": null })).unwrap();
        assert!(read.next_url().is_none());
        assert_eq!(read.into_items().len(), 2);
    }

    #[test]
    fn gaps_overlaps_wrong_offsets_and_inconsistent_totals_are_atomic() {
        let endpoints = Endpoints::default();
        let bad_pages = [
            json!({ "items": [track(1)], "offset": 1, "total": 2 }),
            json!({ "items": [track(1)], "total": 0 }),
            json!({ "items": [], "total": 1 }),
            json!({ "items": [track(1)], "next": endpoints.saved_tracks(2) }),
            json!({ "items": [track(1)], "next": endpoints.saved_tracks(0) }),
            json!({ "items": [], "next": endpoints.saved_tracks(1) }),
            json!({ "items": [track(1)], "next": endpoints.saved_albums(1) }),
            json!({ "items": [track(1)], "next": "https://example.invalid/v1/me/tracks?offset=1" }),
            json!({ "items": [track(1)], "next": format!("{}#fragment", endpoints.saved_tracks(1)) }),
        ];
        for body in bad_pages {
            let mut read = tracks_read();
            let outstanding = read.next_url().unwrap().to_owned();
            let requested = read.requested.clone();
            assert!(ingest(&mut read, &body).is_err(), "accepted {body}");
            assert_eq!(read.next_url(), Some(outstanding.as_str()));
            assert_eq!(read.requested, requested);
            assert_eq!(read.pages_read(), 1);
            assert!(read.items.is_empty());
            ingest(&mut read, &json!({ "items": [track(1)], "total": 1 })).unwrap();
            assert_eq!(read.into_items().len(), 1);
        }
    }

    #[test]
    fn missing_saved_album_continuation_fetches_the_remaining_album() {
        let album = |number: u32| {
            json!({ "album": {
            "id": format!("CoverageAlbum{number}"),
            "uri": format!("spotify:album:CoverageAlbum{number}"),
            "name": format!("Album {number}"), "total_tracks": 1,
            "tracks": { "items": [track(number)["track"].clone()],
                "offset": 0, "total": 1, "next": null }
        } })
        };
        let mut read = LibraryRead::default();
        ingest(
            &mut read,
            &json!({ "items": [album(1)], "offset": 0, "total": 2 }),
        )
        .unwrap();
        let liked = Endpoints::default().saved_tracks(0);
        assert_eq!(read.next_url(), Some(liked.as_str()));
        ingest(&mut read, &json!({ "items": [], "total": 0 })).unwrap();
        let next = Endpoints::default().saved_albums(1);
        assert_eq!(read.next_url(), Some(next.as_str()));
        ingest(
            &mut read,
            &json!({ "items": [album(2)], "offset": 1, "total": 2 }),
        )
        .unwrap();
        assert!(read.next_url().is_none());
        let items = read.into_items();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].album.as_deref(), Some("Album 2"));
    }

    #[test]
    fn album_hydration_recovers_missing_continuations() {
        let mut read = LibraryRead::default();
        ingest(
            &mut read,
            &json!({ "items": [{ "album": {
            "id": "CoverageAlbum", "uri": "spotify:album:CoverageAlbum",
            "name": "Complete Work", "total_tracks": 2
        } }], "total": 1 }),
        )
        .unwrap();
        ingest(&mut read, &json!({ "items": [], "total": 0 })).unwrap();
        for number in 1..=2 {
            let expected = Endpoints::default().album_tracks("CoverageAlbum", number - 1);
            assert_eq!(read.next_url(), Some(expected.as_str()));
            ingest(
                &mut read,
                &json!({
                    "items": [track(number)["track"].clone()],
                    "offset": number - 1, "total": 2, "next": null
                }),
            )
            .unwrap();
        }
        assert!(read.next_url().is_none());
        let items = read.into_items();
        assert_eq!(items.len(), 2);
        assert!(
            items
                .iter()
                .all(|item| item.album.as_deref() == Some("Complete Work"))
        );
    }

    #[test]
    fn coverage_arithmetic_cannot_wrap_an_offset() {
        let endpoints = Endpoints::default();
        let url = endpoints.saved_tracks(u32::MAX);
        let body = serde_json::to_vec(&json!({ "items": [track(1)] })).unwrap();
        let result = Page::<Value>::read(&body, &url, &endpoints, None, |next| {
            endpoints.saved_tracks(next)
        });
        assert!(result.is_err());
    }

    #[test]
    fn later_pages_cannot_erase_a_known_total() {
        let mut read = tracks_read();
        for number in 1..=3 {
            let expected = Endpoints::default().saved_tracks(number - 1);
            assert_eq!(read.next_url(), Some(expected.as_str()));
            let mut body = json!({ "items": [track(number)], "offset": number - 1 });
            if number == 1 {
                body["total"] = 3.into();
            }
            ingest(&mut read, &body).unwrap();
        }
        assert!(read.next_url().is_none());
        assert_eq!(read.into_items().len(), 3);
    }

    #[test]
    fn changed_totals_do_not_consume_the_request_or_its_previous_total() {
        let mut read = tracks_read();
        ingest(&mut read, &json!({ "items": [track(1)], "total": 3 })).unwrap();
        let outstanding = read.next_url().unwrap().to_owned();
        let before = read.items.clone();
        for total in [2, 4] {
            assert!(
                ingest(
                    &mut read,
                    &json!({
                        "items": [track(2)], "offset": 1, "total": total
                    })
                )
                .is_err()
            );
            assert_eq!(read.next_url(), Some(outstanding.as_str()));
            assert_eq!(read.items, before);
            assert_eq!(read.pages_read(), 2);
        }
        // A corrected page without a total still inherits three, not the
        // total in either failed response.
        ingest(&mut read, &json!({ "items": [track(2)], "offset": 1 })).unwrap();
        let last = Endpoints::default().saved_tracks(2);
        assert_eq!(read.next_url(), Some(last.as_str()));
        ingest(&mut read, &json!({ "items": [track(3)], "offset": 2 })).unwrap();
        assert!(read.next_url().is_none());
        assert_eq!(read.into_items().len(), 3);
    }

    #[test]
    fn an_empty_page_without_counts_cannot_hide_an_incomplete_read() {
        let mut read = tracks_read();
        ingest(&mut read, &json!({ "items": [track(1)], "total": 2 })).unwrap();
        let outstanding = read.next_url().unwrap().to_owned();
        assert!(ingest(&mut read, &json!({ "items": [], "next": null })).is_err());
        assert_eq!(read.next_url(), Some(outstanding.as_str()));
        assert_eq!(read.items.len(), 1);
        ingest(&mut read, &json!({ "items": [track(2)], "next": null })).unwrap();
        assert!(read.next_url().is_none());
        assert_eq!(read.into_items().len(), 2);
    }

    #[test]
    fn a_total_first_seen_on_a_later_page_is_retained() {
        let mut read = tracks_read();
        let second = Endpoints::default().saved_tracks(1);
        ingest(&mut read, &json!({ "items": [track(1)], "next": second })).unwrap();
        ingest(
            &mut read,
            &json!({ "items": [track(2)], "offset": 1, "total": 3 }),
        )
        .unwrap();
        let last = Endpoints::default().saved_tracks(2);
        assert_eq!(read.next_url(), Some(last.as_str()));
        ingest(&mut read, &json!({ "items": [track(3)] })).unwrap();
        assert!(read.next_url().is_none());
        assert_eq!(read.into_items().len(), 3);
    }

    #[test]
    fn socket_sync_recovers_missing_pages_and_preserves_cache_until_complete() {
        use crate::cache::{apply_library_read, pool_from_store, sync_library};
        use crate::client::{CachedToken, SCOPE, TokenCache};
        use crate::fake_spotify::{FakeSpotify, config, runtime, scratch_dir};
        use crate::library::LibraryItem;
        use crate::session::Session;
        use asupersync::Cx;
        use asupersync::http::Client;
        use fsonos_core::store::{MemStore, Store};

        let spotify = FakeSpotify::start();
        let data = scratch_dir("library-coverage");
        let cache = TokenCache::in_data_dir(&data);
        cache
            .store(&CachedToken {
                access_token: "access-1".into(),
                refresh_token: "refresh-1".into(),
                expires_at: i64::MAX / 2,
                scope: SCOPE.into(),
            })
            .unwrap();
        let endpoints = spotify.endpoints();
        let target =
            |url: &str| format!("/v1{}", url.strip_prefix(endpoints.api.as_str()).unwrap());
        let last = target(&endpoints.saved_tracks(2));
        {
            let mut fake = spotify.state.lock().unwrap();
            fake.access = "access-1".into();
            fake.refresh = "refresh-1".into();
            fake.library_pages.insert(
                target(&endpoints.saved_albums(0)),
                json!({ "items": [], "offset": 0, "total": 0 }).to_string(),
            );
            fake.library_pages.insert(
                target(&endpoints.saved_tracks(0)),
                json!({ "items": [track(1)], "offset": 0, "total": 3 }).to_string(),
            );
            fake.library_pages.insert(
                target(&endpoints.saved_tracks(1)),
                json!({ "items": [track(2)], "offset": 1 }).to_string(),
            );
        }
        runtime().block_on(async {
            let cx = Cx::current().expect("ambient Cx");
            let mut session = Session::open(config(), cache, Client::default_for_runtime(&cx))
                .unwrap()
                .with_endpoints(endpoints.clone());
            let mut store = MemStore::default();
            apply_library_read(
                &mut store,
                &[LibraryItem {
                    source_uri: "spotify:track:PreviousCoverage".into(),
                    title: "Previously saved song".into(),
                    artists: vec!["Test Artist".into()],
                    ..LibraryItem::default()
                }],
            )
            .unwrap();
            let before = store.library().unwrap();
            for bad in [
                json!({ "items": [], "offset": 2 }),
                json!({ "items": [track(3)], "offset": 2, "total": 4 }),
            ] {
                spotify
                    .state
                    .lock()
                    .unwrap()
                    .library_pages
                    .insert(last.clone(), bad.to_string());
                assert!(sync_library(&mut session, &cx, &mut store).await.is_err());
                assert_eq!(store.library().unwrap(), before);
                assert_eq!(pool_from_store(&store).unwrap().len(), 1);
            }
            spotify.state.lock().unwrap().library_pages.insert(
                last.clone(),
                json!({ "items": [track(3)], "offset": 2 }).to_string(),
            );
            let report = sync_library(&mut session, &cx, &mut store).await.unwrap();
            assert_eq!(report.tracks, 3);
            assert_eq!(report.retired, 1);
            let pool = pool_from_store(&store).unwrap();
            assert_eq!(pool.len(), 3);
            for number in 1..=3 {
                assert!(
                    pool.get(&format!("spotify:track:Coverage{number}"))
                        .is_some()
                );
            }
        });
        let fake = spotify.stop();
        assert_eq!(
            fake.log
                .iter()
                .filter(|line| line.ends_with(last.as_str()))
                .count(),
            3,
            "two incomplete reads and one complete retry reached the recovered last page"
        );
    }
}
