// Inspect the daemon's action log without inferring what can be restored now.
'use strict';

const HistoryRemote = (() => {
  const routes = { historyRead: ['GET', '/actions'] };
  const text = (v, max = 65536) => typeof v === 'string' && v.length <= max;
  const id = (v) => Number.isSafeInteger(v) && v > 0;
  const instant = (v) => Number.isSafeInteger(v) && Math.abs(v) <= 8640000000000;
  const message = (e) => e && typeof e.message === 'string'
    ? `${e.message}${text(e.hint) && e.hint ? ` ${e.hint}` : ''}` : 'The request failed.';

  function actionOf(v) {
    if (!v || !id(v.id) || !instant(v.at) || !text(v.client, 2048) || !text(v.surface, 256)
        || !text(v.intent) || !text(v.decision) || !text(v.result) || typeof v.undoable !== 'boolean'
        || (v.undo_of != null && (!id(v.undo_of) || v.undo_of === v.id || v.undoable))) {
      throw new Error('An action in the log is unreadable. Refresh.');
    }
    return Object.freeze({ id: v.id, at: v.at, client: v.client, surface: v.surface,
      intent: v.intent, decision: v.decision, result: v.result,
      undoable: v.undoable, undo_of: v.undo_of ?? null });
  }

  function actionsOf(value, limit) {
    if (!Array.isArray(value) || value.length > limit) throw new Error('The action list is unreadable or too large.');
    const seen = new Set();
    return value.map((v) => {
      const row = actionOf(v);
      if (seen.has(row.id)) throw new Error('The action list contains duplicate IDs. Refresh.');
      seen.add(row.id);
      return row;
    }); // Keep the store's order; timestamps can move backwards with its clock.
  }

  function visible(rows, query) {
    const needle = query.trim().toLocaleLowerCase();
    return rows.filter((r) => !needle || [r.id, r.client, r.surface, r.intent, r.decision, r.result]
      .some((v) => String(v).toLocaleLowerCase().includes(needle)));
  }

  function model({ call, changed = () => {} }) {
    let version = 0, disposed = false;
    const state = { open: false, rows: [], client: '', limit: 20, search: '', loaded: false,
      loading: false, stale: false, error: '' };
    const publish = () => { if (!disposed) changed(state); };
    const current = (v) => !disposed && state.open && version === v;
    async function refresh() {
      if (disposed) return false;
      state.open = true;
      const v = ++version;
      const selected = { limit: state.limit, client: state.client };
      state.loading = true; state.stale = state.loaded; state.error = ''; publish();
      try {
        const query = { limit: selected.limit, ...(selected.client ? { client: selected.client } : {}) };
        const rows = actionsOf(await call('historyRead', { query }), selected.limit);
        if (!current(v)) return false;
        if (selected.client && rows.some((r) => r.client !== selected.client)) throw new Error('The reply does not match the selected client.');
        state.rows = rows; state.loaded = true; state.stale = false;
        return true;
      } catch (e) {
        if (current(v)) state.error = message(e);
        return false;
      } finally {
        if (current(v)) { state.loading = false; publish(); }
      }
    }
    return { state, refresh,
      filters(client, limit) {
        if (disposed || !text(client, 2048) || ![20, 100, 500].includes(limit)) return false;
        ++version; state.client = client.trim(); state.limit = limit;
        state.rows = []; state.loaded = false; state.stale = false; state.loading = false; state.error = '';
        publish(); return true;
      },
      search(query) { if (!disposed && text(query, 2048)) { state.search = query; publish(); } },
      close() { ++version; state.open = false; state.loading = false; state.stale = state.loaded; publish(); },
      dispose() { disposed = true; ++version; } };
  }

  function panel({ call, el }) {
    const content = el('div', { class: 'automation-body history-body' });
    const node = el('details', { class: 'automation scenes history' }, el('summary', { text: 'Action history' }), content);
    const control = model({ call, changed: render });
    function render(s) {
      const active = document.activeElement;
      const focus = content.contains(active) ? active.getAttribute('data-history-focus') : null;
      const start = active?.selectionStart, end = active?.selectionEnd;
      const opened = new Set([...content.querySelectorAll('details[open]')].map((n) => n.dataset.actionId));
      content.replaceChildren(); content.setAttribute('aria-busy', String(s.loading));
      const client = el('input', { type: 'text', value: s.client, maxlength: 2048,
        'aria-label': 'Exact client name', 'data-history-focus': 'client', placeholder: 'All clients',
        onchange: (e) => { control.filters(e.target.value, s.limit); control.refresh(); } });
      const limit = el('select', { 'aria-label': 'Number of recent actions', 'data-history-focus': 'limit',
        onchange: (e) => { control.filters(s.client, Number(e.target.value)); control.refresh(); } },
        ...[20, 100, 500].map((n) => el('option', { value: n, text: `Newest ${n}` })));
      limit.value = String(s.limit);
      content.append(el('p', { text: 'Who changed the music, what was allowed or limited, and what happened. Times below use this browser’s local time.' }),
        el('div', { class: 'history-filters' }, el('label', {}, 'Client (exact name)', client),
          el('label', {}, 'Recent actions', limit)),
        el('button', { type: 'button', text: s.loading ? 'Reading history…' : 'Refresh history',
          'data-history-focus': 'refresh', onclick: () => control.refresh() }),
        el('label', {}, 'Find in these loaded actions', el('input', { type: 'search', value: s.search,
          maxlength: 2048, 'aria-label': 'Find in loaded actions', 'data-history-focus': 'search',
          oninput: (e) => control.search(e.target.value) })));
      if (s.error) content.append(el('p', { role: 'alert', text: s.error }));
      if (s.stale) content.append(el('p', { role: 'status', text: 'Previously read actions, not a current view. Refresh to check for changes.' }));
      if (!s.loaded) { content.append(el('p', { text: s.loading ? 'Reading the action log…' : 'No action log has been read.' })); }
      else {
        const rows = visible(s.rows, s.search);
        const undone = new Map(s.rows.filter((r) => r.undo_of !== null).map((r) => [r.undo_of, r.id]));
        content.append(el('p', { role: 'status', text: `${rows.length} shown of ${s.rows.length} loaded. This is a recent window, not the full history.` }));
        const list = el('ol', { class: 'history-list', 'aria-label': 'Recent actions' });
        for (const row of rows) {
          const snapshot = undone.has(row.id) ? `An undo of this action appears as #${undone.get(row.id)}.`
            : row.undoable ? 'A before-state was recorded; this does not establish current undo eligibility.'
              : 'No undoable before-state was recorded.';
          const details = el('details', { 'data-action-id': row.id, open: opened.has(String(row.id)) },
            el('summary', { 'data-history-focus': `action-${row.id}`, text: `#${row.id} · ${row.intent || '(no intent reported)'}` }),
            el('dl', {}, el('dt', { text: 'Client / surface' }), el('dd', { text: `${row.client} / ${row.surface}` }),
              el('dt', { text: 'Time (browser local)' }), el('dd', { text: new Date(row.at * 1000).toLocaleString() }),
              el('dt', { text: 'Policy decision' }), el('dd', { text: row.decision }),
              el('dt', { text: 'Result' }), el('dd', { text: row.result })),
            el('p', { text: row.undo_of === null ? snapshot : `Undo attempt for action #${row.undo_of}. Read its result for any incomplete restoration.` }));
          list.append(el('li', {}, details));
        }
        if (!rows.length) content.append(el('p', { text: s.rows.length ? 'No loaded actions match this search.' : 'No actions returned for this client.' }));
        content.append(list);
      }
      const next = [...content.querySelectorAll('[data-history-focus]')].find((n) => n.getAttribute('data-history-focus') === focus);
      if (next) { next.focus({ preventScroll: true }); if (['client', 'search'].includes(focus) && Number.isInteger(start)) next.setSelectionRange(start, end); }
    }
    node.addEventListener('toggle', (e) => {
      if (e.target !== node) return;
      if (node.open) control.refresh(); else control.close();
    });
    render(control.state);
    return { node, dispose: control.dispose };
  }

  return { routes, actionOf, actionsOf, visible, model, panel };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = HistoryRemote;
else {
  Object.assign(ROUTES, HistoryRemote.routes);
  document.getElementById('scenes-section')?.after(HistoryRemote.panel({ call, el }).node);
}
