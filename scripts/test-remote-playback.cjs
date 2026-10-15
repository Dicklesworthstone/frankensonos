// Actual playback controller and card lifecycle; no real speakers or Spotify needed.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const http = require('node:http');
const { once } = require('node:events');
const Playback = require('../crates/fsonos-api/src/remote/playback.js');
const root = path.join(__dirname, '..');
const source = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/remote/remote.js'), 'utf8');
const playbackSource = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/remote/playback.js'), 'utf8');
const clone = (value) => JSON.parse(JSON.stringify(value));
const settle = async () => { for (let n = 0; n < 80; n++) await Promise.resolve(); };
function deferred() {
  let resolve, reject;
  const promise = new Promise((a, b) => { resolve = a; reject = b; });
  return { promise, resolve, reject };
}
function zone(home = 'S2', name = 'Den', members = [name]) {
  return { household: home, coordinator_room: name, members, transport_state: 'playing', degraded: [] };
}
function state(z = zone(), overrides = {}) {
  return { zone: clone(z), transport_state: 'playing', volume: 20, mute: false,
    track: { uri: 'renderer:sample', title: 'Sample', duration_secs: 240, position_secs: 20, queue_position: 3 },
    ...overrides };
}
function clock() {
  let time = 10000, serial = 0;
  const timers = new Map();
  return { now: () => time, timers,
    schedule(fn, delay) { timers.set(++serial, { fn, at: time + delay }); return serial; },
    cancel(id) { timers.delete(id); },
    advance(ms) {
      const end = time + ms;
      for (let n = 0; n < 10000; n++) {
        const first = [...timers].sort((a, b) => a[1].at - b[1].at)[0];
        if (!first || first[1].at > end) { time = end; return; }
        time = first[1].at; timers.delete(first[0]); first[1].fn();
      }
      throw new Error('timer loop');
    },
  };
}
function model(options = {}) {
  const c = clock(), calls = [], views = [];
  const h = { c, calls, views, value: state(options.zone), custom: null };
  h.api = Playback.model({ zone: options.zone || zone(), now: c.now, schedule: c.schedule, cancel: c.cancel,
    call: async (...args) => { calls.push(args); return h.custom ? h.custom(...args) : clone(h.value); },
    changed: (v) => views.push(v), ...options });
  return h;
}

test('time formatting keeps zero, hours and unknowns distinct', () => {
  for (const [n, expected] of [[0, '0:00'], [59, '0:59'], [60, '1:00'], [3599, '59:59'], [3600, '1:00:00'], [3661, '1:01:01']]) {
    assert.equal(Playback.time(n), expected);
  }
  for (const value of [null, undefined, -1, NaN, Infinity, 1.2, '30', 0x100000000]) assert.equal(Playback.time(value), '–:––');
});

test('the public DTO is copied, frozen and qualified by exact group membership', () => {
  const z = zone('S1', 'Room', ['Room', 'Other']), value = state(z);
  const read = Playback.decode(value, Playback.shape(z));
  value.track.position_secs = 99;
  assert.equal(read.track.position, 20); assert.ok(Object.isFrozen(read.track));
  const reordered = clone(z); reordered.members.reverse();
  assert.equal(Playback.shape(z), Playback.shape(reordered));
  assert.throws(() => Playback.decode(state(zone('S2', 'Room')), Playback.shape(z)));
  assert.notEqual(Playback.shape(zone('a/b', 'c')), Playback.shape(zone('a', 'b/c')));
});

test('malformed timing and ambiguous topology never become an observed position', () => {
  const base = state(), group = Playback.shape(base.zone), bad = [null, {}, [], { ...base, transport_state: 'PLAYING' }];
  for (const key of ['position_secs', 'duration_secs', 'queue_position']) {
    for (const value of [-1, 0.2, '20', NaN, Infinity, 0x100000000, false]) {
      bad.push({ ...base, track: { ...base.track, [key]: value } });
    }
  }
  bad.push({ ...base, track: { ...base.track, queue_position: 0 } },
    { ...base, track: { ...base.track, position_secs: 241 } },
    { ...base, track: { ...base.track, uri: 'NOT_IMPLEMENTED' } });
  for (const value of bad) assert.throws(() => Playback.decode(value, group));
  for (const z of [zone('S2', 'Den', []), zone('S2', 'Den', ['Den', 'Den']), zone('S2', 'Den', ['Other'])]) {
    assert.throws(() => Playback.shape(z));
  }
});

test('unknown, zero-duration and absent track data are not fabricated clocks', () => {
  const h = model(); h.api.observe({ ...state(), track: null });
  assert.equal(h.api.view().elapsed, null); assert.equal(h.api.view().duration, null);
  h.api.observe({ ...state(), track: { uri: 'renderer:stream', duration_secs: 0 } });
  assert.equal(h.api.view().elapsed, null); assert.equal(h.c.timers.size, 0);
  h.api.observe({ ...state(), track: { uri: 'renderer:stream', position_secs: 5 } });
  h.c.advance(2000);
  assert.equal(h.api.view().elapsed, 7); assert.equal(h.api.view().remaining, null);
});

test('playing clocks interpolate locally without any network calls', () => {
  const h = model(); h.api.observe(state()); h.c.advance(6500);
  assert.equal(h.api.view().elapsed, 26); assert.equal(h.api.view().remaining, 214);
  assert.equal(h.api.view().estimated, true); assert.equal(h.calls.length, 0);
  assert.equal(h.c.timers.size, 1);
});

test('paused, stopped, transitioning and unknown transport never advance', () => {
  for (const transport_state of ['paused', 'stopped', 'transitioning', 'unknown']) {
    const h = model(); h.api.observe(state(zone(), { transport_state })); h.c.advance(60000);
    assert.equal(h.api.view().elapsed, 20); assert.equal(h.api.view().estimated, false);
    assert.equal(h.c.timers.size, 0); assert.equal(h.calls.length, 0);
  }
});

test('predicted track end is clamped but never becomes an invented transition', () => {
  const h = model(); h.api.observe(state()); h.c.advance(300000);
  assert.equal(h.api.view().elapsed, 240); assert.equal(h.api.view().remaining, 0);
  assert.equal(h.api.view().transport, 'playing'); assert.equal(h.c.timers.size, 0);
  assert.equal(h.calls.length, 0);
});

test('a new authoritative sample replaces estimates including backward seeks and duplicate URIs', () => {
  const h = model(); h.api.observe(state()); h.c.advance(5000);
  const next = state(); next.track.queue_position = 4; next.track.position_secs = 0;
  h.api.observe(next); assert.equal(h.api.view().elapsed, 0);
  h.c.advance(3000); assert.equal(h.api.view().elapsed, 3);
  next.transport_state = 'paused'; next.track.position_secs = 1; h.api.observe(next);
  h.c.advance(3000); assert.equal(h.api.view().elapsed, 1);
});

test('stale clocks freeze, retire timers and recover through exactly one explicit read', async () => {
  const h = model(); h.api.observe(state()); h.c.advance(2500); h.api.stale(); h.c.advance(60000);
  assert.equal(h.api.view().elapsed, 22); assert.equal(h.api.view().fresh, false); assert.equal(h.c.timers.size, 0);
  h.value.track.position_secs = 90;
  assert.equal(await h.api.refresh(), true); assert.equal(h.api.view().elapsed, 90);
  assert.deepEqual(h.calls, [['state', { params: { room: 'Den@S2' } }]]);
});

test('failed reads retain explicitly stale timing and the actionable hint', async () => {
  const h = model(); h.api.observe(state());
  h.custom = async () => { throw Object.assign(new Error('failed'), { hint: 'Reconnect the daemon.' }); };
  assert.equal(await h.api.refresh(), false); assert.equal(h.api.view().elapsed, 20);
  assert.equal(h.api.view().fresh, false); assert.match(h.api.view().message, /Reconnect/);
  h.c.advance(60000); assert.equal(h.calls.length, 1);
});

test('a newer card observation defeats a delayed explicit read', async () => {
  const h = model(), wait = deferred(); h.custom = () => wait.promise;
  const old = h.api.refresh(); const next = state(); next.track.position_secs = 100;
  h.api.observe(next); wait.resolve(state()); await old;
  assert.equal(h.api.view().elapsed, 100); assert.equal(h.api.view().reading, false);
});

test('repeated refresh clicks coalesce while a read is in flight', async () => {
  const h = model(), wait = deferred(); h.custom = () => wait.promise;
  const read = h.api.refresh(); assert.equal(await h.api.refresh(), false);
  wait.resolve(state()); await read; assert.equal(h.calls.length, 1);
});

test('suspension discards late reads and resume waits for a fresh shared observation', async () => {
  const h = model(), wait = deferred(); h.api.observe(state()); h.custom = () => wait.promise;
  const read = h.api.refresh(); h.api.suspend(); h.c.advance(10000); wait.resolve(state()); await read;
  assert.equal(h.api.view().fresh, false); assert.equal(await h.api.refresh(), false);
  assert.equal(h.api.observe(state()), false); h.api.resume();
  assert.equal(h.api.view().fresh, false); assert.equal(h.calls.length, 1);
  h.api.observe(state()); assert.equal(h.api.view().fresh, true);
});

test('disposal and detached ownership cannot schedule or initiate new work', async () => {
  let owned = true; const h = model({ current: () => owned }); h.api.observe(state()); owned = false;
  h.c.advance(1000); assert.equal(h.c.timers.size, 0); assert.equal(await h.api.refresh(), false);
  owned = true; h.api.dispose(); h.api.resume(); h.api.observe(state());
  assert.equal(await h.api.refresh(), false); assert.equal(h.c.timers.size, 0); assert.equal(h.calls.length, 0);
});

test('observer failures and repeated samples never multiply animation timers', () => {
  const h = model({ changed() { throw new Error('view failed'); } });
  for (let n = 0; n < 200; n++) assert.equal(h.api.observe(state()), true);
  assert.equal(h.c.timers.size, 1); h.c.advance(10000); assert.equal(h.calls.length, 0);
  h.api.dispose(); assert.equal(h.c.timers.size, 0);
});

// The production remote runs here, including its route map and per-card hooks.
function browser({ fetcher, install = true } = {}) {
  const c = clock(), ids = new Map(), calls = [];
  class Events {
    constructor() { this.events = new Map(); }
    addEventListener(key, fn) { if (!this.events.has(key)) this.events.set(key, new Set()); this.events.get(key).add(fn); }
    removeEventListener(key, fn) { this.events.get(key)?.delete(fn); }
    emit(key, more = {}) { for (const fn of [...(this.events.get(key) || [])]) fn({ target: this, ...more }); }
  }
  class Element extends Events {
    constructor(tag) { super(); this.tag = tag; this.children = []; this.attributes = {}; this.textContent = ''; this.disabled = false; }
    setAttribute(k, v) { this.attributes[k] = String(v); if (k === 'id') ids.set(v, this); if (k === 'value') this.value = String(v); if (['hidden', 'disabled'].includes(k)) this[k] = true; }
    getAttribute(k) { return this.attributes[k] ?? null; }
    removeAttribute(k) { delete this.attributes[k]; }
    append(...nodes) { this.children.push(...nodes); }
    replaceChildren(...nodes) { this.children = [...nodes]; }
    contains(node) { return this === node || this.children.some((v) => typeof v === 'object' && v.contains(node)); }
    focus() { doc.activeElement = this; }
  }
  const doc = new Events(); doc.hidden = false; doc.activeElement = null;
  doc.getElementById = (id) => ids.get(id); doc.createElement = (tag) => new Element(tag);
  for (const id of ['households', 'toast', 'live', 'pause-all']) { const e = new Element('div'); e.setAttribute('id', id); }
  const win = new Events(); const h = { c, calls, doc, win, zones: [zone()], custom: null };
  const ctx = vm.createContext({ document: doc, window: win, AbortController, URLSearchParams,
    performance: { now: c.now }, setTimeout: c.schedule, clearTimeout: c.cancel,
    fetch: async (url, init) => {
      calls.push([url, init.method, init.body ? JSON.parse(init.body) : null]);
      if (fetcher) return fetcher(url, init);
      let value = h.custom?.(url, init);
      if (value === undefined) {
        if (url === '/zones') value = h.zones;
        else if (url.endsWith('/state')) {
          const name = decodeURIComponent(url.slice(7, -6));
          value = state(h.zones.find((z) => `${z.coordinator_room}@${z.household}` === name));
        } else if (url.endsWith('/dj')) value = { running: false };
        else value = { done: 'Changed', changed: true };
      }
      value = await value; return { ok: true, text: async () => JSON.stringify(value) };
    },
    QueueRemote: { panel({ el }) { return { node: el('details'), dispose() {} }; } },
  });
  vm.runInContext(source.replace(/\nstart\(\);\s*$/, ''), ctx);
  h.install = () => vm.runInContext(playbackSource, ctx);
  if (install) h.install();
  h.api = vm.runInContext('({house, loadZones, refreshZone, render, listen, act})', ctx);
  h.parts = () => [...h.api.house.cards.values()][0];
  return h;
}

test('the embedded panel receives real card observations and persists through ordinary redraws', async () => {
  const h = browser(); await h.api.loadZones(); const p = h.parts(), panel = p.panels[0];
  assert.equal(panel.view().elapsed, 20); const count = h.calls.length;
  h.c.advance(5000); assert.equal(panel.view().elapsed, 25); assert.equal(h.calls.length, count);
  await h.api.loadZones(); assert.equal(h.parts(), p); assert.equal(h.parts().panels[0], panel);
  assert.equal(h.doc.events.get('visibilitychange').size, 1);
});

test('panels registered after a card exists receive its last ordered sample without an extra request', async () => {
  const h = browser({ install: false }); await h.api.loadZones(); const count = h.calls.length;
  h.install(); assert.equal(h.parts().panels[0].view().elapsed, 20); assert.equal(h.calls.length, count);
});

test('group replacement disposes only its old clock and removes all its listeners', async () => {
  const h = browser(); await h.api.loadZones(); const old = h.parts().panels[0];
  h.zones = [zone('S1', 'Other')]; await h.api.loadZones();
  assert.equal(old.view().fresh, false); assert.equal(await old.refresh(), false);
  assert.equal(h.doc.events.get('visibilitychange').size, 1);
  h.api.house.zones = []; h.api.render();
  assert.equal(h.doc.events.get('visibilitychange').size, 0);
  assert.equal(h.win.events.get('pagehide').size, 0); assert.equal(h.c.timers.size, 0);
});

test('failed topology reads freeze retained clocks instead of displaying live stale state', async () => {
  const h = browser(); await h.api.loadZones(); const panel = h.parts().panels[0]; h.c.advance(3000);
  h.custom = (url) => url === '/zones' ? null : undefined;
  await assert.rejects(h.api.loadZones()); h.c.advance(10000);
  assert.equal(panel.view().elapsed, 23); assert.equal(panel.view().fresh, false);
});

test('hiding and page-cache suspension stop clocks without starting a per-card poll on return', async () => {
  const h = browser(); await h.api.loadZones(); const panel = h.parts().panels[0], count = h.calls.length;
  h.doc.hidden = true; h.doc.emit('visibilitychange'); h.c.advance(10000);
  assert.equal(panel.view().fresh, false); assert.equal(panel.observe(state()), false);
  h.doc.hidden = false; h.doc.emit('visibilitychange');
  assert.equal(h.calls.length, count); assert.equal(panel.view().fresh, false);
  await h.api.refreshZone(h.zones[0]); assert.equal(panel.view().fresh, true);
  h.win.emit('pagehide'); assert.equal(panel.observe(state()), false);
  h.win.emit('pageshow'); assert.equal(panel.view().fresh, false);
});

test('the production fetch client reads the exact qualified state over a real loopback socket', async (t) => {
  const received = [], z = zone('S1', 'A & B/?#');
  const server = http.createServer((req, res) => {
    received.push([req.method, req.url]); res.setHeader('Content-Type', 'application/json');
    const data = req.url === '/zones' ? [z] : req.url.endsWith('/state') ? state(z) : { running: false };
    res.end(JSON.stringify(data));
  });
  server.listen(0, '127.0.0.1'); await once(server, 'listening');
  t.after(() => { server.closeAllConnections(); server.close(); });
  const base = `http://127.0.0.1:${server.address().port}`;
  const h = browser({ fetcher: (url, init) => fetch(base + url, init) }); await h.api.loadZones();
  assert.equal(h.parts().panels[0].view().elapsed, 20);
  await h.parts().panels[0].refresh();
  assert.deepEqual(received.filter(([, url]) => url.endsWith('/state')), [
    ['GET', '/zones/A%20%26%20B%2F%3F%23%40S1/state'], ['GET', '/zones/A%20%26%20B%2F%3F%23%40S1/state'],
  ]);
  assert.ok(received.every(([method]) => method === 'GET'));
});

test('the new script and style are embedded, scanned and tested under the unchanged browser policy', () => {
  const rust = fs.readFileSync(path.join(root, 'crates/fsonos-api/src/http/remote.rs'), 'utf8');
  const ci = fs.readFileSync(path.join(root, '.github/workflows/linux-speech.yml'), 'utf8');
  assert.match(rust, /include_str!\("\.\.\/remote\/playback\.js"\)/);
  assert.match(rust, /include_str!\("\.\.\/remote\/playback\.css"\)/);
  assert.match(rust, /\{CONNECT_SCRIPT\}\\n\{PLAYBACK_SCRIPT\}/);
  assert.match(rust, /\("playback\.js", PLAYBACK_SCRIPT\)/);
  assert.match(ci, /run: node --test .*scripts\/test-remote-playback.cjs/);
  assert.match(rust, /script-src 'self'; style-src 'self'/);
  for (const forbidden of ['token', 'authorization', 'x-fsonos', 'tailscale-user', 'password', 'cookie', 'ts.net', 'http://', 'https://']) {
    assert.ok(!playbackSource.toLowerCase().includes(forbidden), forbidden);
  }
});
