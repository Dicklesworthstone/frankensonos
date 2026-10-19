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

const policy = (extra = {}) => ({ you: 'listener@example.test', you_are_capped: true, quiet_hours_now: true,
  defaults: { max_volume: 70, max_step: 20, fade_secs: 0 },
  quiet_hours: { start: '22:00', end: '07:00', max_volume: 25 },
  rooms: [{ room: 'Study@S1', max_volume: 40 }, { room: 'Study@S2', max_step: 0 }],
  clients: [{ client: 'listener@example.test', allow: ['read', 'set_volume'], deny: ['dj_start'], capped: true },
    { client: 'no-tools', allow: [], deny: [] }, { client: 'default-rules', deny: [], capped: false }], ...extra });

test('policy snapshots keep authoritative caller and house-clock flags without browser inference', () => {
  const raw = policy({ you: 'cli', you_are_capped: true });
  const checked = D.policyOf(raw); assert.equal(checked.you_are_capped, true, 'CLI name does not imply uncapped');
  assert.equal(checked.quiet_hours_now, true); assert.equal(checked.quiet_hours.start, '22:00');
  raw.rooms[0].max_volume = 99; raw.clients[0].allow.push('later');
  assert.equal(checked.rooms[0].max_volume, 40); assert.equal(checked.clients[0].allow.length, 2);
  assert.throws(() => checked.clients[0].deny.push('mutated'));
  assert.equal(D.policyOf(policy({ you: 'mcp-stdio', you_are_capped: false })).you_are_capped, false);
});

test('missing and explicitly empty allow lists remain distinct and optional caps inherit', () => {
  const checked = D.policyOf(policy());
  assert.deepEqual(checked.clients[1].allow, []); assert.equal(checked.clients[1].capped, null);
  assert.equal(checked.clients[2].allow, null); assert.equal(checked.clients[2].capped, false);
  assert.equal(checked.rooms[0].max_step, null); assert.equal(checked.rooms[1].max_step, 0);
  const quietless = policy({ quiet_hours_now: false }); delete quietless.quiet_hours;
  assert.equal(D.policyOf(quietless).quiet_hours, null);
});

test('policy admits future tool names and compatible extra fields without interpreting rules', () => {
  const raw = policy({ future_property: { anything: true }, clients: [
    { client: 'future', allow: ['new-tool', 'future-class'], deny: ['future-denial'] }] });
  const result = D.policyOf(raw);
  assert.deepEqual(result.clients[0].allow, ['new-tool', 'future-class']);
  assert.equal(result.future_property, undefined);
  assert.deepEqual(D.policyOf(policy({ clients: [], rooms: [] })).rooms, []);
});

test('invalid or contradictory policy responses never imply unrestricted access', () => {
  for (const bad of [null, {}, [], policy({ you: '' }), policy({ you_are_capped: 'false' }),
    policy({ quiet_hours_now: 1 }), policy({ quiet_hours: null }),
    policy({ defaults: { max_volume: 101, max_step: 20, fade_secs: 0 } }),
    policy({ defaults: { max_volume: 70, max_step: -1, fade_secs: 0 } }),
    policy({ defaults: { max_volume: 70, max_step: 20, fade_secs: 0x100000000 } }),
    policy({ clients: [{ client: 'a', allow: 'all', deny: [] }] }),
    policy({ clients: [{ client: 'a', deny: null }] }), policy({ clients: [{ client: 'a', deny: [], capped: 0 }] }),
    policy({ clients: [{ client: 'a', deny: [] }, { client: 'A', deny: [] }] }),
    policy({ rooms: [{ room: 'a', max_volume: '30' }] }), policy({ rooms: [{ room: 'a' }, { room: 'a' }] }),
    policy({ quiet_hours: { start: '24:00', end: '07:00', max_volume: 25 } })]) assert.throws(() => D.policyOf(bad));
});

test('policy limits admit zero caps and full-day windows but bound display work', () => {
  const checked = D.policyOf(policy({ defaults: { max_volume: 0, max_step: 0, fade_secs: 0xffffffff },
    quiet_hours: { start: '00:00', end: '00:00', max_volume: 0 } }));
  assert.equal(checked.defaults.max_volume, 0); assert.equal(checked.quiet_hours.max_volume, 0);
  for (const bad of [policy({ rooms: Array(2049).fill({ room: 'x' }) }),
    policy({ clients: Array(2049).fill({ client: 'x', deny: [] }) }),
    policy({ clients: [{ client: 'x', allow: Array(2049).fill('tool'), deny: [] }] }),
    policy({ clients: [{ client: 'x', allow: Array(2048).fill('x'.repeat(2048)), deny: [] }] })]) assert.throws(() => D.policyOf(bad));
});

test('policy view is lazy and independent of pending or failed hardware diagnostics', async () => {
  const pending = deferred(), calls = [];
  const checks = D.model({ read: () => { calls.push('doctor'); return pending.promise; } });
  const limits = D.policyModel({ read: async () => { calls.push('policy'); return policy(); } });
  checks.open(); limits.open(); assert.deepEqual(calls, []);
  const work = checks.run(); assert.equal(checks.state.busy, true);
  assert.equal(await limits.run(), true); assert.equal(checks.state.busy, true);
  assert.equal(limits.state.report.you, 'listener@example.test'); assert.match(limits.state.message, /permissions were changed/);
  pending.reject(new Error('No callback')); assert.equal(await work, false);
  assert.equal(limits.state.stale, false); assert.deepEqual(calls, ['doctor', 'policy']);
});

test('policy failures retain a clearly stale last observation, then recover the actual changed caller', async () => {
  let answer = policy(); const control = D.policyModel({ read: async () => { if (answer instanceof Error) throw answer; return answer; } });
  control.open(); await control.run(); const before = control.state.report;
  answer = Object.assign(new Error('Policy unavailable'), { hint: 'Ask the owner to allow get_policy.' });
  assert.equal(await control.run(), false); assert.equal(control.state.report, before); assert.equal(control.state.stale, true);
  assert.match(control.state.message, /get_policy/);
  answer = policy({ you: 'unknown', quiet_hours_now: false });
  assert.equal(await control.run(), true); assert.equal(control.state.report.you, 'unknown'); assert.equal(control.state.stale, false);
});

test('malformed policy refresh never replaces the last verified identity or limits', async () => {
  let answer = policy(); const control = D.policyModel({ read: async () => answer }); control.open(); await control.run();
  const before = control.state.report; answer = { you: 'new', you_are_capped: false };
  assert.equal(await control.run(), false); assert.equal(control.state.report, before); assert.equal(control.state.stale, true);
});

for (const event of ['close', 'dispose']) test(`policy ${event} cannot adopt stale identity or dispatch an overlapping read`, async () => {
  const pending = deferred(); let calls = 0, signal;
  const control = D.policyModel({ read: s => { signal = s; calls++; return pending.promise; } }); control.open();
  const work = control.run(); assert.equal(await control.run(), false);
  control[event](); assert.equal(signal.aborted, true); control.open(); assert.equal(await control.run(), false);
  pending.resolve(policy()); assert.equal(await work, false); assert.equal(control.state.report, null); assert.equal(calls, 1);
});

test('real localhost policy GET uses the existing route without selecting a caller or sending a body', async t => {
  const calls = [];
  const server = http.createServer(async (req, res) => { let body = ''; for await (const part of req) body += part;
    calls.push([req.method, req.url, body]); res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify(policy())); });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => { server.closeAllConnections(); server.close(); });
  const [method, route] = D.routes.diagnosticsPolicy;
  const control = D.policyModel({ read: async signal => {
    const response = await fetch(`http://127.0.0.1:${server.address().port}${route}`, { method, signal }); return response.json();
  } }); control.open(); assert.equal(await control.run(), true);
  assert.deepEqual(calls, [['GET', '/policy', '']]); assert.equal(control.state.report.quiet_hours_now, true);
});

test('policy is discoverable by denied commands, quiet hours and volume caps without a selected room', () => {
  const tool = Studio.identify({ id: '', getAttribute: () => 'House policy and volume caps', querySelector: () => ({ textContent: 'Why volume is capped or a command is denied' }) });
  assert.equal(tool.id, 'house-policy'); assert.equal(tool.workspace, 'activity');
  for (const query of ['policy', 'quiet hours', 'denied', 'volume caps', 'client']) assert.equal(Studio.search(query, [tool]).length, 1);
  assert.deepEqual(D.routes.diagnosticsPolicy, ['GET', '/policy']);
});
