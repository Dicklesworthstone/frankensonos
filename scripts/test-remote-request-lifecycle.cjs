'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const http = require('node:http');
const ReadRequests = require('../crates/fsonos-api/src/remote/read-requests.js');
const remotePath = path.join(__dirname, '../crates/fsonos-api/src/remote/remote.js');
const source = fs.readFileSync(remotePath, 'utf8');
const prefix = source.slice(0, source.indexOf('const requestJson ='));
const RequestRemote = vm.runInNewContext(prefix + '\nRequestRemote;', {
  ReadRequests, AbortController, setTimeout, clearTimeout, URLSearchParams,
});
const read = (signal) => ({ method: 'GET', headers: { Accept: 'application/json' }, cache: 'no-store', ...(signal ? { signal } : {}) });
const write = (signal) => ({ ...read(signal), method: 'POST', body: '{}' });
function gate() { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; }
const tick = () => new Promise(setImmediate);
const response = (status = 200, value = {}, retryAfter = null) => ({ ok: status >= 200 && status < 300, status,
  headers: { get: () => retryAfter }, text: async () => JSON.stringify(value) });
function clock() {
  let now = 0, id = 0; const timers = new Map();
  return { timers, now: () => now,
    schedule(fn, delay) { timers.set(++id, { fn, due: now + delay }); return id; },
    cancel(key) { timers.delete(key); },
    advance(ms) {
      now += ms;
      for (const [key, timer] of [...timers]) if (timer.due <= now) { timers.delete(key); timer.fn(); }
    },
  };
}
function fixture(fetcher) {
  const c = clock(), calls = [];
  const request = RequestRemote.create({ ...c, random: () => 0, fetcher: (where, init) => { calls.push({ where, init }); return fetcher(where, init); } });
  return { c, calls, request };
}

test('production adapter shares a burst and preserves independent parsed replies', async () => {
  const body = gate(); const f = fixture(() => ({ ...response(), text: () => body.promise }));
  const tasks = Array.from({ length: 50 }, () => f.request('/zones', read())); await tick();
  assert.equal(f.calls.length, 1); body.resolve('[{"members":["Test room"]}]');
  const rows = await Promise.all(tasks); rows[0][0].members.push('changed'); assert.deepEqual(rows[1][0].members, ['Test room']);
  assert.equal(f.c.timers.size, 0);
});

test('later preflight does not reuse an earlier in-flight response body', async () => {
  const bodies = []; const f = fixture(() => { const body = gate(); bodies.push(body); return { ...response(), text: () => body.promise }; });
  const old = f.request('/zones', read()); await tick();
  const fresh = f.request('/zones', read()); await tick(); assert.equal(f.calls.length, 2);
  bodies[0].resolve('{"generation":1}'); bodies[1].resolve('{"generation":2}');
  assert.equal((await old).generation, 1); assert.equal((await fresh).generation, 2);
});

test('shared transient recovery has one retry sequence, not one per panel', async () => {
  let count = 0; const f = fixture(() => response(++count < 3 ? 503 : 200, { count }));
  const tasks = Array.from({ length: 20 }, () => f.request('/zones', read()));
  await tick(); assert.equal(count, 1); f.c.advance(250); await tick(); assert.equal(count, 2);
  f.c.advance(500); await tick(); assert.equal(count, 3); await Promise.all(tasks); assert.equal(f.c.timers.size, 0);
});

test('one cancellation leaves sibling readers and the underlying fetch alive', async () => {
  const body = gate(), a = new AbortController(); const f = fixture(() => ({ ...response(), text: () => body.promise }));
  const left = f.request('/zones', read(a.signal)), stay = f.request('/zones', read());
  const rejected = assert.rejects(left, { name: 'AbortError' }); await tick(); a.abort(); await rejected;
  assert.equal(f.calls[0].init.signal.aborted, false); body.resolve('{"good":true}'); assert.equal((await stay).good, true);
});

test('cancelReads settles ignored body aborts, clears timers and permits fresh reads', async () => {
  const blocked = gate(); let stalled = true;
  const f = fixture(() => stalled ? { ...response(), text: () => blocked.promise } : response(200, { new: true }));
  const done = Promise.allSettled([f.request('/zones', read()), f.request('/zones', read()), f.request('/favorites', read())]);
  await tick(); assert.equal(f.calls.length, 2); f.request.cancelReads();
  for (const r of await done) { assert.equal(r.status, 'rejected'); assert.equal(r.reason.code, 'REMOTE_READ_CANCELLED'); }
  assert.ok(f.calls.every((r) => r.init.signal.aborted)); assert.equal(f.c.timers.size, 0);
  stalled = false; assert.equal((await f.request('/zones', read())).new, true);
  blocked.resolve('{"late":true}'); await tick(); assert.equal(f.c.timers.size, 0);
});

test('cancelReads also prevents queued requests that have not reached fetch', async () => {
  const f = fixture(() => response()); const tasks = Promise.allSettled([f.request('/zones', read()), f.request('/zones', read())]);
  f.request.cancelReads(); assert.ok((await tasks).every((t) => t.status === 'rejected')); assert.equal(f.calls.length, 0); assert.equal(f.c.timers.size, 0);
});

test('cancelReads terminates backoff without sending the next retry', async () => {
  const f = fixture(() => response(503)); const done = assert.rejects(f.request('/zones', read()), (e) => e.code === 'REMOTE_READ_CANCELLED');
  await tick(); assert.equal(f.calls.length, 1); assert.equal(f.c.timers.size, 2);
  f.request.cancelReads(); await done; await tick(); assert.equal(f.c.timers.size, 0);
  f.c.advance(60000); await tick(); assert.equal(f.calls.length, 1);
});

test('page-read cancellation does not cancel a sent command or change its acknowledgement', async () => {
  const body = gate(); const f = fixture(() => ({ ...response(), text: () => body.promise }));
  const command = f.request('/pause', write()); f.request.cancelReads(); assert.equal(f.calls[0].init.signal.aborted, false);
  body.resolve('{"done":"paused"}'); assert.equal((await command).done, 'paused'); assert.equal(f.calls.length, 1);
});

test('explicit cancellation of a sent command remains uncertain and is never retried', async () => {
  const body = gate(), control = new AbortController(); const f = fixture(() => ({ ...response(), text: () => body.promise }));
  const task = f.request('/pause', write(control.signal)); const done = assert.rejects(task, (e) => e.code === 'REMOTE_OUTCOME_UNKNOWN' && e.uncertain && /not repeated/.test(e.hint));
  await tick(); control.abort(); await done; assert.equal(f.calls[0].init.signal.aborted, true); assert.equal(f.calls.length, 1);
  assert.equal(f.c.timers.size, 0); body.resolve('{"done":"late"}'); await tick();
});

test('deadline still settles a stuck body without a retry or leaked timer', async () => {
  const body = gate(); const f = fixture(() => ({ ...response(), text: () => body.promise }));
  const done = assert.rejects(f.request('/zones', read()), (e) => e.code === 'REMOTE_READ_TIMEOUT');
  await tick(); f.c.advance(RequestRemote.READ_TIMEOUT); await done; assert.equal(f.calls.length, 1); assert.equal(f.c.timers.size, 0);
  body.resolve('[]'); await tick(); assert.equal(f.calls.length, 1);
});

test('long Retry-After is preserved rather than shortened to fit sharing', async () => {
  const f = fixture(() => response(429, { detail: 'slow down' }, '120'));
  const done = await Promise.allSettled([f.request('/zones', read()), f.request('/zones', read())]);
  for (const r of done) { assert.equal(r.status, 'rejected'); assert.equal(r.reason.status, 429); assert.equal(r.reason.retryAfter, 120000); }
  assert.equal(f.calls.length, 1); assert.equal(f.c.timers.size, 0);
});

test('malformed success is still an error and is not retried', async () => {
  const f = fixture(() => ({ ...response(), text: async () => '<not-json>' }));
  await assert.rejects(f.request('/zones', read()), (e) => e.code === 'REMOTE_INVALID_RESPONSE'); assert.equal(f.calls.length, 1);
});

test('the real call adapter passes signal, exact household path and the command body', async () => {
  const calls = [], body = gate();
  const core = source.slice(0, source.indexOf('/** An element:'));
  const call = vm.runInNewContext(core + '\ncall;', { ReadRequests, AbortController, URLSearchParams, setTimeout, clearTimeout,
    fetch: (where, init) => { calls.push({ where, init }); return { ...response(), text: () => body.promise }; } });
  const one = new AbortController(), two = new AbortController();
  const tasks = Promise.allSettled([call('state', { params: { room: 'Room@S1' }, signal: one.signal }), call('state', { params: { room: 'Room@S1' }, signal: two.signal })]);
  await tick(); assert.equal(calls.length, 1); assert.equal(calls[0].where, '/zones/Room%40S1/state');
  one.abort(); two.abort(); assert.ok((await tasks).every((v) => v.status === 'rejected'));
  const command = call('volume', { body: { zone: 'Room@S1', volume: 25 } });
  assert.deepEqual(JSON.parse(calls[1].init.body), { zone: 'Room@S1', volume: 25 });
  body.resolve('{"done":"volume","volume":25}'); await command;
});

test('actual page suspension hook cancels reads only after deactivating recovery lanes', () => {
  const events = {}, order = [];
  const snippet = source.slice(source.indexOf('  const suspend = () => { controller.suspend();'), source.indexOf('/** The header\'s'));
  vm.runInNewContext(`(() => { ${snippet})();`, {
    controller: { suspend() { order.push('live'); }, resume() { order.push('resume'); }, refresh() {} },
    catalogs: { suspend() { order.push('catalogs'); }, resume() {}, refresh() {} },
    requestJson: { cancelReads() { order.push('reads'); } },
    document: { hidden: true, addEventListener: (name, fn) => { events[name] = fn; } },
    window: { addEventListener: (name, fn) => { events[name] = fn; } },
  });
  events.visibilitychange(); assert.deepEqual(order, ['live', 'catalogs', 'reads']);
  order.length = 0; events.pagehide(); assert.deepEqual(order, ['live', 'catalogs', 'reads']);
});

test('production HTTP adapter coalesces requests and sends fresh preflights', async (t) => {
  const received = []; const pending = [];
  const server = http.createServer((req, res) => {
    received.push([req.method, req.url]);
    const finish = () => { res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify({ sequence: received.length })); };
    if (req.method === 'GET') pending.push(finish); else finish();
  });
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  t.after(() => new Promise((r) => { server.closeAllConnections(); server.close(r); }));
  const request = RequestRemote.create({ fetcher: (where, init) => fetch(`http://127.0.0.1:${server.address().port}` + where, init) });
  const first = Array.from({ length: 30 }, () => request('/zones', read()));
  while (received.length < 1) await tick();
  const preflight = request('/zones', read()); while (received.length < 2) await tick();
  assert.equal(received.length, 2); pending.splice(0).forEach((reply) => reply());
  await Promise.all([...first, preflight]); await request('/pause', write());
  assert.deepEqual(received.map(([method]) => method), ['GET', 'GET', 'POST']);
});


test('served assets install read sharing before the production adapter with unchanged privacy checks', () => {
  const embedding = fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/http/remote.rs'), 'utf8');
  const broker = fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/remote/read-requests.js'), 'utf8');
  assert.ok(embedding.includes('const READ_SCRIPT: &str = include_str!("../remote/read-requests.js");'));
  assert.ok(embedding.includes('{READ_SCRIPT}\\n{QUEUE_SCRIPT}\\n{SCRIPT}'));
  assert.ok(embedding.includes('("read-requests.js", READ_SCRIPT)'));
  for (const secret of ['token', 'authorization', 'x-fsonos', 'tailscale-user', 'password', 'cookie', 'ts.net', 'http://', 'https://']) {
    assert.ok(!broker.toLowerCase().includes(secret), secret);
  }
  new vm.Script(broker + '\n' + source);
});

test('standalone adapter retains deadlines and cancellation when no sharing module is installed', async () => {
  const standalone = vm.runInNewContext(prefix + '\nRequestRemote;', { AbortController, setTimeout, clearTimeout, URLSearchParams });
  const body = gate(); let calls = 0;
  const request = standalone.create({ fetcher: () => { ++calls; return { ...response(), text: () => body.promise }; } });
  const tasks = Promise.allSettled([request('/zones', read()), request('/zones', read())]);
  await tick(); assert.equal(calls, 2); request.cancelReads();
  assert.ok((await tasks).every((row) => row.status === 'rejected' && row.reason.code === 'REMOTE_READ_CANCELLED'));
  body.resolve('[]');
});
