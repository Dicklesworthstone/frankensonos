// The Listening Room browses the daemon cache, not a second Spotify client.
'use strict';
const DashboardLibrary = (() => {
  const P = typeof PlaylistContents !== 'undefined' ? PlaylistContents
    : typeof require === 'function' ? require('./playlist-contents.js') : null;
  const Q = typeof QueueTracksRemote !== 'undefined' ? QueueTracksRemote
    : typeof require === 'function' ? require('./queue-tracks.js') : null;
  const LIMIT = 50;
  const KINDS = Object.freeze({ tracks: 'track', albums: 'album', playlists: 'playlist' });
  const uint = (v) => Number.isInteger(v) && v >= 0 && v <= 0xffffffff;
  const text = (v, max = 4096) => typeof v === 'string' && v.length <= max;
  const uri = (v, kind) => typeof v === 'string' && new RegExp(`^spotify:${kind}:[A-Za-z0-9]{22}$`).test(v);
  const fail = (detail) => { throw new Error(detail); };

  function queryOf(value = {}) {
    const { kind = 'albums', q = '', offset = 0, album_uri = null, playlist_uri = null, expected_revision = null } = value;
    if (!Object.hasOwn(KINDS, kind) || !text(q, 2048) || [...q.trim()].length > 512 || !uint(offset)
        || (album_uri !== null && (kind !== 'tracks' || !uri(album_uri, 'album')))
        || (playlist_uri !== null && (kind !== 'tracks' || album_uri !== null || !P))) {
      fail('Choose a library category and a filter of at most 512 characters.');
    }
    if (playlist_uri !== null) P.queryOf({ playlist_uri, q, offset, expected_revision });
    return Object.freeze({ kind, q: q.trim(), offset, limit: LIMIT, album_uri, playlist_uri, expected_revision });
  }

  function urlOf(value) {
    const q = queryOf(value);
    if (q.playlist_uri) return `/library/playlist?${new URLSearchParams(P.parameters(q))}`;
    const params = new URLSearchParams({ kind: q.kind, offset: q.offset, limit: LIMIT });
    if (q.q) params.set('q', q.q);
    if (q.album_uri) params.set('album_uri', q.album_uri);
    return `/library/browse?${params}`;
  }

  function albumObservationOf(value, albumUri, total = 0) {
    if (value == null) return null;
    if (!albumUri || !['complete', 'library_subset'].includes(value.coverage)
        || !uint(value.cached_total) || value.cached_total < total
        || (value.coverage === 'complete'
          ? !Number.isSafeInteger(value.fetched_at) || value.fetched_at < 0 || value.fetched_at > 8640000000000
          : value.fetched_at !== null)) {
      fail('The cached album observation is incomplete. Refresh the listing.');
    }
    return Object.freeze({ coverage: value.coverage, fetched_at: value.fetched_at, cached_total: value.cached_total });
  }

  function albumCoverage(page) {
    const observation = page?.albumObservation;
    if (!observation) return 'This cache may include older entries and incomplete albums.';
    if (observation.coverage === 'library_subset') {
      return `Library subset: ${observation.cached_total} cached track(s) before filtering. No complete album observation is available.`;
    }
    return `Complete cached album: ${observation.cached_total} playable occurrence(s) before filtering. Read ${new Date(observation.fetched_at * 1000).toLocaleString()} (this browser’s time). Spotify may have changed since.`;
  }

  function itemOf(item, query) {
    if (query.playlist_uri) return P.itemOf(item);
    if (!item || item.kind !== KINDS[query.kind] || !uri(item.source_uri, item.kind)
        || !text(item.title) || !text(item.subtitle ?? '') || typeof item.explicit !== 'boolean'
        || (item.explicit_known !== undefined && typeof item.explicit_known !== 'boolean')
        || (item.explicit_known === false && (!query.album_uri || item.explicit))
        || (query.album_uri && item.album_uri !== query.album_uri)
        || (query.kind === 'albums' && (!uint(item.cached_tracks) || item.cached_tracks < 1))
        || [item.disc_number, item.track_number].some((n) => n != null && !uint(n))) {
      fail('A cached music entry is incomplete. Refresh the listing.');
    }
    return Object.freeze({ kind: item.kind, source_uri: item.source_uri, title: item.title,
      subtitle: item.subtitle ?? '', explicit: item.explicit, explicit_known: item.explicit_known ?? true,
      album_uri: query.album_uri, cached_tracks: query.kind === 'albums' ? item.cached_tracks : null,
      disc_number: item.disc_number ?? null, track_number: item.track_number ?? null });
  }

  function pageOf(value, input) {
    const query = queryOf(input);
    if (query.playlist_uri) return Object.freeze({ ...P.pageOf(value, query), query });
    if (!value || value.kind !== query.kind || value.offset !== query.offset || value.limit !== LIMIT
        || value.album_uri !== query.album_uri || value.coverage !== 'cached' || !uint(value.total)
        || !text(value.revision, 256) || !value.revision.trim() || !Array.isArray(value.items)
        || value.items.length !== Math.min(LIMIT, Math.max(0, value.total - query.offset))) {
      fail('The cached library page is incomplete. Refresh the listing.');
    }
    const end = query.offset + value.items.length, next = end < value.total ? end : null;
    if (value.next_offset !== next) fail('The library page positions disagree. Refresh the listing.');
    const albumObservation = albumObservationOf(value.album_observation, query.album_uri, value.total);
    const complete = albumObservation?.coverage === 'complete';
    const items = value.items.map((item) => itemOf(item, query));
    if (new Set(items.map((item) => complete ? `${item.disc_number}:${item.track_number}` : item.source_uri)).size !== items.length) {
      fail('The library page contains duplicate identities. Refresh the listing.');
    }
    let previous = null;
    for (const item of items) {
      if (!item.explicit_known && !complete) fail('The album metadata has no complete observation. Refresh the listing.');
      if (complete && (!uint(item.disc_number) || !uint(item.track_number) || item.disc_number === 0 || item.track_number === 0
          || (previous && (item.disc_number < previous.disc_number
            || (item.disc_number === previous.disc_number && item.track_number <= previous.track_number))))) {
        fail('The cached album positions disagree. Refresh the listing.');
      }
      previous = item;
    }
    return Object.freeze({ query, total: value.total, next, revision: value.revision, albumObservation, items: Object.freeze(items) });
  }

  function choiceOf(value) {
    if (!value || !text(value.revision, 256) || !value.revision.trim()) fail('Choose music from a fresh library page.');
    const query = queryOf(value.query);
    const albumObservation = albumObservationOf(value.albumObservation, query.album_uri);
    const item = itemOf(value.item, query);
    if (!item.explicit_known && albumObservation?.coverage !== 'complete') fail('Choose this track from a complete album observation.');
    return Object.freeze({ query, revision: value.revision, albumObservation, item });
  }

  function selectionOf(value) {
    if (!Q || !value || !Array.isArray(value.rows) || !value.rows.length || value.rows.length > Q.MAX_TRACKS) {
      fail(`Select 1 to ${Q?.MAX_TRACKS || 10} tracks from fresh library pages.`);
    }
    const listings = new Map();
    const rows = Object.freeze(value.rows.map(row => {
      if (!row || !uint(row.index) || row.index >= LIMIT) fail('The selected track positions are unreadable.');
      const source = row.page ?? value;
      const provenance = Q.basketPage({ ...source, query: { limit: LIMIT, ...source.query } });
      if (provenance.query.limit !== LIMIT) fail('The selected library page size changed. Refresh the listing.');
      const query = queryOf({ ...provenance.query,
        expected_revision: provenance.query.playlist_uri ? provenance.revision : null });
      const albumObservation = albumObservationOf(provenance.albumObservation, query.album_uri);
      const context = Q.contextKey(provenance), identity = JSON.stringify([provenance.revision, albumObservation]);
      if (listings.has(context) && identity !== listings.get(context)) fail('The selected library pages changed. Refresh the listing.');
      listings.set(context, identity);
      const item = itemOf(row.item, query);
      if (!item.explicit_known && albumObservation?.coverage !== 'complete') fail('Choose this track from a complete album observation.');
      return Object.freeze({ index: row.index, item,
        page: Object.freeze({ query, revision: provenance.revision, albumObservation,
          collectionTitle: provenance.collectionTitle }) });
    }));
    Q.tracksOf(rows.map(row => row.item));
    return Object.freeze({ ...rows[0].page, rows });
  }

  async function verifySelection(call, value, check = () => {}) {
    const selection = selectionOf(value), pages = new Map();
    for (const row of selection.rows) {
      const key = Q.pageKey(row.page);
      if (!pages.has(key)) pages.set(key, { page: row.page, rows: [] });
      pages.get(key).rows.push(row);
    }
    for (const { page: before, rows } of pages.values()) {
      check();
      const page = pageOf(await call(urlOf(before.query)), before.query);
      check();
      if (page.revision !== before.revision
          || JSON.stringify(page.albumObservation ?? null) !== JSON.stringify(before.albumObservation)
          || !rows.every(({ index, item }) => JSON.stringify(page.items[index]) === JSON.stringify(item))) {
        fail('The cached library selection changed. Refresh its listing and review again.');
      }
    }
    return selection;
  }

  function model({ call, changed = () => {} }) {
    const state = { open: false, active: true, kind: 'albums', filter: '', album: null, playlist: null,
      page: null, loading: false, stale: true, resetRequired: false, error: '', selected: [], basket: [], selectionNotice: '' };
    const basket = Q?.basket();
    let disposed = false, epoch = 0, running = null, queued = null;
    const emit = () => { if (!disposed) { try { changed(state); } catch (_) { /* Presentation only. */ } } };
    const available = () => !disposed && state.active && state.open;
    const ready = () => available() && !state.loading && !state.stale && !state.resetRequired && state.page;
    function syncSelection() {
      state.basket = basket?.rows ?? [];
      state.selected = state.basket.length && state.page?.query.kind === 'tracks' && state.page.revision !== null
        ? basket.indices(state.page) : [];
    }
    function invalidate(preserveSelection = false) {
      ++epoch; state.stale = true;
      if (!preserveSelection) {
        if (state.basket.length) state.selectionNotice = 'The track selection was cleared when this listing was refreshed or changed.';
        basket?.clear();
      }
      syncSelection();
      state.selected = [];
      if (queued) { queued.resolve(false); queued = null; }
      state.loading = available() && !!running;
    }
    async function pump() {
      if (running || !queued || !available()) return;
      const task = queued; queued = null; running = task;
      const current = () => available() && task.epoch === epoch;
      const retainedContext = task.query.kind === 'tracks' && state.basket.some(row =>
        row.page.query.q === task.query.q && row.page.query.album_uri === task.query.album_uri
        && row.page.query.playlist_uri === task.query.playlist_uri);
      let ok = false;
      try {
        // The view can close synchronously during its loading notification.
        await Promise.resolve();
        if (!current()) return;
        const page = pageOf(await call(urlOf(task.query)), task.query);
        if (!current()) return;
        if (task.revision !== null && page.revision !== task.revision) {
          if (retainedContext) {
            basket?.clear(); syncSelection();
            state.selectionNotice = 'The cached listing changed, so the track selection was cleared.';
          }
          state.resetRequired = true;
          fail('The library changed between pages. Refresh to start at the first page.');
        }
        if (state.basket.length && page.query.kind === 'tracks') {
          if (page.revision === null && retainedContext) {
            basket.clear(); syncSelection(); state.resetRequired = true;
            state.selectionNotice = 'Selected playlist contents are no longer cached, so the track selection was cleared.';
            fail('A selected playlist is no longer available in the cache. Read its current contents before choosing tracks again.');
          }
          if (page.revision !== null) basket.observe(page);
          if (page.revision !== null && state.basket.some(row => Q.pageKey(row.page) === Q.pageKey(page)
              && JSON.stringify(row.item) !== JSON.stringify(page.items[row.index]))) {
            basket.clear(); syncSelection(); state.resetRequired = true;
            state.selectionNotice = 'The selected cached music changed, so the track selection was cleared.';
            fail('The selected cached occurrences changed. Refresh the listing and choose them again.');
          }
        }
        state.page = page; syncSelection(); state.stale = false; state.resetRequired = false; ok = true;
      } catch (error) {
        if (current()) {
          syncSelection();
          state.error = [error?.message || 'The library could not be read.', error?.hint].filter(Boolean).join(' ');
          if (state.basket.length && retainedContext) {
            basket.clear(); syncSelection();
            state.selectionNotice = 'The track selection was cleared after the listing could not be verified.';
            state.error += ' The track selection was cleared; refresh the listing to choose again.';
          }
          if (task.revision !== null) state.resetRequired = true;
        }
      } finally {
        running = null;
        state.loading = available() && !!queued;
        task.resolve(ok);
        emit();
        void pump();
      }
    }
    function request(offset = 0, revision = null, preserveSelection = false) {
      if (!available()) return Promise.resolve(false);
      invalidate(preserveSelection); state.error = '';
      let query;
      try { query = queryOf({ kind: state.kind, q: state.filter, offset, album_uri: state.album?.uri ?? null,
        playlist_uri: state.playlist?.uri ?? null, expected_revision: state.playlist ? revision : null }); }
      catch (error) { state.error = error.message; emit(); return Promise.resolve(false); }
      const result = new Promise((resolve) => { queued = { query, revision, epoch, resolve }; });
      state.loading = true; emit(); void pump(); return result;
    }
    function refresh() { return request(); }
    function applyFilter() { return request(0, null, true); }
    function browse(kind) {
      if (!available() || !Object.hasOwn(KINDS, kind)) return Promise.resolve(false);
      state.kind = kind; state.album = null; state.playlist = null;
      return applyFilter();
    }
    function edit(filter) {
      if (!available() || typeof filter !== 'string') return false;
      invalidate(true); state.filter = filter; state.error = ''; emit(); return true;
    }
    function turn(forward, expected = state.page) {
      if (!ready() || expected !== state.page) return Promise.resolve(false);
      const offset = forward ? expected.next : expected.query.offset > 0 ? Math.max(0, expected.query.offset - LIMIT) : null;
      return offset === null ? Promise.resolve(false) : request(offset, expected.revision, true);
    }
    function choose(item) {
      if (!ready() || !state.page.items.includes(item)) return null;
      return choiceOf({ query: state.page.query, revision: state.page.revision, albumObservation: state.page.albumObservation, item });
    }
    function selectTrack(item, checked) {
      const index = state.page?.items.indexOf(item) ?? -1;
      if (!Q || !ready() || state.kind !== 'tracks' || index < 0 || !Q.supported(item.source_uri)
          || typeof checked !== 'boolean') return false;
      try {
        if (checked) basket.add({ ...state.page, collectionTitle: (state.album || state.playlist)?.title }, index, item);
        else basket.unselect(state.page, index);
        syncSelection(); state.error = ''; state.selectionNotice = ''; emit(); return true;
      } catch (error) { syncSelection(); state.error = error.message; emit(); return false; }
    }
    function editSelection(operation, id, direction) {
      if (!basket || !available() || state.loading) return false;
      try {
        const changed = basket[operation](id, direction);
        syncSelection(); state.error = ''; state.selectionNotice = ''; emit(); return changed;
      } catch (error) { state.error = error.message; emit(); return false; }
    }
    function clearSelection() { basket?.clear(); syncSelection(); state.selectionNotice = 'Track selection cleared.'; emit(); }
    function chooseTracks() {
      if (!Q || !available() || state.loading || !state.basket.length) return null;
      return selectionOf({ rows: state.basket });
    }
    function album(item) {
      if (!choose(item) || item.kind !== 'album') return Promise.resolve(false);
      state.album = Object.freeze({ uri: item.source_uri, title: item.title });
      state.playlist = null;
      state.kind = 'tracks'; state.filter = '';
      return applyFilter();
    }
    function playlist(item) {
      if (!choose(item) || item.kind !== 'playlist') return Promise.resolve(false);
      state.playlist = Object.freeze({ uri: item.source_uri, title: item.title });
      state.album = null; state.kind = 'tracks'; state.filter = '';
      return applyFilter();
    }
    return { state, refresh, applyFilter, browse, edit, choose, selectTrack, clearSelection, chooseTracks, album, playlist,
      removeTrack: id => editSelection('remove', id), repeatTrack: id => editSelection('repeat', id),
      moveTrack: (id, direction) => editSelection('move', id, direction),
      next: (page) => turn(true, page), previous: (page) => turn(false, page),
      open() { if (disposed || !state.active) return Promise.resolve(false); state.open = true; return refresh(); },
      close() { state.open = false; invalidate(); emit(); },
      suspend() { state.active = false; invalidate(); emit(); },
      resume() { if (!disposed) { state.active = true; emit(); } },
      dispose() { disposed = true; state.open = false; invalidate(); },
    };
  }
  return { LIMIT, KINDS, MAX_TRACKS: Q?.MAX_TRACKS || 10, queryOf, urlOf, pageOf, choiceOf, selectionOf, verifySelection, model, albumCoverage,
    selectionOrigin: page => Q.originLabel(page),
    playlistCoverage: (page) => P.describe(page) };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardLibrary;
