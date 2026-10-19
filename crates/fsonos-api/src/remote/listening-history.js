// Recorded listening is separate from the action log and from current playback.
'use strict';
const ListeningHistoryRemote = (() => {
  const routes = { listeningHistory: ['GET', '/history'] };
  const limits = Object.freeze([20, 100, 500]);
  const text = (v, max = 65536) => typeof v === 'string' && v.length <= max;
  const object = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);
  const message = (e) => e && text(e.message) ? [e.message, text(e.hint) ? e.hint : ''].filter(Boolean).join(' ') : 'The listening history could not be read.';

  function playsOf(value, limit) {
    if (!limits.includes(limit) || !Array.isArray(value) || value.length > limit) {
      throw new Error('The listening history is unreadable or exceeds the requested window.');
    }
    return Object.freeze(value.map((row) => {
      if (!object(row) || !text(row.source_uri) || !row.source_uri.trim()
          || !Number.isSafeInteger(row.played_at) || Math.abs(row.played_at) > 8640000000000
          || ['title', 'artist', 'room'].some((key) => row[key] != null && !text(row[key]))) {
        throw new Error('A recorded play is incomplete. Refresh listening history.');
      }
      return Object.freeze({ source_uri: row.source_uri, played_at: row.played_at,
        title: row.title ?? null, artist: row.artist ?? null, room: row.room ?? null });
    })); // Keep repeated listens and the daemon's order, including clock changes.
  }

  function visible(rows, query, room = '') {
    const needle = String(query).trim().toLocaleLowerCase();
    return rows.filter((row) => (!room || row.room === room) && (!needle
      || [row.title, row.artist, row.room, row.source_uri].some((s) => s?.toLocaleLowerCase().includes(needle))));
  }

  function model({ call, changed = () => {}, now = Date.now }) {
    let epoch = 0, disposed = false, active = null;
    const state = { open: false, loading: false, loaded: false, stale: false, rows: Object.freeze([]),
      limit: 20, query: '', room: '', observedAt: null, error: '' };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* View only. */ } } };
    const current = (v) => !disposed && state.open && epoch === v;
    function invalidate() { ++epoch; active?.abort(); active = null; state.loading = false; }

    async function refresh() {
      if (disposed) return false;
      invalidate(); state.open = true;
      const version = epoch, limit = state.limit, controller = new AbortController();
      active = controller;
      state.loading = true; state.stale = state.loaded; state.error = ''; publish();
      try {
        const rows = playsOf(await call('listeningHistory', { query: { limit }, signal: controller.signal }), limit);
        if (!current(version)) return false;
        state.rows = rows; state.loaded = true; state.stale = false; state.observedAt = now();
        return true;
      } catch (error) {
        if (current(version)) state.error = message(error);
        return false;
      } finally {
        if (current(version)) { active = null; state.loading = false; publish(); }
      }
    }
    function window(limit) {
      if (disposed || !limits.includes(limit)) return false;
      invalidate(); state.limit = limit; state.rows = Object.freeze([]); state.loaded = false;
      state.stale = false; state.observedAt = null; state.error = ''; publish(); return true;
    }
    function search(query, room = state.room) {
      if (disposed || !text(query, 2048) || !text(room)) return false;
      state.query = query; state.room = room; publish(); return true;
    }
    function close() {
      if (disposed) return;
      invalidate(); state.open = false; state.stale = state.loaded; publish();
    }
    function dispose() { close(); disposed = true; }
    return { state, refresh, window, search, close, dispose };
  }

  function panel({ call, el, document: doc = document }) {
    const search = el('input', { type: 'search', maxlength: 2048, 'aria-label': 'Find in loaded listening history',
      placeholder: 'Title, artist, room or source' });
    const room = el('select', { 'aria-label': 'Recorded room label' });
    const limit = el('select', { 'aria-label': 'Number of recent plays' },
      ...limits.map((n) => el('option', { value: n, text: `Newest ${n} plays` })));
    const refresh = el('button', { type: 'button', text: 'Refresh listening history' });
    const status = el('p', { role: 'status', 'aria-live': 'polite' });
    const error = el('p', { role: 'alert' });
    const list = el('ol', { class: 'listening-history-list', 'aria-label': 'Recorded listening' });
    const body = el('div', { class: 'listening-history-body' },
      el('p', { text: 'Find music the daemon recorded, separately from the action log. This recent window is not your complete listening history. Repeated plays remain separate.' }),
      el('div', { class: 'row' }, el('label', {}, 'Window ', limit), refresh),
      el('div', { class: 'row' }, el('label', {}, 'Find in loaded plays ', search), el('label', {}, 'Recorded room label ', room)),
      el('p', { class: 'by', text: 'Times use this browser’s local time. A recorded room label does not identify its household or today’s group. It never chooses a playback destination.' }),
      status, error, list);
    const details = el('details', { class: 'listening-history' }, el('summary', { text: 'Recently played · find music again' }), body);
    // The studio already categorizes music-library tools into Music & DJ.
    const node = el('section', { class: 'scenes', 'aria-label': 'Music library listening history' }, details);
    let control, rows = null;
    function render(state) {
      body.setAttribute('aria-busy', String(state.loading));
      limit.value = String(state.limit); refresh.disabled = state.loading;
      if (rows !== state.rows) {
        rows = state.rows;
        const labels = [...new Set(rows.map((r) => r.room).filter(Boolean))];
        if (state.room && !labels.includes(state.room)) labels.push(state.room);
        room.replaceChildren(el('option', { value: '', text: 'All recorded labels' }),
          ...labels.map((label) => el('option', { value: label, text: label })));
      }
      room.value = state.room; error.textContent = state.error;
      const shown = visible(state.rows, state.query, state.room);
      status.textContent = state.loading ? 'Reading recorded listening…' : !state.loaded ? 'Open or refresh to read recorded listening.'
        : `${state.stale ? 'Stale snapshot' : 'Observed'} ${new Date(state.observedAt).toLocaleString()}. ${shown.length} shown of ${state.rows.length} loaded; requested newest ${state.limit}.`;
      list.replaceChildren();
      for (const row of shown) {
        const when = new Date(row.played_at * 1000);
        list.append(el('li', {}, el('strong', { text: row.title || '(Title unavailable)' }),
          el('p', { class: 'by', text: [row.artist, row.room || 'Room no longer identified'].filter(Boolean).join(' · ') }),
          el('time', { datetime: when.toISOString(), text: when.toLocaleString() }),
          el('details', {}, el('summary', { text: 'Recorded source' }), el('code', { text: row.source_uri }))));
      }
      if (state.loaded && !shown.length) list.append(el('li', { text: state.rows.length
        ? 'No loaded plays match these filters. A larger window may include more music.'
        : 'No recorded plays returned. Playback that was not recorded will not appear here.' }));
    }
    control = model({ call, changed: render });
    search.addEventListener('input', () => control.search(search.value));
    room.addEventListener('change', () => control.search(search.value, room.value));
    limit.addEventListener('change', () => { if (control.window(Number(limit.value))) void control.refresh(); });
    refresh.addEventListener('click', () => { void control.refresh(); });
    details.addEventListener('toggle', (event) => {
      if (event.target !== details) return;
      if (details.open) void control.refresh(); else control.close();
    });
    const closeWhenHidden = () => { if (doc.hidden) control.close(); };
    doc.addEventListener('visibilitychange', closeWhenHidden);
    render(control.state);
    return { node, model: control, dispose: () => { doc.removeEventListener('visibilitychange', closeWhenHidden); control.dispose(); } };
  }
  return { routes, limits, playsOf, visible, model, panel };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = ListeningHistoryRemote;
else {
  Object.assign(ROUTES, ListeningHistoryRemote.routes);
  const listeningPanel = ListeningHistoryRemote.panel({ call, el });
  document.getElementById('households')?.before(listeningPanel.node);
}
