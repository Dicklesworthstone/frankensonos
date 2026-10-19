// On-demand setup diagnosis. Report text and remedies never become commands.
'use strict';
const DiagnosticsRemote = (() => {
  const routes = Object.freeze({ diagnostics: ['GET', '/doctor'] });
  const STATUSES = ['pass', 'warn', 'fail', 'skip'];
  const MAX_BYTES = 2 * 1024 * 1024, TIMEOUT = 120000;
  const object = v => v !== null && typeof v === 'object' && !Array.isArray(v);
  const text = (v, max = 65536) => typeof v === 'string' && v.length <= max;
  const uint = v => Number.isSafeInteger(v) && v >= 0;
  const fail = message => { throw new Error(message); };
  const problem = error => [error?.message || 'The diagnostic read failed.', error?.hint].filter(v => text(v)).join(' ');
  function frozen(value) {
    if (value && typeof value === 'object') {
      Object.values(value).forEach(frozen); Object.freeze(value);
    }
    return value;
  }
  function reportOf(raw) {
    if (!object(raw) || raw.schema !== 1 || !object(raw.counts)
        || !Array.isArray(raw.checks) || raw.checks.length > 2048) fail('The diagnostic report has an unsupported or incomplete shape.');
    const counts = { pass: 0, warn: 0, fail: 0, skip: 0 }, ids = new Set();
    let size = 0;
    const checks = raw.checks.map(row => {
      if (!object(row) || !text(row.id, 1024) || !row.id.trim() || ids.has(row.id)
          || !text(row.title) || !row.title.trim() || !STATUSES.includes(row.status)
          || !text(row.summary) || !uint(row.duration_ms)
          || (row.detail != null && !text(row.detail)) || (row.remedy != null && !text(row.remedy))) {
        fail('A diagnostic check is incomplete or has a duplicate identity.');
      }
      ids.add(row.id); counts[row.status]++;
      // Keep arbitrary JSON evidence, but do not recursively render it as UI.
      let evidence;
      try { evidence = JSON.stringify(row.evidence ?? null); }
      catch (_) { fail('Diagnostic evidence is not readable JSON.'); }
      if (!text(evidence, MAX_BYTES)) fail('Diagnostic evidence is too large.');
      size += row.id.length + row.title.length + row.summary.length
        + (row.detail?.length || 0) + (row.remedy?.length || 0) + evidence.length;
      if (size > MAX_BYTES) fail('The diagnostic report exceeds the display limit.');
      return Object.freeze({ id: row.id, title: row.title, status: row.status,
        summary: row.summary, detail: row.detail ?? null, remedy: row.remedy ?? null,
        evidence, duration_ms: row.duration_ms });
    });
    if (STATUSES.some(status => !uint(raw.counts[status]) || raw.counts[status] !== counts[status])
        || raw.exit_code !== (counts.fail ? 7 : counts.warn ? 6 : 0)) fail('The diagnostic totals disagree with the check results.');
    return frozen({ schema: 1, exit_code: raw.exit_code, counts, checks });
  }
  function visible(report, filter, query) {
    const needle = query.trim().toLocaleLowerCase();
    return (report?.checks || []).filter(row => (filter === 'all' || filter === row.status
      || (filter === 'attention' && row.status !== 'pass')) && (!needle
      || [row.id, row.title, row.summary, row.detail, row.remedy].some(v => v?.toLocaleLowerCase().includes(needle))));
  }

  // Doctor can legitimately outlast a normal state GET. Use a fixed endpoint,
  // one bounded response and no automatic retry of this expensive read. An
  // ignored abort never frees the underlying flight for a duplicate request.
  function reader({ fetcher = (...args) => fetch(...args), schedule = setTimeout, cancel = clearTimeout } = {}) {
    let busy = false;
    return async function read(signal) {
      if (busy) fail('The previous diagnostic request is still settling. Do not start another check yet.');
      if (signal?.aborted) fail('The diagnostic read was canceled before sending.');
      busy = true;
      const controller = new AbortController();
      let rejectStopped, timer;
      const stopped = new Promise((_, reject) => { rejectStopped = reject; });
      const stop = message => { rejectStopped(new Error(message)); controller.abort(); };
      const abort = () => stop('The diagnostic read was canceled. Checks on the daemon may still finish.');
      signal?.addEventListener('abort', abort, { once: true });
      timer = schedule(() => stop('No complete diagnostic report arrived before the deadline. Checks may still be running; no playback command was sent.'), TIMEOUT);
      const operation = async () => {
        let stream;
        try {
          const response = await fetcher(routes.diagnostics[1], { method: 'GET',
            headers: { Accept: 'application/json' }, credentials: 'same-origin',
            cache: 'no-store', redirect: 'error', signal: controller.signal });
          if (controller.signal.aborted) fail('Diagnostic read canceled.');
          const declared = Number(response.headers.get('content-length'));
          if (Number.isFinite(declared) && declared > MAX_BYTES) fail('The diagnostic response is too large.');
          if (!response.body?.getReader) fail('The daemon returned no readable diagnostic body.');
          stream = response.body.getReader();
          const decoder = new TextDecoder('utf-8', { fatal: true });
          let body = '', bytes = 0;
          for (;;) {
            const { done, value } = await stream.read();
            if (controller.signal.aborted) fail('Diagnostic read canceled.');
            if (done) break;
            bytes += value.byteLength;
            if (bytes > MAX_BYTES) fail('The diagnostic response is too large.');
            body += decoder.decode(value, { stream: true });
          }
          body += decoder.decode();
          let result;
          try { result = JSON.parse(body); }
          catch (_) { fail(`The daemon returned an unreadable diagnostic response (${response.status}).`); }
          if (!response.ok) {
            const error = new Error(text(result?.detail) && result.detail ? result.detail : `Diagnostic request failed (${response.status}).`);
            if (text(result?.hint)) error.hint = result.hint;
            error.status = response.status; throw error;
          }
          return result;
        } finally {
          // Releasing a response reader need not wait for a stalled producer.
          controller.abort();
          if (stream) { void stream.cancel().catch(() => {}); stream.releaseLock(); }
          busy = false;
        }
      };
      try { return await Promise.race([operation(), stopped]); }
      finally { cancel(timer); signal?.removeEventListener('abort', abort); }
    };
  }

  function model({ read, changed = () => {}, now = Date.now }) {
    let epoch = 0, disposed = false, active = null;
    const state = { open: false, busy: false, report: null, receivedAt: null,
      stale: false, filter: 'attention', query: '', message: '' };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* View only. */ } } };
    const current = version => !disposed && state.open && epoch === version;
    async function run() {
      if (disposed || !state.open || state.busy) return false;
      const version = ++epoch, flight = new AbortController(); active = flight;
      state.busy = true; state.stale = state.report !== null; state.message = ''; publish();
      try {
        const report = reportOf(await read(flight.signal));
        if (!current(version)) return false;
        state.report = report; state.receivedAt = now(); state.stale = false;
        state.message = 'Report received. No fixes were applied and no playback command was sent.';
        return true;
      } catch (error) { if (current(version)) state.message = problem(error); return false; }
      finally { if (active === flight) active = null; state.busy = false; publish(); }
    }
    function close() {
      ++epoch; state.open = false; state.stale = state.report !== null;
      active?.abort(); publish();
    }
    return { state, run,
      open() { if (!disposed) { state.open = true; publish(); } }, close,
      filter(value) { if (!disposed && ['all', 'attention', ...STATUSES].includes(value)) { state.filter = value; publish(); } },
      search(value) { if (!disposed && text(value, 1024)) { state.query = value; publish(); } },
      dispose() { close(); disposed = true; },
    };
  }

  function panel({ read = reader(), el, document: doc = document }) {
    const toggle = el('summary', { text: 'Diagnose missing rooms or silent playback' });
    const details = el('details', { class: 'diagnostics-panel source-player' }, toggle);
    const node = el('section', { class: 'scenes diagnostics', 'aria-label': 'House diagnostics' }, details);
    const run = el('button', { type: 'button', text: 'Run diagnostics' });
    const close = el('button', { type: 'button', text: 'Close diagnostics' });
    const filter = el('select', { 'aria-label': 'Diagnostic result filter' },
      ...[['attention', 'Warnings, failures and skipped'], ['all', 'All checks'], ['fail', 'Failed'],
        ['warn', 'Warnings'], ['skip', 'Skipped'], ['pass', 'Passed']].map(([value, label]) => el('option', { value, text: label })));
    const search = el('input', { type: 'search', maxlength: 1024, 'aria-label': 'Find diagnostic checks', placeholder: 'Room, Spotify, callback, Tailscale…' });
    const status = el('p', { role: 'status', 'aria-live': 'polite' });
    const summary = el('p', { class: 'diagnostics-summary' });
    const results = el('div', { class: 'diagnostics-results' });
    const control = model({ read, changed: render });
    let rendered = null;
    function render(state) {
      run.disabled = state.busy || !state.open;
      run.textContent = state.busy ? 'Checking the daemon’s setup…' : state.report ? 'Run diagnostics again' : 'Run diagnostics';
      filter.value = state.filter;
      status.textContent = state.message || (state.busy ? 'Waiting for a complete report. Playback controls remain independent.' : 'Checks run only when requested here.');
      const report = state.report;
      if (!report) { summary.textContent = 'No diagnostic report has been read here.'; return; }
      const c = report.counts;
      summary.textContent = `${state.stale ? 'Previous report — not current. ' : ''}Received ${new Date(state.receivedAt).toLocaleString()} (browser local): ${c.fail} failed, ${c.warn} warned, ${c.pass} passed, ${c.skip} skipped. ${report.checks.length ? 'Skipped checks did not run; they are not passes.' : 'No checks were reported.'}`;
      const key = [report, state.filter, state.query];
      if (rendered && rendered.every((v, i) => v === key[i])) return;
      rendered = key;
      results.replaceChildren();
      const rows = visible(report, state.filter, state.query);
      results.append(el('p', { text: `${rows.length} of ${report.checks.length} checks shown. Results retain the daemon’s prerequisite order.` }));
      for (const row of rows) {
        const item = el('details', { class: `diagnostics-check diagnostics-${row.status}`, open: row.status === 'fail' || row.status === 'warn' },
          el('summary', { text: `${row.status.toUpperCase()} · ${row.title}` }),
          el('p', { text: row.summary }), el('p', { class: 'by', text: `${row.id} · ${row.duration_ms} ms` }));
        if (row.detail) item.append(el('p', { text: row.detail }));
        if (row.remedy) item.append(el('h4', { text: 'Suggested remedy — not executed' }), el('p', { text: row.remedy }));
        if (row.evidence !== 'null') item.append(el('details', {}, el('summary', { text: 'Observed evidence · review before sharing' }),
          el('p', { text: row.evidence })));
        results.append(item);
      }
    }
    details.append(el('div', { class: 'diagnostics-body' },
      el('p', { text: 'Run the daemon’s read-only setup checks for its speakers, library and connection. These inspect the daemon’s host, not this phone’s network. Remedies are instructions to review, never buttons that execute them.' }),
      el('p', { class: 'by', text: 'A recent report may be reused by the daemon. A returned report is not a listening test or proof that every service was checked.' }),
      el('div', { class: 'row' }, run, close), status, summary,
      el('div', { class: 'diagnostics-fields' }, el('label', {}, 'Show results', filter), el('label', {}, 'Find in the report', search)), results));
    run.addEventListener('click', () => { void control.run(); });
    close.addEventListener('click', () => { details.open = false; control.close(); toggle.focus(); });
    filter.addEventListener('change', () => control.filter(filter.value));
    search.addEventListener('input', () => control.search(search.value));
    details.addEventListener('toggle', event => { if (event.target === details) { if (details.open) control.open(); else control.close(); } });
    const hide = () => { if (doc.hidden) control.close(); else if (details.open) control.open(); };
    const away = () => control.close();
    const back = () => { if (!doc.hidden && details.open) control.open(); };
    doc.addEventListener('visibilitychange', hide);
    doc.defaultView?.addEventListener('pagehide', away);
    doc.defaultView?.addEventListener('pageshow', back);
    render(control.state);
    return { node, model: control, dispose() {
      doc.removeEventListener('visibilitychange', hide);
      doc.defaultView?.removeEventListener('pagehide', away);
      doc.defaultView?.removeEventListener('pageshow', back);
      control.dispose();
    } };
  }
  return { routes, reportOf, visible, reader, model, panel, TIMEOUT, MAX_BYTES };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DiagnosticsRemote;
else {
  const diagnosticHost = document.getElementById('households');
  if (diagnosticHost) diagnosticHost.before(DiagnosticsRemote.panel({ el }).node);
}
