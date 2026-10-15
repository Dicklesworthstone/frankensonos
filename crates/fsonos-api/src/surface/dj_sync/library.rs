//! Read-only browsing of the same cache that supplies search and the DJ.
//! A page contains cached music, not a claim about the current Spotify account.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::policy::Client;
use fsonos_core::store::LibraryEntry;
use fsonos_types::text::normalize;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};

use super::super::Surface;
use crate::failure::{ErrorCode, Failure};

/// Response and query limit, independent of the size of the owner's library.
pub const MAX_PAGE: usize = 50;

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct BrowseQuery {
    /// tracks (default), albums, or playlists.
    pub kind: Option<String>,
    /// Zero-based offset; default zero.
    pub offset: Option<usize>,
    /// One to fifty items; default fifty.
    pub limit: Option<usize>,
    /// Optional words to match in title, artist, or album (at most 512 characters).
    pub q: Option<String>,
    /// With tracks only: cached tracks of this spotify:album URI, in disc order.
    pub album_uri: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct BrowseItem {
    pub kind: String,
    pub source_uri: String,
    pub title: String,
    pub subtitle: String,
    pub album: Option<String>,
    pub album_uri: Option<String>,
    pub disc_number: Option<u32>,
    pub track_number: Option<u32>,
    pub duration_secs: Option<u32>,
    pub explicit: bool,
    /// For an album, how many distinct tracks are represented in this cache.
    /// It is not the complete catalog track count.
    pub cached_tracks: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BrowsePage {
    pub kind: String,
    pub album_uri: Option<String>,
    pub offset: usize,
    pub limit: usize,
    pub total: usize,
    pub next_offset: Option<usize>,
    /// An opaque change detector for this filtered listing, not an authorization token.
    pub revision: String,
    /// Always cached: entries may have been removed from Spotify since the last read.
    pub coverage: String,
    pub items: Vec<BrowseItem>,
}

fn source(uri: &str, kind: &str) -> bool {
    uri.strip_prefix(&format!("spotify:{kind}:"))
        .is_some_and(|id| {
            !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric())
        })
}

impl BrowseQuery {
    pub(crate) fn validate(&self) -> Result<(), Failure> {
        let kind = self.kind.as_deref().unwrap_or("tracks");
        if !matches!(kind, "tracks" | "albums" | "playlists") {
            return Err(Failure::invalid(
                "kind must be tracks, albums, or playlists",
            ));
        }
        if self.limit.is_some_and(|n| n == 0 || n > MAX_PAGE)
            || self.offset.is_some_and(|n| u32::try_from(n).is_err())
        {
            return Err(Failure::invalid(
                "limit must be 1 to 50 and offset must fit a 32-bit index",
            ));
        }
        if self.q.as_ref().is_some_and(|q| q.chars().count() > 512) {
            return Err(Failure::invalid(
                "the browse filter must have at most 512 characters",
            ));
        }
        if self
            .album_uri
            .as_deref()
            .is_some_and(|uri| kind != "tracks" || !source(uri, "album"))
        {
            return Err(Failure::invalid(
                "album_uri requires tracks and a spotify:album URI",
            ));
        }
        Ok(())
    }
}

impl Surface {
    /// Browse without discovery, speaker reads, Spotify requests, or a running DJ.
    pub fn browse_library(
        &self,
        client: &Client,
        query: &BrowseQuery,
    ) -> Result<BrowsePage, Failure> {
        self.guard(client).authorize("browse_library", true)?;
        query.validate()?;
        let log = self.log.as_ref().ok_or_else(|| {
            Failure::new(
                ErrorCode::NotReady,
                "library browsing needs the daemon's local store",
            )
            .with_hint("Connect to fsonos serve and refresh its library first.")
        })?;
        let (library, playlists) = {
            let store = log.store.lock().map_err(|_| {
                Failure::new(ErrorCode::Internal, "the library store is unavailable")
            })?;
            let error = |e: fsonos_core::store::StoreError| {
                Failure::new(ErrorCode::Internal, e.to_string())
            };
            // Only copy the category we need; sorting and filtering happen after
            // releasing the store lock. No lock is held across network I/O.
            if query.kind.as_deref() == Some("playlists") {
                (Vec::new(), store.playlists().map_err(error)?)
            } else {
                (store.library().map_err(error)?, Vec::new())
            }
        };
        browse(&library, &playlists, query)
    }
}

fn item(kind: &str, uri: &str, title: &str, subtitle: &str) -> BrowseItem {
    BrowseItem {
        kind: kind.to_owned(),
        source_uri: uri.to_owned(),
        title: title.to_owned(),
        subtitle: subtitle.to_owned(),
        album: None,
        album_uri: None,
        disc_number: None,
        track_number: None,
        duration_secs: None,
        explicit: false,
        cached_tracks: None,
    }
}

/// One item per cached album, counting its cached tracks.
fn album_items(entries: &[&LibraryEntry]) -> Vec<BrowseItem> {
    let mut albums = BTreeMap::<String, BrowseItem>::new();
    for entry in entries {
        let Some(uri) = entry
            .album_uri
            .as_deref()
            .filter(|uri| source(uri, "album"))
        else {
            continue;
        };
        let album = albums.entry(uri.to_owned()).or_insert_with(|| {
            let mut album = item(
                "album",
                uri,
                entry.track.album.as_deref().unwrap_or("Untitled album"),
                entry
                    .album_artists
                    .as_deref()
                    .or(entry.track.artist.as_deref())
                    .unwrap_or(""),
            );
            album.album_uri = Some(uri.to_owned());
            album.cached_tracks = Some(0);
            album
        });
        album.cached_tracks = Some(album.cached_tracks.unwrap_or_default() + 1);
        album.explicit |= entry.explicit;
    }
    albums.into_values().collect()
}

/// One item per cached track, of `album_uri` only when given.
fn track_items(entries: Vec<&LibraryEntry>, album_uri: Option<&str>) -> Vec<BrowseItem> {
    entries
        .into_iter()
        .filter(|entry| album_uri.is_none_or(|uri| entry.album_uri.as_deref() == Some(uri)))
        .map(|entry| {
            let mut track = item(
                "track",
                &entry.track.source_uri,
                &entry.track.title,
                entry.track.artist.as_deref().unwrap_or(""),
            );
            track.album.clone_from(&entry.track.album);
            track.album_uri = entry.album_uri.clone().filter(|uri| source(uri, "album"));
            track.disc_number = entry.disc_number;
            track.track_number = entry.track_number;
            track.duration_secs = entry.track.duration_secs;
            track.explicit = entry.explicit;
            track
        })
        .collect()
}

/// Pure projection, shared by all callers and deterministic under store reordering.
fn browse(
    library: &[LibraryEntry],
    playlists: &[(String, String)],
    query: &BrowseQuery,
) -> Result<BrowsePage, Failure> {
    query.validate()?;
    let kind = query.kind.as_deref().unwrap_or("tracks");
    let mut entries = library
        .iter()
        .filter(|e| source(&e.track.source_uri, "track"))
        .collect::<Vec<_>>();
    entries.sort_by(|a, b| a.track.source_uri.cmp(&b.track.source_uri));
    entries.dedup_by(|a, b| a.track.source_uri == b.track.source_uri);
    let mut items = match kind {
        "albums" => album_items(&entries),
        "playlists" => {
            let mut lists = BTreeMap::<String, BrowseItem>::new();
            for (uri, name) in playlists.iter().filter(|(uri, _)| source(uri, "playlist")) {
                lists
                    .entry(uri.clone())
                    .or_insert_with(|| item("playlist", uri, name, ""));
            }
            lists.into_values().collect()
        }
        _ => track_items(entries, query.album_uri.as_deref()),
    };
    let q = normalize(query.q.as_deref().unwrap_or(""));
    items.retain(|item| {
        let haystack = normalize(&format!(
            "{} {} {}",
            item.title,
            item.subtitle,
            item.album.as_deref().unwrap_or("")
        ));
        q.split_whitespace().all(|word| haystack.contains(word))
    });
    // Precompute normalized keys once, not during every sort comparison.
    items.sort_by_cached_key(|item| {
        let order = if query.album_uri.is_some() {
            (
                item.disc_number.unwrap_or(u32::MAX),
                item.track_number.unwrap_or(u32::MAX),
            )
        } else {
            (0, 0)
        };
        (
            order,
            normalize(&item.title),
            normalize(&item.subtitle),
            item.source_uri.clone(),
        )
    });
    let mut hash = DefaultHasher::new();
    kind.hash(&mut hash);
    query.album_uri.hash(&mut hash);
    q.hash(&mut hash);
    items.hash(&mut hash);
    let revision = format!("cache-{:016x}", hash.finish());
    let total = items.len();
    let offset = query.offset.unwrap_or_default();
    let limit = query.limit.unwrap_or(MAX_PAGE);
    let items = items
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();
    let end = offset.saturating_add(items.len());
    Ok(BrowsePage {
        kind: kind.to_owned(),
        album_uri: query.album_uri.clone(),
        offset,
        limit,
        total,
        next_offset: (end < total).then_some(end),
        revision,
        coverage: "cached".into(),
        items,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_core::store::{LibraryOrigin, MemStore, Store};

    fn track(id: &str, title: &str, album: &str, disc: u32, number: u32) -> LibraryEntry {
        let mut entry = LibraryEntry::default();
        entry.track.source_uri = format!("spotify:track:{id}");
        entry.track.title = title.into();
        entry.track.artist = Some("Björk".into());
        entry.track.album = Some("Same title".into());
        entry.album_uri = Some(format!("spotify:album:{album}"));
        entry.disc_number = Some(disc);
        entry.track_number = Some(number);
        entry
    }

    #[test]
    fn pagination_is_bounded_complete_and_revision_bound() {
        let library = (0..117)
            .map(|n| track(&format!("T{n:03}"), &format!("Track {n:03}"), "A", 1, n + 1))
            .collect::<Vec<_>>();
        let mut query = BrowseQuery::default();
        let first = browse(&library, &[], &query).unwrap();
        assert_eq!(
            (first.items.len(), first.total, first.next_offset),
            (50, 117, Some(50))
        );
        query.offset = first.next_offset;
        let second = browse(&library, &[], &query).unwrap();
        assert_eq!(second.revision, first.revision);
        query.offset = second.next_offset;
        let third = browse(&library, &[], &query).unwrap();
        assert_eq!((third.items.len(), third.next_offset), (17, None));
        let uris = first
            .items
            .iter()
            .chain(&second.items)
            .chain(&third.items)
            .map(|item| &item.source_uri)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(uris.len(), 117);
        query.offset = Some(999);
        assert!(browse(&library, &[], &query).unwrap().items.is_empty());
    }

    #[test]
    fn same_named_albums_stay_distinct_and_album_tracks_follow_disc_order() {
        let library = vec![
            track("Z", "First", "A", 1, 1),
            track("A", "Finale", "A", 2, 1),
            track("B", "Middle", "A", 1, 2),
            track("C", "Other", "B", 1, 1),
        ];
        let albums = browse(
            &library,
            &[],
            &BrowseQuery {
                kind: Some("albums".into()),
                ..BrowseQuery::default()
            },
        )
        .unwrap();
        assert_eq!(albums.items.len(), 2);
        assert_eq!(albums.items[0].cached_tracks, Some(3));
        let tracks = browse(
            &library,
            &[],
            &BrowseQuery {
                album_uri: Some("spotify:album:A".into()),
                ..BrowseQuery::default()
            },
        )
        .unwrap();
        assert_eq!(
            tracks
                .items
                .iter()
                .map(|item| item.title.as_str())
                .collect::<Vec<_>>(),
            ["First", "Middle", "Finale"]
        );
    }

    #[test]
    fn filters_are_accent_insensitive_and_all_words_must_match() {
        let library = vec![
            track("A", "Jóga", "A", 1, 1),
            track("B", "Other", "B", 1, 1),
        ];
        let page = browse(
            &library,
            &[],
            &BrowseQuery {
                q: Some("bjork joga".into()),
                ..BrowseQuery::default()
            },
        )
        .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].title, "Jóga");
    }

    #[test]
    fn explicit_and_taste_tracks_are_browsable_without_claiming_they_are_saved() {
        let mut entry = track("A", "Explicit", "A", 1, 1);
        entry.explicit = true;
        entry.candidate = Some(false);
        entry.origin = LibraryOrigin::Taste;
        let page = browse(&[entry], &[], &BrowseQuery::default()).unwrap();
        assert_eq!(page.coverage, "cached");
        assert!(page.items[0].explicit);
    }

    #[test]
    fn cache_changes_invalidate_revision_but_reordering_does_not() {
        let mut library = vec![track("A", "A", "A", 1, 1), track("B", "B", "B", 1, 1)];
        let before = browse(&library, &[], &BrowseQuery::default()).unwrap();
        library.reverse();
        assert_eq!(
            browse(&library, &[], &BrowseQuery::default())
                .unwrap()
                .revision,
            before.revision
        );
        library[0].track.title = "Changed".into();
        assert_ne!(
            browse(&library, &[], &BrowseQuery::default())
                .unwrap()
                .revision,
            before.revision
        );
    }

    #[test]
    fn invalid_sources_are_not_promoted_to_playable_items() {
        let mut entry = track("A", "A", "A", 1, 1);
        entry.track.source_uri = "https://untrusted.invalid/".into();
        assert!(
            browse(&[entry], &[], &BrowseQuery::default())
                .unwrap()
                .items
                .is_empty()
        );
        let lists = vec![
            ("spotify:playlist:A".into(), "A".into()),
            ("file:///private".into(), "No".into()),
        ];
        let page = browse(
            &[],
            &lists,
            &BrowseQuery {
                kind: Some("playlists".into()),
                ..BrowseQuery::default()
            },
        )
        .unwrap();
        assert_eq!(page.items.len(), 1);
    }

    #[test]
    fn invalid_queries_fail_instead_of_clamping_or_guessing() {
        for query in [
            BrowseQuery {
                limit: Some(0),
                ..BrowseQuery::default()
            },
            BrowseQuery {
                limit: Some(51),
                ..BrowseQuery::default()
            },
            BrowseQuery {
                kind: Some("artists".into()),
                ..BrowseQuery::default()
            },
            BrowseQuery {
                q: Some("x".repeat(513)),
                ..BrowseQuery::default()
            },
            BrowseQuery {
                album_uri: Some("spotify:track:A".into()),
                ..BrowseQuery::default()
            },
            BrowseQuery {
                kind: Some("albums".into()),
                album_uri: Some("spotify:album:A".into()),
                ..BrowseQuery::default()
            },
        ] {
            assert!(query.validate().is_err());
        }
    }

    #[test]
    fn a_real_store_projection_needs_no_speakers_or_dj() {
        struct NoLan;
        impl fsonos_proto::Transport for NoLan {
            fn soap_post(
                &self,
                _: std::net::IpAddr,
                _: &str,
                _: &str,
                _: &str,
            ) -> Result<String, fsonos_proto::ProtoError> {
                panic!("browsing must not contact a speaker");
            }
        }
        let mut store = MemStore::default();
        store.upsert_library(&[track("A", "A", "A", 1, 1)]).unwrap();
        store
            .save_playlists(&[("spotify:playlist:P".into(), "Playlist".into())])
            .unwrap();
        let surface = Surface::new(
            Box::new(NoLan),
            Box::new(|_| panic!("no discovery")),
            fsonos_core::policy::Policy::default(),
            Box::new(fsonos_core::clock::SystemClock),
        )
        .with_action_log(Box::new(store), "test");
        assert_eq!(
            surface
                .browse_library(&Client::LoopbackHttp, &BrowseQuery::default())
                .unwrap()
                .items
                .len(),
            1
        );
        assert_eq!(
            surface
                .browse_library(
                    &Client::LoopbackHttp,
                    &BrowseQuery {
                        kind: Some("playlists".into()),
                        ..BrowseQuery::default()
                    }
                )
                .unwrap()
                .items
                .len(),
            1
        );
    }
}
