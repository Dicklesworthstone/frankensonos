// Shared wire checks for one ordered, bounded library selection.
'use strict';
const QueueTracksRemote = (() => {
  const MAX_TRACKS = 10;
  const uint = value => Number.isInteger(value) && value >= 0 && value <= 0xffffffff;
  const text = (value, max = 4096) => typeof value === 'string' && value.trim() !== '' && value.length <= max;
  const fail = message => { throw new Error(message); };
  const supported = source => typeof source === 'string' && /^spotify:(track|episode):[A-Za-z0-9]{22}$/.test(source);
  const frozen = value => {
    if (Array.isArray(value)) return Object.freeze(value.map(frozen));
    if (value && typeof value === 'object') return Object.freeze(Object.fromEntries(
      Object.entries(value).map(([key, item]) => [key, frozen(item)])));
    return value;
  };
  function basketPage(value) {
    const query = value?.query;
    if (!query || query.kind !== 'tracks' || !uint(query.offset) || !uint(query.limit)
        || query.limit < 1 || query.limit > 50 || !text(value.revision, 256)
        || (query.q != null && (typeof query.q !== 'string' || [...query.q].length > 512))
        || (query.album_uri != null && !/^spotify:album:[A-Za-z0-9]{22}$/.test(query.album_uri))
        || (query.playlist_uri != null && !/^spotify:playlist:[A-Za-z0-9]{22}$/.test(query.playlist_uri))
        || (value.collectionTitle != null && (typeof value.collectionTitle !== 'string' || value.collectionTitle.length > 4096))
        || (query.album_uri && query.playlist_uri)) {
      fail('Choose tracks from a complete cached listing.');
    }
    return frozen({ query: { kind: 'tracks', offset: query.offset, limit: query.limit,
      q: (query.q ?? '').trim(), album_uri: query.album_uri ?? null, playlist_uri: query.playlist_uri ?? null },
    revision: value.revision, albumObservation: value.albumObservation ?? null,
    ...(value.collectionTitle ? { collectionTitle: value.collectionTitle } : {}) });
  }
  const contextKey = value => { const { query } = basketPage(value); return JSON.stringify([
    query.kind, query.limit, query.q, query.album_uri, query.playlist_uri]); };
  const listingKey = page => JSON.stringify([contextKey(page), page.revision, page.albumObservation]);
  const pageKey = value => { const page = basketPage(value); return JSON.stringify([listingKey(page), page.query.offset]); };
  function originLabel(value) {
    const page = basketPage(value), q = page.query;
    const title = page.collectionTitle?.trim().slice(0, 512);
    const collection = q.album_uri ? `Album: ${title || q.album_uri}`
      : q.playlist_uri ? `Playlist: ${title || q.playlist_uri}` : 'Cached tracks';
    return [collection, q.q ? `Filter: ${q.q}` : ''].filter(Boolean).join(' · ');
  }
  // Keep observed occurrences, not just URIs or current-page checkbox indices.
  // A repeated song gets its own stable id; changing page never changes its proof.
  function basket() {
    let rows = Object.freeze([]), serial = 0, customOrder = false;
    const replace = values => { rows = Object.freeze(values); if (!rows.length) customOrder = false; };
    function observe(value) {
      const page = basketPage(value), key = contextKey(page);
      if (rows.some(row => contextKey(row.page) === key && listingKey(row.page) !== listingKey(page))) {
        replace([]);
        fail('A selected cached listing changed. The selection was cleared; read the music again.');
      }
      return page;
    }
    function add(pageValue, index, item) {
      const page = observe(pageValue), source = item?.source_uri ?? item?.source;
      if (!uint(index) || index >= page.query.limit || !supported(source) || typeof item?.title !== 'string') {
        fail('Choose an observed track from this page.');
      }
      const old = rows.find(row => pageKey(row.page) === pageKey(page) && row.index === index);
      if (old) {
        if (JSON.stringify(old.item) !== JSON.stringify(item)) {
          replace([]); fail('The selected track changed. The selection was cleared; refresh the listing.');
        }
        return false;
      }
      if (rows.length >= MAX_TRACKS) fail(`Select at most ${MAX_TRACKS} tracks, including repeats.`);
      const next = [...rows, frozen({ id: `selection-${++serial}`, page, index, item })];
      if (!customOrder) {
        const contexts = [...new Set(next.map(row => contextKey(row.page)))];
        next.sort((a, b) => contexts.indexOf(contextKey(a.page)) - contexts.indexOf(contextKey(b.page))
          || a.page.query.offset - b.page.query.offset || a.index - b.index);
      }
      replace(next); return true;
    }
    function remove(id) {
      if (!rows.some(row => row.id === id)) return false;
      replace(rows.filter(row => row.id !== id)); return true;
    }
    function repeat(id) {
      const index = rows.findIndex(row => row.id === id);
      if (index < 0) return false;
      if (rows.length >= MAX_TRACKS) fail(`Select at most ${MAX_TRACKS} tracks, including repeats.`);
      const next = [...rows];
      next.splice(index + 1, 0, frozen({ ...rows[index], id: `selection-${++serial}` }));
      customOrder = true; replace(next); return true;
    }
    function move(id, delta) {
      const index = rows.findIndex(row => row.id === id), to = index + delta;
      if (![-1, 1].includes(delta) || index < 0 || to < 0 || to >= rows.length) return false;
      const next = [...rows], [row] = next.splice(index, 1); next.splice(to, 0, row);
      customOrder = true; replace(next); return true;
    }
    function indices(value) {
      const key = pageKey(value);
      return [...new Set(rows.filter(row => pageKey(row.page) === key).map(row => row.index))].sort((a, b) => a - b);
    }
    return { get rows() { return rows; }, observe, add, remove, repeat, move, indices,
      clear() { replace([]); },
      unselect(page, index) { const key = pageKey(page); replace(rows.filter(row => pageKey(row.page) !== key || row.index !== index)); },
    };
  }
  function tracksOf(value) {
    if (!Array.isArray(value) || !value.length || value.length > MAX_TRACKS) fail(`Select 1 to ${MAX_TRACKS} cached tracks.`);
    return Object.freeze(value.map(track => {
      if (!track || !supported(track.source_uri) || (track.title != null && typeof track.title !== 'string')) {
        fail('The selection contains an unsupported track. Refresh the listing.');
      }
      const title = track.title == null ? '' : [...track.title.trim()].slice(0, 256).join('');
      return Object.freeze({ source_uri: track.source_uri, ...(title ? { title } : {}) });
    }));
  }
  function queueOf(value, target) {
    const room = typeof target === 'string' ? target.slice(0, target.lastIndexOf('@')) : '';
    if (!value || !room || value.zone !== room || !uint(value.total) || !text(value.revision, 1024)
        || !Array.isArray(value.items) || value.items.length !== Math.min(1, value.total)
        || value.items.some(item => !item || item.position !== 1 || typeof item.title !== 'string')
        || (value.playing != null && (!uint(value.playing) || value.playing < 1 || value.playing > value.total))) {
      fail('The destination queue is unreadable. Refresh it before reviewing the selection.');
    }
    return Object.freeze({ revision: value.revision, total: value.total, playing: value.playing ?? null });
  }
  function verifyQueue(before, current, next) {
    if (!before || !current || before.revision !== current.revision || before.total !== current.total
        || (next && before.playing !== current.playing)) {
      fail('The destination queue changed. Review the selected tracks again.');
    }
  }
  // Phone selections retain source and group identity while their review is open.
  function playbackOf(value, group) {
    const zone = value?.zone, track = value?.track;
    if (!zone || !text(zone.household) || !text(zone.coordinator_room)
        || !Array.isArray(zone.members) || !zone.members.length || !zone.members.every(member => text(member))
        || !zone.members.includes(zone.coordinator_room) || new Set(zone.members).size !== zone.members.length
        || JSON.stringify([zone.household, zone.coordinator_room, [...zone.members].sort()]) !== group.signature
        || !['playing', 'paused', 'stopped', 'transitioning', 'unknown'].includes(value.transport_state)
        || (track != null && (!text(track.uri) || (track.queue_position != null
          && (!uint(track.queue_position) || track.queue_position === 0))))) {
      fail('Playback or group membership changed. Refresh before reviewing the selection.');
    }
    return JSON.stringify([group.signature, value.transport_state, track?.uri ?? null, track?.queue_position ?? null]);
  }
  function body(zone, tracks, next, queue) {
    if (!text(zone, 128) || typeof next !== 'boolean' || !text(queue?.revision, 1024)) fail('Review the destination queue again.');
    return Object.freeze({ zone, tracks: tracksOf(tracks), next, expected_revision: queue.revision });
  }
  function playBody(zone, tracks, queue) {
    if (!text(zone, 128) || !text(queue?.revision, 1024)) fail('Review the destination queue before replacing it.');
    return Object.freeze({ zone, tracks: tracksOf(tracks), expected_revision: queue.revision });
  }
  function playReceipt(result) {
    if (!result || !text(result.done) || typeof result.changed !== 'boolean'
        || (result.notes !== undefined && (!Array.isArray(result.notes)
          || result.notes.some(note => !note || typeof note.detail !== 'string')))) {
      fail('The selected playback acknowledgement was incomplete; its outcome is uncertain. Read the room and queue before another action.');
    }
    return { done: result.done, changed: result.changed, notes: result.notes };
  }
  function receipt(result, request, before) {
    const room = request.zone.slice(0, request.zone.lastIndexOf('@'));
    if (!result || !text(result.done) || result.zone !== room || !uint(result.first) || result.first < 1
        || result.first > before.total + 1 || result.count !== request.tracks.length
        || typeof result.playing_queue !== 'boolean'
        || (result.notes !== undefined && (!Array.isArray(result.notes)
          || result.notes.some(note => !note || typeof note.detail !== 'string')))) {
      fail('The queue selection acknowledgement was incomplete; its outcome is uncertain. Read the queue before another add.');
    }
    return { done: result.done, changed: true, notes: result.notes };
  }
  return { MAX_TRACKS, supported, basketPage, contextKey, pageKey, originLabel, basket, tracksOf, queueOf, verifyQueue, playbackOf, body, playBody, receipt, playReceipt };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = QueueTracksRemote;
