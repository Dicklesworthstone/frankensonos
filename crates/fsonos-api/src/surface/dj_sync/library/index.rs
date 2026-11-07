//! A normalized read index of the public catalog. No store or speaker locks.
//! Rows keep the legacy browser's ordering and hash inputs unchanged.

use super::{AlbumObservation, BrowseItem, BrowsePage, BrowseQuery, MAX_PAGE};
use super::{album_items, item, source, track_items};
use fsonos_core::store::{CachedAlbum, LibraryEntry};
use fsonos_types::text::normalize;
use std::collections::{BTreeMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::mem::size_of;
use std::ops::Range;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Key {
    Tracks,
    Albums,
    Playlists,
    Album(String),
}

impl Key {
    pub(super) fn of(query: &BrowseQuery) -> Self {
        match query.kind.as_deref().unwrap_or("tracks") {
            "albums" => Self::Albums,
            "playlists" => Self::Playlists,
            _ => query.album_uri.clone().map_or(Self::Tracks, Self::Album),
        }
    }

    pub(super) fn kind(&self) -> &'static str {
        match self {
            Self::Albums => "albums",
            Self::Playlists => "playlists",
            Self::Tracks | Self::Album(_) => "tracks",
        }
    }

    pub(super) fn album_uri(&self) -> Option<&str> {
        match self {
            Self::Album(uri) => Some(uri),
            _ => None,
        }
    }

    pub(super) fn uses_library(&self) -> bool {
        !matches!(self, Self::Playlists)
    }

    fn shares_rows(&self) -> bool {
        matches!(self, Self::Tracks | Self::Albums)
    }

    fn heap_bytes(&self) -> usize {
        match self {
            Self::Album(uri) => uri.capacity(),
            _ => 0,
        }
    }
}

/// The final display order and normalized filter text are computed once.
/// Catalog order is hashed before constructing these rows.
struct Rows {
    items: Vec<BrowseItem>,
    text: Vec<String>,
}

impl Rows {
    fn new(mut items: Vec<BrowseItem>, album: bool) -> Self {
        items.sort_by_cached_key(|row| {
            let order = if album {
                (
                    row.disc_number.unwrap_or(u32::MAX),
                    row.track_number.unwrap_or(u32::MAX),
                )
            } else {
                (0, 0)
            };
            (
                order,
                normalize(&row.title),
                normalize(&row.subtitle),
                row.source_uri.clone(),
            )
        });
        let text = items
            .iter()
            .map(|row| {
                normalize(&format!(
                    "{} {} {}",
                    row.title,
                    row.subtitle,
                    row.album.as_deref().unwrap_or("")
                ))
            })
            .collect();
        Self { items, text }
    }

    fn bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(
                self.items
                    .capacity()
                    .saturating_mul(size_of::<BrowseItem>()),
            )
            .saturating_add(
                self.items
                    .iter()
                    .map(item_heap_bytes)
                    .fold(0, usize::saturating_add),
            )
            .saturating_add(self.text.capacity().saturating_mul(size_of::<String>()))
            .saturating_add(
                self.text
                    .iter()
                    .map(String::capacity)
                    .fold(0, usize::saturating_add),
            )
    }
}

fn item_heap_bytes(row: &BrowseItem) -> usize {
    [
        row.kind.capacity(),
        row.source_uri.capacity(),
        row.title.capacity(),
        row.subtitle.capacity(),
        row.album.as_ref().map_or(0, String::capacity),
        row.album_uri.as_ref().map_or(0, String::capacity),
    ]
    .into_iter()
    .fold(0, usize::saturating_add)
}

/// Display rows are shared by all track/album views; lookups store indices,
/// not another copy of each track or source URI.
pub(super) struct Library {
    tracks: Rows,
    albums: Rows,
    by_uri: Vec<usize>,
    by_album: Vec<usize>,
}

impl Library {
    pub(super) fn new(library: &[LibraryEntry]) -> Self {
        let mut entries = library
            .iter()
            .filter(|entry| source(&entry.track.source_uri, "track"))
            .collect::<Vec<_>>();
        entries.sort_by(|a, b| a.track.source_uri.cmp(&b.track.source_uri));
        entries.dedup_by(|a, b| a.track.source_uri == b.track.source_uri);
        let albums = Rows::new(album_items(&entries), false);
        let tracks = Rows::new(track_items(entries, None), false);
        let mut by_uri = (0..tracks.items.len()).collect::<Vec<_>>();
        by_uri.sort_by(|&a, &b| tracks.items[a].source_uri.cmp(&tracks.items[b].source_uri));
        let mut by_album = by_uri
            .iter()
            .copied()
            .filter(|&index| tracks.items[index].album_uri.is_some())
            .collect::<Vec<_>>();
        // A stable album sort preserves URI order within each recording.
        by_album.sort_by(|&a, &b| tracks.items[a].album_uri.cmp(&tracks.items[b].album_uri));
        Self {
            tracks,
            albums,
            by_uri,
            by_album,
        }
    }

    fn track(&self, uri: &str) -> Option<&BrowseItem> {
        self.by_uri
            .binary_search_by(|&index| self.tracks.items[index].source_uri.as_str().cmp(uri))
            .ok()
            .map(|index| &self.tracks.items[self.by_uri[index]])
    }

    fn album_range(&self, uri: &str) -> Range<usize> {
        let first = self
            .by_album
            .partition_point(|&index| self.tracks.items[index].album_uri.as_deref() < Some(uri));
        let end = self
            .by_album
            .partition_point(|&index| self.tracks.items[index].album_uri.as_deref() <= Some(uri));
        first..end
    }

    fn album_rows(&self, uri: &str) -> impl Iterator<Item = &BrowseItem> {
        self.by_album[self.album_range(uri)]
            .iter()
            .map(|&index| &self.tracks.items[index])
    }

    fn catalog(&self, uri: &str, album: &CachedAlbum) -> Vec<BrowseItem> {
        let title = self.album_rows(uri).find_map(|row| row.album.as_deref());
        let mut rows = album
            .tracks
            .iter()
            .filter(|track| {
                source(&track.source_uri, "track")
                    && track.disc_number > 0
                    && track.track_number > 0
            })
            .map(|track| {
                let saved = self.track(&track.source_uri);
                let mut row = item(
                    "track",
                    &track.source_uri,
                    &track.title,
                    saved.map_or("", |known| known.subtitle.as_str()),
                );
                row.album = title.map(str::to_owned);
                row.album_uri = Some(uri.to_owned());
                row.disc_number = Some(track.disc_number);
                row.track_number = Some(track.track_number);
                row.duration_secs = track.duration_secs;
                row.explicit =
                    saved.is_some_and(|known| known.explicit) || track.explicit == Some(true);
                row.explicit_known = saved.is_some() || track.explicit.is_some();
                row
            })
            .collect::<Vec<_>>();
        rows.sort_by_key(|row| (row.disc_number, row.track_number));
        rows
    }

    pub(super) fn bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.tracks.bytes())
            .saturating_add(self.albums.bytes())
            .saturating_add(self.by_uri.capacity().saturating_mul(size_of::<usize>()))
            .saturating_add(self.by_album.capacity().saturating_mul(size_of::<usize>()))
    }
}

struct Filter {
    query: String,
    /// None means all rows, without an extra index allocation.
    selected: Option<Vec<usize>>,
    revision: String,
}

impl Filter {
    fn new(rows: &Rows, key: &Key, view: &View, query: String) -> Self {
        let selected = if query.split_whitespace().next().is_none() {
            None
        } else {
            Some(
                rows.text
                    .iter()
                    .enumerate()
                    .filter_map(|(index, text)| {
                        query
                            .split_whitespace()
                            .all(|word| text.contains(word))
                            .then_some(index)
                    })
                    .collect::<Vec<_>>(),
            )
        };
        let mut hash = DefaultHasher::new();
        key.kind().hash(&mut hash);
        key.album_uri().hash(&mut hash);
        query.hash(&mut hash);
        match &selected {
            None => rows.items.hash(&mut hash),
            Some(indices) => {
                // Hashing references preserves the original Vec<BrowseItem>
                // sequence hash without cloning every matching row.
                indices
                    .iter()
                    .map(|&index| &rows.items[index])
                    .collect::<Vec<_>>()
                    .hash(&mut hash);
            }
        }
        view.observation.hash(&mut hash);
        view.album_revision.hash(&mut hash);
        Self {
            query,
            selected,
            revision: format!("cache-{:016x}", hash.finish()),
        }
    }

    fn heap_bytes(&self) -> usize {
        self.query
            .capacity()
            .saturating_add(self.revision.capacity())
            .saturating_add(
                self.selected
                    .as_ref()
                    .map_or(0, |rows| rows.capacity().saturating_mul(size_of::<usize>())),
            )
    }
}

/// One listing retains only its most recent filter. Offset/limit changes
/// reuse its match indices and revision, then clone at most one response page.
pub(super) struct View {
    pub(super) key: Key,
    rows: Option<Rows>,
    observation: Option<AlbumObservation>,
    album_revision: Option<u64>,
    filter: Option<Filter>,
}

impl View {
    pub(super) fn new(
        key: Key,
        library: Option<&Library>,
        playlists: &[(String, String)],
        album: Option<&CachedAlbum>,
    ) -> Self {
        let mut observation = None;
        let mut album_revision = None;
        let rows = match &key {
            Key::Tracks | Key::Albums => None,
            Key::Playlists => {
                let mut lists = BTreeMap::new();
                for (uri, name) in playlists.iter().filter(|(uri, _)| source(uri, "playlist")) {
                    lists
                        .entry(uri.clone())
                        .or_insert_with(|| item("playlist", uri, name, ""));
                }
                Some(Rows::new(lists.into_values().collect(), false))
            }
            Key::Album(uri) => {
                let library = library.expect("album views have a library snapshot");
                let items = album.map_or_else(
                    || library.album_rows(uri).cloned().collect(),
                    |album| library.catalog(uri, album),
                );
                observation = Some(AlbumObservation {
                    coverage: if album.is_some() {
                        "complete"
                    } else {
                        "library_subset"
                    }
                    .into(),
                    fetched_at: album.map(|album| album.fetched_at),
                    cached_total: items.len(),
                });
                album_revision = album.map(|_| {
                    let mut hash = DefaultHasher::new();
                    items.hash(&mut hash);
                    hash.finish()
                });
                Some(Rows::new(items, true))
            }
        };
        Self {
            key,
            rows,
            observation,
            album_revision,
            filter: None,
        }
    }

    pub(super) fn needs_library(&self) -> bool {
        self.key.shares_rows()
    }

    pub(super) fn page(&mut self, library: Option<&Library>, query: &BrowseQuery) -> BrowsePage {
        let rows = match &self.rows {
            Some(rows) => rows,
            None => {
                let library = library.expect("shared rows have a library snapshot");
                match self.key {
                    Key::Tracks => &library.tracks,
                    Key::Albums => &library.albums,
                    _ => unreachable!("other views own their rows"),
                }
            }
        };
        let q = normalize(query.q.as_deref().unwrap_or(""));
        if self.filter.as_ref().is_none_or(|filter| filter.query != q) {
            self.filter = Some(Filter::new(rows, &self.key, self, q));
        }
        let filter = self.filter.as_ref().expect("the filter was prepared");
        let total = filter.selected.as_ref().map_or(rows.items.len(), Vec::len);
        let offset = query.offset.unwrap_or_default();
        let limit = query.limit.unwrap_or(MAX_PAGE);
        let items = match &filter.selected {
            None => rows
                .items
                .iter()
                .skip(offset)
                .take(limit)
                .cloned()
                .collect(),
            Some(indices) => indices
                .iter()
                .skip(offset)
                .take(limit)
                .map(|&index| rows.items[index].clone())
                .collect::<Vec<_>>(),
        };
        let end = offset.saturating_add(items.len());
        BrowsePage {
            kind: self.key.kind().into(),
            album_uri: self.key.album_uri().map(str::to_owned),
            offset,
            limit,
            total,
            next_offset: (end < total).then_some(end),
            revision: filter.revision.clone(),
            coverage: "cached".into(),
            album_observation: self.observation.clone(),
            items,
        }
    }

    pub(super) fn heap_bytes(&self) -> usize {
        self.key
            .heap_bytes()
            .saturating_add(self.rows.as_ref().map_or(0, Rows::bytes))
            .saturating_add(
                self.observation
                    .as_ref()
                    .map_or(0, |observation| observation.coverage.capacity()),
            )
            .saturating_add(self.filter.as_ref().map_or(0, Filter::heap_bytes))
    }
}

#[cfg(test)]
mod tests;
