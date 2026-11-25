// The shipped event transport, live reconciler and page lifecycle together.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.join(__dirname, '..');
const remote = fs.readFileSync(process.env.REMOTE_SOURCE || path.join(root, 'crates/fsonos-api/src/remote/remote.js'), 'utf8');
const transport = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/remote/event-feed.js'), 'utf8');
const broker = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/remote/read-requests.js'), 'utf8');
const settle = async () => { for (let i = 0; i < 80; i++) await Promise.resolve(); };
const deferred = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; };
function clock() {
  let now = 0, id = 0;
  const timers = new Map();
  return {
    now: () => now,
    schedule(fn, ms) { timers.set(++id, { fn, at: now + ms }); return id; },
    cancel(key) { timers.delete(key); },
    get pending() { return timers.size; },
    async tick(ms) {
      const end = now + ms; await settle();
      for (;;) {
        const next = [...timers].filter(([, t]) => t.at <= end).sort((a, b) => a[1].at - b[1].at || a[0] - b[0])[0];
        if (!next) break;
        timers.delete(next[0]); now = next[1].at; next[1].fn(); await settle();
      }
      now = end; await settle();
    },
  };
}
class Events {
  listeners = new Map();
  addEventListener(type, listener) { if (!this.listeners.has(type)) this.listeners.set(type, []); this.listeners.get(type).push(listener); }
  emit(type, data = {}) { for (const listener of this.listeners.get(type) || []) listener({ type, data: JSON.stringify(data) }); }
  dispatchEvent(event) { for (const listener of this.listeners.get(event.type) || []) listener(event); return true; }
}
class Element {
  textContent = ''; value = ''; children = []; disabled = false;
  setAttribute() {}
  removeAttribute() {}
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.children = children; }
  contains() { return false; }
}
function stream() {
  const waiting = [], queued = [];
  let cancelled = 0;
  const reader = { read() { if (queued.length) return Promise.resolve(queued.shift()); const d = deferred(); waiting.push(d); return d.promise; },
    cancel() { cancelled++; return Promise.resolve(); } };
  const put = (value) => { const d = waiting.shift(); if (d) d.resolve(value); else queued.push(value); };
  return {
    response: { status: 200, headers: { get: () => 'text/event-stream' }, body: { getReader: () => reader } },
    send(text) { put({ done: false, value: new TextEncoder().encode(text) }); },
    end() { put({ done: true }); },
    get cancelled() { return cancelled; },
  };
}
const reply = (value) => ({ ok: true, status: 200, text: async () => JSON.stringify(value) });
function runtime(t, { feed = true, globals = {} } = {}) {
  const time = clock(), streams = [], requests = [], dom = new Map(), window = new Events(), document = new Events();
  document.hidden = false;
  document.getElementById = (id) => { if (!dom.has(id)) dom.set(id, new Element()); return dom.get(id); };
  document.createElement = () => new Element();
  let answer = async () => [], response = () => null;
  const math = Object.create(Math); math.random = () => 0;
  const context = { window, document, Math: math, AbortController, TextDecoder, ReadableStream, URLSearchParams,
    CustomEvent: class { constructor(type, options = {}) { this.type = type; this.detail = options.detail; } },
    setTimeout: time.schedule, clearTimeout: time.cancel, performance: { now: time.now },
    fetch: async (url, init) => {
      requests.push({ url, init });
      if (url === '/events') { const s = stream(); streams.push(s); return s.response; }
      const direct = response(url, init); if (direct) return direct;
      return reply(await answer(url, init));
    }, ...globals };
  const api = vm.runInNewContext((feed ? transport : '') + '\n' + broker + '\n'
    + remote.replace(/\nstart\(\);\s*$/, '')
    + '\ncatalogs.resume = () => {}; catalogs.reconcile = () => {};'
    + '\n({ listen, house, zoneKey, groupShape, refreshZone, refreshVolume, loadZones, call, requestJson, LiveRemote, connect: () => typeof connectEvents === "function" ? connectEvents() : null });', context);
  const controller = api.listen();
  t.after(() => { controller.dispose(); api.requestJson.cancelReads(); });
  return { ...api, controller, time, streams, requests, document, window, context,
    answer(fn) { answer = fn; }, response(fn) { response = fn; }, label: () => document.getElementById('live').textContent };
}
const reads = (h) => h.requests.filter(r => r.url === '/zones').length;
const mutationRequests = (h) => h.requests.filter(r => r.init.method !== 'GET');
function room(h, name = 'Den', household = 'S2') {
  const zone = { household, coordinator_room: name, members: [name], transport_state: 'playing' };
  const target = `${name}@${household}`;
  const level = { version: 0, readVersion: 0, pendingVolume: null, timer: null, mutedPending: false,
    room: target, input: new Element(), shown: new Element(), mute: new Element() };
  let observations = 0;
  const parts = { zone, node: new Element(), name: target, disposed: false, readVersion: 0, tag: '',
    sliders: new Map([[name, level]]), panels: [], queue: { observe() { observations++; }, stale() {} },
    play: new Element(), title: new Element(), by: new Element(), state: new Element(), art: new Element() };
  h.house.zones.push(zone); h.house.layout = JSON.stringify(h.house.zones.map(h.groupShape)); h.house.dj = false;
  h.house.cards.set(h.zoneKey(zone), parts);
  const state = (volume = 20) => ({ zone, transport_state: 'playing', track: null, volume, mute: false });
  h.answer(async url => url === '/zones' ? [zone] : state());
  return { zone, parts, level, state, observed: () => observations };
}

test('the served bundle installs the heartbeat client before the remote without loosening CSP', () => {
  const rust = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/http/remote.rs'), 'utf8');
  assert.ok(rust.includes('const EVENT_FEED_SCRIPT: &str = include_str!("../remote/event-feed.js");'));
  assert.ok(rust.includes('"{EVENT_FEED_SCRIPT}\\n{READ_SCRIPT}\\n{QUEUE_SCRIPT}\\n{SCRIPT}'));
  assert.ok(rust.includes('("event-feed.js", EVENT_FEED_SCRIPT)'));
  assert.match(rust, /connect-src 'self'; base-uri 'none'; form-action 'none'/);
  for (const forbidden of ['token', 'authorization', 'cookie', 'password', 'http://', 'https://', 'ts.net', 'innerHTML', 'eval(']) {
    assert.ok(!transport.toLowerCase().includes(forbidden.toLowerCase()), forbidden);
  }
});

test('the actual listen function selects the heartbeat client and opens exactly one connection', async t => {
  const h = runtime(t); await h.time.tick(300);
  assert.equal(h.streams.length, 1); assert.equal(reads(h), 1); assert.equal(h.label(), 'Live');
  assert.equal(mutationRequests(h).length, 0);
});

test('quiet heartbeat comments neither refresh snapshots nor create a second subscription', async t => {
  const h = runtime(t); await h.time.tick(300);
  for (let i = 0; i < 12; i++) { h.streams[0].send(': heartbeat\n\n'); await h.time.tick(15000); }
  assert.equal(h.label(), 'Live'); assert.equal(h.streams.length, 1); assert.equal(reads(h), 1);
});

test('silent body loss leaves Live immediately, then reconnects through a fresh snapshot', async t => {
  const h = runtime(t); await h.time.tick(300);
  await h.time.tick(44700);
  assert.equal(h.label(), 'Refreshing without live events'); assert.equal(h.streams[0].cancelled, 1);
  await h.time.tick(1000); assert.equal(h.label(), 'Synchronizing…');
  await h.time.tick(300); assert.equal(h.label(), 'Live');
  assert.equal(h.streams.length, 2); assert.equal(reads(h), 2); assert.equal(mutationRequests(h).length, 0);
});

for (const kind of ['disconnect', 'reset', 'suspend']) {
  test(`${kind} fences in-flight playback and member-volume reads, not just their displayed freshness`, async t => {
    const h = runtime(t), r = room(h); await h.time.tick(300);
    const held = deferred(); h.answer(() => held.promise);
    const observations = r.observed(), generation = r.parts.readVersion;
    const playback = h.refreshZone(r.zone).catch(e => e);
    const volume = h.refreshVolume(r.level, r.parts).catch(e => e);
    await settle();
    if (kind === 'disconnect') h.streams[0].end();
    else if (kind === 'reset') h.streams[0].send('event: events.reset\ndata: {"oldest":1}\n\n');
    else h.controller.suspend();
    await settle();
    assert.ok(r.parts.readVersion > generation + 1);
    assert.equal(r.parts.lastState, null);
    held.resolve(r.state(99)); await playback; await volume; await settle();
    assert.equal(r.observed(), observations, 'late playback cannot become a new observation');
    assert.notEqual(r.level.input.value, '99', 'late member read cannot repaint a level');
    assert.equal(mutationRequests(h).length, 0);
  });
}

for (const kind of ['disconnect', 'reset']) {
  test(`${kind} retires an old topology response before it can replace the household`, async t => {
    const h = runtime(t); await h.time.tick(300);
    const held = deferred(); h.answer(() => held.promise);
    const reading = h.loadZones().catch(e => e); await settle();
    if (kind === 'disconnect') h.streams[0].end();
    else h.streams[0].send('event: events.reset\ndata: {}\n\n');
    await settle();
    held.resolve([{ household: 'OLD', coordinator_room: 'Obsolete', members: ['Obsolete'], transport_state: 'stopped' }]);
    await reading; await settle(); assert.equal(h.house.zones.length, 0);
  });
}

for (const kind of ['disconnect', 'reset', 'pagehide']) {
  test(`${kind} does not cancel or resend an already submitted music command`, async t => {
    const h = runtime(t); await h.time.tick(300);
    const held = deferred(); h.answer((url, init) => init.method === 'POST' ? held.promise : []);
    const writing = h.call('next', { body: { zone: 'Den@S2' } }); await settle();
    if (kind === 'disconnect') h.streams[0].end();
    else if (kind === 'reset') h.streams[0].send('event: events.reset\ndata: {}\n\n');
    else h.window.emit('pagehide');
    await settle();
    const [command] = mutationRequests(h);
    assert.equal(command.init.signal.aborted, false);
    held.resolve({ done: 'Advanced', changed: true }); assert.equal((await writing).done, 'Advanced');
    await h.time.tick(1300); assert.equal(mutationRequests(h).length, 1);
  });
}

for (const kind of ['visibility', 'pagehide']) {
  test(`${kind} releases the stream, stops background work and resumes once`, async t => {
    const h = runtime(t); await h.time.tick(300);
    if (kind === 'visibility') { h.document.hidden = true; h.document.emit('visibilitychange'); }
    else h.window.emit('pagehide');
    await settle(); assert.equal(h.streams[0].cancelled, 1); assert.equal(h.label(), 'Live updates paused');
    const before = h.requests.length; await h.time.tick(600000); assert.equal(h.requests.length, before);
    h.document.hidden = false;
    if (kind === 'visibility') h.document.emit('visibilitychange'); else h.window.emit('pageshow');
    h.window.emit('pageshow'); await h.time.tick(300);
    assert.equal(h.streams.length, 2); assert.equal(reads(h), 2); assert.equal(h.label(), 'Live');
    h.streams[0].send('event: zone.state\ndata: {"room":"Den"}\n\n'); await h.time.tick(300);
    assert.equal(reads(h), 2, 'a retired stream cannot trigger another read');
  });
}

test('old read-only preflight cancellation never proceeds to its dependent command', async t => {
  const h = runtime(t); await h.time.tick(300);
  const held = deferred(); h.answer(() => held.promise);
  const operation = h.call('state', { params: { room: 'Den@S2' } })
    .then(() => h.call('pause', { body: { zone: 'Den@S2' } })).catch(e => e);
  await settle(); h.streams[0].end(); await settle(); held.resolve({});
  assert.equal((await operation).code, 'REMOTE_READ_CANCELLED'); assert.equal(mutationRequests(h).length, 0);
});

function reconciler() {
  const time = clock(), feed = new Events(), starts = [], labels = [], lost = [];
  feed.readyState = 1; feed.close = () => {};
  const live = vm.runInNewContext(remote.replace(/\nstart\(\);\s*$/, '') + '\nLiveRemote;', { setTimeout, clearTimeout });
  return { time, feed, starts, labels, lost, create(extra = {}) {
    return live.model({ connect: () => feed, schedule: time.schedule, cancel: time.cancel,
      reload: async () => starts.push('all'), refresh: async room => starts.push(room),
      status: label => labels.push(label), invalidate: (_, reset) => { if (reset) lost.push(true); }, ...extra });
  } };
}

for (const kind of ['error', 'events.reset']) {
  test(`${kind} stops unstarted members of a bounded read batch`, async () => {
    const h = reconciler(), held = deferred();
    const c = h.create({ refresh: async room => { h.starts.push(room); await held.promise; } });
    c.resume(); h.feed.emit('open'); await h.time.tick(300);
    for (const room of ['A', 'B', 'C', 'D', 'E']) h.feed.emit('zone.state', { room });
    await h.time.tick(300); h.feed.emit(kind); held.resolve(); await settle();
    assert.deepEqual(h.starts, ['all', 'A', 'B', 'C', 'D']); assert.ok(h.lost.length >= 2);
    c.dispose(); assert.equal(h.time.pending, 0);
  });
}

test('a journal reset during a snapshot cannot mark the old snapshot synchronized', async () => {
  const h = reconciler(), first = deferred(); let reads = 0;
  const c = h.create({ reload: async () => { if (++reads === 1) await first.promise; } });
  c.resume(); h.feed.emit('open'); await h.time.tick(300);
  h.feed.emit('events.reset'); first.resolve(); await settle();
  assert.equal(h.labels.at(-1), 'Synchronizing…');
  await h.time.tick(300); assert.equal(h.labels.at(-1), 'Live'); assert.equal(reads, 2); c.dispose();
});

test('standalone embeds preserve native EventSource when the helper is absent', async t => {
  let instances = 0;
  class Native extends Events { constructor(url) { super(); assert.equal(url, '/events'); instances++; } close() {} }
  const window = new Events(); window.EventSource = Native;
  const h = runtime(t, { feed: false, globals: { EventSource: Native, window } });
  await h.time.tick(300); assert.equal(instances, 1); assert.equal(h.streams.length, 0); assert.equal(reads(h), 1);
});

test('no streaming capability preserves bounded read-only fallback', async t => {
  const h = runtime(t, { globals: { ReadableStream: undefined, TextDecoder: undefined } });
  await h.time.tick(300); await h.time.tick(10300);
  assert.equal(h.streams.length, 0); assert.equal(reads(h), 2); assert.equal(h.label(), 'Refreshing without live events');
});


test('renderer error and null status events refresh the existing remote card without playback commands', async t => {
  const h = runtime(t), r = room(h);
  let status = 'ERROR_OCCURRED';
  h.answer(async url => url === '/zones' ? [r.zone]
    : { ...r.state(), transport_state: 'stopped', transport_status: status });
  await h.time.tick(350);
  assert.equal(h.streams.length, 1);
  for (status of ['ERROR_OCCURRED', 'OK', null]) {
    h.streams[0].send('event: zone.state\ndata: ' + JSON.stringify({ room: 'Den', transport_status: status }) + '\n\n');
    await h.time.tick(350);
    assert.equal(r.parts.lastState.transport_status, status);
    if (status === 'ERROR_OCCURRED') assert.match(r.parts.state.textContent, /Playback error reported by the speaker/);
    else assert.equal(r.parts.state.textContent, 'stopped');
  }
  assert.equal(mutationRequests(h).length, 0);
});

test('same-length native queue edits refresh a paused room once per burst', async t => {
  const h = runtime(t), r = room(h); r.zone.transport_state = 'paused';
  h.answer(async url => url === '/zones' ? [r.zone] : { ...r.state(), transport_state: 'paused', queue_length: 3 });
  await h.time.tick(300); const before = h.requests.length, observed = r.observed();
  h.streams[0].send('event: zone.state\ndata: {"room":"Den","queue_changed":true}\n\n'.repeat(60));
  await h.time.tick(300);
  assert.equal(h.requests.slice(before).filter(r => r.url.endsWith('/state')).length, 1);
  assert.equal(r.observed(), observed + 1); assert.equal(r.parts.lastState.queue_length, 3);
  assert.equal(reads(h), 1); assert.equal(mutationRequests(h).length, 0);
});

test('catalog events reach the native catalog lane without rereading unrelated playback', async () => {
  const h = reconciler(), received = [], c = h.create({ catalog: change => received.push(change) });
  c.resume(); h.feed.emit('open'); await h.time.tick(300); received.length = 0;
  h.feed.emit('catalog.changed', { household: 'S1', favorites: true, playlists: false });
  await h.time.tick(300);
  assert.deepEqual(JSON.parse(JSON.stringify(received)), [{ household: 'S1', favorites: true, playlists: false }]);
  assert.deepEqual(h.starts, ['all']);
  h.feed.emit('catalog.changed', []); assert.equal(received.at(-1), null);
  c.dispose();
});

test('the actual phone page forwards native catalog changes and journal gaps to its open panels', async t => {
  const h = runtime(t); await h.time.tick(300); const received = [];
  h.document.addEventListener('fsonos:catalog-changed', event => received.push(event.detail));
  h.streams[0].send('event: catalog.changed\ndata: {"household":"S2","favorites":false,"playlists":true}\n\n');
  await settle();
  assert.deepEqual(JSON.parse(JSON.stringify(received)), [{ household: 'S2', favorites: false, playlists: true }]);
  assert.equal(reads(h), 1);
  h.streams[0].send('event: events.reset\ndata: {"oldest":3}\n\n'); await settle();
  assert.equal(received.at(-1), null); assert.equal(mutationRequests(h).length, 0);
});

test('an unavailable room does not delay later healthy-room events or force full-house retries', async t => {
  const h = runtime(t), broken = room(h, 'Side'), healthy = room(h, 'Main');
  let unavailable = false;
  h.answer(async url => url === '/zones' ? [broken.zone, healthy.zone]
    : url.includes('Side%40S2') ? unavailable ? {} : broken.state() : healthy.state());
  await h.time.tick(300); unavailable = true;
  h.streams[0].send('event: zone.state\ndata: {"room":"Side"}\n\n');
  await h.time.tick(300);
  const before = healthy.observed(), brokenReads = h.requests.filter(r => r.url.includes('Side%40S2')).length;
  h.streams[0].send('event: zone.state\ndata: {"room":"Main"}\n\n');
  await h.time.tick(300);
  assert.equal(healthy.observed(), before + 1, 'healthy playback follows its event during another room’s retry delay');
  for (let i = 0; i < 10; ++i) {
    h.streams[0].send('event: zone.state\ndata: {"room":"Side"}\n\n');
    await h.time.tick(1000);
  }
  assert.equal(h.requests.filter(r => r.url.includes('Side%40S2')).length, brokenReads, 'failed-room event storms respect its backoff');
  unavailable = false; await h.time.tick(10000);
  assert.equal(broken.observed(), 2, 'the failed room recovers independently');
  assert.equal(reads(h), 1, 'room recovery does not rescan or reread healthy rooms');
  assert.equal(h.label(), 'Live'); assert.equal(mutationRequests(h).length, 0);
});

test('a partial initial snapshot remains live for healthy rooms and isolates same-name households', async t => {
  const h = runtime(t), broken = room(h, 'Den', 'S1'), healthy = room(h, 'Den', 'S2');
  let unavailable = true;
  h.answer(async url => url === '/zones' ? [broken.zone, healthy.zone]
    : url.includes('Den%40S1') ? unavailable ? {} : broken.state() : healthy.state());
  await h.time.tick(300); assert.equal(healthy.observed(), 1);
  h.streams[0].send('event: zone.state\ndata: {"room":"Den"}\n\n');
  await h.time.tick(300);
  assert.equal(healthy.observed(), 2, 'the same room name never makes one household inherit another household’s failure');
  assert.equal(h.requests.filter(r => r.url.includes('Den%40S1')).length, 1);
  unavailable = false; await h.time.tick(20000);
  assert.equal(broken.observed(), 1); assert.equal(healthy.observed(), 2);
  assert.equal(h.label(), 'Live'); assert.equal(reads(h), 1); assert.equal(mutationRequests(h).length, 0);
});

test('full-house events preserve failed-room backoff and retire departed recovery targets', async t => {
  const h = runtime(t), broken = room(h, 'Side'), healthy = room(h, 'Main');
  let zones = [broken.zone, healthy.zone];
  h.answer(async url => url === '/zones' ? zones : url.includes('Side%40S2') ? {} : healthy.state());
  await h.time.tick(300);
  const before = healthy.observed();
  h.streams[0].send('event: action.logged\ndata: {}\n\n'); await h.time.tick(300);
  assert.equal(healthy.observed(), before + 1);
  assert.equal(h.requests.filter(r => r.url.includes('Side%40S2')).length, 1, 'a house-wide event does not shorten room backoff');
  zones = [healthy.zone];
  h.streams[0].send('event: topology.changed\ndata: {}\n\n'); await h.time.tick(300);
  assert.equal(h.label(), 'Live'); assert.equal(h.house.cards.has(h.zoneKey(broken.zone)), false);
  const requests = h.requests.length;
  await h.time.tick(25000);
  assert.equal(h.requests.length, requests, 'a departed group cannot leave a future recovery request');
  assert.equal(mutationRequests(h).length, 0);
});

test('a failed room retains its own exponential recovery deadline while healthy events continue', async t => {
  const h = runtime(t), broken = room(h, 'Side'), healthy = room(h, 'Main');
  let unavailable = true;
  h.answer(async url => url === '/zones' ? [broken.zone, healthy.zone]
    : url.includes('Side%40S2') ? unavailable ? {} : broken.state() : healthy.state());
  await h.time.tick(300); await h.time.tick(20300);
  assert.equal(h.requests.filter(r => r.url.includes('Side%40S2')).length, 2);
  const readsAtFailure = healthy.observed();
  for (let i = 0; i < 3; ++i) {
    h.streams[0].send('event: zone.state\ndata: {"room":"Main"}\n\n: heartbeat\n\n');
    await h.time.tick(10000);
  }
  assert.equal(healthy.observed(), readsAtFailure + 3);
  assert.equal(h.requests.filter(r => r.url.includes('Side%40S2')).length, 2, 'healthy successes do not reset a failed room’s 40-second delay');
  unavailable = false; await h.time.tick(10300);
  assert.equal(broken.observed(), 1); assert.equal(h.label(), 'Live'); assert.equal(reads(h), 1);
});

test('server Retry-After gates live snapshot reads and survives page hide and resume', async t => {
  const h = runtime(t), broken = room(h, 'Side'), healthy = room(h, 'Main');
  let limited = false;
  h.answer(async url => url === '/zones' ? [broken.zone, healthy.zone]
    : url.includes('Side%40S2') ? broken.state() : healthy.state());
  h.response(url => limited && url.includes('Side%40S2') ? {
    ok: false, status: 503, headers: { get: name => name === 'retry-after' ? '90' : null },
    text: async () => '{"code":"UNAVAILABLE","detail":"Read again later"}',
  } : null);
  await h.time.tick(300); limited = true;
  h.streams[0].send('event: zone.state\ndata: {"room":"Side"}\n\n'); await h.time.tick(300);
  h.streams[0].send('event: zone.state\ndata: {"room":"Main"}\n\n');
  const before = h.requests.filter(r => r.url !== '/events').length;
  h.window.emit('pagehide'); h.window.emit('pageshow'); await settle();
  for (let i = 0; i < 5; ++i) { h.streams.at(-1).send(': heartbeat\n\n'); await h.time.tick(15000); }
  await h.time.tick(14999);
  assert.equal(h.requests.filter(r => r.url !== '/events').length, before);
  limited = false; await h.time.tick(301);
  assert.equal(h.label(), 'Live'); assert.equal(broken.observed(), 2); assert.equal(healthy.observed(), 2);
  assert.equal(mutationRequests(h).length, 0);
});

test('group membership disagreement retains full-topology recovery instead of retrying an obsolete group', async t => {
  const h = runtime(t), r = room(h);
  let disagree = true;
  h.answer(async url => url === '/zones' ? [r.zone]
    : disagree ? { ...r.state(), zone: { ...r.zone, members: ['Den', 'Side'] } } : r.state());
  await h.time.tick(300);
  assert.equal(h.label(), 'State refresh failed; retrying…'); assert.equal(r.observed(), 0);
  disagree = false; await h.time.tick(20300);
  assert.equal(reads(h), 2); assert.equal(r.observed(), 1); assert.equal(h.label(), 'Live');
});
