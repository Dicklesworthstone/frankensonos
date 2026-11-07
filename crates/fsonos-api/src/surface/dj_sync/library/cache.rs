//! Revision-fenced reuse of the browser's immutable public catalog rows.
//! The cache mutex serializes misses, so concurrent first pages cannot each
//! clone a whole library. The store lock is released before indexing or filtering.

use super::index::{Key, Library, View};
use super::{BrowsePage, BrowseQuery};
use crate::failure::{ErrorCode, Failure};
use fsonos_core::store::{CachedAlbum, LibraryEntry, Store, StoreError};
use std::mem::size_of;
use std::sync::Mutex;

/// Retained row/string/index buffers, not a truncation of the public catalog.
const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_VIEWS: usize = 4;
const SNAPSHOT_ATTEMPTS: usize = 3;

pub(in crate::surface) struct Cache {
    state: Mutex<State>,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            state: Mutex::new(State::new(MAX_BYTES)),
        }
    }
}

impl Cache {
    pub(in crate::surface) fn browse(
        &self,
        store: &Mutex<Box<dyn Store + Send>>,
        query: &BrowseQuery,
    ) -> Result<BrowsePage, Failure> {
        self.read(store, query)
    }

    fn read(&self, source: &impl Source, query: &BrowseQuery) -> Result<BrowsePage, Failure> {
        query.validate()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| Failure::new(ErrorCode::Internal, "the library index is unavailable"))?;
        let result = read(&mut state, source, query);
        if result.is_err() {
            // A failed revision read must never authorize reuse of an old index.
            state.clear();
        }
        result
    }
}

struct State {
    revision: Option<u64>,
    library: Option<Library>,
    /// Oldest first; one filter is retained per listing, not per arbitrary query.
    views: Vec<View>,
    budget: usize,
}

impl State {
    fn new(budget: usize) -> Self {
        Self {
            revision: None,
            library: None,
            // Account for every view slot before deciding whether a base fits.
            views: Vec::with_capacity(MAX_VIEWS),
            budget,
        }
    }

    fn clear(&mut self) {
        self.revision = None;
        self.library = None;
        self.views.clear();
    }

    fn observe(&mut self, revision: Option<u64>) {
        if revision.is_none() || revision != self.revision {
            self.clear();
            self.revision = revision;
        }
    }

    fn hit(&mut self, key: &Key, query: &BrowseQuery) -> Option<BrowsePage> {
        let at = self.views.iter().position(|view| &view.key == key)?;
        let mut view = self.views.remove(at);
        let page = view.page(self.library.as_ref(), query);
        self.retain_view(view);
        Some(page)
    }

    fn retain_library(&mut self, library: Library) {
        if self.fixed_bytes().saturating_add(library.bytes()) > self.budget {
            // Return the complete uncached result without evicting reusable
            // small playlist/album views to make room for an index that cannot fit.
            return;
        }
        self.library = Some(library);
        self.evict_to_budget();
    }

    fn retain_view(&mut self, view: View) {
        if view.needs_library() && self.library.is_none() {
            return;
        }
        if self.views.len() == MAX_VIEWS {
            self.views.remove(0);
        }
        self.views.push(view);
        self.evict_to_budget();
    }

    fn evict_to_budget(&mut self) {
        while self.bytes() > self.budget && !self.views.is_empty() {
            self.views.remove(0);
        }
    }

    fn fixed_bytes(&self) -> usize {
        size_of::<Cache>().saturating_add(self.views.capacity().saturating_mul(size_of::<View>()))
    }

    fn bytes(&self) -> usize {
        self.fixed_bytes()
            .saturating_add(self.library.as_ref().map_or(0, Library::bytes))
            .saturating_add(
                self.views
                    .iter()
                    .map(View::heap_bytes)
                    .fold(0, usize::saturating_add),
            )
    }
}

struct Snapshot {
    before: Option<u64>,
    after: Option<u64>,
    library: Option<Vec<LibraryEntry>>,
    playlists: Vec<(String, String)>,
    album: Option<CachedAlbum>,
}

/// Kept separate from projection so tests can place a competing publication
/// at each revision boundary without sleeping or starting a network service.
trait Source {
    fn revision(&self) -> Result<Option<u64>, Failure>;
    fn load(&self, library: bool, key: &Key) -> Result<Snapshot, Failure>;
}

fn store_error(error: StoreError) -> Failure {
    Failure::new(ErrorCode::Internal, error.to_string())
}

impl Source for Mutex<Box<dyn Store + Send>> {
    fn revision(&self) -> Result<Option<u64>, Failure> {
        self.lock()
            .map_err(|_| Failure::new(ErrorCode::Internal, "the library store is unavailable"))?
            .library_catalog_revision()
            .map_err(store_error)
    }

    fn load(&self, library: bool, key: &Key) -> Result<Snapshot, Failure> {
        let store = self
            .lock()
            .map_err(|_| Failure::new(ErrorCode::Internal, "the library store is unavailable"))?;
        let before = store.library_catalog_revision().map_err(store_error)?;
        let library = if library {
            Some(store.library().map_err(store_error)?)
        } else {
            None
        };
        let playlists = if matches!(key, Key::Playlists) {
            store.playlists().map_err(store_error)?
        } else {
            Vec::new()
        };
        // Exactly one complete album list is read for a drill-down. Its lookup
        // overlay comes from the shared library index, not another full decode.
        let album = key
            .album_uri()
            .map(|uri| store.album_tracks(uri))
            .transpose()
            .map_err(store_error)?
            .flatten();
        let after = store.library_catalog_revision().map_err(store_error)?;
        Ok(Snapshot {
            before,
            after,
            library,
            playlists,
            album,
        })
    }
}

fn read(
    state: &mut State,
    source: &impl Source,
    query: &BrowseQuery,
) -> Result<BrowsePage, Failure> {
    let key = Key::of(query);
    for _ in 0..SNAPSHOT_ATTEMPTS {
        let revision = source.revision()?;
        state.observe(revision);
        if revision.is_some()
            && let Some(page) = state.hit(&key, query)
        {
            // This read linearizes at the fresh revision check above.
            return Ok(page);
        }
        let snapshot = source.load(key.uses_library() && state.library.is_none(), &key)?;
        if snapshot.before != revision || snapshot.after != revision {
            state.clear();
            continue;
        }

        // No store guard is alive here. Large sorts, normalization, filtering
        // and response hashing happen once for a stable catalog generation.
        let library = snapshot.library.as_deref().map(Library::new);
        let rows = library.as_ref().or(state.library.as_ref());
        let mut view = View::new(
            key.clone(),
            rows,
            &snapshot.playlists,
            snapshot.album.as_ref(),
        );
        let page = view.page(rows, query);
        // Another connection can publish while the index is being prepared.
        // Its rows must not be admitted under the preceding generation.
        if source.revision()? != revision {
            state.clear();
            continue;
        }
        if revision.is_some() {
            if let Some(library) = library {
                state.retain_library(library);
            }
            state.retain_view(view);
        }
        return Ok(page);
    }
    Err(Failure::new(
        ErrorCode::NotReady,
        "the library changed repeatedly while its browser index was prepared",
    )
    .with_hint("Retry the library request after the current catalog update completes."))
}

#[cfg(test)]
mod tests;
