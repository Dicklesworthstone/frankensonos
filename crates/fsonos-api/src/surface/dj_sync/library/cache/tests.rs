use super::super::{MAX_PAGE, browse};
use super::*;
use fsonos_core::store::AlbumTrack;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Default)]
struct Reads {
    revision: usize,
    library: usize,
    playlists: usize,
    albums: usize,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Mutation {
    #[default]
    Never,
    AtRevision(usize),
    DuringLoad,
    EveryLoad,
}

struct Fixture {
    revision: Cell<Option<u64>>,
    library: RefCell<Vec<LibraryEntry>>,
    playlists: RefCell<Vec<(String, String)>>,
    albums: RefCell<BTreeMap<String, CachedAlbum>>,
    reads: Cell<Reads>,
    mutation: Cell<Mutation>,
    fail_revision: Cell<bool>,
    fail_load: Cell<bool>,
}

impl Fixture {
    fn new(count: u32) -> Self {
        Self {
            revision: Cell::new(Some(1)),
            library: RefCell::new(
                (0..count)
                    .map(|n| {
                        let mut entry = LibraryEntry::default();
                        entry.track.source_uri = format!("spotify:track:T{n:05}");
                        entry.track.title = format!("Track {n:05}");
                        entry.track.artist = Some("Performer".into());
                        entry.track.album = Some("Recording".into());
                        entry.album_uri = Some("spotify:album:A".into());
                        entry.disc_number = Some(1);
                        entry.track_number = Some(n + 1);
                        entry
                    })
                    .collect(),
            ),
            playlists: RefCell::new(vec![("spotify:playlist:P".into(), "Favorites".into())]),
            albums: RefCell::new(BTreeMap::new()),
            reads: Cell::new(Reads::default()),
            mutation: Cell::new(Mutation::Never),
            fail_revision: Cell::new(false),
            fail_load: Cell::new(false),
        }
    }

    fn change(&self) {
        {
            let mut library = self.library.borrow_mut();
            library[0].track.title = "Changed title".into();
            library[0].track.artist = Some("Changed performer".into());
        }
        self.revision
            .set(self.revision.get().map(|value| value + 1));
    }

    fn album(&self, id: &str) {
        self.albums.borrow_mut().insert(
            format!("spotify:album:{id}"),
            CachedAlbum {
                fetched_at: 1234,
                tracks: vec![AlbumTrack {
                    source_uri: "spotify:track:T00000".into(),
                    title: format!("Catalog {id}"),
                    disc_number: 1,
                    track_number: 1,
                    duration_secs: Some(99),
                    explicit: None,
                }],
            },
        );
    }
}

impl Source for Fixture {
    fn revision(&self) -> Result<Option<u64>, Failure> {
        let mut reads = self.reads.get();
        reads.revision += 1;
        self.reads.set(reads);
        if self.fail_revision.get() {
            return Err(Failure::new(ErrorCode::Internal, "revision unavailable"));
        }
        if self.mutation.get() == Mutation::AtRevision(reads.revision) {
            self.mutation.set(Mutation::Never);
            self.change();
        }
        Ok(self.revision.get())
    }

    fn load(&self, library: bool, key: &Key) -> Result<Snapshot, Failure> {
        let before = self.revision()?;
        if self.fail_load.get() {
            return Err(Failure::new(ErrorCode::Internal, "catalog unavailable"));
        }
        let mut reads = self.reads.get();
        reads.library += usize::from(library);
        reads.playlists += usize::from(matches!(key, Key::Playlists));
        reads.albums += usize::from(key.album_uri().is_some());
        self.reads.set(reads);
        let library = library.then(|| self.library.borrow().clone());
        let playlists = if matches!(key, Key::Playlists) {
            self.playlists.borrow().clone()
        } else {
            Vec::new()
        };
        let album = key
            .album_uri()
            .and_then(|uri| self.albums.borrow().get(uri).cloned());
        if matches!(
            self.mutation.get(),
            Mutation::DuringLoad | Mutation::EveryLoad
        ) {
            self.change();
            if self.mutation.get() == Mutation::DuringLoad {
                self.mutation.set(Mutation::Never);
            }
        }
        let after = self.revision()?;
        Ok(Snapshot {
            before,
            after,
            library,
            playlists,
            album,
        })
    }
}

fn album_query(id: &str) -> BrowseQuery {
    BrowseQuery {
        album_uri: Some(format!("spotify:album:{id}")),
        ..BrowseQuery::default()
    }
}

fn bounded(bytes: usize) -> Cache {
    Cache {
        state: Mutex::new(State::new(bytes)),
    }
}

#[test]
fn one_full_decode_serves_a_hundred_large_library_pages() {
    let source = Fixture::new(5000);
    let cache = Cache::default();
    let mut query = BrowseQuery::default();
    let mut seen = 0;
    loop {
        let page = cache.read(&source, &query).unwrap();
        assert_eq!(page.total, 5000);
        seen += page.items.len();
        let Some(next) = page.next_offset else {
            break;
        };
        query.offset = Some(next);
    }
    assert_eq!(seen, 5000);
    assert_eq!(source.reads.get().library, 1);
    // Four clock reads establish the first snapshot; later pages need one.
    assert_eq!(source.reads.get().revision, 103);
    assert_eq!(source.reads.get().playlists, 0);
    assert_eq!(source.reads.get().albums, 0);
}

#[test]
fn new_filters_and_album_views_reuse_the_same_library_index() {
    let source = Fixture::new(5000);
    source.album("A");
    source.album("B");
    let cache = Cache::default();
    for q in ["", "track 000", "performer", "absent"] {
        let query = BrowseQuery {
            q: Some(q.into()),
            ..BrowseQuery::default()
        };
        assert_eq!(
            cache.read(&source, &query).unwrap(),
            browse(&source.library.borrow(), &[], &query).unwrap()
        );
    }
    cache
        .read(
            &source,
            &BrowseQuery {
                kind: Some("albums".into()),
                ..BrowseQuery::default()
            },
        )
        .unwrap();
    for id in ["A", "B", "A"] {
        let page = cache.read(&source, &album_query(id)).unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].title, format!("Catalog {id}"));
    }
    assert_eq!(source.reads.get().library, 1);
    assert_eq!(source.reads.get().albums, 2);
}

#[test]
fn a_new_public_revision_reloads_rows_and_changes_the_response() {
    let source = Fixture::new(1);
    let cache = Cache::default();
    let query = BrowseQuery::default();
    let first = cache.read(&source, &query).unwrap();
    source.change();
    let next = cache.read(&source, &query).unwrap();
    assert_eq!(next.items[0].title, "Changed title");
    assert_ne!(first.revision, next.revision);
    assert_eq!(source.reads.get().library, 2);
}

#[test]
fn unsupported_revision_stores_get_complete_uncached_pages() {
    let source = Fixture::new(117);
    source.revision.set(None);
    let cache = Cache::default();
    for offset in [0, 50, 100] {
        let query = BrowseQuery {
            offset: Some(offset),
            ..BrowseQuery::default()
        };
        assert_eq!(
            cache.read(&source, &query).unwrap(),
            browse(&source.library.borrow(), &[], &query).unwrap()
        );
    }
    assert_eq!(source.reads.get().library, 3);
    let state = cache.state.lock().unwrap();
    assert!(state.library.is_none() && state.views.is_empty());
}

#[test]
fn loss_of_revision_support_discards_previously_reusable_rows() {
    let source = Fixture::new(1);
    let cache = Cache::default();
    cache.read(&source, &BrowseQuery::default()).unwrap();
    source.revision.set(None);
    source.library.borrow_mut()[0].track.title = "Legacy fresh value".into();
    let next = cache.read(&source, &BrowseQuery::default()).unwrap();
    assert_eq!(next.items[0].title, "Legacy fresh value");
    assert_eq!(source.reads.get().library, 2);
    assert!(cache.state.lock().unwrap().library.is_none());
}

#[test]
fn changes_during_source_materialization_restart_before_index_admission() {
    let source = Fixture::new(1);
    source.mutation.set(Mutation::DuringLoad);
    let cache = Cache::default();
    let page = cache.read(&source, &BrowseQuery::default()).unwrap();
    assert_eq!(page.items[0].title, "Changed title");
    assert_eq!(source.reads.get().library, 2);
    assert_eq!(cache.state.lock().unwrap().revision, Some(2));
}

#[test]
fn changes_during_projection_restart_before_returning_or_retaining_rows() {
    let source = Fixture::new(1);
    // First fresh clock, load's before/after clocks, then admission clock.
    source.mutation.set(Mutation::AtRevision(4));
    let cache = Cache::default();
    let page = cache.read(&source, &BrowseQuery::default()).unwrap();
    assert_eq!(page.items[0].title, "Changed title");
    assert_eq!(source.reads.get().library, 2);
    assert_eq!(cache.state.lock().unwrap().revision, Some(2));
}

#[test]
fn a_clock_change_between_lookup_and_load_cannot_mix_old_base_and_new_album() {
    let source = Fixture::new(1);
    source.album("A");
    let cache = Cache::default();
    cache.read(&source, &BrowseQuery::default()).unwrap();
    source.mutation.set(Mutation::AtRevision(6));
    let page = cache.read(&source, &album_query("A")).unwrap();
    assert_eq!(page.items[0].title, "Catalog A");
    assert_eq!(page.items[0].subtitle, "Changed performer");
    assert_eq!(source.reads.get().library, 2);
    assert_eq!(cache.state.lock().unwrap().revision, Some(2));
}

#[test]
fn perpetual_publication_is_bounded_and_never_publishes_a_mixed_index() {
    let source = Fixture::new(1);
    source.mutation.set(Mutation::EveryLoad);
    let cache = Cache::default();
    let failure = cache.read(&source, &BrowseQuery::default()).unwrap_err();
    assert_eq!(failure.code, ErrorCode::NotReady);
    assert_eq!(source.reads.get().library, SNAPSHOT_ATTEMPTS);
    let state = cache.state.lock().unwrap();
    assert!(state.library.is_none() && state.views.is_empty());
}

#[test]
fn revision_and_source_errors_never_answer_from_an_older_cache() {
    let source = Fixture::new(1);
    let cache = Cache::default();
    cache.read(&source, &BrowseQuery::default()).unwrap();
    source.fail_revision.set(true);
    assert!(cache.read(&source, &BrowseQuery::default()).is_err());
    source.fail_revision.set(false);
    source.fail_load.set(true);
    assert!(cache.read(&source, &BrowseQuery::default()).is_err());
    source.fail_load.set(false);
    source.change();
    let page = cache.read(&source, &BrowseQuery::default()).unwrap();
    assert_eq!(page.items[0].title, "Changed title");
    assert_eq!(source.reads.get().library, 2);
}

#[test]
fn oversized_catalogs_are_complete_but_not_retained() {
    let source = Fixture::new(5000);
    let cache = bounded(8192);
    for offset in [0, 4950] {
        let query = BrowseQuery {
            offset: Some(offset),
            ..BrowseQuery::default()
        };
        let page = cache.read(&source, &query).unwrap();
        assert_eq!((page.total, page.items.len()), (5000, 50));
        assert_eq!(page, browse(&source.library.borrow(), &[], &query).unwrap());
        let state = cache.state.lock().unwrap();
        assert!(state.library.is_none());
        assert!(state.bytes() <= state.budget);
    }
    assert_eq!(source.reads.get().library, 2);
}

#[test]
fn four_lru_views_share_one_base_and_evict_album_rows_without_losing_tracks() {
    let source = Fixture::new(117);
    for id in ["A", "B", "C", "D", "E"] {
        source.album(id);
    }
    let cache = Cache::default();
    for id in ["A", "B", "C", "D", "B", "E", "A"] {
        cache.read(&source, &album_query(id)).unwrap();
        let state = cache.state.lock().unwrap();
        assert!(state.views.len() <= MAX_VIEWS);
        assert!(state.bytes() <= state.budget);
    }
    assert_eq!(source.reads.get().library, 1);
    assert_eq!(source.reads.get().albums, 6);
    cache.read(&source, &BrowseQuery::default()).unwrap();
    assert_eq!(source.reads.get().library, 1);
}

#[test]
fn filter_match_buffers_share_the_same_total_memory_limit() {
    let source = Fixture::new(5000);
    let cache = Cache::default();
    cache.read(&source, &BrowseQuery::default()).unwrap();
    {
        let mut state = cache.state.lock().unwrap();
        state.budget = state.bytes() + 128;
    }
    let query = BrowseQuery {
        q: Some("track".into()),
        ..BrowseQuery::default()
    };
    let page = cache.read(&source, &query).unwrap();
    assert_eq!(page.total, 5000);
    let state = cache.state.lock().unwrap();
    assert!(state.bytes() <= state.budget);
    assert!(state.library.is_some());
    assert!(state.views.is_empty());
    assert_eq!(source.reads.get().library, 1);
}

#[test]
fn playlist_pages_do_not_decode_the_library() {
    let source = Fixture::new(5000);
    let cache = Cache::default();
    let query = BrowseQuery {
        kind: Some("playlists".into()),
        ..BrowseQuery::default()
    };
    for _ in 0..3 {
        let page = cache.read(&source, &query).unwrap();
        assert_eq!(page.items[0].title, "Favorites");
    }
    assert_eq!(source.reads.get().library, 0);
    assert_eq!(source.reads.get().playlists, 1);
}

#[test]
fn invalid_queries_are_rejected_before_any_clock_or_catalog_read() {
    let source = Fixture::new(1);
    let cache = Cache::default();
    assert!(
        cache
            .read(
                &source,
                &BrowseQuery {
                    limit: Some(0),
                    ..BrowseQuery::default()
                },
            )
            .is_err()
    );
    assert_eq!(source.reads.get().revision, 0);
    assert_eq!(source.reads.get().library, 0);
}

#[test]
fn concurrent_first_pages_share_one_serialized_materialization() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, mpsc};

    struct Parallel {
        library: Vec<LibraryEntry>,
        loads: AtomicUsize,
        entered: Barrier,
        release: Barrier,
    }

    impl Source for Parallel {
        fn revision(&self) -> Result<Option<u64>, Failure> {
            Ok(Some(1))
        }

        fn load(&self, library: bool, _: &Key) -> Result<Snapshot, Failure> {
            if self.loads.fetch_add(1, Ordering::SeqCst) == 0 {
                self.entered.wait();
                self.release.wait();
            }
            Ok(Snapshot {
                before: Some(1),
                after: Some(1),
                library: library.then(|| self.library.clone()),
                playlists: Vec::new(),
                album: None,
            })
        }
    }

    let source = Arc::new(Parallel {
        library: Fixture::new(117).library.into_inner(),
        loads: AtomicUsize::new(0),
        entered: Barrier::new(2),
        release: Barrier::new(2),
    });
    let cache = Arc::new(Cache::default());
    let first = {
        let cache = Arc::clone(&cache);
        let source = Arc::clone(&source);
        std::thread::spawn(move || cache.read(source.as_ref(), &BrowseQuery::default()))
    };
    source.entered.wait();
    let (send, receive) = mpsc::channel();
    let second = {
        let cache = Arc::clone(&cache);
        let source = Arc::clone(&source);
        std::thread::spawn(move || {
            send.send(cache.state.try_lock().is_err()).unwrap();
            cache.read(source.as_ref(), &BrowseQuery::default())
        })
    };
    let first_holds_the_miss_lane = receive.recv().unwrap();
    source.release.wait();
    assert_eq!(
        first.join().unwrap().unwrap(),
        second.join().unwrap().unwrap()
    );
    assert!(first_holds_the_miss_lane);
    assert_eq!(source.loads.load(Ordering::SeqCst), 1);
}

#[test]
fn first_view_allocation_cannot_push_an_exact_fit_base_over_budget() {
    let source = Fixture::new(117);
    let allowance =
        State::new(MAX_BYTES).fixed_bytes() + Library::new(&source.library.borrow()).bytes();
    let cache = bounded(allowance);
    let page = cache.read(&source, &BrowseQuery::default()).unwrap();
    assert_eq!((page.total, page.items.len()), (117, MAX_PAGE));
    let state = cache.state.lock().unwrap();
    assert!(state.library.is_some());
    assert!(state.views.is_empty());
    assert!(state.bytes() <= allowance);
}

#[test]
fn an_oversized_library_does_not_evict_a_small_reusable_playlist_view() {
    let source = Fixture::new(5000);
    let cache = bounded(8192);
    let query = BrowseQuery {
        kind: Some("playlists".into()),
        ..BrowseQuery::default()
    };
    let first = cache.read(&source, &query).unwrap();
    assert_eq!(
        cache.read(&source, &BrowseQuery::default()).unwrap().total,
        5000
    );
    assert_eq!(cache.read(&source, &query).unwrap(), first);
    assert_eq!(source.reads.get().playlists, 1);
    let state = cache.state.lock().unwrap();
    assert!(state.library.is_none());
    assert!(state.bytes() <= state.budget);
}
