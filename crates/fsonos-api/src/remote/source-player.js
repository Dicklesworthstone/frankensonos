// Play links, saved Sonos playlists and native inputs through the playback API.
'use strict';
const SourcePlayerRemote = (() => {
  const targets = () => typeof SourceTargetsRemote !== 'undefined' ? SourceTargetsRemote : require('./source-targets.js');
  const routes = { sourcePlay: ['POST', '/play'], sourceQueue: ['POST', '/queue'],
    sourceSpotifySearch: ['GET', '/spotify/search'],
    sourceLineInSources: ['GET', '/line-in'], sourceLineInPlay: ['POST', '/line-in/play'],
    sourceQueueState: ['GET', '/zones/{room}/queue'],
    sourcePlaylists: ['GET', '/sonos/playlists'], sourcePlaylistPlay: ['POST', '/sonos/playlists/play'],
    sourcePlaylistContents: ['GET', '/sonos/playlists/contents'], sourcePlaylistTrack: ['POST', '/sonos/playlists/track'],
    sourcePlaylistTracks: ['POST', '/sonos/playlists/tracks'],
    sourcePlaylistRename: ['POST', '/sonos/playlists/rename'], sourcePlaylistRemove: ['POST', '/sonos/playlists/remove'],
    sourcePlaylistRemoveTracks: ['POST', '/sonos/playlists/remove-tracks'],
    sourcePlaylistMoveTracks: ['POST', '/sonos/playlists/move-tracks'] };
  const modes = Object.freeze({ play: 'Play now', next: 'Queue next', end: 'Queue at end' });
  const MAX_BYTES = 2048, MAX_TITLE = 256, MAX_PLAYLIST_TRACKS = 10;
  const catalogKinds = Object.freeze({ track: 'Tracks', album: 'Albums', artist: 'Artists',
    playlist: 'Playlists', show: 'Podcast shows', episode: 'Episodes' });
  const CATALOG_LIMIT = 10, MAX_QUERY_BYTES = 256;
  const text = (v) => typeof v === 'string' && v.trim() !== '';
  const problem = (e) => [e?.message || 'The request failed.', e?.hint].filter(Boolean).join(' ');

  function spotify(kind, id) {
    kind = kind.toLowerCase();
    if (!['track', 'album', 'playlist', 'artist', 'episode', 'show'].includes(kind)) {
      throw new Error('Choose a Spotify track, album, playlist, artist, episode or show.');
    }
    if (!/^[a-zA-Z0-9]{22}$/.test(id)) throw new Error('The Spotify id must contain exactly 22 letters and digits. Paste the complete link.');
    return Object.freeze({ uri: `spotify:${kind}:${id}`, kind, container: ['album', 'playlist', 'artist', 'show'].includes(kind) });
  }

  function source(raw) {
    if (typeof raw !== 'string') throw new Error('Paste a Spotify link or an audio stream address.');
    const uri = raw.trim();
    if (!uri || new TextEncoder().encode(uri).length > MAX_BYTES) throw new Error(`Use one complete source address, at most ${MAX_BYTES} bytes.`);
    if (/[\s\p{Cc}]/u.test(uri)) throw new Error('The source contains whitespace or control characters. Paste one complete address.');
    if (/^spotify:/i.test(uri)) {
      const parts = uri.split(':');
      if (parts.length === 3) return spotify(parts[1], parts[2]);
      if (parts.length === 5 && /^user$/i.test(parts[1]) && /^playlist$/i.test(parts[3])) return spotify('playlist', parts[4]);
      throw new Error('Use a Spotify track, album, playlist, artist, episode or show URI.');
    }
    // Only media addresses: never resolve short links, load pages, or execute a URI.
    const authority = /^https?:\/\/([^/?#]+)/i.exec(uri);
    if (!authority || authority[1].includes('@') || uri.includes('\\')) {
      throw new Error('Use a complete HTTP or HTTPS audio address without embedded sign-in details.');
    }
    let url;
    try { url = new URL(uri); } catch (_) { throw new Error('The audio address is malformed.'); }
    if (!url.hostname || !['http:', 'https:'].includes(url.protocol)) throw new Error('Use an HTTP or HTTPS audio address.');
    const host = url.hostname.toLowerCase().replace(/\.$/, '');
    if (host === 'spotify.link' || host.endsWith('.spotify.link')) {
      throw new Error('Open the shortened link yourself and paste the complete open.spotify.com address.');
    }
    if (host === 'open.spotify.com' || host === 'play.spotify.com') {
      const segments = url.pathname.split('/').filter(Boolean);
      if (segments[0]?.startsWith('intl-')) segments.shift();
      if (segments[0] === 'embed') segments.shift();
      if (segments.length === 2) return spotify(...segments);
      if (segments.length === 4 && segments[0] === 'user' && segments[2] === 'playlist') return spotify('playlist', segments[3]);
      throw new Error('Paste a link to one Spotify track, album, playlist, artist, episode or show.');
    }
    // Preserve escaping, query parameters and letter case, which may be signed.
    return Object.freeze({ uri, kind: 'stream', container: false });
  }

  // Import only a source and an optional display title. A historical room,
  // queue position or action must never implicitly select the next command.
  function offeredSource(value) {
    if (!value || typeof value !== 'object' || Array.isArray(value)
        || Object.keys(value).some((key) => !['source_uri', 'title'].includes(key))) {
      throw new Error('The source handoff is incomplete or contains a playback destination.');
    }
    const item = source(value.source_uri);
    if (value.title !== undefined && (typeof value.title !== 'string'
        || [...value.title].length > MAX_TITLE || /[\p{Cc}]/u.test(value.title))) {
      throw new Error('The source display title is invalid.');
    }
    return Object.freeze({ input: item.uri, title: value.title?.trim() || '' });
  }

  function catalogQuery(raw, kind) {
    if (typeof raw !== 'string' || /[\p{Cc}]/u.test(raw)) {
      throw new Error('Use a Spotify search without control characters.');
    }
    const q = raw.trim();
    if (!q || new TextEncoder().encode(q).length > MAX_QUERY_BYTES) {
      throw new Error(`Enter a Spotify search between 1 and ${MAX_QUERY_BYTES} bytes.`);
    }
    if (!Object.hasOwn(catalogKinds, kind)) throw new Error('Choose a Spotify catalog category.');
    return Object.freeze({ q, kind, limit: CATALOG_LIMIT });
  }

  function catalogPage(value, query) {
    const fail = () => { throw new Error('The Spotify catalog reply is incomplete. Search again before choosing a source.'); };
    if (!value || value.query !== query.q || value.kind !== query.kind || typeof value.more !== 'boolean'
        || !Array.isArray(value.items) || value.items.length > query.limit) fail();
    const validText = v => typeof v === 'string' && new TextEncoder().encode(v).length <= 4096 && !/[\p{Cc}]/u.test(v);
    const seen = new Set();
    const items = value.items.map(item => {
      if (!item || !validText(item.title) || !item.title.trim()
          || (item.subtitle != null && !validText(item.subtitle))) fail();
      const parsed = source(item.source_uri);
      if (parsed.kind !== query.kind || parsed.uri !== item.source_uri || seen.has(parsed.uri)) fail();
      seen.add(parsed.uri);
      return Object.freeze({ source_uri: parsed.uri, title: item.title, subtitle: item.subtitle || '' });
    });
    return Object.freeze({ query: query.q, kind: query.kind, items: Object.freeze(items), more: value.more });
  }

  // Both surfaces use the same read-only search lane. Editing, changing rooms
  // or closing retires an outstanding read even if the provider ignores abort.
  function catalogModel({ call, available, changed = () => {} }) {
    let version = 0, reading = null;
    const state = { query: '', kind: 'track', status: 'idle', items: [], more: false, message: '' };
    function retire() {
      ++version; reading?.abort(); reading = null;
      state.status = 'idle'; state.items = []; state.more = false; state.message = '';
    }
    async function search() {
      if (!available()) return false;
      retire();
      let query;
      try { query = catalogQuery(state.query, state.kind); }
      catch (error) { state.status = 'error'; state.message = problem(error); changed(); return false; }
      const v = version, abort = new AbortController();
      reading = abort; state.status = 'loading'; state.message = 'Searching the Spotify catalog…'; changed();
      const current = () => v === version && !abort.signal.aborted && available();
      try {
        const value = await call(query, { signal: abort.signal });
        if (!current()) return false;
        const page = catalogPage(value, query);
        state.items = page.items; state.more = page.more; state.status = 'ready';
        state.message = !page.items.length ? 'No matching Spotify results. Try another search or category.'
          : `${page.items.length} Spotify result(s). Choose a source, then review its destination and action.${page.more ? ' More matches are available; refine your search.' : ''}`;
        return true;
      } catch (error) {
        if (current()) { state.status = 'error'; state.message = problem(error); }
        return false;
      } finally {
        if (reading === abort) {
          reading = null;
          if (state.status === 'loading') { state.status = 'idle'; state.message = 'The search is no longer current. Search again.'; }
          changed();
        }
      }
    }
    return { state, search, retire,
      edit(field, value) {
        if (!available() || !['query', 'kind'].includes(field) || typeof value !== 'string'
            || (field === 'kind' && !Object.hasOwn(catalogKinds, value))) return false;
        retire(); state[field] = value; changed(); return true;
      },
      select(item, items = state.items) {
        if (!available() || state.status !== 'ready' || items !== state.items || !items.includes(item)) return null;
        return offeredSource({ source_uri: item.source_uri, title: [...item.title].slice(0, MAX_TITLE).join('') });
      },
      cancel() { retire(); state.message = 'Spotify search cancelled.'; changed(); },
    };
  }

  function renderCatalog(list, state, doc, { choose, disabled = false }) {
    list.replaceChildren();
    const items = state.items;
    for (const item of items) {
      const row = doc.createElement('li'), title = doc.createElement('strong');
      title.textContent = item.title; row.append(title);
      if (item.subtitle) { const byline = doc.createElement('span'); byline.textContent = item.subtitle; row.append(byline); }
      const button = doc.createElement('button'); button.type = 'button'; button.textContent = 'Use this source';
      button.disabled = disabled;
      button.addEventListener('click', () => choose(item, items)); row.append(button); list.append(row);
    }
  }

  function groups(value) {
    if (!Array.isArray(value)) throw new Error('The room list is unreadable. Refresh rooms.');
    const seen = new Set();
    return value.map((zone) => {
      if (!zone || !text(zone.household) || !text(zone.coordinator_room)
          || !Array.isArray(zone.members) || !zone.members.length
          || !zone.members.every(text) || !zone.members.includes(zone.coordinator_room)) throw new Error('The room list is incomplete. Refresh rooms.');
      for (const room of zone.members) {
        const key = JSON.stringify([zone.household, room]);
        if (seen.has(key)) throw new Error('The room list assigns a room more than once. Refresh rooms.');
        seen.add(key);
      }
      const target = `${zone.coordinator_room}@${zone.household}`;
      if ([...target].length > 128) throw new Error('The group address exceeds the supported length.');
      return Object.freeze({ target, household: zone.household, coordinator: zone.coordinator_room,
        label: `${zone.members.join(' + ')} (${zone.household})`,
        signature: JSON.stringify([zone.household, zone.coordinator_room, [...zone.members].sort()]) });
    });
  }

  function playback(value, group) {
    if (!value || groups([value.zone])[0].signature !== group.signature
        || !['playing', 'paused', 'stopped', 'transitioning', 'unknown'].includes(value.transport_state)) {
      throw new Error('The playback group changed. Refresh rooms.');
    }
    const track = value.track;
    if (track != null && (!text(track.uri) || (track.queue_position != null
        && (!Number.isInteger(track.queue_position) || track.queue_position < 1 || track.queue_position > 0xffffffff)))) {
      throw new Error('The current source is unreadable. Read it again before replacing playback.');
    }
    return Object.freeze({ signature: JSON.stringify([value.transport_state, track?.uri ?? null, track?.queue_position ?? null]),
      label: track ? `${track.title || track.uri} (${value.transport_state})` : `No current track (${value.transport_state})` });
  }

  function queue(value, group) {
    const uint = (n) => Number.isInteger(n) && n >= 0 && n <= 0xffffffff;
    if (!value || value.zone !== group.coordinator || !uint(value.total)
        || !text(value.revision) || value.revision.length > 512
        || !Array.isArray(value.items) || value.items.length !== Math.min(1, value.total)
        || (value.items.length && value.items[0]?.position !== 1)
        || (value.playing != null && (!uint(value.playing) || value.playing < 1 || value.playing > value.total))) {
      throw new Error('The queue snapshot is incomplete. Refresh and review the addition again.');
    }
    return Object.freeze({ total: value.total, revision: value.revision, playing: value.playing ?? null });
  }

  function queued(value, group) {
    const positive = (n) => Number.isInteger(n) && n > 0 && n <= 0xffffffff;
    if (!value || value.zone !== group.coordinator || !text(value.done)
        || !positive(value.first) || !positive(value.count)
        || value.first + value.count - 1 > 0xffffffff || typeof value.playing_queue !== 'boolean') {
      throw new Error('The queue addition was not confirmed by a complete response.');
    }
    return `${value.done} ${value.count} track(s) added starting at position ${value.first}.${value.playing_queue
      ? ' Existing playback was not restarted.' : ' The group is not using its queue; adding music did not start it.'}`;
  }

  function outcome(value) {
    if (!value || !text(value.done) || typeof value.changed !== 'boolean'
        || (value.notes != null && (!Array.isArray(value.notes)
          || value.notes.some((note) => !note || typeof note.detail !== 'string')))) throw new Error('The playback reply is incomplete.');
    return [value.done, ...(value.notes || []).map((note) => note.detail)].filter(Boolean).join(' ');
  }

  function prepare(raw, title, mode) {
    if (!Object.hasOwn(modes, mode)) throw new Error('Choose Play now, Queue next or Queue at end.');
    const item = source(raw);
    if (mode !== 'play' && item.kind === 'stream') {
      throw new Error('Only Spotify sources can be queued here. Choose Play now for an audio stream.');
    }
    if (typeof title !== 'string') throw new Error('The display title is invalid.');
    title = [...title.trim()].slice(0, MAX_TITLE).join('');
    if (/[\p{Cc}]/u.test(title)) throw new Error('The display title contains control characters.');
    return Object.freeze({ item, title, mode });
  }

  function playlists(value) {
    if (!Array.isArray(value) || value.some(item => !item || typeof item.id !== 'string' || !/^SQ:[0-9]+$/.test(item.id)
        || typeof item.title !== 'string') || new Set(value.map(item => item.id)).size !== value.length) {
      throw new Error('The saved Sonos playlist list is unreadable. Refresh playlists.');
    }
    return Object.freeze(value.map(item => Object.freeze({ id: item.id, title: item.title })));
  }

  function preparePlaylist(id, catalog, mode) {
    if (!Object.hasOwn(modes, mode)) throw new Error('Choose Play now, Queue next or Queue at end.');
    const selected = catalog.find(item => item.id === id);
    if (!selected) throw new Error('Choose a saved Sonos playlist for this group.');
    return Object.freeze({ item: Object.freeze({ ...selected, kind: 'sonos_playlist', container: true }),
      title: selected.title || selected.id, mode });
  }

  function verifyPlaylist(value, item) {
    const fresh = playlists(value).find(playlist => playlist.id === item.id);
    if (!fresh || fresh.title !== item.title) {
      throw new Error('The selected Sonos playlist changed or disappeared. Refresh playlists and review again.');
    }
  }

  function lineInSources(value) {
    const validText = value => typeof value === 'string' && !/[\p{Cc}]/u.test(value);
    const players = new Set();
    const identify = item => {
      if (!item || !text(item.player) || !validText(item.player) || !text(item.room)
          || !validText(item.room) || players.has(item.player)) return false;
      players.add(item.player); return true;
    };
    if (!value || !Array.isArray(value.sources) || !Array.isArray(value.unavailable)
        || value.sources.some(item => !identify(item) || !validText(item.name) || !validText(item.icon))
        || value.unavailable.some(item => !identify(item) || !text(item.detail) || !validText(item.detail))) {
      throw new Error('The line-in source list is incomplete. Read the inputs again.');
    }
    return Object.freeze({ sources: Object.freeze(value.sources.map(item => Object.freeze({
      player: item.player, room: item.room, name: item.name, icon: item.icon,
    }))), unavailable: Object.freeze(value.unavailable.map(item => Object.freeze({
      player: item.player, room: item.room, detail: item.detail,
    }))) });
  }

  const lineInLabel = item => `${item.name || 'Line-in'} · ${item.room}`;
  function lineInHelp(state) {
    if (state.lineInStatus !== 'ready') return state.target
      ? 'Read the line-in sources in this destination’s household.' : 'Choose a destination group first.';
    const available = state.lineInSources.length
      ? 'Choose the player supplying analog audio. Cable and signal state are not reported; start your source device yourself.'
      : state.lineInUnavailable.length ? 'No line-in source could be read. Refresh to retry unavailable players.'
        : 'No line-in inputs are advertised in this destination’s household.';
    return [available, ...state.lineInUnavailable.map(item => `${item.room}: ${item.detail}`)].join(' ');
  }
  function prepareLineIn(player, catalog, mode) {
    if (mode !== 'play') throw new Error('Line-in is a live source. Choose Play now.');
    const selected = catalog.find(item => item.player === player);
    if (!selected) throw new Error('Choose a line-in source for this destination.');
    return Object.freeze({ item: Object.freeze({ ...selected, kind: 'line_in', container: false }),
      title: lineInLabel(selected), mode });
  }
  function verifyLineIn(value, item) {
    const fresh = lineInSources(value).sources.find(source => source.player === item.player);
    if (!fresh || fresh.room !== item.room || fresh.name !== item.name || fresh.icon !== item.icon) {
      throw new Error('The selected line-in source changed or became unavailable. Read the inputs and review again.');
    }
  }

  function contentsRequest(page, history, direction, playlist) {
    if (!['refresh', 'next', 'previous'].includes(direction)) return null;
    if (direction !== 'refresh' && (!page || page.playlist.id !== playlist)) return null;
    const offset = direction === 'refresh' ? 0 : direction === 'next' ? page.next_offset : history.at(-1);
    if (offset === null || offset === undefined) return null;
    return { query: { playlist, offset, limit: 100,
      ...(direction === 'refresh' ? {} : { expected_revision: page.revision }) },
      history: direction === 'refresh' ? [] : direction === 'next' ? [...history, page.offset] : history.slice(0, -1) };
  }

  function playlistContents(value, query, selected, group) {
    const fail = () => { throw new Error('The saved playlist contents changed or are unreadable. Read tracks again.'); };
    const uint = n => Number.isInteger(n) && n >= 0 && n <= 0xffffffff;
    if (!value || !group || value.zone !== group.coordinator || !value.playlist || value.playlist.id !== query.playlist
        || value.playlist.title !== selected.title || typeof value.revision !== 'string' || !value.revision
        || value.revision.length > 4096 || /[\p{Cc}]/u.test(value.revision)
        || (query.expected_revision && value.revision !== query.expected_revision)
        || !uint(value.total) || value.offset !== query.offset || !Array.isArray(value.items)
        || value.items.length > query.limit || value.offset + value.items.length > value.total) fail();
    const next = value.offset + value.items.length;
    if ((value.next_offset ?? null) !== (next < value.total ? next : null)
        || (!value.items.length && next < value.total)) fail();
    const items = value.items.map((item, index) => {
      if (!item || item.position !== value.offset + index + 1 || typeof item.title !== 'string'
          || ['creator', 'album', 'uri', 'art_uri'].some(key => item[key] != null && typeof item[key] !== 'string')
          || (item.duration_secs != null && !uint(item.duration_secs))) fail();
      return Object.freeze({ ...item });
    });
    return Object.freeze({ ...value, playlist: Object.freeze({ ...value.playlist }),
      next_offset: value.next_offset ?? null, items: Object.freeze(items) });
  }

  function contentsSummary(page) {
    if (!page) return 'Read the saved tracks without changing playback.';
    if (!page.total) return 'This saved Sonos playlist is empty.';
    return `Tracks ${page.offset + 1}–${page.offset + page.items.length} of ${page.total} · ${page.playlist.title || page.playlist.id}`;
  }

  function preparePlaylistTrack(page, position, mode) {
    if (!Object.hasOwn(modes, mode)) throw new Error('Choose Play now, Play next or Add to queue.');
    const track = page?.items.find(item => item.position === position);
    if (!track || !Number.isInteger(position) || position < 1 || position > 100000
        || !text(track.uri) || track.uri.trim() === 'NOT_IMPLEMENTED') {
      throw new Error('This saved occurrence has no playable resource. Read the tracks again or choose another track.');
    }
    return Object.freeze({ item: Object.freeze({ kind: 'sonos_playlist_track', container: false,
      id: page.playlist.id, playlistTitle: page.playlist.title, position, revision: page.revision,
      title: track.title, uri: track.uri }), title: track.title || 'Untitled track', mode });
  }

  function trackQuery(item, target) {
    return { zone: target, playlist: item.id, offset: item.position - 1, limit: 1, expected_revision: item.revision };
  }

  function verifyPlaylistTrack(value, item, group) {
    const page = playlistContents(value, trackQuery(item, group.target), { title: item.playlistTitle }, group);
    const track = page.items[0];
    if (page.items.length !== 1 || track.position !== item.position || track.uri !== item.uri || track.title !== item.title) {
      throw new Error('The selected saved track changed. Read tracks and choose it again.');
    }
  }

  function trackOutcome(value, reviewed) {
    const item = reviewed.item, mode = reviewed.mode === 'end' ? 'append' : reviewed.mode;
    if (!value || value.zone !== reviewed.group.coordinator || value.playlist !== item.id
        || value.position !== item.position || value.mode !== mode || value.title !== item.title
        || (mode !== 'play' && value.count !== 1)) {
      throw new Error('The saved track command was not confirmed for the reviewed occurrence.');
    }
    const detail = outcome(value);
    return mode === 'play' ? detail : [queued(value, reviewed.group), ...(value.notes || []).map(note => note.detail)].join(' ');
  }

  function preparePlaylistTracks(page, positions, mode) {
    if (!Array.isArray(positions) || !positions.length || positions.length > MAX_PLAYLIST_TRACKS) {
      throw new Error(`Choose 1 to ${MAX_PLAYLIST_TRACKS} saved track occurrences from this page.`);
    }
    const tracks = positions.map(position => preparePlaylistTrack(page, position, mode).item);
    return Object.freeze({ item: Object.freeze({ kind: 'sonos_playlist_tracks', container: true,
      id: page.playlist.id, playlistTitle: page.playlist.title, revision: page.revision,
      total: page.total, positions: Object.freeze([...positions]), tracks: Object.freeze(tracks) }),
      title: `${tracks.length} selected track(s) from ${page.playlist.title || page.playlist.id}`, mode });
  }

  function tracksQuery(item, target) {
    const first = Math.min(...item.positions), last = Math.max(...item.positions);
    return { zone: target, playlist: item.id, offset: first - 1, limit: last - first + 1,
      expected_revision: item.revision };
  }

  function verifyPlaylistTracks(value, item, group) {
    const page = playlistContents(value, tracksQuery(item, group.target), { title: item.playlistTitle }, group);
    if (page.total !== item.total || item.tracks.some(selected => {
      const track = page.items.find(track => track.position === selected.position);
      return !track || track.uri !== selected.uri || track.title !== selected.title;
    })) throw new Error('The selected saved tracks changed. Read tracks and choose them again.');
  }

  function tracksOutcome(value, reviewed) {
    const item = reviewed.item, mode = reviewed.mode === 'end' ? 'append' : reviewed.mode;
    if (!value || value.zone !== reviewed.group.coordinator || value.playlist !== item.id || value.mode !== mode
        || !Array.isArray(value.positions) || JSON.stringify(value.positions) !== JSON.stringify(item.positions)
        || !Array.isArray(value.titles) || JSON.stringify(value.titles) !== JSON.stringify(item.tracks.map(track => track.title))
        || (mode !== 'play' && value.count !== item.positions.length)) {
      throw new Error('The saved track selection was not confirmed in its reviewed order.');
    }
    const detail = outcome(value);
    return mode === 'play' ? detail : [queued(value, reviewed.group), ...(value.notes || []).map(note => note.detail)].join(' ');
  }

  function renderContents(list, page, doc, { review, select, edit, selected = [], disabled = false } = {}) {
    const focused = list.contains?.(doc.activeElement) ? doc.activeElement?.getAttribute('data-playlist-select') : null;
    const scroll = list.scrollTop;
    let focus;
    list.replaceChildren();
    for (const item of page?.items || []) {
      const row = doc.createElement('li'); row.value = item.position;
      const title = doc.createElement('strong'); title.textContent = item.title || 'Untitled track'; row.append(title);
      const detail = [item.creator, item.album].filter(Boolean);
      if (item.duration_secs != null) detail.push(Math.floor(item.duration_secs / 60) + ':' + String(item.duration_secs % 60).padStart(2, '0'));
      if (detail.length) { const text = doc.createElement('span'); text.textContent = detail.join(' · '); row.append(text); }
      if (select) {
        const button = doc.createElement('button'), index = selected.indexOf(item.position);
        button.type = 'button'; button.className = 'source-track-select';
        button.textContent = index < 0 ? 'Select track' : `Selected · ${index + 1} in order`;
        button.setAttribute('aria-pressed', String(index >= 0));
        button.setAttribute('data-playlist-select', String(item.position));
        button.setAttribute('aria-label', `${index < 0 ? 'Select' : 'Remove selection for'} track ${item.position}, ${item.title || 'Untitled track'}`);
        button.disabled = disabled || !text(item.uri) || item.uri.trim() === 'NOT_IMPLEMENTED'
          || (index < 0 && selected.length >= MAX_PLAYLIST_TRACKS);
        button.addEventListener('click', () => { if (!button.disabled) select(item.position, page); });
        if (String(item.position) === focused) focus = button;
        row.append(button);
      }
      if (review) {
        const actions = doc.createElement('div'); actions.className = 'source-track-actions';
        for (const [mode, label] of [['play', 'Play now'], ['next', 'Play next'], ['end', 'Add to queue']]) {
          const button = doc.createElement('button'); button.type = 'button'; button.textContent = label;
          button.disabled = disabled || !text(item.uri) || item.uri.trim() === 'NOT_IMPLEMENTED';
          button.setAttribute('aria-label', `${label}: track ${item.position}, ${item.title || 'Untitled track'}`);
          button.addEventListener('click', () => { if (!button.disabled) void review(item.position, mode, page); });
          actions.append(button);
        }
        row.append(actions);
      }
      if (edit) {
        const button = doc.createElement('button'); button.type = 'button'; button.textContent = 'Edit saved track';
        button.disabled = disabled;
        button.setAttribute('aria-label', `Edit saved track ${item.position}, ${item.title || 'Untitled track'}`);
        button.addEventListener('click', () => { if (!button.disabled) edit(item.position, page); });
        row.append(button);
      }
      list.append(row);
    }
    if (scroll !== undefined) list.scrollTop = scroll;
    focus?.focus?.({ preventScroll: true });
  }

  function renderSelectedTracks(list, item, doc) {
    list.replaceChildren();
    for (const track of item?.tracks || []) {
      const row = doc.createElement('li');
      row.textContent = `Track ${track.position}: ${track.title || 'Untitled track'}`;
      list.append(row);
    }
  }

  function playlistTrackSelectionPanel(doc, { review, clear }) {
    const node = doc.createElement('section'), help = doc.createElement('p'), list = doc.createElement('ol');
    node.className = 'source-track-selection'; node.setAttribute('aria-label', 'Selected saved tracks');
    help.className = 'source-help'; help.setAttribute('role', 'status'); help.setAttribute('aria-live', 'polite');
    list.className = 'source-selected-tracks'; list.setAttribute('aria-label', 'Selected tracks in playback order');
    const actions = doc.createElement('div'); actions.className = 'source-track-actions';
    let current = null, shown = null, shownPage = null;
    const buttons = [['play', 'Play selected now'], ['next', 'Play selected next'], ['end', 'Add selected to queue']]
      .map(([mode, label]) => {
        const button = doc.createElement('button'); button.type = 'button'; button.textContent = label;
        button.addEventListener('click', () => { if (!button.disabled) void review(mode, current); });
        actions.append(button); return button;
      });
    const reset = doc.createElement('button'); reset.type = 'button'; reset.textContent = 'Clear selection';
    reset.addEventListener('click', () => { if (!reset.disabled) clear(); }); actions.append(reset);
    node.append(help, list, actions);
    return { node, render(page, positions, disabled) {
      current = page; node.hidden = !page;
      help.textContent = positions.length ? `${positions.length} of ${MAX_PLAYLIST_TRACKS} selected, in the order shown. Clear the selection to review the whole playlist.`
        : `Select up to ${MAX_PLAYLIST_TRACKS} tracks from this page in the order you want. Repeated saved occurrences remain separate. Changing pages clears the selection.`;
      list.hidden = !positions.length; actions.hidden = !positions.length;
      for (const button of [...buttons, reset]) button.disabled = disabled || !positions.length;
      if (shown !== positions || shownPage !== page) {
        shown = positions; shownPage = page;
        renderSelectedTracks(list, { tracks: positions.map(position => page?.items.find(track => track.position === position)).filter(Boolean) }, doc);
      }
    } };
  }

  function commandBody(choice, target) {
    if (choice.item.kind === 'line_in') {
      if (choice.mode !== 'play') throw new Error('Line-in cannot be added to the queue.');
      return { zone: target, source: choice.item.player };
    }
    if (choice.item.kind === 'sonos_playlist_track') return { zone: target, playlist: choice.item.id,
      position: choice.item.position, expected_revision: choice.item.revision, mode: choice.mode === 'end' ? 'append' : choice.mode };
    if (choice.item.kind === 'sonos_playlist_tracks') return { zone: target, playlist: choice.item.id,
      positions: choice.item.positions, expected_revision: choice.item.revision, mode: choice.mode === 'end' ? 'append' : choice.mode };
    return { zone: target, ...(choice.item.kind === 'sonos_playlist'
      ? { playlist: choice.item.id }
      : { source_uri: choice.item.uri, ...(choice.title ? { title: choice.title } : {}) }),
      ...(choice.mode === 'play' ? {} : { next: choice.mode === 'next' }) };
  }

  function nativePlaylistTitle(raw) {
    if (typeof raw !== 'string') throw new Error('Enter a new playlist title.');
    const title = raw.replace(/^\p{White_Space}+|\p{White_Space}+$/gu, '');
    if (!title || /[\u0000-\u001f\u007f-\u009f\uD800-\uDFFF]/u.test(title)
        || new TextEncoder().encode(title).length > 1023) {
      throw new Error('Use a nonblank playlist title of at most 1023 UTF-8 bytes without control characters.');
    }
    return title;
  }

  const editsTracks = operation => ['remove_tracks', 'move_tracks'].includes(operation);
  function playlistTrackEdit(operation, fields, total = 1000) {
    const integer = (value, max) => /^\d+$/.test(String(value)) && Number.isSafeInteger(Number(value))
      && Number(value) >= 1 && Number(value) <= max;
    if (!integer(fields.position, 1000) || !integer(fields.count, 1000)
        || total > 1000 || Number(fields.position) + Number(fields.count) - 1 > total
        || (operation === 'move_tracks' && !integer(fields.insert_before, total + 1))) {
      throw new Error('Choose a valid saved-track range in a playlist of at most 1,000 tracks. Move before an existing position, or use the track count plus one for the end.');
    }
    return Object.freeze({ position: Number(fields.position), count: Number(fields.count),
      ...(operation === 'move_tracks' ? { insert_before: Number(fields.insert_before) } : {}) });
  }
  const editChanges = reviewed => reviewed.operation === 'remove_tracks'
    || reviewed.edit.insert_before < reviewed.edit.position
    || reviewed.edit.insert_before > reviewed.edit.position + reviewed.edit.count;
  const editSignature = observed => JSON.stringify([observed.tracks, observed.anchor]);

  function managedPlaylist(value, reviewed) {
    const item = playlists([value?.playlist])[0];
    const trackEdit = editsTracks(reviewed.operation);
    if (!value || value.zone !== reviewed.group.coordinator || item.id !== reviewed.item.id
        || value.operation !== reviewed.operation || typeof value.changed !== 'boolean' || typeof value.logged !== 'boolean'
        || !text(value.done) || item.title !== (reviewed.operation === 'rename' ? reviewed.title : reviewed.item.title)
        || value.changed !== (trackEdit ? editChanges(reviewed) : reviewed.operation === 'remove' || reviewed.title !== reviewed.item.title)
        || (trackEdit && (value.position !== reviewed.edit.position || value.count !== reviewed.edit.count
          || (value.insert_before ?? null) !== (reviewed.edit.insert_before ?? null)
          || value.previous_total !== reviewed.total
          || value.total !== reviewed.total - (reviewed.operation === 'remove_tracks' ? reviewed.edit.count : 0)
          || !text(value.revision) || value.revision.length > 4096 || /[\p{Cc}]/u.test(value.revision)
          || (value.changed ? value.revision === reviewed.revision : value.revision !== reviewed.revision)))) {
      throw new Error('The saved-playlist command response was incomplete.');
    }
    return Object.freeze({ ...value, playlist: item, detail: value.done + (value.logged ? ''
      : ' The catalog result was confirmed, but the action log could not be written. Do not repeat this operation.') });
  }

  // One catalog-write workflow for both remotes. Its receipt survives source,
  // room and dialog changes; only an explicit original-household read resolves
  // an uncertain dispatch. Playback refreshes never acknowledge catalog edits.
  function playlistManager({ call, selection, available, live, changed = () => {}, completed = () => {},
    commit = (_, work) => work({ check() {}, send: (route, body) => call(route, { body }) }),
    readCatalog = (target, decode) => call('sourcePlaylists', { query: { zone: target } }).then(decode) }) {
    let epoch = 0;
    const state = { title: '', position: '1', count: '1', insert_before: '1', basis: null,
      pending: null, reading: false, writing: false, result: null, recovery: null, message: '' };
    const publish = () => { try { changed(state); } catch (_) { /* Presentation only. */ } };
    const busy = () => state.reading || state.writing;
    const same = reviewed => {
      const current = selection();
      return current && current.group.target === reviewed.group.target && current.group.signature === reviewed.group.signature
        && current.item.id === reviewed.item.id && current.item.title === reviewed.item.title;
    };
    const invalidate = () => { ++epoch; state.pending = null; state.reading = false; state.basis = null; };
    async function inspect(reviewed, valid, revision) {
      valid();
      const fresh = groups(await call('zones')); valid();
      if (!fresh.some(group => group.target === reviewed.group.target && group.signature === reviewed.group.signature)) {
        throw new Error('The playlist’s original group changed. Read its current catalog and review again.');
      }
      const query = { zone: reviewed.group.target, playlist: reviewed.item.id, offset: 0, limit: 1,
        ...(revision ? { expected_revision: revision } : {}) };
      const value = await call('sourcePlaylistContents', { query }); valid();
      const page = playlistContents(value, query, reviewed.item, reviewed.group);
      if (revision && page.revision !== revision) throw new Error('The saved playlist changed. Read it and review again.');
      return page;
    }
    async function inspectTracks(reviewed, valid) {
      const read = async (offset, limit) => {
        valid();
        const query = { zone: reviewed.group.target, playlist: reviewed.item.id, offset, limit,
          expected_revision: reviewed.revision };
        const page = playlistContents(await call('sourcePlaylistContents', { query }), query, reviewed.item, reviewed.group);
        valid();
        if (page.total !== reviewed.total || !page.items.length) throw new Error('The saved track range changed. Read tracks again.');
        return page;
      };
      const tracks = [], end = reviewed.edit.position + reviewed.edit.count - 1;
      let offset = reviewed.edit.position - 1;
      while (offset < end) {
        const page = await read(offset, Math.min(100, end - offset));
        tracks.push(...page.items); offset += page.items.length;
      }
      let anchor = null;
      if (reviewed.operation === 'move_tracks' && reviewed.edit.insert_before <= reviewed.total) {
        anchor = tracks.find(item => item.position === reviewed.edit.insert_before)
          || (await read(reviewed.edit.insert_before - 1, 1)).items[0];
      }
      return Object.freeze({ tracks: Object.freeze(tracks), anchor });
    }
    async function review(operation) {
      if (!live() || !available() || busy() || state.recovery || !['rename', 'remove', 'remove_tracks', 'move_tracks'].includes(operation)) return null;
      const selected = selection();
      if (!selected) return null;
      const basis = state.basis;
      invalidate(); const version = epoch;
      let title, edit;
      try {
        title = operation === 'rename' ? nativePlaylistTitle(state.title) : selected.item.title;
        if (editsTracks(operation)) edit = playlistTrackEdit(operation, state);
      }
      catch (error) { state.message = problem(error); publish(); return null; }
      const reviewed = { ...selected, operation, title };
      const valid = () => { if (!live() || version !== epoch || !same(reviewed)) throw new Error('This playlist review is no longer current. Review it again.'); };
      state.reading = true; state.message = 'Reading the selected playlist in its original household…'; publish();
      try {
        const page = await inspect(reviewed, valid, edit ? basis?.revision : undefined); valid();
        const observed = { ...reviewed, revision: page.revision, total: page.total };
        let tracks;
        if (edit) {
          observed.edit = playlistTrackEdit(operation, edit, page.total);
          tracks = await inspectTracks(observed, valid); valid();
        }
        state.pending = Object.freeze({ ...observed, ...(tracks || {}),
          body: Object.freeze({ zone: reviewed.group.target, playlist: reviewed.item.id, expected_revision: page.revision,
            ...(operation === 'rename' ? { title } : edit || {}) }) });
        state.message = ''; return state.pending;
      } catch (error) { if (version === epoch) state.message = problem(error); return null; }
      finally { if (version === epoch) { state.reading = false; publish(); } }
    }
    async function confirm(reviewed = state.pending) {
      if (!live() || !available() || busy() || state.recovery || !reviewed || reviewed !== state.pending || !same(reviewed)) return false;
      const version = ++epoch;
      state.pending = null; state.writing = true; state.message = 'Checking the reviewed playlist again…'; publish();
      let sent = false, receipt = null;
      try {
        await commit(reviewed, async ({ check, send }) => {
          const valid = () => { check(); if (!live() || version !== epoch || !same(reviewed)) throw new Error('This playlist review closed or changed before sending.'); };
          const page = await inspect(reviewed, valid, reviewed.revision); valid();
          if (page.total !== reviewed.total) throw new Error('The saved playlist changed. Review it again.');
          if (editsTracks(reviewed.operation) && editSignature(await inspectTracks(reviewed, valid)) !== editSignature(reviewed)) {
            throw new Error('The reviewed saved tracks changed. Read tracks and review again.');
          }
          valid();
          if (sent) throw new Error('The playlist command was already dispatched.');
          sent = true;
          const route = { rename: 'sourcePlaylistRename', remove: 'sourcePlaylistRemove',
            remove_tracks: 'sourcePlaylistRemoveTracks', move_tracks: 'sourcePlaylistMoveTracks' }[reviewed.operation];
          receipt = managedPlaylist(await send(route, reviewed.body), reviewed);
          return receipt;
        });
        if (!receipt) throw new Error('The playlist command was not sent. Review it again.');
        state.result = Object.freeze({ status: 'accepted', group: reviewed.group, item: reviewed.item,
          operation: reviewed.operation, playlist: receipt.playlist, detail: receipt.detail });
        state.message = '';
        try { completed(state.result); } catch (_) { /* A confirmed native result stays successful. */ }
        return true;
      } catch (error) {
        state.result = Object.freeze({ status: sent ? 'uncertain' : 'not_sent', group: reviewed.group, item: reviewed.item,
          operation: reviewed.operation, detail: problem(error) + (sent
            ? ' The household catalog may have changed. Read the original household catalog before another edit; nothing was retried.'
            : ' No catalog edit was sent.') });
        if (sent) state.recovery = state.result;
        state.message = ''; return false;
      } finally { state.writing = false; publish(); }
    }
    async function recover() {
      const result = state.recovery;
      if (!live() || busy() || !result) return null;
      const version = ++epoch; state.pending = null; state.reading = true;
      state.message = 'Reading saved playlists for ' + result.group.target + '…'; publish();
      const valid = () => { if (!live() || version !== epoch || state.recovery !== result) throw new Error('Read the original household catalog again.'); };
      try {
        const catalog = await readCatalog(result.group.target, value => { valid(); return playlists(value); }); valid();
        if (catalog === null) throw new Error('The original household catalog was not refreshed.');
        const found = catalog.find(item => item.id === result.item.id);
        state.recovery = null;
        state.result = Object.freeze({ ...result, status: 'observed', detail: result.detail + ' Current observation in '
          + result.group.target + ': ' + (found ? `${found.title} (${found.id}) is present.` : `${result.item.id} is absent.`)
          + ' This observation does not prove how the earlier request finished. Review any new edit separately.' });
        state.message = ''; return catalog;
      } catch (error) { if (version === epoch) state.message = problem(error); return null; }
      finally { if (version === epoch) { state.reading = false; publish(); } }
    }
    return { state, busy, review, confirm, recover, invalidate,
      catalogChanged(change = null) {
        if (change?.playlists === false) return;
        const group = state.recovery?.group || state.pending?.group || selection()?.group;
        if (!group) return;
        if (change?.household && group?.household !== change.household) return;
        invalidate(); state.message = 'The saved catalog changed. Read its current contents before another review.'; publish();
      },
      canReview: () => !!(live() && available() && !busy() && !state.recovery && selection()),
      editTitle(value) {
        if (!live() || !available() || busy() || typeof value !== 'string' || value.length > 8192) return false;
        invalidate(); state.title = value; state.message = ''; publish(); return true;
      },
      editTracks(field, value) {
        if (!live() || !available() || busy() || !['position', 'count', 'insert_before'].includes(field)
            || typeof value !== 'string' || value.length > 10) return false;
        const basis = state.basis; invalidate(); state.basis = basis;
        state[field] = value; state.message = ''; publish(); return true;
      },
      chooseTrack(position, page) {
        const current = selection();
        if (!live() || !available() || busy() || state.recovery || !current || !page
            || page.playlist.id !== current.item.id || page.playlist.title !== current.item.title
            || page.zone !== current.group.coordinator || !page.items.some(item => item.position === position)) return false;
        invalidate(); state.position = String(position); state.count = '1'; state.insert_before = String(position);
        state.basis = Object.freeze({ revision: page.revision });
        state.message = `Saved track ${position} selected. Choose removal, or set its new position and review the move.`;
        publish(); return true;
      },
      cancel() { if (!state.writing) { invalidate(); state.message = ''; publish(); } },
    };
  }

  function playlistManagerPanel(manager, doc) {
    const el = (tag, content, className) => {
      const node = doc.createElement(tag); if (content) node.textContent = content; if (className) node.className = className; return node;
    };
    const node = el('section', null, 'source-playlist-manage');
    node.setAttribute('aria-label', 'Manage saved Sonos playlist');
    const fields = el('div'), title = el('input'), label = el('label', 'New playlist title ');
    title.type = 'text'; title.maxLength = 1023; title.autocomplete = 'off'; title.setAttribute('aria-label', 'New playlist title'); label.append(title);
    const rename = el('button', 'Review rename'), remove = el('button', 'Review deletion');
    const actions = el('div', null, 'row'); actions.append(rename, remove);
    fields.append(el('h3', 'Manage saved playlist'), label, actions);
    const trackFields = el('div', null, 'source-playlist-track-edit'), inputs = {};
    trackFields.append(el('h4', 'Edit saved tracks'), el('p', 'Use the numbered saved tracks above, or choose Edit saved track beside an occurrence. A range keeps its order and includes repeated tracks.'));
    for (const [key, caption] of [['position', 'First saved track'], ['count', 'Number of tracks'], ['insert_before', 'Move before track']]) {
      const input = el('input'), label = el('label', caption + ' ');
      input.type = 'number'; input.min = '1'; input.max = key === 'insert_before' ? '1001' : '1000'; input.step = '1';
      input.setAttribute('aria-label', caption); label.append(input); trackFields.append(label); inputs[key] = input;
      input.addEventListener('input', () => manager.editTracks(key, input.value));
    }
    trackFields.append(el('p', 'Move before uses the original track numbers. To move to the end, enter the playlist’s track count plus one. Playlists up to 1,000 tracks can be edited here.'));
    const removeTracks = el('button', 'Review track removal'), moveTracks = el('button', 'Review track move');
    const trackActions = el('div', null, 'row'); trackActions.append(removeTracks, moveTracks); trackFields.append(trackActions);
    fields.append(trackFields);
    const message = el('p'), consent = el('div', null, 'source-review'), result = el('p'), recovery = el('button', 'Read original household catalog');
    for (const item of [message, result]) { item.setAttribute('role', 'status'); item.setAttribute('aria-live', 'polite'); }
    for (const button of [rename, remove, removeTracks, moveTracks, recovery]) button.type = 'button';
    node.append(fields, message, consent, result, recovery);
    title.addEventListener('input', () => manager.editTitle(title.value));
    rename.addEventListener('click', () => { void manager.review('rename'); });
    remove.addEventListener('click', () => { void manager.review('remove'); });
    removeTracks.addEventListener('click', () => { void manager.review('remove_tracks'); });
    moveTracks.addEventListener('click', () => { void manager.review('move_tracks'); });
    recovery.addEventListener('click', () => { void manager.recover(); });
    let shown = null, confirm = null;
    return { node, render(selected) {
      const s = manager.state;
      node.hidden = !selected && !s.result && !s.message && !s.pending;
      fields.hidden = !selected;
      if (title.value !== s.title) title.value = s.title;
      title.disabled = !manager.canReview(); rename.disabled = !manager.canReview() || !s.title.trim(); remove.disabled = !manager.canReview();
      for (const [key, input] of Object.entries(inputs)) {
        if (input.value !== s[key]) input.value = s[key];
        input.disabled = !manager.canReview();
      }
      removeTracks.disabled = moveTracks.disabled = !manager.canReview();
      message.textContent = s.message; result.textContent = s.result ? `${s.result.group.target}: ${s.result.detail}` : '';
      recovery.hidden = !s.recovery; recovery.disabled = manager.busy();
      consent.hidden = !s.pending;
      if (shown !== s.pending) {
        shown = s.pending; consent.replaceChildren(); confirm = null;
        if (shown) {
          const pending = shown;
          if (editsTracks(pending.operation)) {
            const edit = pending.edit, removing = pending.operation === 'remove_tracks', list = el('ol', null, 'source-playlist-edit-preview');
            consent.append(el('p', `${removing ? 'Remove' : 'Move'} ${edit.count} saved track(s) in “${pending.item.title}” (${pending.item.id}) in ${pending.group.label}?`));
            for (const track of pending.tracks) {
              const row = el('li'); row.value = track.position;
              row.textContent = `Track ${track.position}: ${track.title || 'Untitled track'}${track.creator ? ' · ' + track.creator : ''}`;
              list.append(row);
            }
            consent.append(list, el('p', removing
              ? `${pending.total - edit.count} of ${pending.total} tracks will remain. This removes only these saved occurrences; other copies stay in the playlist.`
              : !editChanges(pending) ? 'The range is already in that position. Confirming checks the saved playlist without moving any track.'
                : pending.anchor ? `Move this range before original track ${pending.anchor.position}: ${pending.anchor.title || 'Untitled track'}.`
                  : `Move this range to the end, after the other ${pending.total - edit.count} tracks.`),
            el('p', 'The saved playlist changes for future playback and scene recalls. Current playback, loaded queues and the DJ continue unchanged. This edit cannot be undone here.'));
            confirm = el('button', removing ? 'Remove saved tracks' : 'Move saved tracks');
          } else {
            consent.append(el('p', `${pending.operation === 'remove' ? 'Delete' : 'Rename'} “${pending.item.title}” (${pending.item.id}) in ${pending.group.label}?`),
            el('p', pending.operation === 'remove'
              ? `Permanently remove this saved playlist of ${pending.total} track(s) from this household. This cannot be undone here. Current playback and queues stay unchanged.`
              : `New title: “${pending.title}”. The ${pending.total} saved track(s), current playback and queues stay unchanged.`),
            el('p', pending.operation === 'remove'
              ? 'Saved scenes and routines using this playlist will refuse playback until you update them.'
              : 'Saved scenes using this playlist must be saved again with its new title before those scenes or their routines can play it.'));
          confirm = el('button', pending.operation === 'remove' ? 'Delete saved playlist' : 'Rename saved playlist');
          }
          const back = el('button', 'Cancel catalog edit'); confirm.type = back.type = 'button';
          confirm.addEventListener('click', () => { void manager.confirm(pending); }); back.addEventListener('click', () => manager.cancel());
          consent.append(confirm, back);
        }
      }
      if (confirm) confirm.disabled = manager.busy();
    } };
  }

  function model({ call, changed = () => {}, execute = (_, work) => work(), executeSet = (_, work) => work() }) {
    let epoch = 0, disposed = false, setReader = null;
    const state = { open: false, groups: [], target: '', sourceType: 'link', input: '', title: '', mode: 'play', pending: null,
      targetMode: 'group', targetQuery: '', targetSuggestions: [], nativeQuery: '',
      playlists: [], playlist: '', playlistTarget: '', playlistStatus: 'idle', contents: null, contentsHistory: [], contentsSelection: Object.freeze([]),
      lineInSources: [], lineInUnavailable: [], lineIn: '', lineInTarget: '', lineInStatus: 'idle',
      reading: false, writing: false, message: '', result: null };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* Presentation only. */ } } };
    const catalog = catalogModel({
      call: (query, options) => call('sourceSpotifySearch', { query, signal: options.signal }),
      available: () => !disposed && state.open && state.sourceType === 'catalog' && !state.reading && !state.writing,
      changed: publish,
    });
    state.catalog = catalog.state;
    const selected = () => state.groups.find((group) => group.target === state.target);
    const collective = () => state.targetMode === 'set' || state.sourceType === 'favorite';
    const manager = playlistManager({ call,
      selection: () => {
        const group = selected(), item = state.playlists.find(item => item.id === state.playlist);
        return !collective() && state.sourceType === 'playlist' && state.playlistStatus === 'ready'
          && group && item && state.playlistTarget === group.target ? { group, item } : null;
      },
      available: () => !state.reading && !state.writing && !state.pending,
      live: () => !disposed && state.open, changed: publish,
      commit: (reviewed, work) => execute(reviewed.group, () => work({ check() {}, send: (route, body) => call(route, { body }) })),
      completed: () => { clearPlaylists(); publish(); },
    });
    state.managing = manager.state;
    const blocked = () => state.writing || manager.state.writing;
    const current = (version) => !disposed && state.open && epoch === version;
    const invalidate = (manage = true) => {
      ++epoch; setReader?.abort(); setReader = null; state.pending = null; state.reading = false;
      if (manage) manager.invalidate();
      catalog.retire();
      if (state.playlistStatus === 'loading') state.playlistStatus = 'idle';
      if (state.lineInStatus === 'loading') state.lineInStatus = 'idle';
    };
    const clearContents = () => { state.contents = null; state.contentsHistory = []; state.contentsSelection = Object.freeze([]); };
    const clearPlaylists = () => { state.playlists = []; state.playlist = ''; state.playlistTarget = ''; state.playlistStatus = 'idle'; clearContents(); };
    const clearLineIn = () => { state.lineInSources = []; state.lineInUnavailable = []; state.lineIn = ''; state.lineInTarget = ''; state.lineInStatus = 'idle'; };

    async function refresh() {
      if (disposed) return false;
      state.open = true;
      if (blocked()) { publish(); return false; }
      const prior = selected();
      invalidate(); clearPlaylists(); clearLineIn(); const version = epoch;
      state.reading = true; state.message = ''; publish();
      try {
        const fresh = groups(await call('zones'));
        if (!current(version)) return false;
        state.groups = fresh;
        if (!prior || !fresh.some((group) => group.signature === prior.signature)) state.target = '';
        if (!fresh.length) state.message = 'No groups discovered. Refresh after the speakers are available.';
        return true;
      } catch (error) {
        if (current(version)) { state.groups = []; state.target = ''; state.message = problem(error); }
        return false;
      } finally { if (current(version)) { state.reading = false; publish(); } }
    }

    function edit(field, value) {
      if (disposed || blocked() || !['target', 'targetMode', 'targetQuery', 'nativeQuery', 'sourceType', 'playlist', 'lineIn', 'input', 'title', 'mode'].includes(field) || typeof value !== 'string') return false;
      if (field === 'target' && !state.groups.some((group) => group.target === value)) return false;
      if (field === 'targetMode' && (!['group', 'set'].includes(value) || value === 'set' && state.sourceType === 'line_in')) return false;
      if (field === 'mode' && (!Object.hasOwn(modes, value) || state.sourceType === 'line_in' && value !== 'play')) return false;
      if (field === 'sourceType' && (!['link', 'playlist', 'favorite', 'catalog', 'line_in'].includes(value) || value === 'line_in' && state.targetMode === 'set')) return false;
      if (field === 'playlist' && (state.playlistTarget !== state.target || !state.playlists.some(item => item.id === value))) return false;
      if (field === 'lineIn' && (state.lineInStatus !== 'ready' || state.lineInTarget !== state.target
          || !state.lineInSources.some(item => item.player === value))) return false;
      invalidate();
      if (['target', 'targetMode', 'targetQuery', 'sourceType'].includes(field)) { clearPlaylists(); clearLineIn(); }
      if (field === 'sourceType' && value === 'line_in') state.mode = 'play';
      if (field === 'playlist') clearContents();
      state[field] = value; state.message = ''; publish(); return true;
    }

    async function loadTargetSuggestions() {
      if (disposed || !state.open || blocked() || state.reading) return false;
      invalidate(); const version = epoch, abort = new AbortController(); setReader = abort;
      state.reading = true; state.message = ''; publish();
      try {
        const choices = targets().suggestions(await call('sourceTargetRooms', { signal: abort.signal }));
        if (!current(version)) return false;
        state.targetSuggestions = choices; return true;
      } catch (error) { if (current(version)) state.message = problem(error); return false; }
      finally { if (current(version)) { setReader = null; state.reading = false; publish(); } }
    }

    function chooseCatalog(item, items) {
      const incoming = catalog.select(item, items);
      if (!incoming) return false;
      invalidate(); clearPlaylists(); clearLineIn(); state.sourceType = 'link';
      state.input = incoming.input; state.title = incoming.title;
      state.message = 'Spotify source selected. Review the destination and action before sending it.';
      publish(); return true;
    }

    async function loadPlaylists() {
      const group = selected();
      if (disposed || !state.open || blocked() || collective() || state.sourceType !== 'playlist' || !group) return false;
      invalidate(); clearPlaylists(); const version = epoch;
      state.reading = true; state.playlistStatus = 'loading'; state.message = 'Reading saved Sonos playlists…'; publish();
      try {
        const catalog = playlists(await call('sourcePlaylists', { query: { zone: group.target } }));
        if (!current(version)) return false;
        state.playlists = catalog; state.playlistTarget = group.target; state.playlistStatus = 'ready'; state.message = '';
        return true;
      } catch (error) {
        if (current(version)) { state.playlistStatus = 'error'; state.message = problem(error); }
        return false;
      } finally { if (current(version)) { state.reading = false; publish(); } }
    }

    async function loadLineInSources() {
      const group = selected();
      if (disposed || !state.open || blocked() || state.reading || state.sourceType !== 'line_in' || !group) return false;
      invalidate(); clearLineIn(); const version = epoch;
      state.reading = true; state.lineInStatus = 'loading'; state.message = 'Reading line-in sources…'; publish();
      try {
        const catalog = lineInSources(await call('sourceLineInSources', { query: { zone: group.target } }));
        if (!current(version)) return false;
        state.lineInSources = catalog.sources; state.lineInUnavailable = catalog.unavailable;
        state.lineInTarget = group.target; state.lineInStatus = 'ready'; state.message = ''; return true;
      } catch (error) {
        if (current(version)) { state.lineInStatus = 'error'; state.message = problem(error); }
        return false;
      } finally { if (current(version)) { state.reading = false; publish(); } }
    }

    async function loadContents(direction = 'refresh') {
      const group = selected(), item = state.playlists.find(item => item.id === state.playlist);
      if (disposed || !state.open || blocked() || state.reading || collective() || state.sourceType !== 'playlist'
          || !group || !item || state.playlistStatus !== 'ready' || state.playlistTarget !== group.target) return false;
      const request = contentsRequest(state.contents, state.contentsHistory, direction, item.id);
      if (!request) return false;
      invalidate(); state.contentsSelection = Object.freeze([]); const version = epoch;
      state.reading = true; state.message = 'Reading saved tracks…'; publish();
      try {
        const value = await call('sourcePlaylistContents', { query: { zone: group.target, ...request.query } });
        if (!current(version)) return false;
        state.contents = playlistContents(value, request.query, item, group); state.contentsHistory = request.history;
        state.message = ''; return true;
      } catch (error) {
        if (current(version)) { clearContents(); state.message = problem(error); }
        return false;
      } finally { if (current(version)) { state.reading = false; publish(); } }
    }

    async function offer(value) {
      if (disposed) return false;
      let incoming;
      try { incoming = offeredSource(value); }
      catch (error) { state.message = problem(error); publish(); return false; }
      state.open = true;
      if (blocked() || state.reading || manager.busy() || state.pending || state.input.trim() || state.title.trim() || state.nativeQuery.trim() || state.playlist || state.lineIn) {
        state.message = 'Finish the current source action, or explicitly clear its draft before importing another source.';
        publish(); return false;
      }
      invalidate(); state.input = incoming.input; state.title = incoming.title;
      state.target = ''; state.targetMode = 'group'; state.targetQuery = ''; state.nativeQuery = ''; state.mode = 'play'; state.sourceType = 'link'; clearPlaylists(); clearLineIn();
      // Refresh only the selectable destinations. Review and confirmation remain
      // separate user actions, and share the existing write coordination lane.
      return refresh();
    }

    async function browseSaved(value) {
      if (disposed) return false;
      let incoming;
      try {
        if (!value || typeof value !== 'object' || Array.isArray(value)
            || Object.keys(value).some(key => !['target', 'signature', 'playlist'].includes(key))
            || !text(value.target) || !text(value.signature)) {
          throw new Error('The saved-playlist handoff is incomplete.');
        }
        incoming = { target: value.target, signature: value.signature, playlist: playlists([value.playlist])[0] };
      } catch (error) { state.message = problem(error); publish(); return false; }
      state.open = true;
      if (blocked() || state.reading || manager.busy() || state.pending || state.input.trim() || state.title.trim()
          || state.targetQuery.trim() || state.nativeQuery.trim() || state.playlist || state.lineIn) {
        state.message = 'Finish the current source action, or explicitly clear its draft before browsing the saved playlist.';
        publish(); return false;
      }
      invalidate(); const version = epoch;
      state.reading = true; state.message = 'Reading the saved playlist’s current household…'; publish();
      try {
        const fresh = groups(await call('zones'));
        if (!current(version)) return false;
        const group = fresh.find(group => group.target === incoming.target && group.signature === incoming.signature);
        if (!group) throw new Error('The saved playlist’s original group changed. Choose its current household and read the saved catalog.');
        const catalog = playlists(await call('sourcePlaylists', { query: { zone: group.target } }));
        if (!current(version)) return false;
        verifyPlaylist(catalog, incoming.playlist);
        clearPlaylists(); clearLineIn();
        state.groups = fresh; state.target = group.target; state.targetMode = 'group'; state.targetQuery = '';
        state.sourceType = 'playlist'; state.mode = 'play'; state.playlists = catalog;
        state.playlistTarget = group.target; state.playlistStatus = 'ready'; state.playlist = incoming.playlist.id;
        state.message = 'Saved playlist selected for browsing. Read its tracks, or review a playback action separately.';
        return true;
      } catch (error) {
        if (current(version)) state.message = problem(error);
        return false;
      } finally { if (current(version)) { state.reading = false; publish(); } }
    }

    function clearDraft() {
      if (disposed || blocked()) return false;
      invalidate(); clearPlaylists(); clearLineIn(); state.input = ''; state.title = ''; state.target = '';
      state.targetMode = 'group'; state.targetQuery = ''; state.nativeQuery = ''; state.mode = 'play'; state.sourceType = 'link';
      state.message = 'Source draft cleared. No playback command was sent.';
      // A previous uncertain outcome is still useful evidence, not erased here.
      publish(); return true;
    }

    async function inspect(group, mode, item) {
      const fresh = groups(await call('zones'));
      if (!fresh.some((entry) => entry.signature === group.signature)) throw new Error('The destination group changed. Refresh rooms and review again.');
      if (item.kind === 'sonos_playlist') verifyPlaylist(await call('sourcePlaylists', { query: { zone: group.target } }), item);
      if (item.kind === 'line_in') verifyLineIn(await call('sourceLineInSources', { query: { zone: group.target } }), item);
      if (item.kind === 'sonos_playlist_track') {
        verifyPlaylistTrack(await call('sourcePlaylistContents', { query: trackQuery(item, group.target) }), item, group);
      }
      if (item.kind === 'sonos_playlist_tracks') {
        verifyPlaylistTracks(await call('sourcePlaylistContents', { query: tracksQuery(item, group.target) }), item, group);
      }
      const before = playback(await call('state', { params: { room: group.target } }), group);
      if (mode === 'play' && !item.container) return before;
      const snapshot = queue(await call('sourceQueueState', {
        params: { room: group.target }, query: { offset: 0, limit: 1 },
      }), group);
      // Appending tolerates track advances; queue-next and replacement bind playback.
      return Object.freeze({ ...before, queue: snapshot, signature: JSON.stringify([
        snapshot.revision, snapshot.total,
        ...(mode !== 'end' ? [snapshot.playing, before.signature] : []),
      ]) });
    }

    async function review(choice = null) {
      if (disposed || !state.open || blocked() || state.reading || state.sourceType === 'catalog') return false;
      invalidate();
      const version = epoch, group = selected(), mode = choice?.mode || state.mode;
      if (collective()) {
        try {
          if (choice || state.sourceType === 'line_in') throw new Error('Choose one displayed group for line-in or an exact saved playlist occurrence.');
          const destination = state.targetMode === 'set' ? state.targetQuery : group?.target;
          const prepared = ['playlist', 'favorite'].includes(state.sourceType)
            ? targets().prepareNative(state.sourceType, state.nativeQuery, mode) : prepare(state.input, state.title, mode);
          const abort = new AbortController(); setReader = abort;
          state.reading = true; state.message = ''; publish();
          const reviewed = await targets().inspect({ call, player: SourcePlayerRemote, target: destination,
            item: prepared.item, mode, signal: abort.signal, valid: () => {
              if (!current(version)) throw new Error('The source draft changed. Review it again.');
            } });
          if (!current(version)) return false;
          const request = targets().request(prepared, reviewed.target, SourcePlayerRemote);
          state.pending = Object.freeze({ ...prepared, targets: reviewed, body: request.body, route: request.route });
          return true;
        } catch (error) { if (current(version)) state.message = problem(error); return false; }
        finally { if (current(version)) { setReader = null; state.reading = false; publish(); } }
      }
      let item, title;
      try {
        if (!group) throw new Error('Choose the group that should play this source.');
        if (state.sourceType === 'playlist' && (state.playlistStatus !== 'ready' || state.playlistTarget !== group.target)) {
          throw new Error('Read the saved Sonos playlists for this group first.');
        }
        if (state.sourceType === 'line_in' && (state.lineInStatus !== 'ready' || state.lineInTarget !== group.target)) {
          throw new Error('Read the line-in sources for this destination first.');
        }
        ({ item, title } = choice || (state.sourceType === 'playlist'
          ? state.contentsSelection.length ? preparePlaylistTracks(state.contents, state.contentsSelection, mode)
            : preparePlaylist(state.playlist, state.playlists, mode) : state.sourceType === 'line_in'
            ? prepareLineIn(state.lineIn, state.lineInSources, mode) : prepare(state.input, state.title, mode)));
      } catch (error) { state.message = problem(error); publish(); return false; }
      state.reading = true; state.message = ''; publish();
      try {
        const before = await inspect(group, mode, item);
        if (!current(version)) return false;
        const body = commandBody({ item, title, mode }, group.target);
        if (item.kind === 'sonos_playlist_tracks') body.expected_queue_revision = before.queue.revision;
        state.pending = Object.freeze({ group, item, title, mode, before, body: Object.freeze(body) });
        return true;
      } catch (error) { if (current(version)) state.message = problem(error); return false; }
      finally { if (current(version)) { state.reading = false; publish(); } }
    }

    async function reviewTrack(position, mode, page = state.contents) {
      if (disposed || !state.open || blocked() || state.reading || collective() || state.sourceType !== 'playlist'
          || !page || page !== state.contents || page.playlist.id !== state.playlist) return false;
      let choice;
      try { choice = preparePlaylistTrack(page, position, mode); }
      catch (error) { invalidate(); state.message = problem(error); publish(); return false; }
      return review(choice);
    }

    function selectTrack(position, page = state.contents) {
      if (disposed || !state.open || blocked() || manager.busy() || state.reading || collective() || state.sourceType !== 'playlist'
          || !page || page !== state.contents || page.playlist.id !== state.playlist) return false;
      const selected = state.contentsSelection.includes(position);
      try {
        preparePlaylistTrack(page, position, 'play');
        if (!selected && state.contentsSelection.length >= MAX_PLAYLIST_TRACKS) throw new Error(`Choose at most ${MAX_PLAYLIST_TRACKS} tracks.`);
      } catch (error) { state.message = problem(error); publish(); return false; }
      invalidate(); state.contentsSelection = Object.freeze(selected ? state.contentsSelection.filter(value => value !== position)
        : [...state.contentsSelection, position]);
      state.message = ''; publish(); return true;
    }

    async function reviewTracks(mode, page = state.contents) {
      if (disposed || !state.open || blocked() || manager.busy() || state.reading || collective() || state.sourceType !== 'playlist'
          || !page || page !== state.contents || page.playlist.id !== state.playlist) return false;
      let choice;
      try { choice = preparePlaylistTracks(page, state.contentsSelection, mode); }
      catch (error) { invalidate(); state.message = problem(error); publish(); return false; }
      return review(choice);
    }

    function clearTrackSelection() {
      if (disposed || !state.open || blocked() || manager.busy() || state.reading) return false;
      invalidate(); state.contentsSelection = Object.freeze([]); state.message = ''; publish(); return true;
    }

    async function confirm(pending) {
      if (disposed || !state.open || blocked() || state.reading || !pending || pending !== state.pending) return false;
      const version = ++epoch;
      state.pending = null; state.writing = true; state.message = 'Checking the destination…'; state.result = null; publish();
      let sent = false, accepted = false;
      try {
        const run = pending.targets ? executeSet : execute;
        await run(pending.targets ? pending.targets.groups : pending.group, async () => {
          if (sent || !current(version)) return;
          let before;
          if (pending.targets) {
            const abort = new AbortController(); setReader = abort;
            before = await targets().inspect({ call, player: SourcePlayerRemote, target: pending.targets.target,
              item: pending.item, mode: pending.mode, signal: abort.signal, valid: () => {
                if (!current(version)) throw new Error('The source review closed or changed before sending.');
              } });
          } else before = await inspect(pending.group, pending.mode, pending.item);
          if (sent || !current(version)) return;
          if (before.signature !== (pending.targets || pending.before).signature) throw new Error('Playback, the queue, or the resolved destinations changed since your review. Review the current destinations again.');
          setReader = null;
          sent = true;
          const route = pending.route || (pending.item.kind === 'sonos_playlist_tracks' ? 'sourcePlaylistTracks'
            : pending.item.kind === 'sonos_playlist_track' ? 'sourcePlaylistTrack'
            : pending.item.kind === 'line_in' ? 'sourceLineInPlay'
            : pending.mode === 'play' ? pending.item.kind === 'sonos_playlist' ? 'sourcePlaylistPlay' : 'sourcePlay' : 'sourceQueue');
          const response = await call(route, { body: pending.body });
          const detail = pending.targets ? null : pending.item.kind === 'sonos_playlist_tracks' ? tracksOutcome(response, pending)
            : pending.item.kind === 'sonos_playlist_track' ? trackOutcome(response, pending)
            : pending.mode === 'play' ? outcome(response) : queued(response, pending.group);
          // A dispatched outcome belongs to this operation, even if its view closed.
          state.result = pending.targets ? targets().result(response, pending.targets, SourcePlayerRemote)
            : Object.freeze({ status: 'accepted', detail });
          accepted = true;
        });
        if (!sent && current(version)) state.result = Object.freeze({ status: 'not_sent', detail: 'No command was sent. Review the source again.' });
      } catch (error) {
        if (!accepted) state.result = Object.freeze({ status: sent ? 'uncertain' : 'not_sent',
          detail: `${problem(error)} ${sent
            ? 'Playback or the queue may have changed, or the request may still finish. Check the room before another action; this request was not repeated.'
            : 'No playback command was sent.'}` });
      } finally {
        setReader = null;
        state.writing = false;
        if (current(version)) state.message = '';
        publish();
      }
      return accepted;
    }

    return { state, manager, catalog, chooseCatalog, refresh, edit, loadTargetSuggestions, loadPlaylists, loadLineInSources, loadContents,
      review, reviewTrack, selectTrack, reviewTracks, clearTrackSelection, confirm, offer, browseSaved, clearDraft,
      catalogChanged(change = null) {
        manager.catalogChanged(change);
        if (disposed || !['playlist', 'favorite'].includes(state.sourceType)
            || state.sourceType === 'playlist' && change?.playlists === false
            || state.targetMode !== 'set' && change?.household && selected()?.household !== change.household) return;
        invalidate(false); clearPlaylists();
        state.message = 'The saved household sources changed. Read the current sources and review again.';
        publish();
      },
      cancel() { if (!blocked() && !disposed) { invalidate(); state.message = ''; publish(); } },
      close() { state.open = false; invalidate(); state.message = ''; publish(); },
      dispose() { disposed = true; state.open = false; invalidate(); },
    };
  }

  function panel({ call, el, execute, executeSet, document: doc = document }) {
    const target = el('select', { 'aria-label': 'Destination group for music' });
    const targetField = el('label', {}, 'Destination group ', target);
    const targetMode = el('select', { 'aria-label': 'Destination selection' },
      el('option', { value: 'group', text: 'Choose one displayed group' }),
      el('option', { value: 'set', text: 'Choose rooms or a room set' }));
    const targetQuery = el('input', { type: 'text', 'aria-label': 'Rooms or room set', maxlength: '128',
      autocomplete: 'off', placeholder: 'Room, alias, here, all@S2, or everywhere' });
    const targetSuggestions = el('select', { 'aria-label': 'Suggested room or room set' });
    const refreshTargets = el('button', { type: 'button', text: 'Read room and alias suggestions' });
    const targetSetFields = el('div', { class: 'source-target-set', hidden: true },
      el('label', {}, 'Rooms or room set ', targetQuery),
      el('label', {}, 'Suggestions ', targetSuggestions), refreshTargets,
      el('p', { class: 'source-help', text: 'Review every affected group before sending. Members already grouped together count once; groups stay independent, including across households.' }));
    const sourceType = el('select', { 'aria-label': 'Music source' },
      el('option', { value: 'link', text: 'Link or stream' }), el('option', { value: 'playlist', text: 'Saved Sonos playlist' }),
      el('option', { value: 'favorite', text: 'Sonos favorite by title' }),
      el('option', { value: 'catalog', text: 'Spotify catalog' }), el('option', { value: 'line_in', text: 'Line-in' }));
    const nativeQuery = el('input', { type: 'text', 'aria-label': 'Favorite or playlist title', maxlength: '256',
      autocomplete: 'off', placeholder: 'The complete title saved in each household' });
    const nativeFields = el('div', { class: 'source-catalog', hidden: true },
      el('label', {}, 'Complete saved title ', nativeQuery),
      el('p', { class: 'source-help', text: 'The same complete title must identify one playable source in every selected household. Review shows each household’s own match; native IDs and numbered choices are not reused across households.' }));
    const catalogInput = el('input', { type: 'search', 'aria-label': 'Search the Spotify catalog',
      placeholder: 'Search music and podcasts on Spotify', autocomplete: 'off', maxlength: MAX_QUERY_BYTES });
    const catalogKind = el('select', { 'aria-label': 'Spotify catalog category' });
    for (const [value, label] of Object.entries(catalogKinds)) catalogKind.append(el('option', { value, text: label }));
    const catalogSearch = el('button', { type: 'submit', text: 'Search Spotify' });
    const catalogCancel = el('button', { type: 'button', text: 'Cancel search', hidden: true });
    const catalogMessage = el('p', { class: 'source-help', role: 'status', 'aria-live': 'polite' });
    const catalogList = el('ol', { class: 'source-online-results', 'aria-label': 'Spotify catalog results' });
    const catalogForm = el('form', { class: 'source-online-form' },
      el('label', {}, 'Spotify catalog search ', catalogInput), el('label', {}, 'Category ', catalogKind),
      el('div', { class: 'row' }, catalogSearch, catalogCancel));
    const catalogFields = el('section', { class: 'source-online', hidden: true, 'aria-label': 'Spotify catalog search' },
      el('p', { class: 'source-help', text: 'Search Spotify online using the owner’s connected account. Your cached library and saved Sonos playlists are separate. Choosing a result prepares a source for review.' }),
      catalogForm, catalogMessage, catalogList);
    const playlist = el('select', { 'aria-label': 'Saved Sonos playlist' });
    const refreshPlaylists = el('button', { type: 'button', text: 'Refresh playlists' });
    const playlistHelp = el('p', { class: 'source-help', role: 'status' });
    const readContents = el('button', { type: 'button', text: 'Read tracks' });
    const previousContents = el('button', { type: 'button', text: '← Previous tracks' });
    const nextContents = el('button', { type: 'button', text: 'Next tracks →' });
    const contentsHelp = el('p', { class: 'source-help', role: 'status', 'aria-live': 'polite' });
    const contentsList = el('ol', { class: 'source-track-list', 'aria-label': 'Saved playlist tracks' });
    const contentsFields = el('div', { class: 'source-contents', hidden: true },
      el('div', { class: 'row' }, readContents, previousContents, nextContents), contentsHelp, contentsList);
    const playlistFields = el('div', { class: 'source-catalog', hidden: true },
      el('label', {}, 'Saved Sonos playlist ', playlist), refreshPlaylists, playlistHelp, contentsFields);
    const lineIn = el('select', { 'aria-label': 'Line-in source' });
    const refreshLineIn = el('button', { type: 'button', text: 'Read line-in sources' });
    const lineInDescription = el('p', { class: 'source-help', role: 'status' });
    const lineInFields = el('div', { class: 'source-catalog', hidden: true },
      el('label', {}, 'Line-in source ', lineIn), refreshLineIn, lineInDescription);
    const input = el('input', { type: 'text', 'aria-label': 'Spotify link or audio stream address',
      placeholder: 'Spotify link, Spotify URI, or HTTP audio address', autocomplete: 'off', spellcheck: 'false' });
    const mode = el('select', { 'aria-label': 'Play now or add to queue' });
    for (const [value, label] of Object.entries(modes)) mode.append(el('option', { value, text: label }));
    const modeField = el('label', {}, 'Action ', mode);
    const title = el('input', { type: 'text', 'aria-label': 'Optional display title', placeholder: 'Optional display title', maxlength: MAX_TITLE });
    const refresh = el('button', { type: 'button', text: 'Refresh rooms' });
    const review = el('button', { type: 'button', text: 'Review playback' });
    const clear = el('button', { type: 'button', text: 'Clear source draft' });
    const message = el('p', { role: 'status', 'aria-live': 'polite' });
    const result = el('p', { role: 'status', 'aria-live': 'polite' });
    const consent = el('div', { class: 'source-review', hidden: true, tabindex: '-1', 'aria-label': 'Review selected playback' });
    const managerHost = el('div');
    const inputField = el('label', {}, 'Link or stream ', input), titleField = el('label', {}, 'Title ', title);
    const details = el('details', { class: 'zone source-player' }, el('summary', { text: 'Spotify, links, playlists & line-in' }),
      el('div', {}, el('p', { text: 'Search Spotify, choose a saved Sonos playlist, favorite or line-in source, paste a link, or send a direct audio stream. Review the destinations and action before playing or adding music to the queue.' }),
        targetField, el('label', {}, 'Destination selection ', targetMode), targetSetFields,
        el('label', {}, 'Music source ', sourceType), inputField, titleField, nativeFields, playlistFields, catalogFields, lineInFields,
        modeField, el('div', { class: 'row' }, refresh, review, clear), message, managerHost, consent, result));
    const node = el('section', { class: 'scenes', 'aria-label': 'Spotify, links, Sonos playlists and line-in' }, details);
    let shownGroups = null, shownPlaylists = null, shownPlaylistStatus = '', shownReview = null, shownContents = null, shownContentsBusy = null, shownContentsSelection = null, confirmButton = null;
    let shownCatalog = null, shownCatalogBusy = null, shownSuggestions = null;
    let shownLineIn = null, shownLineInStatus = '';
    const control = model({ call, execute, executeSet, changed: render });
    const selectionPanel = playlistTrackSelectionPanel(doc, { review: control.reviewTracks, clear: control.clearTrackSelection });
    contentsFields.append(selectionPanel.node);
    const managerPanel = playlistManagerPanel(control.manager, doc);
    managerHost.append(managerPanel.node);
    function render(state) {
      const writing = state.writing || state.managing.writing;
      const busy = state.reading || writing || control.manager.busy();
      if (shownGroups !== state.groups) {
        shownGroups = state.groups;
        target.replaceChildren(el('option', { value: '', text: 'Choose a group…', disabled: true }));
        for (const group of state.groups) target.append(el('option', { value: group.target, text: group.label }));
      }
      target.value = state.target; mode.value = state.mode; sourceType.value = state.sourceType;
      const set = state.targetMode === 'set', favorite = state.sourceType === 'favorite';
      const saved = state.sourceType === 'playlist', online = state.sourceType === 'catalog', analog = state.sourceType === 'line_in';
      managerPanel.render(saved && !set && !!state.playlist && state.playlistStatus === 'ready');
      const nativeTitle = favorite || set && saved;
      targetMode.value = state.targetMode; targetField.hidden = set; targetSetFields.hidden = !set;
      if (targetQuery.value !== state.targetQuery) targetQuery.value = state.targetQuery;
      if (nativeQuery.value !== state.nativeQuery) nativeQuery.value = state.nativeQuery;
      targetMode.disabled = writing || analog; targetQuery.disabled = writing;
      nativeQuery.disabled = writing; refreshTargets.disabled = busy; targetSuggestions.disabled = busy;
      if (shownSuggestions !== state.targetSuggestions) {
        shownSuggestions = state.targetSuggestions;
        targetSuggestions.replaceChildren(el('option', { value: '', text: 'Choose a suggestion…' }));
        for (const option of state.targetSuggestions) targetSuggestions.append(el('option', { value: option.query, text: option.label }));
      }
      for (const option of sourceType.children) if (option.value === 'line_in') option.disabled = set;
      inputField.hidden = saved || favorite || online || analog; titleField.hidden = saved || favorite || online || analog;
      nativeFields.hidden = !nativeTitle; playlistFields.hidden = !saved || set; catalogFields.hidden = !online; lineInFields.hidden = !analog; modeField.hidden = analog;
      if (catalogInput.value !== state.catalog.query) catalogInput.value = state.catalog.query;
      catalogKind.value = state.catalog.kind;
      catalogInput.disabled = busy; catalogKind.disabled = busy;
      catalogSearch.disabled = busy || state.catalog.status === 'loading';
      catalogCancel.hidden = state.catalog.status !== 'loading';
      catalogMessage.textContent = state.catalog.message;
      catalogFields.setAttribute('aria-busy', String(state.catalog.status === 'loading'));
      if (shownCatalog !== state.catalog.items || shownCatalogBusy !== busy) {
        shownCatalog = state.catalog.items; shownCatalogBusy = busy;
        renderCatalog(catalogList, state.catalog, doc, { choose: control.chooseCatalog, disabled: busy });
      }
      if (shownPlaylists !== state.playlists || shownPlaylistStatus !== state.playlistStatus) {
        shownPlaylists = state.playlists; shownPlaylistStatus = state.playlistStatus;
        const caption = state.playlistStatus === 'loading' ? 'Loading playlists…'
          : state.playlistStatus === 'ready' ? state.playlists.length ? 'Choose a playlist…' : 'No saved Sonos playlists'
            : 'Refresh playlists for this group';
        playlist.replaceChildren(el('option', { value: '', text: caption, disabled: true }));
        for (const item of state.playlists) playlist.append(el('option', { value: item.id, text: item.title || item.id }));
      }
      playlist.value = state.playlist;
      playlist.disabled = writing || state.playlistStatus !== 'ready' || !state.playlists.length;
      refreshPlaylists.disabled = busy || !state.target;
      playlistHelp.textContent = state.playlistStatus === 'ready' ? state.playlists.length
        ? 'Saved on the speakers in this household. Read its tracks before choosing how to play it.'
        : 'This household has no saved Sonos playlists. Save a playlist in Sonos, then refresh here.'
        : state.target ? 'Read the saved playlists for this destination.' : 'Choose a destination group first.';
      if (shownLineIn !== state.lineInSources || shownLineInStatus !== state.lineInStatus) {
        shownLineIn = state.lineInSources; shownLineInStatus = state.lineInStatus;
        const caption = state.lineInStatus === 'loading' ? 'Reading inputs…'
          : state.lineInStatus === 'ready' ? state.lineInSources.length ? 'Choose a line-in source…' : 'No available inputs'
            : 'Read inputs for this destination';
        lineIn.replaceChildren(el('option', { value: '', text: caption, disabled: true }));
        for (const item of state.lineInSources) lineIn.append(el('option', { value: item.player, text: lineInLabel(item) }));
      }
      lineIn.value = state.lineIn;
      lineIn.disabled = busy || state.lineInStatus !== 'ready' || !state.lineInSources.length;
      refreshLineIn.disabled = busy || !state.target;
      refreshLineIn.textContent = state.lineInStatus === 'ready' ? 'Refresh line-in sources' : 'Read line-in sources';
      lineInDescription.textContent = lineInHelp(state);
      contentsFields.hidden = !state.playlist || state.playlistStatus !== 'ready';
      readContents.disabled = busy || !state.playlist;
      readContents.textContent = state.contents ? 'Refresh tracks' : 'Read tracks';
      previousContents.hidden = !state.contentsHistory.length; previousContents.disabled = busy;
      nextContents.hidden = state.contents?.next_offset == null; nextContents.disabled = busy;
      contentsHelp.textContent = contentsSummary(state.contents);
      selectionPanel.render(state.contents, state.contentsSelection, busy);
      if (shownContents !== state.contents || shownContentsBusy !== busy || shownContentsSelection !== state.contentsSelection) {
        shownContents = state.contents; shownContentsBusy = busy; shownContentsSelection = state.contentsSelection;
        renderContents(contentsList, state.contents, doc, { review: control.reviewTrack, select: control.selectTrack,
          edit: (position, page) => { if (page === state.contents) control.manager.chooseTrack(position, page); },
          selected: state.contentsSelection, disabled: busy });
      }
      review.textContent = state.contentsSelection.length ? state.mode === 'play' ? 'Review selected playback' : 'Review selected queue addition'
        : state.mode === 'play' ? 'Review playback' : 'Review queue addition';
      for (const [field, element] of [['input', input], ['title', title]]) if (element.value !== state[field]) element.value = state[field];
      for (const element of [target, sourceType, input, title, mode]) element.disabled = writing;
      mode.disabled = writing || analog;
      clear.disabled = writing;
      refresh.disabled = busy; review.disabled = busy || online || !(set ? state.targetQuery.trim() : state.target)
        || (nativeTitle ? !state.nativeQuery.trim() : saved ? !state.playlist || state.playlistStatus !== 'ready'
        : analog ? !state.lineIn || state.lineInStatus !== 'ready' : !state.input.trim());
      message.textContent = state.message || (state.reading ? 'Reading the destination…' : '');
      result.textContent = state.result?.detail || '';
      if (shownReview !== state.pending) {
        shownReview = state.pending; consent.replaceChildren(); consent.hidden = !shownReview;
        if (shownReview) {
          const pending = shownReview;
          confirmButton = el('button', { type: 'button', text: modes[pending.mode] });
          const cancel = el('button', { type: 'button', text: 'Cancel' });
          confirmButton.addEventListener('click', () => { void control.confirm(pending); });
          cancel.addEventListener('click', () => control.cancel());
          if (pending.targets) {
            consent.append(el('p', { text: `${modes[pending.mode]}: ${pending.title || pending.item.uri} · ${pending.targets.groups.length} independent group(s)` }));
            const affected = el('ul', { class: 'source-target-preview', 'aria-label': 'Reviewed source destinations' });
            for (const entry of pending.targets.entries) {
              const q = entry.before.queue;
              const effect = pending.mode === 'play' ? entry.item.container
                ? `Replace this group’s ${q.total} queue entries and start the selected source.`
                : 'Replace the current source and start playback; its queue stays available.'
                : pending.mode === 'next' && q.playing !== null
                  ? `Add after queue position ${q.playing}, keeping playback unchanged.`
                  : `Append after ${q.total} existing queue entries without starting or resuming playback.`;
              affected.append(el('li', {}, el('strong', { text: entry.group.label }),
                el('span', { text: 'Source: ' + (entry.sourceLabel || pending.title) }),
                el('span', { text: 'Current: ' + entry.before.label }), el('span', { text: effect })));
            }
            consent.append(affected,
              ...(pending.item.kind === 'artist' || pending.item.kind === 'show' ? [el('p', { class: 'source-help', text: 'Spotify sign-in is required. One selection of up to 10 tracks or episodes is shared across the groups.' })] : []),
              el('p', { class: 'source-help', text: 'The whole room set and each source or queue are checked again before one request. A partial result lists each affected group; check failures before another action.' }),
              el('div', { class: 'row' }, confirmButton, cancel));
            consent.focus?.({ preventScroll: true }); consent.scrollIntoView?.({ block: 'nearest' });
          } else {
          const snapshot = pending.before.queue;
          const effect = pending.mode === 'play'
            ? pending.item.kind === 'sonos_playlist_tracks' ? 'This replaces the group’s queue with the selected tracks and starts the first in the order shown, with shuffle and repeat off. The saved playlist remains unchanged.'
              : pending.item.kind === 'artist' ? 'This replaces the group’s queue with up to 10 artist tracks and starts them in order.'
              : pending.item.kind === 'show' ? 'This replaces the group’s queue with up to 10 available episodes and starts them in Spotify’s order.'
              : pending.item.container ? 'This replaces the group’s queue and starts the album or playlist.'
              : 'This replaces the group’s current source and starts playback. The existing queue remains available.'
            : pending.mode === 'next' && snapshot.playing !== null
              ? `Add after queue position ${snapshot.playing}, keeping the existing queue and playback.`
              : `Append after the ${snapshot.total} existing queue entries. This does not start or resume playback.`;
          const selectedTracks = pending.item.kind === 'sonos_playlist_tracks'
            ? el('ol', { class: 'source-selected-tracks', 'aria-label': 'Reviewed saved tracks in playback order' }) : null;
          if (selectedTracks) renderSelectedTracks(selectedTracks, pending.item, doc);
          consent.append(el('p', { text: `${modes[pending.mode]}: ${pending.title || pending.item.uri} in ${pending.group.label}?` }),
            el('p', { text: `Current playback: ${pending.before.label}.` }),
            el('p', { text: effect }),
            ...(selectedTracks ? [selectedTracks, el('p', { class: 'source-help', text: 'The selected saved occurrences and destination queue are checked again before one request. Selection order and repeated tracks are retained; the saved playlist is unchanged.' })] : []),
            ...(pending.item.kind === 'artist' ? [el('p', { class: 'source-help', text: 'Uses the artist’s top tracks or a small sample from their albums, in Spotify’s order. Requires Spotify sign-in.' })] : []),
            ...(pending.item.kind === 'show' ? [el('p', { class: 'source-help', text: 'Selects up to 10 available episodes from the show’s first episode page, in Spotify’s order. Requires Spotify sign-in.' })] : []),
            ...(snapshot && pending.mode !== 'play' && snapshot.playing === null ? [el('p', { text: 'The group is not using its queue. Select an existing queue entry separately when you are ready to start it.' })] : []),
            ...(pending.item.kind === 'sonos_playlist' ? [el('p', { class: 'source-help', text: 'The saved playlist is read again before sending. Its current contents are supplied by this household’s speakers.' })] : []),
            ...(pending.item.kind === 'sonos_playlist_track' ? [el('p', { class: 'source-help', text: `Track ${pending.item.position} from “${pending.item.playlistTitle}”. The exact saved occurrence is checked again before sending.` })] : []),
            ...(pending.item.kind === 'line_in' ? [el('p', { class: 'source-help', text: `Analog input from ${pending.item.room}. The source is checked again before sending. Cable and signal state are not reported; start your source device yourself.` })] : []),
            el('p', { class: 'source-help', text: 'The destination is checked again before sending. Another controller can still change it afterward. The speakers must support and reach the source.' }),
            el('div', { class: 'row' }, confirmButton, cancel));
          consent.focus?.({ preventScroll: true });
          consent.scrollIntoView?.({ block: 'nearest' });
          }
        }
      }
      if (confirmButton) confirmButton.disabled = busy || !state.pending;
    }
    async function refreshSources() {
      if (await control.refresh() && control.state.sourceType === 'playlist') await control.loadPlaylists();
    }
    target.addEventListener('change', () => {
      if (control.edit('target', target.value) && control.state.sourceType === 'playlist') void control.loadPlaylists();
    });
    targetMode.addEventListener('change', () => {
      if (control.edit('targetMode', targetMode.value) && targetMode.value === 'set' && !control.state.targetSuggestions.length) void control.loadTargetSuggestions();
    });
    targetQuery.addEventListener('input', () => control.edit('targetQuery', targetQuery.value));
    nativeQuery.addEventListener('input', () => control.edit('nativeQuery', nativeQuery.value));
    targetSuggestions.addEventListener('change', () => {
      if (targetSuggestions.value) control.edit('targetQuery', targetSuggestions.value);
      targetSuggestions.value = '';
    });
    refreshTargets.addEventListener('click', () => { void control.loadTargetSuggestions(); });
    sourceType.addEventListener('change', () => {
      if (control.edit('sourceType', sourceType.value) && control.state.sourceType === 'playlist') void control.loadPlaylists();
    });
    catalogInput.addEventListener('input', () => control.catalog.edit('query', catalogInput.value));
    catalogKind.addEventListener('change', () => control.catalog.edit('kind', catalogKind.value));
    catalogForm.addEventListener('submit', event => { event.preventDefault(); void control.catalog.search(); });
    catalogCancel.addEventListener('click', () => control.catalog.cancel());
    playlist.addEventListener('change', () => control.edit('playlist', playlist.value));
    refreshPlaylists.addEventListener('click', () => { void control.loadPlaylists(); });
    lineIn.addEventListener('change', () => control.edit('lineIn', lineIn.value));
    refreshLineIn.addEventListener('click', () => { void control.loadLineInSources(); });
    readContents.addEventListener('click', () => { void control.loadContents(); });
    previousContents.addEventListener('click', () => { void control.loadContents('previous'); });
    nextContents.addEventListener('click', () => { void control.loadContents('next'); });
    input.addEventListener('input', () => control.edit('input', input.value));
    title.addEventListener('input', () => control.edit('title', title.value));
    mode.addEventListener('change', () => control.edit('mode', mode.value));
    refresh.addEventListener('click', () => { void refreshSources(); });
    review.addEventListener('click', () => { void control.review(); });
    clear.addEventListener('click', () => control.clearDraft());
    details.addEventListener('toggle', (event) => {
      if (event && event.target !== details) return;
      if (!details.open) control.close();
      else if (!control.state.open) void refreshSources();
    });
    const hide = () => { if (doc.hidden) control.close(); else if (details.open) void refreshSources(); };
    const catalogChanged = event => control.catalogChanged(event.detail);
    const win = doc.defaultView;
    const pagehide = () => control.close();
    const pageshow = () => { if (!doc.hidden && details.open) void refreshSources(); };
    doc.addEventListener('visibilitychange', hide);
    doc.addEventListener('fsonos:catalog-changed', catalogChanged);
    win?.addEventListener('pagehide', pagehide);
    win?.addEventListener('pageshow', pageshow);
    render(control.state);
    return { node, ...control, openSource(value) {
      const offered = control.offer(value);
      if (control.state.open) {
        details.open = true;
        target.focus({ preventScroll: true });
        node.scrollIntoView({ block: 'start' });
      }
      return offered;
    }, openSavedPlaylist(value) {
      const opened = control.browseSaved(value);
      if (control.state.open) {
        details.open = true;
        target.focus({ preventScroll: true });
        node.scrollIntoView({ block: 'start' });
      }
      return opened;
    }, dispose() {
      doc.removeEventListener('visibilitychange', hide);
      doc.removeEventListener('fsonos:catalog-changed', catalogChanged);
      win?.removeEventListener('pagehide', pagehide);
      win?.removeEventListener('pageshow', pageshow);
      control.dispose();
    } };
  }

  return { source, prepare, playlists, preparePlaylist, verifyPlaylist, contentsRequest, playlistContents, contentsSummary, renderContents,
    nativePlaylistTitle, playlistTrackEdit, managedPlaylist, playlistManager, playlistManagerPanel,
    lineInSources, lineInLabel, lineInHelp, prepareLineIn, verifyLineIn,
    preparePlaylistTrack, trackQuery, verifyPlaylistTrack, trackOutcome,
    preparePlaylistTracks, tracksQuery, verifyPlaylistTracks, tracksOutcome, renderSelectedTracks, playlistTrackSelectionPanel, MAX_PLAYLIST_TRACKS,
    commandBody, offeredSource, groups, playback, queue, queued, outcome, model, panel, routes, modes, MAX_BYTES, MAX_TITLE,
    catalogKinds, catalogQuery, catalogPage, catalogModel, renderCatalog, CATALOG_LIMIT, MAX_QUERY_BYTES };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = SourcePlayerRemote;
else if (typeof ROUTES !== 'undefined') {
  Object.assign(ROUTES, SourcePlayerRemote.routes);
  if (typeof SourceTargetsRemote !== 'undefined') Object.assign(ROUTES, SourceTargetsRemote.routes);
  const host = document.getElementById('households');
  if (host) {
    const setLevels = typeof RoomSetsRemote !== 'undefined' && typeof volumeFlights !== 'undefined'
      ? RoomSetsRemote.levelLanes({ cards: () => house.cards.values(), flights: volumeFlights }) : null;
    if (setLevels) registerCardPanel(parts => {
      const observe = () => setLevels.observe(parts); observe();
      return { node: el('span', { hidden: true }), observe, block: observe };
    });
    const sourcePanel = SourcePlayerRemote.panel({ call, el,
      async executeSet(groups, work) {
        if (typeof RoomSetsRemote === 'undefined') throw new Error('Room-set controls are unavailable. Reload the remote.');
        return RoomSetsRemote.coordinated({ cards: () => house.cards.values(), refresh: loadZones,
          begin: beginAction, end: endAction, shape: groupShape, levels: setLevels })(groups, work);
      },
      async execute(group, work) {
        let parts = [...house.cards.values()].find((card) => groupShape(card.zone) === group.signature);
        if (!parts) {
          await loadZones();
          parts = [...house.cards.values()].find((card) => groupShape(card.zone) === group.signature);
        }
        if (!parts || !beginAction(parts)) throw new Error('The destination changed or another playback action is busy. Refresh rooms and review again.');
        try { return await work(); } finally { endAction(parts); }
      },
    });
    host.before(sourcePanel.node);
    document.addEventListener('fsonos:source-review', (event) => {
      event.preventDefault();
      void sourcePanel.openSource(event.detail);
    });
    document.addEventListener('fsonos:playlist-browse', event => {
      if (!event.detail || typeof event.detail !== 'object' || Array.isArray(event.detail)) return;
      event.preventDefault();
      event.detail.response = sourcePanel.openSavedPlaylist(event.detail.selection);
    });
  }
}
