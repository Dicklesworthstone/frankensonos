// Music selection through the same HTTP API as the room cards.
'use strict';

const LibraryRemote = (() => {
  const P = typeof PlaylistContents !== 'undefined' ? PlaylistContents
    : typeof require === 'function' ? require('./playlist-contents.js') : null;
  const Q = typeof QueueTracksRemote !== 'undefined' ? QueueTracksRemote
    : typeof require === 'function' ? require('./queue-tracks.js') : null;
  const targetsApi = () => typeof SourceTargetsRemote !== 'undefined' ? SourceTargetsRemote : require('./source-targets.js');
  const playerApi = () => typeof SourcePlayerRemote !== 'undefined' ? SourcePlayerRemote : require('./source-player.js');
  const LIMIT = 50;
  const MAX_QUERY = 512;
  const routes = {
    librarySearch: ['GET', '/library/search'],
    libraryBrowse: ['GET', '/library/browse'],
    libraryPlaylist: ['GET', '/library/playlist'],
    libraryHistory: ['GET', '/history'],
    libraryPlay: ['POST', '/play'],
    libraryAdd: ['POST', '/queue'],
    libraryTracks: ['POST', '/queue/tracks'],
    libraryPlayTracks: ['POST', '/play/tracks'],
  };
  const text = (value) => typeof value === 'string' && value.trim() !== '';
  const number = (value) => Number.isInteger(value) && value >= 0 && value <= 4294967295;
  const message = (error) => error && typeof error.message === 'string'
    ? `${error.message}${error.hint ? ` ${error.hint}` : ''}` : 'The request failed.';

  function zonesOf(value) {
    if (!Array.isArray(value)) throw new Error('The room list is unreadable. Refresh rooms.');
    const seen = new Set();
    return value.map((zone) => {
      if (!zone || !text(zone.household) || !text(zone.coordinator_room)
          || !Array.isArray(zone.members) || !zone.members.length
          || !zone.members.every(text) || !zone.members.includes(zone.coordinator_room)
          || new Set(zone.members).size !== zone.members.length) {
        throw new Error('The room list is incomplete. Refresh rooms.');
      }
      const target = `${zone.coordinator_room}@${zone.household}`;
      if (seen.has(target)) throw new Error('The destination is ambiguous. Refresh rooms.');
      seen.add(target);
      return Object.freeze({ target, label: `${zone.members.join(' + ')} (${zone.household})`,
        household: zone.household, members: Object.freeze(zone.members.map((room) => `${room}@${zone.household}`).sort()),
        signature: JSON.stringify([zone.household, zone.coordinator_room, [...zone.members].sort()]) });
    });
  }

  const spotify = (uri) => typeof uri === 'string' && /^spotify:(track|album|playlist|artist|episode|show):[a-zA-Z0-9]+$/.exec(uri);

  function hitsOf(value) {
    if (!Array.isArray(value) || value.length > LIMIT) {
      throw new Error('The library response is unreadable. Search again.');
    }
    return value.map((hit) => {
      if (!hit || typeof hit.title !== 'string' || typeof hit.kind !== 'string') {
        throw new Error('The library response is incomplete. Search again.');
      }
      const match = spotify(hit.source_uri);
      const source = match && match[1] === hit.kind ? hit.source_uri : null;
      const favorite = hit.kind === 'favorite' && typeof hit.favorite === 'string'
        && /^FV:2\/[0-9]+$/.test(hit.favorite) ? hit.favorite : null;
      // Future/unsupported kinds remain visible, never guessed into a write.
      return Object.freeze({ title: hit.title, kind: hit.kind,
        subtitle: typeof hit.subtitle === 'string' ? hit.subtitle : '', source, favorite });
    });
  }

  function historyOf(value) {
    if (!Array.isArray(value) || value.length > LIMIT) {
      throw new Error('The listening history is unreadable. Read it again.');
    }
    return value.map((record) => {
      if (!record || !text(record.source_uri) || !Number.isSafeInteger(record.played_at)
          || Math.abs(record.played_at) > 8640000000000) {
        throw new Error('The listening history is incomplete. Read it again.');
      }
      const match = spotify(record.source_uri);
      // Preserve each listen, including repeated URIs, in the server's order.
      // Ephemeral streams and other unsupported sources are not replay guesses.
      return Object.freeze({ kind: match ? match[1] : 'recorded source',
        title: text(record.title) ? record.title : record.source_uri,
        subtitle: typeof record.artist === 'string' ? record.artist : '',
        source: match ? record.source_uri : null, favorite: null, playedAt: record.played_at });
    });
  }

  function routineSourceOf(hit, zone = null) {
    const bounded = (value) => typeof value === 'string' && value.length <= 2048 && !/[\x00-\x1f\x7f]/.test(value);
    if (!hit || !bounded(hit.title) || !bounded(hit.subtitle)) return null;
    const group = zone ? Object.freeze({ target: zone.target, members: zone.members }) : null;
    if (hit.favorite && zone && hit.title.trim()) return Object.freeze({ kind: 'favorite',
      favorite: hit.favorite, title: hit.title, subtitle: hit.subtitle, household: zone.household, group });
    if (!/^spotify:(track|album|playlist|artist|episode|show):[A-Za-z0-9]{22}$/.test(hit.source || '')) return null;
    return Object.freeze({ kind: 'spotify', source_uri: hit.source, title: hit.title.trim() || `Spotify ${hit.kind}`,
      subtitle: hit.subtitle, household: zone?.household || null, group });
  }

  const categories = Object.freeze({ tracks: 'track', albums: 'album', playlists: 'playlist' });

  function albumObservationOf(value, albumUri, total) {
    if (value == null) return null;
    if (!albumUri || !['complete', 'library_subset'].includes(value.coverage)
        || !number(value.cached_total) || value.cached_total < total
        || (value.coverage === 'complete'
          ? !Number.isSafeInteger(value.fetched_at) || value.fetched_at < 0 || value.fetched_at > 8640000000000
          : value.fetched_at !== null)) {
      throw new Error('The cached album observation is incomplete. Refresh the listing.');
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

  function pageOf(value, kind, offset, album) {
    const albumUri = album ? album.uri : null;
    if (!value || value.kind !== kind || value.offset !== offset || value.limit !== LIMIT
        || value.album_uri !== albumUri || value.coverage !== 'cached'
        || !number(value.total) || !text(value.revision) || value.revision.length > 256
        || !Array.isArray(value.items)
        || value.items.length !== Math.min(LIMIT, Math.max(0, value.total - offset))) {
      throw new Error('The library page is incomplete. Refresh the listing.');
    }
    const end = offset + value.items.length;
    const next = end < value.total ? end : null;
    if (value.next_offset !== next) throw new Error('The library page positions disagree. Refresh the listing.');
    const albumObservation = albumObservationOf(value.album_observation, albumUri, value.total);
    const complete = albumObservation?.coverage === 'complete';
    const seen = new Set();
    let previous = null;
    const hits = hitsOf(value.items).map((hit, i) => {
      const raw = value.items[i];
      const known = raw.explicit_known === undefined ? true : raw.explicit_known;
      const identity = complete ? `${raw.disc_number}:${raw.track_number}` : hit.source;
      if (hit.kind !== categories[kind] || !hit.source || seen.has(identity)
          || typeof raw.explicit !== 'boolean' || typeof known !== 'boolean'
          || (!known && (!complete || raw.explicit))
          || (album && raw.album_uri !== albumUri)
          || (kind === 'albums' && (!number(raw.cached_tracks) || raw.cached_tracks < 1))
          || [raw.disc_number, raw.track_number].some((n) => n != null && !number(n))) {
        throw new Error('The library page identities disagree. Refresh the listing.');
      }
      if (complete && (!number(raw.disc_number) || !number(raw.track_number) || raw.disc_number === 0 || raw.track_number === 0
          || (previous && (raw.disc_number < previous.disc
            || (raw.disc_number === previous.disc && raw.track_number <= previous.track))))) {
        throw new Error('The cached album positions disagree. Refresh the listing.');
      }
      seen.add(identity);
      previous = { disc: raw.disc_number, track: raw.track_number };
      return Object.freeze({ ...hit, explicit: raw.explicit, explicitKnown: known, cachedTracks: raw.cached_tracks,
        disc: raw.disc_number, track: raw.track_number });
    });
    return Object.freeze({ kind, offset, total: value.total, next, revision: value.revision,
      hits: Object.freeze(hits), albumObservation });
  }

  function playlistPageOf(value, query) {
    if (!P) throw new Error('Playlist browsing is unavailable. Reload the remote.');
    const page = P.pageOf(value, query);
    const hits = page.items.map(item => Object.freeze({ kind: 'track', title: item.title,
      subtitle: item.subtitle, source: item.source_uri, favorite: null, position: item.position,
      explicit: item.explicit, disc: item.disc_number, track: item.track_number }));
    return Object.freeze({ kind: 'tracks', offset: page.query.offset, total: page.total,
      next: page.next, revision: page.revision, hits: Object.freeze(hits), playlist: page });
  }

  function model({ call, changed = () => {}, played = () => {}, executeSet = async (_, work) => work() }) {
    let epoch = 0;
    let disposed = false;
    const selection = Q?.basket();
    const state = { open: false, zones: [], target: '', query: '', hits: [], view: 'search',
      loaded: false, loading: false, writing: false, pending: null, message: '',
      page: null, browseKind: 'tracks', browseAlbum: null, browsePlaylist: null, selected: [],
      basket: Object.freeze([]), selectionNotice: '', trackDestination: 'group', trackTarget: '', result: null };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* View only. */ } } };
    const destination = () => state.zones.find((zone) => zone.target === state.target);
    const observation = (page = state.page) => Q.basketPage({
      query: { kind: 'tracks', offset: page.offset, limit: LIMIT, q: state.query,
        album_uri: state.browseAlbum?.uri, playlist_uri: state.browsePlaylist?.uri },
      revision: page.revision, albumObservation: page.albumObservation,
      collectionTitle: state.browseAlbum?.title || state.browsePlaylist?.title });
    function syncSelection() {
      state.basket = selection?.rows ?? Object.freeze([]);
      state.selected = selection?.rows.length && state.page && text(state.page.revision) && state.browseKind === 'tracks'
        ? selection.indices(observation()) : [];
    }
    const invalidate = (preserveSelection = false) => {
      ++epoch;
      state.pending = null;
      state.selected = [];
      if (!preserveSelection) {
        if (selection?.rows.length) state.selectionNotice = 'The previous track selection was cleared. Choose tracks from this listing.';
        selection?.clear();
      }
      state.page = null;
      state.hits = [];
      state.loaded = false;
      state.loading = false;
      syncSelection();
    };
    const current = (version) => !disposed && state.open && version === epoch;

    async function refreshRooms() {
      if (disposed || state.writing) return false;
      state.open = true;
      const previous = destination();
      invalidate(true);
      const version = epoch;
      state.loading = true;
      state.message = '';
      publish();
      try {
        const zones = zonesOf(await call('zones'));
        if (!current(version)) return false;
        state.zones = zones;
        if (!previous || !zones.some((zone) => zone.signature === previous.signature)) state.target = '';
        if (!zones.length) state.message = 'No rooms discovered yet. Refresh rooms after discovery.';
        return true;
      } catch (error) {
        if (current(version)) {
          state.zones = [];
          state.target = '';
          state.message = message(error);
        }
        return false;
      } finally {
        if (current(version)) { state.loading = false; publish(); }
      }
    }

    function target(value) {
      if (disposed || state.writing || !state.zones.some((zone) => zone.target === value)) return false;
      // Cached browsing is independent of the destination. Keep its observed
      // page, but consume any review and invalidate an outstanding read.
      const cached = state.view === 'browse' && state.loaded ? state.page : null;
      invalidate(true);
      if (cached) { state.page = cached; state.hits = cached.hits; state.loaded = true; syncSelection(); }
      state.target = value;
      state.message = '';
      publish();
      return true;
    }

    function query(value) {
      if (disposed || state.writing || typeof value !== 'string') return false;
      invalidate(true);
      state.query = value;
      if (state.view !== 'browse') state.view = 'search';
      state.message = '';
      publish();
      return true;
    }

    async function read(view, preserveSelection = true) {
      if (disposed || !state.open || state.writing) return false;
      const zone = destination();
      const q = state.query.trim();
      invalidate(preserveSelection);
      state.view = view;
      if (!zone || (view === 'search' && (!q || q.length > MAX_QUERY))) {
        state.message = !zone ? 'Choose the destination group first.'
          : `Enter 1 to ${MAX_QUERY} characters to search your library.`;
        publish();
        return false;
      }
      const version = epoch;
      state.loading = true;
      state.message = '';
      publish();
      try {
        const hits = view === 'history'
          ? historyOf(await call('libraryHistory', { query: { zone: zone.target, limit: LIMIT } }))
          : hitsOf(await call('librarySearch', { query: { q, zone: zone.target, limit: LIMIT } }));
        if (!current(version)) return false;
        state.hits = hits;
        state.loaded = true;
        return true;
      } catch (error) {
        if (current(version)) state.message = message(error);
        return false;
      } finally {
        if (current(version)) { state.loading = false; publish(); }
      }
    }

    async function browse(kind = state.browseKind, offset = 0, album = null, revision = null, playlist = null, preserveSelection = true) {
      if (disposed || !state.open || state.writing || !Object.hasOwn(categories, kind)
          || !number(offset) || (album && (kind !== 'tracks' || spotify(album.uri)?.[1] !== 'album'))
          || (playlist && (kind !== 'tracks' || album || !P || spotify(playlist.uri)?.[1] !== 'playlist'))) return false;
      invalidate(preserveSelection);
      state.view = 'browse';
      state.browseKind = kind;
      state.browseAlbum = album;
      state.browsePlaylist = playlist;
      const q = state.query.trim();
      const retainedContext = kind === 'tracks' && selection?.rows.some(row => row.page.query.q === q
        && row.page.query.album_uri === (album?.uri ?? null)
        && row.page.query.playlist_uri === (playlist?.uri ?? null));
      if ([...q].length > MAX_QUERY) {
        state.message = `Use at most ${MAX_QUERY} characters to filter the listing.`;
        publish();
        return false;
      }
      const version = epoch;
      state.loading = true;
      state.message = '';
      publish();
      try {
        const query = { kind, offset, limit: LIMIT };
        if (q) query.q = q;
        if (album) query.album_uri = album.uri;
        const playlistQuery = playlist ? { playlist_uri: playlist.uri, offset, q, expected_revision: revision } : null;
        const value = await call(playlist ? 'libraryPlaylist' : 'libraryBrowse', { query: playlist ? P.parameters(playlistQuery) : query });
        if (!current(version)) return false;
        const page = playlist ? playlistPageOf(value, playlistQuery) : pageOf(value, kind, offset, album);
        if (revision !== null && page.revision !== revision) {
          if (retainedContext) {
            selection?.clear(); syncSelection();
            state.selectionNotice = 'A selected cached listing changed, so the track selection was cleared.';
          }
          throw new Error('The library changed between pages. Refresh the listing to start from the beginning.');
        }
        if (retainedContext && !text(page.revision)) {
          throw new Error('A selected cached listing is no longer available. Refresh it and choose the tracks again.');
        }
        if (selection?.rows.length && kind === 'tracks' && text(page.revision)) {
          const currentPage = selection.observe(observation(page));
          if (selection.rows.some(row => Q.pageKey(row.page) === Q.pageKey(currentPage)
              && JSON.stringify(row.item) !== JSON.stringify(page.hits[row.index]))) {
            selection.clear(); syncSelection();
            state.selectionNotice = 'The cached listing changed, so the track selection was cleared.';
            throw new Error('The selected cached occurrences changed. Refresh the listing and choose them again.');
          }
        }
        state.page = page;
        state.hits = page.hits;
        state.loaded = true;
        syncSelection();
        return true;
      } catch (error) {
        if (current(version)) {
          state.message = message(error);
          if (retainedContext) {
            selection.clear(); syncSelection();
            state.selectionNotice = 'A selected listing could not be verified, so the track selection was cleared. Refresh the listing to choose again.';
          }
        }
        return false;
      } finally {
        if (current(version)) { state.loading = false; publish(); }
      }
    }

    function turn(forward, expected = state.page) {
      if (!expected || expected !== state.page || !state.loaded || state.loading || state.writing) return false;
      const offset = forward ? expected.next : expected.offset > 0 ? Math.max(0, expected.offset - LIMIT) : null;
      return offset === null ? false : browse(state.browseKind, offset, state.browseAlbum, expected.revision, state.browsePlaylist, true);
    }

    function openAlbum(index, expected = state.hits[index]) {
      const hit = state.hits[index];
      if (disposed || !state.open || state.loading || state.writing || !state.loaded
          || !hit || hit !== expected || hit.kind !== 'album' || !hit.source) return false;
      // An album title filter should not silently filter its movement list.
      state.query = '';
      return browse('tracks', 0, Object.freeze({ uri: hit.source, title: hit.title }));
    }

    function openPlaylist(index, expected = state.hits[index]) {
      const hit = state.hits[index];
      if (disposed || !state.open || state.loading || state.writing || !state.loaded
          || !hit || hit !== expected || hit.kind !== 'playlist' || !hit.source) return false;
      state.query = '';
      return browse('tracks', 0, null, null, Object.freeze({ uri: hit.source, title: hit.title }));
    }

    const refresh = () => state.view === 'browse'
      ? browse(state.browseKind, 0, state.browseAlbum, null, state.browsePlaylist, false) : read('search', false);
    const filter = () => state.view === 'browse'
      ? browse(state.browseKind, 0, state.browseAlbum, null, state.browsePlaylist) : read('search');
    const search = () => read('search');
    const recent = () => read('history');

    function selectTrack(index, checked, expected = state.hits[index]) {
      const hit = state.hits[index];
      if (!Q || disposed || !state.open || state.loading || state.writing || state.pending || !state.loaded
          || state.view !== 'browse' || state.browseKind !== 'tracks' || !state.page
          || !hit || hit !== expected || !Q.supported(hit.source) || typeof checked !== 'boolean') return false;
      try {
        if (checked) selection.add(observation(), index, hit);
        else selection.unselect(observation(), index);
        syncSelection(); state.selectionNotice = ''; state.message = ''; publish(); return true;
      } catch (error) { syncSelection(); state.message = message(error); publish(); return false; }
    }

    function clearSelection() {
      if (disposed || state.writing || state.loading) return;
      selection?.clear(); syncSelection(); state.pending = null; state.message = ''; state.selectionNotice = ''; publish();
    }

    function editSelection(action, id, delta) {
      if (!selection || disposed || !state.open || state.loading || state.writing || state.pending) return false;
      try {
        const changed = action === 'move' ? selection.move(id, delta) : selection[action](id);
        syncSelection(); state.message = ''; state.selectionNotice = ''; publish(); return changed;
      } catch (error) { state.message = message(error); publish(); return false; }
    }

    async function verifySelection(rows, valid) {
      const pages = new Map(rows.map(row => [Q.pageKey(row.page), row.page]));
      for (const [key, observed] of pages) {
        if (!valid()) return false;
        const q = observed.query, playlistQuery = q.playlist_uri ? P.queryOf({
          playlist_uri: q.playlist_uri, offset: q.offset, q: q.q, expected_revision: observed.revision }) : null;
        const query = { kind: 'tracks', offset: q.offset, limit: LIMIT,
          ...(q.album_uri ? { album_uri: q.album_uri } : {}), ...(q.q ? { q: q.q } : {}) };
        const value = await call(playlistQuery ? 'libraryPlaylist' : 'libraryBrowse', {
          query: playlistQuery ? P.parameters(playlistQuery) : query });
        if (!valid()) return false;
        const page = playlistQuery ? playlistPageOf(value, playlistQuery)
          : pageOf(value, 'tracks', q.offset, q.album_uri ? { uri: q.album_uri } : null);
        if (page.revision !== observed.revision
            || JSON.stringify(page.albumObservation ?? null) !== JSON.stringify(observed.albumObservation)
            || rows.some(row => Q.pageKey(row.page) === key
              && JSON.stringify(page.hits[row.index]) !== JSON.stringify(row.item))) {
          throw new Error('The cached library selection changed. Refresh its listing and review again.');
        }
      }
      return true;
    }

    function trackTarget(field, value) {
      if (disposed || !state.open || state.writing || !['trackDestination', 'trackTarget'].includes(field)
          || typeof value !== 'string' || (field === 'trackDestination' && !['group', 'set'].includes(value))) return false;
      ++epoch; state.pending = null; state.loading = false; state.result = null;
      state[field] = value; state.message = ''; publish(); return true;
    }

    async function sceneSelection() {
      const zone = destination();
      if (!Q || disposed || !state.open || state.loading || state.writing || state.pending
          || state.trackDestination !== 'group' || !zone || !state.basket.length) return null;
      const rows = Object.freeze(state.basket.map(row => Object.freeze({ ...row, hit: row.item }))), version = ++epoch;
      state.loading = true; state.message = 'Checking the selected music before opening a scene draft…'; publish();
      try {
        if (!await verifySelection(rows, () => current(version))) return null;
        const tracks = Q.tracksOf(rows.map(({ hit }) => ({ source_uri: hit.source, title: hit.title })));
        state.message = '';
        return Object.freeze({ kind: 'tracks', tracks, group: Object.freeze({ target: zone.target, members: zone.members }) });
      } catch (error) {
        if (current(version)) state.message = message(error);
        return null;
      } finally { if (current(version)) { state.loading = false; publish(); } }
    }

    async function playlistSelection(offer = null) {
      const zone = destination();
      if (!Q || disposed || !state.open || state.loading || state.writing || state.pending
          || state.trackDestination !== 'group' || !zone || !state.basket.length) return null;
      const rows = Object.freeze(state.basket.map(row => Object.freeze({ ...row }))), version = ++epoch;
      state.loading = true; state.message = 'Checking the selected music before choosing a saved playlist…'; publish();
      try {
        if (!await verifySelection(rows, () => current(version))) return null;
        const tracks = Q.tracksOf(rows.map(({ item }) => ({ source_uri: item.source, title: item.title })));
        state.message = '';
        const selected = Object.freeze({ group: zone, tracks, verify: async valid => {
          await verifySelection(rows, () => { valid(); return true; }); valid(); return tracks;
        } });
        if (!offer) return selected;
        // Keep the basket locked while the saved-playlist panel reads its
        // destination catalog. A closed or changed browser must never let an
        // older handoff clear a newer selection or playback review.
        const accepted = await offer(selected);
        if (!current(version)) return false;
        if (accepted) { selection.clear(); syncSelection(); state.selectionNotice = ''; }
        state.message = accepted
          ? 'Choose a saved playlist in the selected household, then review the ordered additions there.'
          : 'Finish or clear the current source draft, then choose Add selection to Sonos playlist again.';
        return Boolean(accepted);
      } catch (error) { if (current(version)) state.message = message(error); return null; }
      finally { if (current(version)) { state.loading = false; publish(); } }
    }

    async function chooseTracks(action) {
      const zone = destination(), set = state.trackDestination === 'set';
      if (!Q || disposed || !state.open || state.loading || state.writing || state.pending || (set ? !state.trackTarget.trim() : !zone)
          || !state.basket.length || !['play', 'append', 'next'].includes(action)) return false;
      const rows = Object.freeze(state.basket.map(row => Object.freeze({ ...row, hit: row.item })));
      const catalog = Object.freeze({ rows });
      const version = ++epoch;
      state.loading = true; state.result = null; state.message = 'Checking selected pages and destination queues for review…'; publish();
      try {
        if (!await verifySelection(rows, () => current(version))) return false;
        const tracks = Q.tracksOf(rows.map(({ hit }) => ({ source_uri: hit.source, title: hit.title })));
        if (set) {
          const T = targetsApi(), player = playerApi(), choice = T.prepareTracks(tracks, action === 'append' ? 'end' : action);
          const valid = () => { if (!current(version)) throw new Error('The track selection or destination changed during review.'); };
          const targets = await T.inspect({ call, player, target: state.trackTarget, ...choice, valid });
          valid();
          state.pending = Object.freeze({ tracks: catalog, action, version, choice, targets, ...T.request(choice, targets, player) });
          state.message = ''; return true;
        }
        const [queueValue, playbackValue] = await Promise.all([
          call('queue', { params: { room: zone.target }, query: { offset: 0, limit: 1 } }),
          call('state', { params: { room: zone.target } }),
        ]);
        if (!current(version)) return false;
        const queue = Q.queueOf(queueValue, zone.target), playback = Q.playbackOf(playbackValue, zone);
        state.pending = Object.freeze({ tracks: catalog, zone, action, version, queue, playback,
          route: action === 'play' ? 'libraryPlayTracks' : 'libraryTracks',
          body: action === 'play' ? Q.playBody(zone.target, tracks, queue) : Q.body(zone.target, tracks, action === 'next', queue) });
        state.message = ''; return true;
      } catch (error) {
        if (current(version)) state.message = message(error);
        return false;
      } finally {
        if (current(version)) { state.loading = false; publish(); }
      }
    }

    function choose(index, action, expected = state.hits[index]) {
      const zone = destination();
      const hit = state.hits[index];
      if (disposed || !state.open || state.loading || state.writing || !state.loaded || !zone || !hit || hit !== expected
          || !['play', 'append', 'next'].includes(action)
          || !(hit.source || hit.favorite)) return false;
      const body = hit.favorite ? { zone: zone.target, favorite: hit.favorite }
        : { zone: zone.target, source_uri: hit.source, title: hit.title };
      if (action !== 'play') body.next = action === 'next';
      state.pending = Object.freeze({ hit, zone, action, version: epoch,
        album: state.view === 'browse' ? state.browseAlbum : null,
        albumRevision: state.browseAlbum ? state.page.revision : null,
        albumQuery: state.view === 'browse' && state.browseAlbum ? Object.freeze({
          kind: 'tracks', album_uri: state.browseAlbum.uri, offset: state.page.offset, limit: LIMIT,
          ...(state.query.trim() ? { q: state.query.trim() } : {}) }) : null,
        playlistQuery: state.view === 'browse' && state.browsePlaylist ? P.queryOf({
          playlist_uri: state.browsePlaylist.uri, offset: state.page.offset, q: state.query,
          expected_revision: state.page.revision }) : null,
        route: action !== 'play' ? 'libraryAdd' : hit.favorite ? 'playFavorite' : 'libraryPlay',
        body: Object.freeze(body) });
      state.message = '';
      publish();
      return true;
    }

    function routine(index, expected = state.hits[index]) {
      const hit = state.hits[index];
      return disposed || !state.open || state.loading || state.writing || state.pending || !state.loaded || !hit || hit !== expected
        ? null : routineSourceOf(hit, destination());
    }

    function cancel() {
      if (disposed || state.writing) return;
      state.pending = null;
      publish();
    }

    async function confirm(pending = state.pending) {
      if (disposed || !state.open || state.writing || !pending || pending !== state.pending
          || pending.version !== epoch) return false;
      state.writing = true;
      state.pending = null;
      state.message = 'Checking the destination before sending…';
      const version = ++epoch;
      let sent = false;
      publish();
      try {
        if (pending.targets) {
          const T = targetsApi(), player = playerApi();
          const valid = () => { if (!current(version)) throw new Error('The track selection review closed or changed before sending.'); };
          await executeSet(pending.targets.groups, async () => {
            if (sent) return;
            valid();
            if (!await verifySelection(pending.tracks.rows, () => current(version))) return;
            const before = await T.inspect({ call, player, target: pending.targets.target, ...pending.choice, valid });
            valid();
            if (before.signature !== pending.targets.signature) throw new Error('Playback, queues or resolved destinations changed. Review the selected tracks again.');
            sent = true;
            const value = await call(pending.route, { body: pending.body });
            state.result = T.result(value, pending.targets, player);
            state.message = state.result.detail;
          });
          if (!sent) throw new Error('No selected-track command was sent. Review the destinations again.');
          selection?.clear(); syncSelection(); return true;
        }
        const zones = zonesOf(await call('zones'));
        if (!current(version)) return false;
        if (!zones.some((zone) => zone.signature === pending.zone.signature)) {
          state.zones = zones;
          state.target = '';
          throw new Error('The destination group changed. Choose it again and search again.');
        }
        if (pending.albumQuery) {
          const value = await call('libraryBrowse', { query: pending.albumQuery });
          if (!current(version)) return false;
          const page = pageOf(value, 'tracks', pending.albumQuery.offset, pending.album);
          if (page.revision !== pending.albumRevision
              || !page.hits.some(hit => JSON.stringify(hit) === JSON.stringify(pending.hit))) {
            throw new Error('The cached album selection changed. Read it again before choosing music.');
          }
        }
        if (pending.playlistQuery) {
          const value = await call('libraryPlaylist', { query: P.parameters(pending.playlistQuery) });
          if (!current(version)) return false;
          const page = playlistPageOf(value, pending.playlistQuery);
          if (!page.hits.some(hit => JSON.stringify(hit) === JSON.stringify(pending.hit))) {
            throw new Error('The cached playlist selection changed. Read it again before choosing music.');
          }
        }
        if (pending.tracks) {
          if (!await verifySelection(pending.tracks.rows, () => current(version))) return false;
          const [queueValue, playbackValue] = await Promise.all([
            call('queue', { params: { room: pending.zone.target }, query: { offset: 0, limit: 1 } }),
            call('state', { params: { room: pending.zone.target } }),
          ]);
          if (!current(version)) return false;
          Q.verifyQueue(pending.queue, Q.queueOf(queueValue, pending.zone.target), pending.action !== 'append');
          if (Q.playbackOf(playbackValue, pending.zone) !== pending.playback) {
            throw new Error('Playback changed since review. Review the selected tracks again.');
          }
        }
        if (pending.hit?.favorite) {
          const favorites = await call('favorites', { query: { zone: pending.zone.target } });
          if (!current(version)) return false;
          const matches = Array.isArray(favorites) ? favorites.filter((favorite) => favorite
            && favorite.id === pending.hit.favorite && favorite.title === pending.hit.title
            && (favorite.description || '') === pending.hit.subtitle
            && ['track', 'stream', 'container'].includes(favorite.kind)) : [];
          if (matches.length !== 1) throw new Error('The household favorite changed. Search again.');
          if (pending.action !== 'play' && !['track', 'container'].includes(matches[0].kind)) {
            throw new Error('Station favorites cannot be queued. Use Play now instead.');
          }
        }
        if (!current(version)) return false;
        sent = true;
        state.message = 'Sending the reviewed action…';
        publish();
        const result = await call(pending.route, { body: pending.body });
        if (pending.tracks) {
          if (pending.action === 'play') Q.playReceipt(result);
          else Q.receipt(result, pending.body, pending.queue);
        }
        const valid = result && text(result.done) && (pending.action === 'play'
          ? typeof result.changed === 'boolean'
          : number(result.first) && result.first > 0 && number(result.count) && result.count > 0
            && typeof result.playing_queue === 'boolean');
        const notes = result && result.notes;
        if (!valid || (notes !== undefined && (!Array.isArray(notes)
            || notes.some((note) => !note || typeof note.detail !== 'string')))) {
          throw new Error('The action returned an unreadable result; its outcome is uncertain.');
        }
        state.message = [result.done, ...(notes || []).map((note) => note.detail)].join(' ');
        selection?.clear(); syncSelection();
        return true;
      } catch (error) {
        state.hits = [];
        state.loaded = false;
        selection?.clear(); syncSelection();
        state.result = sent && pending.targets ? targetsApi().failure(error, pending.targets, playerApi()) : null;
        state.message = state.result?.detail || `${message(error)}${sent
          ? ' Read the rooms and queues before another action; the request was not repeated.' : ' No playback request was sent.'}`;
        return false;
      } finally {
        state.writing = false;
        publish();
        // Refresh the room cards even after a lost reply, but never retry a write.
        if (sent && !disposed) Promise.resolve().then(played).catch(() => {});
      }
    }

    return { state, refreshRooms, target, query, search, recent, browse, openAlbum, openPlaylist, refresh, filter,
      catalogChanged(change = null) {
        if (disposed || state.view !== 'search' || change?.favorites === false
            || change?.household && destination()?.household !== change.household) return;
        invalidate(true); state.message = 'The household favorites changed. Search again for current results.'; publish();
      },
      next: (page) => turn(true, page), previous: (page) => turn(false, page), choose, selectTrack, clearSelection, trackTarget, chooseTracks, sceneSelection, playlistSelection, routine, cancel, confirm,
      removeTrack: id => editSelection('remove', id), repeatTrack: id => editSelection('repeat', id),
      moveTrack: (id, delta) => editSelection('move', id, delta),
      close() { state.open = false; invalidate(); publish(); },
      dispose() { disposed = true; invalidate(); } };
  }

  function panel({ call, el, played, executeSet }) {
    const body = el('div', { class: 'library-body' });
    const node = el('details', { class: 'library' }, el('summary', { text: 'Find music' }), body);
    const control = model({ call, played, executeSet, changed: render });
    const button = (label, action, disabled = false, key = label) => el('button', {
      type: 'button', text: label, onclick: action, disabled, 'data-library-focus': key,
    });
    const destination = el('select', { 'aria-label': 'Destination group' });
    const trackDestination = el('select', { 'aria-label': 'Selected track destination' },
      el('option', { value: 'group', text: 'Chosen listening group' }),
      el('option', { value: 'set', text: 'Rooms or saved room set' }));
    const trackTarget = el('input', { type: 'text', maxlength: 128, 'aria-label': 'Rooms or room set for selected tracks',
      placeholder: 'Saved alias, everywhere, all@S1 or all@S2' });
    const trackTargetLabel = el('label', {}, 'Rooms or room set', trackTarget);
    const trackTargets = el('div', { class: 'row' }, el('label', {}, 'Selected tracks in', trackDestination), trackTargetLabel);
    const input = el('input', { type: 'search', maxlength: MAX_QUERY, placeholder: 'Artist, title, album or playlist',
      'aria-label': 'Search your music library', class: 'library-query' });
    const search = button('Search library', () => control.search());
    const rooms = button('Refresh rooms', () => control.refreshRooms());
    const recent = button('Recently played', () => control.recent());
    const browseButtons = Object.keys(categories).map((kind) =>
      [kind, button(`Browse ${kind}`, () => control.browse(kind))]);
    const previous = button('Previous page', () => control.previous());
    const next = button('Next page', () => control.next());
    const refresh = button('Refresh listing', () => control.refresh());
    const back = button('Back to albums', () => { control.query(''); control.browse(control.state.browsePlaylist ? 'playlists' : 'albums'); });
    const navigation = el('div', { class: 'row' }, previous, next, refresh, back);
    let reviewFocus = null;
    const status = el('p', { role: 'status', 'aria-live': 'polite' });
    const results = el('div', { class: 'library-results' });
    function createRoutine(index, expected) {
      const selection = control.routine(index, expected);
      if (!selection || document.hidden || node.closest('[hidden], [inert]')) return;
      const event = new CustomEvent('fsonos:routine-source', { cancelable: true, detail: selection });
      status.textContent = document.dispatchEvent(event)
        ? 'Music routines are not available on this page. Open the full remote to create a routine.'
        : 'Music routines opened. Check the music, group and timing there before saving.';
    }
    async function createSelectionScene() {
      const selection = await control.sceneSelection();
      if (!selection || document.hidden || !node.open || node.closest('[hidden], [inert]')) return;
      const event = new CustomEvent('fsonos:scene-selection', { cancelable: true, detail: selection });
      status.textContent = document.dispatchEvent(event)
        ? 'Close any unfinished scene draft, then choose Save selection as scene again.'
        : 'The selected tracks opened in Scenes. Name the scene, check its group and playback setting, then review before saving.';
    }
    async function addSelectionToPlaylist() {
      return control.playlistSelection(async selection => {
        if (document.hidden || !node.open || node.closest('[hidden], [inert]')) return false;
        const detail = { selection, response: null };
        const event = new CustomEvent('fsonos:playlist-selection', { cancelable: true, detail });
        return !document.dispatchEvent(event) && Boolean(await detail.response);
      });
    }
    destination.addEventListener('change', () => control.target(destination.value));
    trackDestination.addEventListener('change', () => control.trackTarget('trackDestination', trackDestination.value));
    trackTarget.addEventListener('input', () => control.trackTarget('trackTarget', trackTarget.value));
    input.addEventListener('input', () => control.query(input.value));
    input.addEventListener('keydown', (event) => {
      if (event.key === 'Enter' && !event.isComposing) { event.preventDefault(); control.filter(); }
    });
    body.append(el('p', { text: 'Browse cached tracks, albums and playlists, or search your library and the chosen household’s Sonos favorites. Choose a group before playing.' }),
      el('label', {}, 'Play in', destination), el('div', { class: 'row' }, rooms),
      input, el('div', { class: 'row' }, search, recent),
      el('div', { class: 'row' }, ...browseButtons.map(([, button]) => button)), navigation, trackTargets, status, results);

    function render(state) {
      const active = document.activeElement;
      const focus = results.contains(active) ? active.getAttribute('data-library-focus') : null;
      body.setAttribute('aria-busy', String(state.loading || state.writing));
      destination.replaceChildren(el('option', { value: '', text: 'Choose a group…', disabled: true }));
      for (const zone of state.zones) destination.append(el('option', { value: zone.target, text: zone.label }));
      destination.value = state.target;
      destination.disabled = state.writing || state.loading || !state.zones.length;
      input.disabled = state.writing || (state.loading && !state.target);
      if (input.value !== state.query) input.value = state.query;
      search.disabled = state.writing || state.loading || !state.target || !state.query.trim();
      rooms.disabled = state.writing || state.loading;
      recent.disabled = state.writing || state.loading || !state.target;
      for (const [kind, button] of browseButtons) {
        button.disabled = state.loading || state.writing;
        button.setAttribute('aria-pressed', String(state.view === 'browse' && state.browseKind === kind));
      }
      navigation.hidden = state.view !== 'browse' || Boolean(state.pending);
      const busy = state.loading || state.writing;
      trackTargets.hidden = !state.basket.length;
      trackDestination.value = state.trackDestination; trackDestination.disabled = state.writing;
      if (trackTarget.value !== state.trackTarget) trackTarget.value = state.trackTarget;
      trackTargetLabel.hidden = state.trackDestination !== 'set'; trackTarget.disabled = state.writing;
      previous.disabled = busy || !state.loaded || !state.page || state.page.offset === 0;
      next.disabled = busy || !state.loaded || !state.page || state.page.next === null;
      refresh.disabled = busy;
      back.disabled = busy;
      back.hidden = !state.browseAlbum && !state.browsePlaylist;
      back.textContent = state.browsePlaylist ? 'Back to playlists' : 'Back to albums';
      recent.setAttribute('aria-pressed', String(state.view === 'history'));
      search.setAttribute('aria-pressed', String(state.view === 'search'));
      status.textContent = [state.selectionNotice, state.message || (state.loading ? 'Reading…' : '')].filter(Boolean).join(' ');
      results.replaceChildren();
      if (Q && !state.pending && (state.basket.length || state.loaded && state.view === 'browse' && state.browseKind === 'tracks')) {
        const choose = action => { reviewFocus = `selected-${action}`; void control.chooseTracks(action); };
        const locked = busy || !state.basket.length || (state.trackDestination === 'set' ? !state.trackTarget.trim() : !state.target);
        const basketList = el('ol', { class: 'library-selection', 'aria-label': 'Selected tracks in playback order' });
        state.basket.forEach((entry, index) => {
          const hit = entry.item;
          const edit = (label, action, disabled, name) => {
            const result = button(label, action, disabled, `basket-${entry.id}-${name}`);
            result.setAttribute('aria-label', `${label}: selected track ${index + 1}, ${hit.title || 'Untitled track'}`);
            return result;
          };
          basketList.append(el('li', {}, el('p', { text: [hit.title || 'Untitled track', hit.subtitle,
            hit.position ? `Playlist position ${hit.position}` : hit.disc != null ? `Disc ${hit.disc}, track ${hit.track ?? '?'}` : '',
            hit.explicitKnown === false ? 'Explicit-content metadata unavailable' : hit.explicit ? 'Explicit' : '',
            Q.originLabel(entry.page),
            `Listing page ${Math.floor(entry.page.query.offset / LIMIT) + 1}`].filter(Boolean).join(' · ') }),
          el('div', { class: 'row' },
            edit('Earlier', () => control.moveTrack(entry.id, -1), busy || index === 0, 'earlier'),
            edit('Later', () => control.moveTrack(entry.id, 1), busy || index === state.basket.length - 1, 'later'),
            edit('Repeat track', () => control.repeatTrack(entry.id), busy || state.basket.length >= Q.MAX_TRACKS, 'repeat'),
            edit('Remove selection', () => control.removeTrack(entry.id), busy, 'remove'))));
        });
        results.append(el('section', { class: 'library-selected-tracks', 'aria-label': 'Build a track selection' },
          el('strong', { text: `${state.basket.length} selected · ${Q.MAX_TRACKS} track limit` }),
          el('p', { text: 'Build a selection from different albums, playlists and filtered track listings. Your tracks stay selected as you browse or search. Each collection starts in listing order; use Earlier, Later or Repeat track to choose your order. Refresh listing or closing this music browser clears the selection.' }),
          basketList, el('div', { class: 'row', role: 'group', 'aria-label': 'Selected track actions' },
            button('Play selected now', () => choose('play'), locked, 'selected-play'),
            button('Queue selected next', () => choose('next'), locked, 'selected-next'),
            button('Add selected to end', () => choose('append'), locked, 'selected-append'),
            button('Save selection as scene', () => createSelectionScene(), locked || state.trackDestination !== 'group', 'selected-scene'),
            button('Add selection to Sonos playlist', () => addSelectionToPlaylist(), locked || state.trackDestination !== 'group', 'selected-playlist'),
            button('Clear selection', () => control.clearSelection(), busy || !state.basket.length))));
      }
      if (state.pending?.tracks) {
        const pending = state.pending, label = { play: 'Play selected now', next: 'Queue selected next', append: 'Add selected to end' }[pending.action];
        const list = el('ol');
        for (const { hit, page } of pending.tracks.rows) list.append(el('li', { text: [hit.title || 'Untitled track', hit.subtitle,
          hit.position ? `Playlist position ${hit.position}` : hit.disc != null ? `Disc ${hit.disc}, track ${hit.track ?? '?'}` : '',
          hit.explicitKnown === false ? 'Explicit-content metadata unavailable' : hit.explicit ? 'Explicit' : '',
          Q.originLabel(page),
          hit.source].filter(Boolean).join(' · ') }));
        results.append(el('div', { class: 'library-review', role: 'group', 'aria-label': 'Review selected tracks' },
          el('strong', { text: `${label}: ${pending.tracks.rows.length} track(s)` }),
          el('p', { text: `Destination: ${pending.targets ? pending.targets.target : pending.zone.label}` }), list,
          pending.targets ? el('ul', {}, ...targetsApi().reviewGroups(pending.targets).map(text => el('li', { text }))) : null,
          el('p', { text: pending.action === 'play'
            ? `Replaces ${pending.targets ? 'each reviewed group’s queue' : `this group’s ${pending.queue.total} existing queue entries`} with only the tracks shown, then starts from the first track in this order. Repeated occurrences are preserved. Shuffle and repeat are turned off. This changes current playback.`
            : 'Adds this exact list in the shown order, including repeated occurrences. Playback stays as it is. The whole successful selection has one undo record.' }),
          !pending.targets && pending.action === 'next' && pending.queue.playing === null
            ? el('p', { text: 'This group is not playing its queue. The selection will be added to its end without changing the source.' }) : null,
          el('div', { class: 'row' }, button('Cancel', () => control.cancel()),
            button(`Confirm ${label.toLowerCase()}`, () => control.confirm(pending)))));
      } else if (state.pending) {
        const pending = state.pending;
        const label = { play: 'Play now', append: 'Add to end', next: 'Queue next' }[pending.action];
        const warning = pending.action !== 'play' ? 'Adds to the queue without starting playback.'
          : pending.hit.kind === 'track' ? 'Replaces the current source for this group.'
            : 'Replaces current playback and may replace the group’s queue.';
        results.append(el('div', { class: 'library-review', role: 'group', 'aria-label': 'Review music action' },
          el('strong', { text: `${label}: ${pending.hit.title || 'Untitled'}` }),
          el('p', { text: `Destination: ${pending.zone.label}` }), el('p', { text: warning }),
          pending.hit.position ? el('p', { text: `Playlist position ${pending.hit.position}. Uses the selected track's exact source.` }) : null,
          pending.hit.disc != null && pending.hit.track != null ? el('p', { text: `Disc ${pending.hit.disc}, track ${pending.hit.track}. Source: ${pending.hit.source}` }) : null,
          pending.hit.explicitKnown === false ? el('p', { text: 'Artist or explicit-content metadata may be unavailable for this catalog track.' }) : null,
          pending.hit.favorite && pending.action !== 'play'
            ? el('p', { text: 'Only track, album and playlist favorites can be queued. The household favorite is checked again before sending; stations use Play now.' }) : null,
          pending.hit.playedAt !== undefined && pending.action === 'play'
            ? el('p', { text: 'Replays this item from the beginning, not its old queue position.' }) : null,
          el('div', { class: 'row' }, button('Cancel', () => control.cancel()),
            button(`Confirm ${label.toLowerCase()}`, () => control.confirm(pending)))));
      } else if (state.loaded) {
        const count = state.hits.length;
        const collection = state.browseAlbum || state.browsePlaylist;
        const heading = state.view === 'browse'
          ? `${collection ? `Cached tracks in ${collection.title || 'Untitled collection'}` : `Cached ${state.browseKind}`}: ${count
            ? `${state.page.offset + 1}–${state.page.offset + count} of ${state.page.total}` : 'no matching entries'}. ${state.page.playlist ? P.describe(state.page.playlist) : albumCoverage(state.page)}`
          : state.view === 'history'
          ? count ? `${count} recent recorded play(s) for this group. Times are shown in this browser's local time.`
            : 'No recorded plays for this group yet.'
          : count ? `${count} match(es)${count === LIMIT ? '; refine the search for more specific results' : ''}.`
            : 'No matches. Try different words or sync your Spotify library on the daemon.';
        results.append(el('p', { text: heading }));
        const list = el('ul', { class: 'library-hits' });
        state.hits.forEach((hit, index) => {
          const row = el('li', {}, el('div', { class: 'track' },
            el('strong', { text: `${hit.position ? `${hit.position}. ` : ''}${hit.title || 'Untitled'}` }),
            el('div', { class: 'by', text: [hit.kind, hit.subtitle || (state.browseAlbum ? 'Artist unavailable' : '')].filter(Boolean).join(' · ') })));
          if (hit.playedAt !== undefined) {
            const at = new Date(hit.playedAt * 1000);
            row.append(el('time', { datetime: at.toISOString(), text: at.toLocaleString() }));
          }
          const actions = el('div', { class: 'row' });
          if (state.view === 'browse') {
            if (Q && state.browseKind === 'tracks' && Q.supported(hit.source)) {
              const checked = state.selected.includes(index);
              const checkbox = el('input', { type: 'checkbox', checked, 'data-library-focus': `select-${index}`,
                disabled: busy || (!checked && state.basket.length >= Q.MAX_TRACKS),
                'aria-label': `Select ${hit.position ? `playlist position ${hit.position}, ` : ''}${hit.title || 'Untitled track'}`,
                onchange: event => control.selectTrack(index, event.target.checked, hit) });
              row.append(el('label', { class: 'library-select-track' }, checkbox, 'Select track'));
            }
            if (hit.kind === 'album') {
              row.append(el('p', { class: 'by', text: `${hit.cachedTracks} cached track(s); the album may contain more.` }));
              actions.append(button('View cached tracks', () => control.openAlbum(index, hit),
                state.loading || state.writing, `album-${index}`));
            }
            if (hit.kind === 'playlist') actions.append(button('View cached tracks', () => control.openPlaylist(index, hit),
              state.loading || state.writing, `playlist-${index}`));
            if (state.browseAlbum && (hit.disc != null || hit.track != null)) {
              row.append(el('p', { class: 'by', text: `Disc ${hit.disc ?? '?'}, track ${hit.track ?? '?'}` }));
            }
            if (hit.explicitKnown === false) row.append(el('span', { class: 'by', text: 'Explicit-content metadata unavailable' }));
            else if (hit.explicit) row.append(el('span', { class: 'by', text: 'Explicit' }));
          }
          const locked = state.loading || state.writing || !state.target;
          const select = (action) => {
            reviewFocus = `${action}-${index}`;
            control.choose(index, action, hit);
          };
          if (hit.source || hit.favorite) actions.append(button('Play now', () => select('play'), locked, `play-${index}`));
          if (hit.source || hit.favorite) actions.append(button('Add to end', () => select('append'), locked, `append-${index}`),
            button('Queue next', () => select('next'), locked, `next-${index}`));
          if (routineSourceOf(hit, state.zones.find((zone) => zone.target === state.target))) {
            actions.append(button('Create routine', () => createRoutine(index, hit), state.loading || state.writing, `routine-${index}`));
          }
          if (!hit.source && !hit.favorite) actions.append(el('span', { text: 'Read-only: unsupported item.' }));
          row.append(actions);
          list.append(row);
        });
        results.append(list);
      }
      const key = state.pending ? 'Cancel' : focus === 'Cancel' ? reviewFocus : focus;
      const selected = [...results.querySelectorAll('[data-library-focus]')]
        .find((element) => element.getAttribute('data-library-focus') === key);
      if (selected && !selected.disabled) selected.focus({ preventScroll: true });
    }

    node.addEventListener('toggle', () => { if (node.open) control.refreshRooms(); else control.close(); });
    render(control.state);
    const catalogChanged = event => control.catalogChanged(event.detail);
    document.addEventListener('fsonos:catalog-changed', catalogChanged);
    return { node, dispose() {
      document.removeEventListener('fsonos:catalog-changed', catalogChanged);
      control.dispose();
    } };
  }

  return { model, panel, routes, LIMIT, MAX_QUERY, routineSourceOf, albumCoverage };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = LibraryRemote;
else {
  Object.assign(ROUTES, LibraryRemote.routes);
  const libraryRoot = document.getElementById('music-library');
  if (libraryRoot) {
    let levels;
    registerCardPanel(parts => {
      const observe = () => levels?.observe(parts); observe();
      return { node: el('span', { hidden: true }), observe, block: observe };
    });
    libraryRoot.append(LibraryRemote.panel({ call, el, played: loadZones,
      executeSet(groups, work) {
        levels ||= RoomSetsRemote.levelLanes({ cards: () => house.cards.values(), flights: volumeFlights });
        return RoomSetsRemote.coordinated({ cards: () => house.cards.values(), refresh: loadZones,
          begin: beginAction, end: endAction, shape: groupShape, levels })(groups, work);
      },
    }).node);
  }
}
