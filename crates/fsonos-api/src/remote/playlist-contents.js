// A playlist cache preserves occurrences, not just distinct track identities.
'use strict';
const PlaylistContents = (() => {
  const LIMIT = 50;
  const uint = (v) => Number.isInteger(v) && v >= 0 && v <= 0xffffffff;
  const text = (v, max = 16384) => typeof v === 'string' && v.length <= max;
  const uri = (v, kind) => typeof v === 'string' && new RegExp(`^spotify:${kind}:[A-Za-z0-9]{1,128}$`).test(v);
  const fail = () => { throw new Error('The cached playlist is inconsistent. Refresh its first page.'); };

  function queryOf(value) {
    const { playlist_uri, offset = 0, q = '', expected_revision = null } = value || {};
    if (!uri(playlist_uri, 'playlist') || !uint(offset) || !text(q, 2048) || [...q.trim()].length > 512
        || (expected_revision !== null && (!text(expected_revision, 256) || !expected_revision.trim() || /[\x00-\x1f\x7f]/.test(expected_revision)))
        || (offset > 0 && expected_revision === null)) fail();
    return Object.freeze({ playlist_uri, offset, limit: LIMIT, q: q.trim(), expected_revision });
  }

  function parameters(value) {
    const q = queryOf(value), params = { playlist_uri: q.playlist_uri, offset: q.offset, limit: LIMIT };
    if (q.q) params.q = q.q;
    if (q.expected_revision !== null) params.expected_revision = q.expected_revision;
    return params;
  }

  function itemOf(raw) {
    if (!raw || !uint(raw.position) || raw.position < 1 || !uri(raw.source_uri, 'track')
        || !text(raw.title) || !text(raw.subtitle) || typeof raw.explicit !== 'boolean'
        || (raw.album != null && !text(raw.album))
        || (raw.album_uri != null && !uri(raw.album_uri, 'album'))
        || [raw.disc_number, raw.track_number, raw.duration_secs].some(v => v != null && !uint(v))) fail();
    return Object.freeze({ kind: 'track', position: raw.position, source_uri: raw.source_uri,
      title: raw.title, subtitle: raw.subtitle, explicit: raw.explicit, album: raw.album ?? null,
      album_uri: raw.album_uri ?? null, disc_number: raw.disc_number ?? null,
      track_number: raw.track_number ?? null, duration_secs: raw.duration_secs ?? null, cached_tracks: null });
  }

  function pageOf(value, input) {
    const query = queryOf(input);
    if (!value || value.playlist_uri !== query.playlist_uri || value.q !== query.q || !text(value.name)
        || value.offset !== query.offset || value.limit !== LIMIT
        || !['complete', 'partial', 'not_cached'].includes(value.coverage)
        || !uint(value.total) || !uint(value.cached_total) || !uint(value.examined)
        || value.total > value.cached_total || value.cached_total > value.examined || value.cached_total > 200
        || (!query.q && value.total !== value.cached_total)
        || (value.source_total !== null && (!uint(value.source_total) || value.examined > value.source_total))
        || !Array.isArray(value.items) || value.items.length !== Math.min(LIMIT, Math.max(0, value.total - query.offset))) fail();
    if (value.coverage === 'not_cached') {
      if (value.revision !== null || value.fetched_at !== null || value.source_total !== null
          || value.examined !== 0 || value.cached_total !== 0 || value.total !== 0) fail();
    } else if (!text(value.revision, 256) || !value.revision.trim()
        || !Number.isSafeInteger(value.fetched_at) || Math.abs(value.fetched_at) > 8640000000000
        || (value.coverage === 'complete' && value.source_total !== null && value.examined !== value.source_total)) fail();
    const end = query.offset + value.items.length, next = end < value.total ? end : null;
    if (value.next_offset !== next) fail();
    if (query.expected_revision !== null && value.revision !== query.expected_revision) {
      throw new Error('The cached playlist changed between pages. Refresh to start at its first page.');
    }
    let position = 0;
    const items = value.items.map((raw, index) => {
      const item = itemOf(raw);
      if (item.position <= position || item.position > value.examined || item.position <= query.offset + index) fail();
      position = item.position;
      return item;
    });
    return Object.freeze({ query, name: value.name, total: value.total, cached_total: value.cached_total,
      next, revision: value.revision, coverage: value.coverage, fetched_at: value.fetched_at,
      source_total: value.source_total, examined: value.examined, items: Object.freeze(items) });
  }

  function describe(page) {
    if (page.coverage === 'not_cached') return 'Playlist contents have not been cached. Refresh the library to read eligible playlists; an unread playlist is not an empty one.';
    const count = `${page.examined}${page.source_total === null ? '' : ` of ${page.source_total}`} source entries read; ${page.cached_total} playable track occurrences cached.`;
    const skipped = page.examined - page.cached_total;
    return `${page.coverage === 'partial' ? 'Partial playlist' : 'Playlist read to the end'}: ${count}${skipped ? ` ${skipped} unavailable or unsupported entries leave gaps in the positions.` : ''} Cached ${new Date(page.fetched_at * 1000).toLocaleString()}. Spotify may have changed since this read.`;
  }

  return { LIMIT, queryOf, parameters, itemOf, pageOf, describe };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = PlaylistContents;
