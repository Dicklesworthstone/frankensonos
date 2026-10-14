//! Recover an album's tracks when its embedded page cannot cover the album.
//! Album metadata still comes from the owner's save, including its timestamp.

use super::coverage::Coverage;
use super::{Fetch, LibraryRead};
use crate::SpotifyError;
use crate::client::SavedAlbum;

impl LibraryRead {
    pub(super) fn saved_album(&mut self, saved: SavedAlbum) -> Result<(), SpotifyError> {
        let added_at = saved.added_unix();
        let mut coverage = Coverage {
            offset: Some(0),
            total: saved
                .album
                .tracks
                .as_ref()
                .map(|page| page.total)
                .filter(|total| *total > 0),
        };
        coverage.inherit((saved.album.total_tracks > 0).then_some(saved.album.total_tracks))?;
        let reload = saved.album.tracks.as_ref().is_none_or(|page| {
            page.offset != 0
                || (page.next.is_none()
                    && u32::try_from(page.items.len())
                        .is_ok_and(|count| coverage.total.is_some_and(|total| count < total)))
        });
        let first = self.endpoints.album_tracks(&saved.album.id, 0);
        let next = if reload {
            // Missing tracks are not an empty album. Re-read from zero rather
            // than append to an embedded fragment with unknown coverage.
            Some(first)
        } else if let Some(page) = &saved.album.tracks {
            let next = coverage.next(
                page.items.len(),
                &first,
                &self.endpoints,
                page.next.clone(),
                |offset| self.endpoints.album_tracks(&saved.album.id, offset),
            )?;
            self.items.extend(saved.library_items());
            next
        } else {
            None
        };
        if let Some(url) = next {
            let album = Box::new(crate::client::Album {
                tracks: None,
                ..saved.album
            });
            self.follow(Some(url), |url| {
                Fetch::AlbumTracks(url, album, added_at, coverage.total)
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Endpoints;
    use crate::library::Origin;
    use serde_json::{Value, json};

    const ALBUM: &str = "RecoveryAlbum000000001";

    fn track(number: u32) -> Value {
        json!({
            "id": format!("RecoveryTrack{number}"),
            "uri": format!("spotify:track:RecoveryTrack{number}"),
            "name": format!("Movement {number}"),
            "artists": [{ "name": "Test Performer" }],
            "disc_number": 1, "track_number": number,
            "duration_ms": 120_000, "is_playable": number != 2
        })
    }

    fn page(numbers: &[u32], offset: u32, next: Option<String>) -> Value {
        json!({
            "items": numbers.iter().copied().map(track).collect::<Vec<_>>(),
            "offset": offset, "limit": 50, "total": 3, "next": Value::from(next)
        })
    }

    fn saved(embedded: Option<Value>) -> Value {
        json!({ "items": [{
            "added_at": "2025-01-15T20:31:02Z",
            "album": {
                "id": ALBUM, "uri": format!("spotify:album:{ALBUM}"),
                "name": "Test Album", "total_tracks": 3,
                "artists": [{ "name": "Test Album Artist" }],
                "genres": ["chamber music"], "release_date": "1982",
                "label": "Test Label", "tracks": Value::from(embedded)
            }
        }], "offset": 0, "total": 1, "next": null })
    }

    fn ingest(read: &mut LibraryRead, page: &Value) {
        read.ingest(&serde_json::to_vec(page).unwrap()).unwrap();
    }

    fn liked(read: &mut LibraryRead) {
        let url = Endpoints::default().saved_tracks(0);
        assert_eq!(read.next_url(), Some(url.as_str()));
        ingest(read, &json!({ "items": [], "total": 0, "next": null }));
    }

    fn assert_recovered(read: LibraryRead) {
        assert!(read.next_url().is_none());
        let items = read.into_items();
        assert_eq!(items.len(), 2, "unplayable entries stay excluded");
        assert_eq!(items[0].track_number, Some(1));
        assert_eq!(items[1].track_number, Some(3));
        for item in items {
            assert_eq!(item.album.as_deref(), Some("Test Album"));
            assert_eq!(item.album_artists, ["Test Album Artist"]);
            assert_eq!(item.artists, ["Test Performer"]);
            assert_eq!(item.origin, Origin::SavedAlbum);
            assert_eq!(item.added_at, Some(1_736_973_062));
            assert_eq!(item.release_year, Some(1982));
            assert_eq!(item.genres, ["chamber music"]);
            assert_eq!(item.label.as_deref(), Some("Test Label"));
            assert_eq!(item.duration_secs, Some(120));
        }
    }

    #[test]
    fn absent_and_null_embedded_tracks_are_fetched_and_paged_with_save_metadata() {
        for absent in [false, true] {
            let mut body = saved(None);
            if absent {
                body["items"][0]["album"]
                    .as_object_mut()
                    .unwrap()
                    .remove("tracks");
            }
            let mut read = LibraryRead::default();
            ingest(&mut read, &body);
            liked(&mut read);
            let first = Endpoints::default().album_tracks(ALBUM, 0);
            assert_eq!(read.next_url(), Some(first.as_str()));
            let next = Endpoints::default().album_tracks(ALBUM, 2);
            ingest(&mut read, &page(&[1, 2], 0, Some(next.clone())));
            assert_eq!(read.next_url(), Some(next.as_str()));
            ingest(&mut read, &page(&[3], 2, None));
            assert_eq!(read.pages_read(), 4);
            assert_recovered(read);
        }
    }

    #[test]
    fn truncated_or_noninitial_embedded_pages_restart_without_duplicating_the_fragment() {
        for embedded in [page(&[], 0, None), page(&[1], 0, None), page(&[3], 2, None)] {
            let mut read = LibraryRead::default();
            ingest(&mut read, &saved(Some(embedded)));
            liked(&mut read);
            let first = Endpoints::default().album_tracks(ALBUM, 0);
            assert_eq!(read.next_url(), Some(first.as_str()));
            ingest(&mut read, &page(&[1, 2, 3], 0, None));
            assert_recovered(read);
        }
    }

    #[test]
    fn complete_embedded_pages_need_no_extra_request() {
        let mut read = LibraryRead::default();
        ingest(&mut read, &saved(Some(page(&[1, 2, 3], 0, None))));
        liked(&mut read);
        assert_eq!(read.pages_read(), 2);
        assert_recovered(read);
    }

    #[test]
    fn a_valid_embedded_continuation_is_followed_without_refetching_its_prefix() {
        let mut read = LibraryRead::default();
        let next = Endpoints::default().album_tracks(ALBUM, 2);
        let embedded = page(&[1, 2], 0, Some(next.clone()));
        ingest(&mut read, &saved(Some(embedded)));
        liked(&mut read);
        assert_eq!(read.next_url(), Some(next.as_str()));
        ingest(&mut read, &page(&[3], 2, None));
        assert_eq!(read.pages_read(), 3);
        assert_recovered(read);
    }

    #[test]
    fn missing_count_is_not_proof_that_an_album_has_no_tracks() {
        let mut body = saved(None);
        body["items"][0]["album"]
            .as_object_mut()
            .unwrap()
            .remove("total_tracks");
        let mut read = LibraryRead::default();
        ingest(&mut read, &body);
        liked(&mut read);
        let first = Endpoints::default().album_tracks(ALBUM, 0);
        assert_eq!(read.next_url(), Some(first.as_str()));
        ingest(&mut read, &page(&[1, 2, 3], 0, None));
        assert_recovered(read);
    }

    #[test]
    fn hydration_retains_album_total_when_every_track_page_omits_it() {
        let mut read = LibraryRead::default();
        ingest(&mut read, &saved(None));
        liked(&mut read);
        for number in 1..=3 {
            let expected = Endpoints::default().album_tracks(ALBUM, number - 1);
            assert_eq!(read.next_url(), Some(expected.as_str()));
            let mut body = page(&[number], number - 1, None);
            body.as_object_mut().unwrap().remove("total");
            ingest(&mut read, &body);
        }
        assert_eq!(read.pages_read(), 5);
        assert_recovered(read);
    }

    #[test]
    fn embedded_continuations_cannot_skip_or_repeat_raw_track_positions() {
        for offset in [0, 1, 3] {
            let mut read = LibraryRead::default();
            let wrong = Endpoints::default().album_tracks(ALBUM, offset);
            let body = saved(Some(page(&[1, 2], 0, Some(wrong))));
            assert!(read.ingest(&serde_json::to_vec(&body).unwrap()).is_err());
            assert!(read.items.is_empty());
            assert_eq!(read.pages_read(), 0);
            assert_eq!(read.pending.len(), 2);
            let right = Endpoints::default().album_tracks(ALBUM, 2);
            ingest(
                &mut read,
                &saved(Some(page(&[1, 2], 0, Some(right.clone())))),
            );
            liked(&mut read);
            assert_eq!(read.next_url(), Some(right.as_str()));
            let mut tail = page(&[3], 2, None);
            tail.as_object_mut().unwrap().remove("total");
            ingest(&mut read, &tail);
            assert_recovered(read);
        }
    }

    #[test]
    fn conflicting_album_and_embedded_totals_leave_the_save_page_pending() {
        let mut read = LibraryRead::default();
        let next = Endpoints::default().album_tracks(ALBUM, 2);
        let mut embedded = page(&[1, 2], 0, Some(next.clone()));
        embedded["total"] = 2.into();
        let body = saved(Some(embedded));
        assert!(read.ingest(&serde_json::to_vec(&body).unwrap()).is_err());
        assert!(read.items.is_empty());
        assert_eq!(read.pages_read(), 0);
        let initial = Endpoints::default().saved_albums(0);
        assert_eq!(read.next_url(), Some(initial.as_str()));
        ingest(&mut read, &saved(Some(page(&[1, 2], 0, Some(next)))));
        liked(&mut read);
        ingest(&mut read, &page(&[3], 2, None));
        assert_recovered(read);
    }
}
