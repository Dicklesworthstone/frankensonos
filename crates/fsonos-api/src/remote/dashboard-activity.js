// Inspect real action provenance and undo only the daemon's reviewed candidate.
'use strict';
const DashboardActivity = (() => {
  const C = typeof DashboardCore !== 'undefined' ? DashboardCore : require('./dashboard-core.js');
  const text = (v, max = 65536) => typeof v === 'string' && v.length <= max;
  const id = (v) => Number.isSafeInteger(v) && v > 0;
  const fail = (message) => { throw new Error(message); };
  function actionOf(v) {
    if (!v || !id(v.id) || !Number.isSafeInteger(v.at) || Math.abs(v.at) > 8640000000000
        || !text(v.client, 2048) || !text(v.surface, 256) || !text(v.intent)
        || !text(v.decision) || !text(v.result) || typeof v.undoable !== 'boolean'
        || (v.undo_of != null && (!id(v.undo_of) || v.undo_of === v.id || v.undoable))) {
      fail('An action in the log is unreadable. Refresh activity.');
    }
    return Object.freeze({ id: v.id, at: v.at, client: v.client, surface: v.surface,
      intent: v.intent, decision: v.decision, result: v.result,
      undoable: v.undoable, undo_of: v.undo_of ?? null });
  }
  function actionsOf(value, limit) {
    if (!Array.isArray(value) || value.length > limit) fail('The action window is unreadable.');
    const rows = value.map(actionOf);
    if (new Set(rows.map((r) => r.id)).size !== rows.length) fail('The action window contains duplicate IDs.');
    return Object.freeze(rows); // Keep store order, even after a wall-clock correction.
  }
  function previewOf(value) {
    if (value === null) return null;
    const row = actionOf(value);
    if (!row.undoable || row.undo_of !== null || !row.client.trim() || !row.intent.trim()) {
      fail('The undo preview does not identify an eligible action.');
    }
    return row;
  }
  function resultOf(value, target) {
    const failures = value?.failures === undefined ? [] : value.failures;
    if (!value || value.undone !== target.id || value.intent !== target.intent
        || !text(value.summary) || !value.summary.trim() || !Array.isArray(failures)
        || failures.length > 2000 || !failures.every((v) => text(v) && v.trim())) {
      fail('The response did not confirm the reviewed undo.');
    }
    return Object.freeze({ undone: value.undone, summary: value.summary,
      failures: Object.freeze([...failures]), complete: failures.length === 0 });
  }
  const signature = (row) => JSON.stringify(row);
  function model({ call, controller, changed = () => {}, now = () => performance.now() }) {
    let active = true, disposed = false, version = 0, flight = null, dirty = true;
    let observedScope = '', writingScope = '', notBefore = 0;
    const state = { open: false, limit: 20, query: '', rows: [], loaded: false, stale: true,
      loading: false, historyError: '', previewError: '', previewLoaded: false, candidate: null,
      pending: null, writing: false, result: null, message: '' };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* View only. */ } } };
    const scope = () => {
      const item = controller.selected();
      return item ? C.shape(item.zone) : '';
    };
    const current = (v) => !disposed && active && state.open && v === version;
    const readable = () => !disposed && active && state.open && !state.writing
      && !controller.state.busy && !controller.state.loading;
    const canReview = () => readable() && !state.loading && state.previewLoaded
      && !!state.candidate && now() >= notBefore && controller.canControl();
    function retire() {
      ++version; dirty = true; state.stale = true; state.previewLoaded = false; state.pending = null;
    }
    async function read() {
      if (!readable() || flight || now() < notBefore) return false;
      const v = version, limit = state.limit, token = {};
      flight = token; dirty = false; state.loading = true; state.previewLoaded = false;
      state.historyError = state.previewError = ''; publish();
      try {
        const values = await Promise.allSettled([
          Promise.resolve().then(() => call(`/actions?limit=${limit}`)).then((v) => actionsOf(v, limit)),
          Promise.resolve().then(() => call('/undo/preview')).then(previewOf),
        ]);
        // A rate limit still binds an obsolete read and survives hide/show.
        for (const value of values) {
          const delay = value.status === 'rejected' ? value.reason?.retryAfter : 0;
          if (typeof delay === 'number' && delay > 0) notBefore = Math.max(notBefore, now() + delay);
        }
        if (!current(v)) return false;
        if (values[0].status === 'fulfilled') {
          state.rows = values[0].value; state.loaded = true; state.stale = false;
        } else { state.stale = true; state.historyError = C.problem(values[0].reason); }
        if (values[1].status === 'fulfilled') {
          state.candidate = values[1].value; state.previewLoaded = true;
        } else { state.candidate = null; state.previewError = C.problem(values[1].reason); }
        return values.every((value) => value.status === 'fulfilled');
      } finally {
        if (flight === token) { flight = null; state.loading = false; }
        publish();
        // Only an explicit new intent/invalidation queues another read. A failed
        // read never polls by itself, and repeated events retain only one refresh.
        if (dirty && readable()) void read();
      }
    }
    function refresh() {
      if (disposed || !active || state.writing) return Promise.resolve(false);
      state.open = true; retire(); publish(); return read();
    }
    function review() {
      if (!canReview() || state.pending || now() < notBefore) return null;
      state.result = null; state.message = '';
      state.pending = Object.freeze({ action: state.candidate, scope: scope(), version });
      publish(); return state.pending;
    }
    async function confirm(pending) {
      if (!canReview() || !pending || pending !== state.pending || pending.version !== version
          || pending.scope !== scope()) return false;
      const v = version;
      state.pending = null; state.writing = true; writingScope = pending.scope;
      state.previewLoaded = false; state.result = null; state.message = ''; publish();
      let sent = false;
      try {
        const ok = await controller.mutate(async ({ check, send }) => {
          const valid = () => {
            check();
            if (!current(v) || pending.scope !== scope()) fail('The undo review is no longer current.');
          };
          valid();
          const latest = previewOf(await call('/undo/preview'));
          valid();
          if (!latest || signature(latest) !== signature(pending.action)) {
            fail('The eligible action changed. Read a new preview and review it again.');
          }
          // Never force an undo, impersonate another caller or fall back to the
          // unguarded /undo endpoint. The daemon closes the final ID race.
          const response = send('/undo/checked', { expected_id: pending.action.id });
          sent = true;
          const result = resultOf(await response, pending.action);
          // A sent command's result is meaningful even after this panel hides.
          state.result = result;
          state.message = `${result.summary} ${result.complete ? 'No restoration failures reported.'
            : 'Partial restoration: inspect the failures below. No automatic retry.'}`;
          return { done: state.message, changed: true };
        });
        if (!ok) state.message = controller.state.message || 'No undo was sent. Refresh before trying again.';
        return ok;
      } finally {
        state.writing = false; writingScope = ''; retire(); state.candidate = null; publish();
        // Inspection cannot clear DashboardCore's uncertain-write lock. Only
        // the dashboard's explicit playback Refresh can do that.
        if (sent && readable()) void read();
      }
    }
    function observe() {
      if (disposed) return;
      const next = scope();
      if (next !== observedScope) {
        observedScope = next;
        if (state.pending || (state.writing && next !== writingScope)) retire();
      }
      if (state.pending && !controller.canControl()) state.pending = null;
      if (dirty && readable() && !flight) void read();
      publish();
    }
    return { state, refresh, review, confirm, canReview, observe,
      invalidate() { if (!disposed) { retire(); publish(); } },
      cancel() { if (!disposed) { ++version; state.pending = null; publish(); } },
      limit(value) {
        if (disposed || state.writing || ![20, 100].includes(value)) return false;
        state.limit = value; void refresh(); return true;
      },
      search(value) { if (!disposed && text(value, 512)) { state.query = value; publish(); } },
      visible() {
        const q = state.query.trim().toLocaleLowerCase();
        return state.rows.filter((r) => !q || [r.id, r.client, r.surface, r.intent, r.decision, r.result]
          .some((v) => String(v).toLocaleLowerCase().includes(q)));
      },
      close() { state.open = false; retire(); publish(); },
      suspend() { active = false; retire(); publish(); },
      resume() { if (!disposed && !active) { active = true; retire(); observe(); } },
      dispose() { disposed = true; active = false; state.open = false; retire(); },
    };
  }
  return { actionOf, actionsOf, previewOf, resultOf, model };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardActivity;
