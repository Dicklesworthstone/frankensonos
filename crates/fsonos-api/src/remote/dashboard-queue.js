// Native speaker-queue pages. DJ selections are a separate view, never a substitute.
'use strict';
const DashboardQueue = (() => {
  const LIMIT = 40;
  const uint = (n) => Number.isInteger(n) && n >= 0 && n <= 0xffffffff;
  const text = (s) => typeof s === 'string' && s.length <= 65536;
  const group = (z) => JSON.stringify([z.household, z.coordinator_room, [...z.members].sort()]);
  const address = (z) => `${z.coordinator_room}@${z.household}`;
  function decode(value, zone, offset) {
    if (!value || value.zone !== zone.coordinator_room || !uint(value.total)
        || (value.revision != null && (!text(value.revision) || !value.revision.length))
        || (value.playing != null && (!uint(value.playing) || !value.playing || value.playing > value.total))
        || !Array.isArray(value.items)
        || value.items.length !== Math.min(LIMIT, Math.max(0, value.total - offset))) {
      throw new Error('The speaker queue is incomplete. Refresh to read it again.');
    }
    const items = value.items.map((row, index) => {
      if (!row || row.position !== offset + index + 1 || !text(row.title)
          || ['creator', 'album', 'uri'].some((k) => row[k] != null && !text(row[k]))) {
        throw new Error('The speaker queue positions disagree. Refresh before navigating.');
      }
      return Object.freeze({ position: row.position, title: row.title, creator: row.creator ?? '',
        album: row.album ?? '', uri: row.uri ?? null });
    });
    return Object.freeze({ total: value.total, revision: value.revision ?? null,
      playing: value.playing ?? null, offset, items: Object.freeze(items) });
  }
  function model({ call, changed = () => {} }) {
    let selected = null, identity = '', generation = 0, disposed = false, active = true;
    let dirty = true, inFlight = null;
    const state = { open: false, target: '', loading: false, stale: true, page: null, error: '' };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* View only. */ } } };
    const current = (v) => !disposed && active && state.open && generation === v;
    async function read(offset = state.page?.offset ?? 0) {
      if (disposed || !active || !state.open || !selected?.fresh || !uint(offset)
          || offset > 0xffffffff - LIMIT || inFlight) return false;
      const z = selected.zone, version = ++generation;
      const flight = {}; inFlight = flight; dirty = false;
      state.loading = true; state.stale = true; state.error = ''; publish();
      try {
        let page = decode(await call(`/zones/${encodeURIComponent(address(z))}/queue?offset=${offset}&limit=${LIMIT}`), z, offset);
        if (!current(version)) return false;
        // Removing the tail can invalidate a page. One bounded read returns to the start.
        if (offset && offset >= page.total) {
          page = decode(await call(`/zones/${encodeURIComponent(address(z))}/queue?offset=0&limit=${LIMIT}`), z, 0);
          if (!current(version)) return false;
        }
        state.page = page; state.stale = dirty; return true;
      } catch (error) {
        if (current(version)) state.error = [error?.message || 'Queue unavailable.', error?.hint].filter(Boolean).join(' ');
        return false;
      } finally {
        if (inFlight === flight) inFlight = null;
        if (current(version)) { state.loading = false; publish(); }
      }
    }
    return { state, read,
      observe(item) {
        if (disposed) return;
        const next = item ? group(item.zone) : '';
        selected = item;
        if (identity !== next) {
          identity = next; ++generation; dirty = true;
          state.target = item ? address(item.zone) : '';
          state.page = null; state.loading = false; state.stale = true; state.error = ''; publish();
        }
        if (!item?.fresh) { state.stale = true; dirty = true; publish(); return; }
        if (state.open && active && dirty && !inFlight) void read();
      },
      open() { if (disposed) return; state.open = true; dirty = true; publish(); void read(); },
      close() { ++generation; state.open = false; state.loading = false; state.stale = true; dirty = true; publish(); },
      invalidate() { if (!disposed) { dirty = true; state.stale = true; publish(); } },
      navigate(direction) {
        const p = state.page;
        if (state.loading || state.stale || !p || ![-1, 1].includes(direction)) return false;
        const offset = p.offset + direction * LIMIT;
        if (offset < 0 || offset >= p.total) return false;
        void read(offset); return true;
      },
      suspend() { active = false; ++generation; state.loading = false; dirty = true; state.stale = true; publish(); },
      resume() { if (!disposed) { active = true; dirty = true; } },
      dispose() { disposed = true; active = false; ++generation; },
    };
  }
  return { LIMIT, decode, model };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardQueue;
