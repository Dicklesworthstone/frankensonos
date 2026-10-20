'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const L = require('../crates/fsonos-api/src/remote/dashboard-live.js');
const copy = (v) => structuredClone(v);
const settle = async () => { for (let i = 0; i < 60; i++) await Promise.resolve(); };
function deferred() { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; }
function clock() {
  let now = 0, id = 0;
  const timers = new Map();
  return {
    now: () => now, schedule(fn, delay) { timers.set(++id, { fn, at: now + delay }); return id; },
    cancel(id) { timers.delete(id); }, get size() { return timers.size; },
    async tick(ms) {
      const end = now + ms; await settle();
      for (let n = 0; ; n++) {
        assert.ok(n < 10000, 'no timer storm');
        const t = [...timers].filter(([, t]) => t.at <= end).sort((a, b) => a[1].at - b[1].at)[0];
        if (!t) break;
        timers.delete(t[0]); now = t[1].at; t[1].fn(); await settle();
      }
      now = end; await settle();
    },
  };
}
function observe(p) {
  const result = {};
  result.done = p.then(value => { result.value = value; }, error => { result.error = error; }).then(() => { result.settled = true; });
  return result;
}
const zone = { household: 'S2', coordinator_room: 'Den', members: ['Den'], transport_state: 'playing' };
const playback = () => ({ zone: copy(zone), transport_state: 'playing', volume: 20, mute: false,
  track: { uri: 'fixture:one', title: 'A song', duration_secs: 1000, position_secs: 20, queue_position: 1 } });
function model(options = {}) {
  const requests = [], t = clock(); let override = null;
  const call = async (url, body) => {
    requests.push({ url, body });
    if (override) { const value = await override(url, body); if (value !== undefined) return value; }
    if (body !== undefined) return { done: 'Paused', changed: true };
    return url === '/zones' ? [copy(zone)] : playback();
  };
  const c = C.model({ call, now: t.now, ...options });
  return { c, t, call, requests, override(fn) { override = fn; } };
}
class Feed {
  readyState = 0; closed = 0; handlers = new Map();
  addEventListener(name, fn) { this.handlers.set(name, fn); }
  emit(name, detail = {}) { if (name === 'open') this.readyState = 1; this.handlers.get(name)?.(detail); }
  close() { this.closed++; this.readyState = 2; }
}
function live(options = {}) {
  const t = clock(), feeds = [], reads = [], invalid = [], states = [];
  const c = L.create({ connect: () => { const f = new Feed(); feeds.push(f); return f; },
    reload: async user => { reads.push({ at: t.now(), user }); return true; },
    invalidate: hard => invalid.push(hard), connection: value => states.push(value),
    schedule: t.schedule, cancel: t.cancel, now: t.now, ...options });
  return { t, c, feeds, reads, invalid, states };
}
async function opened(h) { h.c.resume(); h.feeds[0].emit('open'); await h.t.tick(350); }

test('the event contract includes the actual daemon reset event', () => assert.ok(C.EVENTS.includes('events.reset')));

test('a live quiet snapshot remains usable, but disconnect immediately invalidates it', async () => {
  const h = model(); await h.c.refresh(); h.c.connection(true);
  await h.t.tick(120000); assert.equal(h.c.canControl(), true);
  h.c.connection(false); assert.equal(h.c.selected().fresh, false); assert.equal(h.c.canControl(), false);
  await h.c.refresh(); assert.equal(h.c.canControl(), true, 'fallback snapshot still permits freshly preflighted control');
  await h.t.tick(30001); assert.equal(h.c.canControl(), false, 'disconnected observations still age out');
});

for (const stage of ['inventory', 'playback']) test(`invalidation fences an old ${stage} reply`, async () => {
  const h = model(); await h.c.refresh(); const gate = deferred();
  h.override(url => (stage === 'inventory' ? url === '/zones' : url.endsWith('/state')) ? gate.promise : undefined);
  const run = h.c.refresh(true); await settle(); h.c.invalidate();
  gate.resolve(stage === 'inventory' ? [copy(zone)] : playback()); await run;
  assert.equal(h.c.selected().fresh, false); assert.equal(h.c.state.loading, false);
  h.override(null); assert.equal(await h.c.refresh(), true); assert.equal(h.c.selected().fresh, true);
});

test('suspend and resume cannot overlap an old snapshot that ignores cancellation', async () => {
  const h = model(), gate = deferred(); h.override(url => url === '/zones' ? gate.promise : undefined);
  const first = h.c.refresh(); await settle(); h.c.suspend(); h.c.resume();
  assert.equal(await h.c.refresh(), false); assert.equal(h.requests.length, 1);
  gate.resolve([copy(zone)]); await first; assert.equal(h.c.state.items.length, 0);
  h.override(null); assert.equal(await h.c.refresh(), true);
});

test('obsolete member failure cannot impose its backoff on a new lifecycle', async () => {
  const h = model(); await h.c.refresh(); const gate = deferred();
  h.override(url => url.endsWith('/state') ? gate.promise : undefined);
  const run = h.c.refresh(); await settle(); h.c.invalidate();
  gate.reject(Object.assign(new Error('rate limited'), { retryAfter: 100000 })); await run;
  assert.equal(h.c.state.retryAfter, 0); assert.equal(h.c.state.error, '');
});

test('invalidation during a command preflight prevents any POST', async () => {
  const h = model(); await h.c.refresh(); const gate = deferred();
  h.override(url => url.endsWith('/state') ? gate.promise : undefined);
  const run = h.c.command('pause'); await settle(); h.c.invalidate(); gate.resolve(playback());
  assert.equal(await run, false); assert.equal(h.requests.filter(r => r.body !== undefined).length, 0);
  assert.equal(h.c.state.uncertain, false); assert.match(h.c.state.message, /No command was sent/);
});

test('invalidation after dispatch preserves the single acknowledgement without a hidden retry', async () => {
  const h = model(); await h.c.refresh(); const gate = deferred();
  h.override((url, body) => body !== undefined ? gate.promise : undefined);
  const run = h.c.command('pause'); await settle(); h.c.invalidate();
  gate.resolve({ done: 'Paused', changed: true }); assert.equal(await run, true);
  assert.equal(h.requests.filter(r => r.body !== undefined).length, 1); assert.equal(h.c.selected().fresh, false);
  assert.equal(h.c.state.message, 'Paused');
});

test('automatic recovery cannot clear an uncertain command outcome', async () => {
  const h = model(); await h.c.refresh();
  h.override((url, body) => { if (body !== undefined) throw new Error('lost reply'); });
  await h.c.command('pause'); h.c.invalidate(); await h.c.refresh(); h.c.connection(true);
  assert.equal(h.c.state.uncertain, true); assert.equal(h.c.canControl(), false);
  await h.c.refresh(true); assert.equal(h.c.state.uncertain, false); assert.equal(h.c.canControl(), true);
});

for (const stage of ['connect', 'body']) test(`cancelling a stuck read ${stage} settles independently of transport abort`, async () => {
  const t = clock(), gate = deferred(); let signal;
  const request = C.request({ schedule: t.schedule, cancel: t.cancel,
    fetcher: async (_, init) => { signal = init.signal; return stage === 'connect' ? gate.promise
      : { ok: true, text: () => gate.promise }; } });
  const run = observe(request('/zones')); await settle(); request.cancelReads(); await run.done;
  assert.equal(run.error.code, 'DASHBOARD_READ_CANCELLED'); assert.equal(signal.aborted, true); assert.equal(t.size, 0);
  gate.resolve(stage === 'connect' ? { ok: true, text: async () => '[]' } : '[]'); await settle();
  assert.equal(run.value, undefined);
});

test('cancelling reads never aborts a sent command and every deadline is released', async () => {
  const t = clock(), gates = [], calls = [];
  const request = C.request({ schedule: t.schedule, cancel: t.cancel, fetcher: async (url, init) => {
    const gate = deferred(); gates.push(gate); calls.push({ url, init }); return gate.promise;
  } });
  const read = observe(request('/zones')), write = observe(request('/pause', { zone: 'Den@S2' }));
  await settle(); request.cancelReads(); await read.done;
  assert.equal(calls[0].init.signal.aborted, true); assert.equal(calls[1].init.signal.aborted, false);
  gates[1].resolve({ ok: true, text: async () => '{"done":"Paused","changed":true}' }); await write.done;
  assert.equal(write.value.done, 'Paused'); assert.equal(calls.length, 2); assert.equal(t.size, 0);
  gates[0].resolve({ ok: true, text: async () => '[]' }); await settle();
});

test('a healthy quiet stream needs only the initial snapshot', async () => {
  const h = live(); await opened(h); await h.t.tick(600000);
  assert.equal(h.reads.length, 1); assert.equal(h.t.size, 0); assert.equal(h.states.at(-1), true);
});

test('the dashboard waits for a new snapshot before declaring a reconnected stream live', async () => {
  const gate = deferred(); const h = live({ reload: () => gate.promise });
  h.c.resume(); h.feeds[0].emit('open'); await h.t.tick(350);
  assert.equal(h.states.at(-1), false); gate.resolve(true); await settle(); assert.equal(h.states.at(-1), true);
});

test('event bursts have one fixed deadline and reset is handled without another event', async () => {
  const h = live(); await opened(h);
  for (let i = 0; i < 3; i++) { h.feeds[0].emit('zone.state'); await h.t.tick(100); }
  assert.equal(h.reads.length, 1); await h.t.tick(50); assert.equal(h.reads.length, 2);
  h.feeds[0].emit('events.reset'); assert.equal(h.invalid.at(-1), true); assert.equal(h.states.at(-1), false);
  await h.t.tick(350); assert.equal(h.reads.length, 3); assert.equal(h.states.at(-1), true);
});

test('all public event kinds coalesce without parsing speculative state deltas', async () => {
  const h = live(); await opened(h);
  for (const kind of L.EVENTS) h.feeds[0].emit(kind, { data: '{broken' });
  await h.t.tick(350); assert.equal(h.reads.length, 2);
});

test('events during a slow snapshot cause exactly one later snapshot without overlap', async () => {
  const gate = deferred(); let calls = 0, active = 0, peak = 0;
  const h = live({ reload: async () => { peak = Math.max(peak, ++active); if (++calls === 2) await gate.promise; active--; return true; } });
  await opened(h); h.feeds[0].emit('zone.state'); await h.t.tick(350);
  for (let i = 0; i < 100; i++) h.feeds[0].emit('zone.state');
  await h.t.tick(2000); assert.equal(calls, 2);
  gate.resolve(); await settle(); await h.t.tick(350); assert.equal(calls, 3); assert.equal(peak, 1);
});

test('disconnect triggers a new observation then bounded fallback while the feed reconnects', async () => {
  const h = live(); await opened(h); h.feeds[0].readyState = 0; h.feeds[0].emit('error');
  await h.t.tick(350); assert.equal(h.reads.length, 2); assert.equal(h.states.at(-1), false);
  await h.t.tick(15000); assert.equal(h.reads.length, 3); assert.equal(h.feeds.length, 1);
  h.feeds[0].emit('open'); await h.t.tick(350); await h.t.tick(60000); assert.equal(h.reads.length, 4);
});

for (const connect of [null, () => { throw new Error('unavailable'); }]) test('missing event delivery keeps serialized read-only fallback useful', async () => {
  const h = live({ connect }); h.c.resume(); await h.t.tick(350); await h.t.tick(30000);
  assert.equal(h.reads.length, 3); assert.equal(h.states.at(-1), false); h.c.dispose(); assert.equal(h.t.size, 0);
});

test('permanently closed feeds are replaced without duplicating a connecting feed', async () => {
  const h = live(); await opened(h); h.feeds[0].readyState = 2; h.feeds[0].emit('error');
  await h.t.tick(350); assert.equal(h.feeds[0].closed, 1); assert.equal(h.feeds.length, 2);
  h.c.refresh(); h.c.refresh(); await h.t.tick(350); assert.equal(h.feeds.length, 2);
});

test('failed snapshots back off; event storms cannot bypass recovery delay', async () => {
  let attempts = 0;
  const h = live({ reload: async () => ++attempts > 2 }); h.c.resume(); h.feeds[0].emit('open'); await h.t.tick(350);
  for (let i = 0; i < 30; i++) { h.feeds[0].emit('zone.state'); await h.t.tick(1000); }
  assert.equal(attempts, 2); await h.t.tick(60000); assert.equal(attempts, 3); assert.equal(h.t.size, 0);
});

test('long Retry-After minima survive events, explicit refresh, visibility and reconnect', async () => {
  let attempts = 0, minimum = 180000;
  const h = live({ reload: async () => { attempts++; return attempts > 1; }, retryAfter: () => { const n = minimum; minimum = 0; return n; } });
  await opened(h); await h.t.tick(1000); h.c.suspend(); h.c.resume(); h.feeds[1].emit('open'); h.c.refresh(true);
  await h.t.tick(178999); assert.equal(attempts, 1); await h.t.tick(1); assert.equal(attempts, 2);
  assert.equal(h.t.size, 0);
});

test('unrepresentable Retry-After cannot overflow into a timer storm', async () => {
  const h = live({ reload: async () => false, retryAfter: () => Infinity }); await opened(h);
  h.c.refresh(true); await h.t.tick(600000); assert.equal(h.t.size, 0);
  h.c.suspend(); h.c.resume(); await h.t.tick(600000); assert.equal(h.t.size, 0);
});

test('suspension fences replies, closes the feed and prevents hidden refreshes', async () => {
  const gate = deferred(); let calls = 0;
  const h = live({ reload: async () => { if (++calls === 1) await gate.promise; return true; } });
  h.c.resume(); h.feeds[0].emit('open'); await h.t.tick(350); h.c.suspend();
  gate.resolve(); await settle(); await h.t.tick(60000); assert.equal(calls, 1); assert.equal(h.t.size, 0);
  h.c.resume(); h.feeds[1].emit('open'); await h.t.tick(350); assert.equal(calls, 2); assert.equal(h.states.at(-1), true);
});

test('explicit recovery survives a superseding event but not a hide/show lifecycle', async () => {
  const gate = deferred(), users = []; let count = 0;
  const h = live({ reload: async user => { users.push(user); if (++count === 2) await gate.promise; return true; } });
  await opened(h); h.c.refresh(true); await h.t.tick(0); h.feeds[0].emit('zone.state');
  gate.resolve(); await settle(); await h.t.tick(350); assert.deepEqual(users, [false, true, true]);
  h.c.refresh(true); h.c.suspend(); h.c.resume(); h.feeds[1].emit('open'); await h.t.tick(350);
  assert.equal(users.at(-1), false);
});

test('commands in progress defer refresh admission without making a network request', async () => {
  let blocked = true; const h = live({ blocked: () => blocked }); await opened(h); await h.t.tick(3000);
  assert.equal(h.reads.length, 0); blocked = false; await h.t.tick(1000); assert.equal(h.reads.length, 1);
});

test('demo mode performs no connection or recurring polling', async () => {
  const h = live({ demo: true }); h.c.resume(); await h.t.tick(600000);
  assert.equal(h.feeds.length, 0); assert.equal(h.reads.length, 1); assert.equal(h.t.size, 0);
});

test('disposed controllers and obsolete feeds cannot resume delivery', async () => {
  const h = live(); await opened(h); h.c.suspend(); h.c.resume(); h.feeds[1].emit('open'); await h.t.tick(350);
  h.feeds[0].emit('open'); h.feeds[0].emit('events.reset'); await h.t.tick(350); assert.equal(h.reads.length, 2);
  h.c.dispose(); h.c.resume(); h.c.refresh(); await h.t.tick(600000); assert.equal(h.reads.length, 2); assert.equal(h.t.size, 0);
});

test('real HTTP: cancelling a stalled JSON body does not abort or repeat a submitted command', { timeout: 5000 }, async t => {
  const reading = deferred(), posted = deferred(), socketClosed = deferred(); let writes = 0, confirm;
  const server = http.createServer((req, res) => {
    req.resume();
    if (req.method === 'POST') { writes++; confirm = () => res.end('{"done":"Paused","changed":true}'); posted.resolve(); }
    else { res.writeHead(200, { 'content-type': 'application/json' }); res.write('['); res.on('close', socketClosed.resolve); }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise(resolve => { server.close(resolve); server.closeAllConnections(); }));
  const request = C.request({ fetcher: async (url, init) => {
    const response = await fetch(`http://127.0.0.1:${server.address().port}${url}`, init);
    if (init.method === 'GET') { const text = response.text.bind(response); response.text = () => { reading.resolve(); return text(); }; }
    return response;
  } });
  const read = observe(request('/zones')), write = observe(request('/pause', { zone: 'Den@S2' }));
  await Promise.all([reading.promise, posted.promise]); request.cancelReads(); await read.done; await socketClosed.promise;
  assert.equal(read.error.code, 'DASHBOARD_READ_CANCELLED'); assert.equal(write.settled, undefined);
  confirm(); await write.done; assert.equal(write.value.done, 'Paused'); assert.equal(writes, 1);
});
