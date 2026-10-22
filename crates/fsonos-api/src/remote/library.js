// Music selection through the same HTTP API as the room cards.
'use strict';

const LibraryRemote = (() => {
  const P = typeof PlaylistContents !== 'undefined' ? PlaylistContents
    : typeof require === 'function' ? require('./playlist-contents.js') : null;
  const LIMIT = 50;
  const MAX_QUERY = 512;
  const routes = {
    librarySearch: ['GET', '/library/search'],
    libraryBrowse: ['GET', '/library/browse'],
    libraryPlaylist: ['GET', '/library/playlist'],
    libraryHistory: ['GET', '/history'],
    libraryPlay: ['POST', '/play'],
    libraryAdd: ['POST', '/queue'],
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

  const spotify = (uri) => typeof uri === 'string' && /^spotify:(track|album|playlist):[a-zA-Z0-9]+$/.exec(uri);

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
    if (!/^spotify:(track|album|playlist):[A-Za-z0-9]{22}$/.test(hit.source || '')) return null;
    return Object.freeze({ kind: 'spotify', source_uri: hit.source, title: hit.title.trim() || `Spotify ${hit.kind}`,
      subtitle: hit.subtitle, household: zone?.household || null, group });
  }

  const categories = Object.freeze({ tracks: 'track', albums: 'album', playlists: 'playlist' });

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
    const seen = new Set();
    const hits = hitsOf(value.items).map((hit, i) => {
      const raw = value.items[i];
      if (hit.kind !== categories[kind] || !hit.source || seen.has(hit.source)
          || typeof raw.explicit !== 'boolean'
          || (album && raw.album_uri !== albumUri)
          || (kind === 'albums' && (!number(raw.cached_tracks) || raw.cached_tracks < 1))
          || [raw.disc_number, raw.track_number].some((n) => n != null && !number(n))) {
        throw new Error('The library page identities disagree. Refresh the listing.');
      }
      seen.add(hit.source);
      return Object.freeze({ ...hit, explicit: raw.explicit, cachedTracks: raw.cached_tracks,
        disc: raw.disc_number, track: raw.track_number });
    });
    return Object.freeze({ kind, offset, total: value.total, next, revision: value.revision,
      hits: Object.freeze(hits) });
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

  function model({ call, changed = () => {}, played = () => {} }) {
    let epoch = 0;
    let disposed = false;
    const state = { open: false, zones: [], target: '', query: '', hits: [], view: 'search',
      loaded: false, loading: false, writing: false, pending: null, message: '',
      page: null, browseKind: 'tracks', browseAlbum: null, browsePlaylist: null };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* View only. */ } } };
    const destination = () => state.zones.find((zone) => zone.target === state.target);
    const invalidate = () => {
      ++epoch;
      state.pending = null;
      state.page = null;
      state.hits = [];
      state.loaded = false;
      state.loading = false;
    };
    const current = (version) => !disposed && state.open && version === epoch;

    async function refreshRooms() {
      if (disposed || state.writing) return false;
      state.open = true;
      const previous = destination();
      invalidate();
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
      invalidate();
      if (cached) { state.page = cached; state.hits = cached.hits; state.loaded = true; }
      state.target = value;
      state.message = '';
      publish();
      return true;
    }

    function query(value) {
      if (disposed || state.writing || typeof value !== 'string') return false;
      invalidate();
      state.query = value;
      if (state.view !== 'browse') state.view = 'search';
      state.message = '';
      publish();
      return true;
    }

    async function read(view) {
      if (disposed || !state.open || state.writing) return false;
      const zone = destination();
      const q = state.query.trim();
      invalidate();
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

    async function browse(kind = state.browseKind, offset = 0, album = null, revision = null, playlist = null) {
      if (disposed || !state.open || state.writing || !Object.hasOwn(categories, kind)
          || !number(offset) || (album && (kind !== 'tracks' || spotify(album.uri)?.[1] !== 'album'))
          || (playlist && (kind !== 'tracks' || album || !P || spotify(playlist.uri)?.[1] !== 'playlist'))) return false;
      invalidate();
      state.view = 'browse';
      state.browseKind = kind;
      state.browseAlbum = album;
      state.browsePlaylist = playlist;
      const q = state.query.trim();
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
          throw new Error('The library changed between pages. Refresh the listing to start from the beginning.');
        }
        state.page = page;
        state.hits = page.hits;
        state.loaded = true;
        return true;
      } catch (error) {
        if (current(version)) state.message = message(error);
        return false;
      } finally {
        if (current(version)) { state.loading = false; publish(); }
      }
    }

    function turn(forward, expected = state.page) {
      if (!expected || expected !== state.page || !state.loaded || state.loading || state.writing) return false;
      const offset = forward ? expected.next : expected.offset > 0 ? Math.max(0, expected.offset - LIMIT) : null;
      return offset === null ? false : browse(state.browseKind, offset, state.browseAlbum, expected.revision, state.browsePlaylist);
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
      ? browse(state.browseKind, 0, state.browseAlbum, null, state.browsePlaylist) : read('search');
    const search = () => read('search');
    const recent = () => read('history');

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
        const zones = zonesOf(await call('zones'));
        if (!current(version)) return false;
        if (!zones.some((zone) => zone.signature === pending.zone.signature)) {
          state.zones = zones;
          state.target = '';
          throw new Error('The destination group changed. Choose it again and search again.');
        }
        if (pending.playlistQuery) {
          const value = await call('libraryPlaylist', { query: P.parameters(pending.playlistQuery) });
          if (!current(version)) return false;
          const page = playlistPageOf(value, pending.playlistQuery);
          if (!page.hits.some(hit => JSON.stringify(hit) === JSON.stringify(pending.hit))) {
            throw new Error('The cached playlist selection changed. Read it again before choosing music.');
          }
        }
        if (pending.hit.favorite) {
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
        return true;
      } catch (error) {
        state.hits = [];
        state.loaded = false;
        state.message = `${message(error)}${sent
          ? ' Read the room and queue before trying again; the request was not repeated.' : ' No playback request was sent.'}`;
        return false;
      } finally {
        state.writing = false;
        publish();
        // Refresh the room cards even after a lost reply, but never retry a write.
        if (sent && !disposed) Promise.resolve().then(played).catch(() => {});
      }
    }

    return { state, refreshRooms, target, query, search, recent, browse, openAlbum, openPlaylist, refresh,
      next: (page) => turn(true, page), previous: (page) => turn(false, page), choose, routine, cancel, confirm,
      close() { state.open = false; invalidate(); publish(); },
      dispose() { disposed = true; invalidate(); } };
  }

  function panel({ call, el, played }) {
    const body = el('div', { class: 'library-body' });
    const node = el('details', { class: 'library' }, el('summary', { text: 'Find music' }), body);
    const control = model({ call, played, changed: render });
    const button = (label, action, disabled = false, key = label) => el('button', {
      type: 'button', text: label, onclick: action, disabled, 'data-library-focus': key,
    });
    const destination = el('select', { 'aria-label': 'Destination group' });
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
    destination.addEventListener('change', () => control.target(destination.value));
    input.addEventListener('input', () => control.query(input.value));
    input.addEventListener('keydown', (event) => {
      if (event.key === 'Enter' && !event.isComposing) { event.preventDefault(); control.refresh(); }
    });
    body.append(el('p', { text: 'Browse cached tracks, albums and playlists, or search your library and the chosen household’s Sonos favorites. Choose a group before playing.' }),
      el('label', {}, 'Play in', destination), el('div', { class: 'row' }, rooms),
      input, el('div', { class: 'row' }, search, recent),
      el('div', { class: 'row' }, ...browseButtons.map(([, button]) => button)), navigation, status, results);

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
      previous.disabled = busy || !state.loaded || !state.page || state.page.offset === 0;
      next.disabled = busy || !state.loaded || !state.page || state.page.next === null;
      refresh.disabled = busy;
      back.disabled = busy;
      back.hidden = !state.browseAlbum && !state.browsePlaylist;
      back.textContent = state.browsePlaylist ? 'Back to playlists' : 'Back to albums';
      recent.setAttribute('aria-pressed', String(state.view === 'history'));
      search.setAttribute('aria-pressed', String(state.view === 'search'));
      status.textContent = state.message || (state.loading ? 'Reading…' : '');
      results.replaceChildren();
      if (state.pending) {
        const pending = state.pending;
        const label = { play: 'Play now', append: 'Add to end', next: 'Queue next' }[pending.action];
        const warning = pending.action !== 'play' ? 'Adds to the queue without starting playback.'
          : pending.hit.kind === 'track' ? 'Replaces the current source for this group.'
            : 'Replaces current playback and may replace the group’s queue.';
        results.append(el('div', { class: 'library-review', role: 'group', 'aria-label': 'Review music action' },
          el('strong', { text: `${label}: ${pending.hit.title || 'Untitled'}` }),
          el('p', { text: `Destination: ${pending.zone.label}` }), el('p', { text: warning }),
          pending.hit.position ? el('p', { text: `Playlist position ${pending.hit.position}. Uses the selected track's exact source.` }) : null,
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
            ? `${state.page.offset + 1}–${state.page.offset + count} of ${state.page.total}` : 'no matching entries'}. ${state.page.playlist ? P.describe(state.page.playlist) : 'This cache may include older entries and incomplete albums.'}`
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
            el('div', { class: 'by', text: [hit.kind, hit.subtitle].filter(Boolean).join(' · ') })));
          if (hit.playedAt !== undefined) {
            const at = new Date(hit.playedAt * 1000);
            row.append(el('time', { datetime: at.toISOString(), text: at.toLocaleString() }));
          }
          const actions = el('div', { class: 'row' });
          if (state.view === 'browse') {
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
            if (hit.explicit) row.append(el('span', { class: 'by', text: 'Explicit' }));
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
    return { node, dispose: control.dispose };
  }

  return { model, panel, routes, LIMIT, MAX_QUERY, routineSourceOf };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = LibraryRemote;
else {
  Object.assign(ROUTES, LibraryRemote.routes);
  const libraryRoot = document.getElementById('music-library');
  if (libraryRoot) libraryRoot.append(LibraryRemote.panel({ call, el, played: loadZones }).node);
}
