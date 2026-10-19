// On-demand setup diagnosis. Report text and remedies never become commands.
'use strict';
const DiagnosticsRemote = (() => {
  const routes = Object.freeze({ diagnostics: ['GET', '/doctor'], diagnosticsPolicy: ['GET', '/policy'] });
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

  function model({ read, changed = () => {}, now = Date.now, validate = reportOf,
    receipt = 'Report received. No fixes were applied and no playback command was sent.' }) {
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
        const report = validate(await read(flight.signal));
        if (!current(version)) return false;
        state.report = report; state.receivedAt = now(); state.stale = false;
        state.message = receipt;
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

  // Display the daemon's policy, not an independently implemented guard. In
  // particular, an absent allow list and an explicitly empty one differ.
  function policyOf(raw) {
    const volume = v => uint(v) && v <= 100;
    const named = v => text(v, 2048) && v.trim() !== '';
    const optional = (v, check) => v == null || check(v);
    const minute = v => typeof v === 'string' && /^([01]\d|2[0-3]):[0-5]\d$/.test(v);
    const invalid = () => fail('The policy response is incomplete. Read it again before relying on its limits.');
    if (!object(raw) || !named(raw.you) || typeof raw.you_are_capped !== 'boolean'
        || typeof raw.quiet_hours_now !== 'boolean' || !object(raw.defaults)
        || !volume(raw.defaults.max_volume) || !volume(raw.defaults.max_step)
        || !uint(raw.defaults.fade_secs) || raw.defaults.fade_secs > 0xffffffff
        || !Array.isArray(raw.rooms) || raw.rooms.length > 2048
        || !Array.isArray(raw.clients) || raw.clients.length > 2048) invalid();
    let budget = raw.you.length;
    const strings = value => {
      if (!Array.isArray(value) || value.length > 2048 || !value.every(named)) invalid();
      budget += value.reduce((sum, v) => sum + v.length, 0);
      if (budget > MAX_BYTES) invalid();
      return [...value];
    };
    let quiet = null;
    if (raw.quiet_hours != null) {
      const q = raw.quiet_hours;
      if (!object(q) || !minute(q.start) || !minute(q.end) || !volume(q.max_volume)) invalid();
      quiet = { start: q.start, end: q.end, max_volume: q.max_volume };
    } else if (raw.quiet_hours_now) invalid();
    const roomNames = new Set(), clientNames = new Set();
    const rooms = raw.rooms.map(room => {
      if (!object(room) || !named(room.room) || roomNames.has(room.room)
          || !optional(room.max_volume, volume) || !optional(room.max_step, volume)) invalid();
      roomNames.add(room.room); strings([room.room]);
      return { room: room.room, max_volume: room.max_volume ?? null, max_step: room.max_step ?? null };
    });
    const clients = raw.clients.map(client => {
      if (!object(client) || !named(client.client) || clientNames.has(client.client.toLowerCase())
          || !optional(client.capped, v => typeof v === 'boolean')) invalid();
      clientNames.add(client.client.toLowerCase()); strings([client.client]);
      return { client: client.client, allow: client.allow == null ? null : strings(client.allow),
        deny: strings(client.deny), capped: client.capped ?? null };
    });
    return frozen({ you: raw.you, you_are_capped: raw.you_are_capped,
      quiet_hours_now: raw.quiet_hours_now, defaults: { max_volume: raw.defaults.max_volume,
        max_step: raw.defaults.max_step, fade_secs: raw.defaults.fade_secs }, quiet_hours: quiet, rooms, clients });
  }

  function policyModel(options) {
    return model({ ...options, validate: policyOf,
      receipt: 'Policy received. No settings or permissions were changed.' });
  }

  function policyPanel({ read, el, document: doc = document }) {
    const toggle = el('summary', { text: 'Why volume is capped or a command is denied' });
    const details = el('details', { class: 'policy-panel source-player' }, toggle);
    const node = el('section', { class: 'scenes house-policy', 'aria-label': 'House policy and volume caps' }, details);
    const run = el('button', { type: 'button', text: 'Read house policy' });
    const close = el('button', { type: 'button', text: 'Close policy' });
    const status = el('p', { role: 'status', 'aria-live': 'polite' });
    const summary = el('p', { class: 'policy-summary by' });
    const facts = el('div', { class: 'policy-facts' });
    const control = policyModel({ read, changed: render });
    let shown = null;
    function render(state) {
      run.disabled = state.busy || !state.open;
      run.textContent = state.busy ? 'Reading house policy…' : state.report ? 'Refresh house policy' : 'Read house policy';
      status.textContent = state.message || (state.busy ? 'Reading the policy for this request’s caller…' : 'Policy reads are independent of diagnostic checks.');
      summary.textContent = state.report ? `${state.stale ? 'Previous policy — not current. ' : ''}Received ${new Date(state.receivedAt).toLocaleString()} (browser local). Quiet-hours status was reported by the daemon, not calculated here.` : 'No policy has been read here.';
      if (shown === state.report) return;
      shown = state.report; facts.replaceChildren();
      if (!shown) return;
      const value = shown, defaults = value.defaults, q = value.quiet_hours;
      facts.append(el('h3', { text: 'This request’s caller' }),
        el('p', { text: value.you }), el('p', { text: value.you_are_capped
          ? 'The daemon says volume caps apply to this caller.' : 'The daemon says this caller is uncapped. Tool restrictions still apply.' }),
        el('h3', { text: 'Configured house limits' }),
        el('p', { text: `Default maximum room volume: ${defaults.max_volume} / 100. Largest single increase: ${defaults.max_step}. Scheduled volume ramp: ${defaults.fade_secs} seconds.` }),
        el('p', { text: q ? `Quiet hours: ${q.start}–${q.end} on the house clock; maximum volume ${q.max_volume} / 100. ${value.quiet_hours_now ? 'In effect when read.' : 'Not in effect when read.'}` : 'No quiet-hours window is configured.' }));
      const rooms = el('details', {}, el('summary', { text: `${value.rooms.length} room-specific limits` }));
      rooms.append(el('p', { text: 'These are configured room labels, not destinations selected for an action. An absent room limit inherits the default; quiet hours can lower the cap.' }));
      for (const room of value.rooms) rooms.append(el('p', { text: `${room.room}: maximum volume ${room.max_volume ?? `default (${defaults.max_volume})`}; largest increase ${room.max_step ?? `default (${defaults.max_step})`}.` }));
      const clients = el('details', {}, el('summary', { text: `${value.clients.length} explicit client rule entries` }));
      clients.append(el('p', { text: 'Lists can name tools or tool classes. Deny rules and checks of nested operations still apply. No allow list shown does not mean unrestricted access.' }));
      for (const client of value.clients) {
        const item = el('section', {}, el('h4', { text: client.client }),
          el('p', { text: client.allow === null ? 'Allow: no explicit list in this entry; base rules still apply.'
            : client.allow.length ? `Allow only: ${client.allow.join(', ')}.` : 'Allow: an explicitly empty list — no tools.' }),
          el('p', { text: client.deny.length ? `Deny: ${client.deny.join(', ')}.` : 'Deny: no explicit exclusions in this entry.' }),
          el('p', { text: client.capped === null ? 'Caps: inherit caller defaults.' : client.capped ? 'Caps: explicitly enabled.' : 'Caps: explicitly disabled.' }));
        clients.append(item);
      }
      facts.append(rooms, clients);
    }
    details.append(el('div', {},
      el('p', { text: 'Inspect the same policy that the daemon enforces for this browser request. This view cannot edit limits, change identity or grant permissions. The daemon decides each command when it is executed; a cached policy view is not a command approval.' }),
      el('div', { class: 'row' }, run, close), status, summary, facts));
    run.addEventListener('click', () => { void control.run(); });
    close.addEventListener('click', () => { details.open = false; control.close(); toggle.focus(); });
    details.addEventListener('toggle', event => { if (event.target === details) { if (details.open) control.open(); else control.close(); } });
    const away = () => control.close();
    const back = () => { if (!doc.hidden && details.open) control.open(); };
    const visibility = () => { if (doc.hidden) away(); else back(); };
    doc.addEventListener('visibilitychange', visibility);
    doc.defaultView?.addEventListener('pagehide', away);
    doc.defaultView?.addEventListener('pageshow', back);
    render(control.state);
    return { node, model: control, dispose() {
      doc.removeEventListener('visibilitychange', visibility);
      doc.defaultView?.removeEventListener('pagehide', away);
      doc.defaultView?.removeEventListener('pageshow', back);
      control.dispose();
    } };
  }

  return { routes, reportOf, visible, reader, model, panel, policyOf, policyModel, policyPanel, TIMEOUT, MAX_BYTES };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DiagnosticsRemote;
else {
  const diagnosticHost = document.getElementById('households');
  if (diagnosticHost) {
    diagnosticHost.before(DiagnosticsRemote.panel({ el }).node);
    if (typeof call === 'function' && typeof ROUTES !== 'undefined') {
      Object.assign(ROUTES, DiagnosticsRemote.routes);
      diagnosticHost.before(DiagnosticsRemote.policyPanel({ el,
        read: signal => call('diagnosticsPolicy', { signal }) }).node);
    }
  }
}
