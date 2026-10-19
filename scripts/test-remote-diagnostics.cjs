'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const fs = require('node:fs');
const path = require('node:path');
const D = require('../crates/fsonos-api/src/remote/studio-diagnostics.js');
const Studio = require('../crates/fsonos-api/src/remote/studio.js');
const row = (id, status = 'pass') => ({ id, status, title: id, summary: 'Observed result',
  detail: null, remedy: null, evidence: null, duration_ms: 3 });
const report = (checks = [row('lan.ssdp'), { ...row('spotify.render_params.s1', 'fail'),
  remedy: 'Add a Spotify favorite in the S1 app, then check again.', evidence: { generation: 'S1', linked: false } },
  { ...row('lan.gena_callback', 'warn'), detail: 'One callback was not received.' }, row('spotify.enqueue.s1', 'skip')]) => {
  const counts = { pass: 0, warn: 0, fail: 0, skip: 0 };
  checks.forEach(c => counts[c.status]++);
  return { schema: 1, checks, counts, exit_code: counts.fail ? 7 : counts.warn ? 6 : 0 };
};
const deferred = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; };
const tick = () => new Promise(resolve => setImmediate(resolve));
function clock() {
  const tasks = new Map(); let next = 0;
  return { tasks, schedule(fn, delay) { const id = ++next; tasks.set(id, { fn, delay }); return id; }, cancel(id) { tasks.delete(id); } };
}
const json = (body, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });

test('doctor report preserves prerequisite order, distinct S1/S2 evidence and unknown check ids', () => {
  const raw = report([row('future.check'), row('spotify.render_params.s1'), row('spotify.render_params.s2', 'skip')]);
  const got = D.reportOf(raw);
  assert.deepEqual(got.checks.map(r => r.id), raw.checks.map(r => r.id));
  assert.equal(got.counts.pass, 2); assert.equal(got.counts.skip, 1); assert.equal(got.exit_code, 0);
  assert.throws(() => { got.checks[0].summary = 'changed'; });
  raw.checks[0].summary = 'later'; assert.notEqual(got.checks[0].summary, 'later');
});

test('empty and all-skipped reports are valid but retain zero actual passes', () => {
  for (const checks of [[], [row('unavailable', 'skip')]]) {
    const got = D.reportOf(report(checks)); assert.equal(got.counts.pass, 0); assert.equal(got.exit_code, 0);
  }
});

test('check failures are data in a successful GET, not request errors', async () => {
  const m = D.model({ read: async () => report(), now: () => 1234 }); m.open();
  assert.equal(await m.run(), true); assert.equal(m.state.report.exit_code, 7);
  assert.equal(m.state.receivedAt, 1234); assert.equal(m.state.stale, false);
});

test('unsupported versions, inconsistent counts and exit codes cannot show a healthy report', () => {
  const cases = [null, [], {}, { ...report(), schema: 2 }, { ...report(), exit_code: 0 },
    { ...report(), counts: { pass: 99, fail: 1, warn: 1, skip: 1 } },
    { ...report(), checks: [row('x'), row('x')] }, { ...report(), checks: Array(2049).fill(row('x')) }];
  for (const value of cases) assert.throws(() => D.reportOf(value));
  for (const patch of [{ id: '' }, { title: '' }, { status: 'ok' }, { status: '__proto__' },
    { summary: null }, { duration_ms: -1 }, { duration_ms: Number.MAX_SAFE_INTEGER + 1 },
    { detail: {} }, { remedy: false }, { id: 'x'.repeat(1025) }]) {
    assert.throws(() => D.reportOf(report([{ ...row('x'), ...patch }])));
  }
});

test('evidence remains literal JSON, detached and bounded', () => {
  const r = row('script'); r.evidence = { literal: '<img src=x onerror=alert(1)>', nested: ['a', 2, null] };
  const got = D.reportOf(report([r])); r.evidence.nested.push('later');
  assert.equal(JSON.parse(got.checks[0].evidence).nested.length, 3);
  assert.match(got.checks[0].evidence, /<img/);
  const cyclic = {}; cyclic.self = cyclic;
  assert.throws(() => D.reportOf(report([{ ...row('cycle'), evidence: cyclic }])));
  assert.throws(() => D.reportOf(report([{ ...row('large'), evidence: 'x'.repeat(D.MAX_BYTES) }])));
});

test('local filtering never conflates skipped with passed and searches literal text', () => {
  const r = D.reportOf(report());
  assert.deepEqual(D.visible(r, 'attention', '').map(c => c.status), ['fail', 'warn', 'skip']);
  assert.deepEqual(D.visible(r, 'skip', '').map(c => c.id), ['spotify.enqueue.s1']);
  assert.equal(D.visible(r, 'all', 'FAVORITE').length, 1);
  assert.equal(D.visible(r, 'all', '.*').length, 0);
  assert.deepEqual(D.visible(null, 'all', ''), []);
});

test('mounting, opening and filtering send no request; checks are explicit and single flight', async () => {
  const pending = deferred(), calls = [];
  const m = D.model({ read: signal => { calls.push(signal); return pending.promise; } });
  assert.equal(await m.run(), false); m.open(); m.filter('all'); m.search('spotify'); assert.equal(calls.length, 0);
  const first = m.run(); assert.equal(await m.run(), false); assert.equal(calls.length, 1);
  pending.resolve(report()); assert.equal(await first, true); assert.equal(m.state.busy, false);
});

for (const event of ['close', 'dispose']) test(`${event} cancels view work and rejects late results without freeing a pending read early`, async () => {
  const pending = deferred(); let signal, calls = 0;
  const m = D.model({ read: s => { signal = s; calls++; return pending.promise; } }); m.open();
  const first = m.run(); m[event](); assert.equal(signal.aborted, true); m.open();
  assert.equal(await m.run(), false); pending.resolve(report());
  assert.equal(await first, false); assert.equal(m.state.report, null); assert.equal(calls, 1);
});

test('failed refresh retains a visibly stale prior report and the daemon remedy', async () => {
  let fail = false;
  const m = D.model({ read: async () => { if (fail) throw Object.assign(new Error('POLICY_DENIED'), { hint: 'Allow doctor for this caller.' }); return report(); } }); m.open();
  await m.run(); const before = m.state.report; fail = true;
  assert.equal(await m.run(), false); assert.equal(m.state.report, before); assert.equal(m.state.stale, true);
  assert.match(m.state.message, /Allow doctor/); m.close(); m.open(); assert.equal(m.state.stale, true);
});

test('unreadable refresh is not adopted and observer failures cannot affect data', async () => {
  let raw = report(); const m = D.model({ read: async () => raw, changed() { throw new Error('view'); } }); m.open();
  await m.run(); const before = m.state.report; raw = {};
  assert.equal(await m.run(), false); assert.equal(m.state.report, before); assert.equal(m.state.stale, true);
});

test('diagnostic transport has a fixed same-origin read path, long deadline and no action body', async () => {
  const timers = clock(), calls = [];
  const read = D.reader({ ...timers, fetcher: async (...args) => { calls.push(args); return json(report()); } });
  const pending = read(); assert.equal([...timers.tasks.values()][0].delay, 120000);
  assert.deepEqual(await pending, report()); assert.equal(timers.tasks.size, 0);
  const [url, init] = calls[0]; assert.equal(url, '/doctor'); assert.equal(init.method, 'GET');
  assert.equal(init.body, undefined); assert.equal(init.credentials, 'same-origin'); assert.equal(init.redirect, 'error'); assert.equal(init.cache, 'no-store');
});

for (const status of [401, 403, 429, 500, 503]) test(`HTTP ${status} retains its fix and is not automatically retried`, async () => {
  let calls = 0;
  const read = D.reader({ fetcher: async () => { calls++; return json({ detail: 'Check denied', hint: 'Read the house policy.' }, status); } });
  await assert.rejects(read(), e => e.status === status && e.hint === 'Read the house policy.'); assert.equal(calls, 1);
});

test('network and malformed successes fail without a retry', async () => {
  for (const value of [new Error('offline'), new Response('<html>unavailable</html>'), new Response(null, { status: 204 })]) {
    let calls = 0; const read = D.reader({ fetcher: async () => { calls++; if (value instanceof Error) throw value; return value; } });
    await assert.rejects(read()); assert.equal(calls, 1);
  }
});

test('deadline covers the body and ignored abort retains physical flight exclusion', async () => {
  const timers = clock(), pending = deferred(); let calls = 0;
  const read = D.reader({ ...timers, fetcher: async () => { calls++; return pending.promise; } });
  const first = read(); const rejected = assert.rejects(first, /deadline/);
  [...timers.tasks.values()][0].fn(); await rejected;
  await assert.rejects(read(), /still settling/); assert.equal(calls, 1);
  pending.resolve(json(report())); await tick();
  assert.equal(timers.tasks.size, 0);
});

test('abort before sending and mid-body cancellation never start a second diagnostic', async () => {
  const before = new AbortController(); before.abort(); let calls = 0;
  const read = D.reader({ fetcher: async () => { calls++; return json(report()); } });
  await assert.rejects(read(before.signal), /before sending/); assert.equal(calls, 0);
  const timers = clock(), stalled = deferred();
  const readBody = D.reader({ ...timers, fetcher: async () => ({ ok: true, status: 200, headers: new Headers(),
    body: { getReader: () => ({ read: () => stalled.promise, cancel: async () => {}, releaseLock() {} }) } }) });
  const control = new AbortController(); const first = readBody(control.signal); await tick();
  control.abort(); await assert.rejects(first, /canceled/); await assert.rejects(readBody(), /still settling/);
  stalled.resolve({ done: true }); await tick(); assert.equal(timers.tasks.size, 0);
});

test('both declared and streaming size limits cancel oversized replies', async () => {
  for (const declared of [true, false]) {
    let canceled = false;
    const body = new ReadableStream({ start(c) { c.enqueue(new Uint8Array(D.MAX_BYTES + 1)); }, cancel() { canceled = true; } });
    const read = D.reader({ fetcher: async () => new Response(body, { headers: declared ? { 'content-length': String(D.MAX_BYTES + 1) } : {} }) });
    await assert.rejects(read(), /too large/);
    // For streaming replies the reader itself is explicitly canceled.
    if (!declared) assert.equal(canceled, true);
  }
});

test('real localhost JSON round trip uses only the diagnostic GET, no room discovery dependency', async t => {
  const calls = [];
  const server = http.createServer(async (req, res) => { let body = ''; for await (const chunk of req) body += chunk;
    calls.push([req.method, req.url, body]); res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify(report())); });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => { server.closeAllConnections(); server.close(); });
  const read = D.reader({ fetcher: (url, init) => fetch(`http://127.0.0.1:${server.address().port}${url}`, init) });
  const m = D.model({ read }); m.open(); assert.equal(await m.run(), true);
  assert.deepEqual(calls, [['GET', '/doctor', '']]); assert.equal(m.state.report.counts.fail, 1);
});

test('diagnostics are embedded before navigation and searchable in Activity with no known groups', () => {
  const tool = Studio.identify({ id: '', getAttribute: () => 'House diagnostics', querySelector: () => ({ textContent: 'Diagnose missing rooms or silent playback' }) });
  assert.equal(tool.id, 'diagnostics'); assert.equal(tool.workspace, 'activity');
  for (const query of ['doctor', 'silence', 'setup', 'missing rooms', 'tailscale']) assert.ok(Studio.search(query, [tool]).length);
  const root = path.join(__dirname, '..');
  const rust = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/http/remote/studio.rs'), 'utf8');
  assert.match(rust, /\{DIAGNOSTICS_SCRIPT\}\\n\{SCRIPT\}/);
  for (const file of ['studio-diagnostics.js']) {
    const source = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/remote', file), 'utf8').toLowerCase();
    for (const forbidden of ['authorization', 'password', 'cookie', 'http://', 'https://', 'innerhtml', 'eval(']) assert.ok(!source.includes(forbidden), forbidden);
  }
});
