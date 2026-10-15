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

function seeking(options = {}) {
  const h = model(options);
  h.custom = async (name, options) => {
    if (name === 'seek') {
      h.value.track.position_secs = options.body.to_secs;
      return { done: 'Seek accepted', changed: true };
    }
    return clone(h.value);
  };
  h.api.observe(h.value);
  return h;
}
const writes = (h) => h.calls.filter(([name]) => name === 'seek');
const descendants = (node) => [node, ...(node.children || []).flatMap((child) =>
  typeof child === 'object' ? descendants(child) : [])];
const button = (node, label) => descendants(node).find((node) => node.tag === 'button' && node.textContent === label);

test('seek previews, nudges, cancellation and invalid positions send no commands', () => {
  const h = seeking();
  const selected = h.api.preview(90); assert.equal(selected.target, 90); assert.ok(Object.isFrozen(selected));
  assert.equal(h.api.nudge(15).target, 35); assert.equal(h.api.nudge(-15).target, 5);
  h.api.cancelReview(); assert.equal(h.api.view().review, null);
  for (const target of [-1, 240, 1000, 0.1, '20', null, NaN, Infinity]) {
    h.api.preview(90); assert.equal(h.api.preview(target), null);
    assert.equal(h.api.view().review, null);
  }
  assert.equal(h.calls.length, 0);
});

test('absolute seeks include zero and the final second, use one native POST and read back actual timing', async () => {
  for (const target of [0, 1, 90, 239]) {
    const h = seeking(); const selected = h.api.preview(target);
    assert.equal(await h.api.confirm(selected), true);
    assert.deepEqual(h.calls.map(([name]) => name), ['state', 'seek', 'state']);
    assert.deepEqual(writes(h), [['seek', { body: { zone: 'Den@S2', to_secs: target } }]]);
    assert.equal(h.api.view().elapsed, target); assert.equal(h.api.view().result.status, 'accepted');
    assert.equal(await h.api.confirm(selected), false); assert.equal(writes(h).length, 1);
  }
});

test('nudges are absolute positions from the visible estimate and never exceed the track bounds', () => {
  const h = seeking(); h.c.advance(5000); assert.equal(h.api.nudge(15).target, 40);
  h.value.track.position_secs = 2; h.api.observe(h.value); assert.equal(h.api.nudge(-15).target, 0);
  h.value.track.position_secs = 235; h.api.observe(h.value); assert.equal(h.api.nudge(15).target, 239);
  assert.equal(h.calls.length, 0);
});

test('seeking a paused track neither starts it nor changes source, queue or levels', async () => {
  const h = seeking(); h.value.transport_state = 'paused'; h.api.observe(h.value);
  assert.equal(await h.api.confirm(h.api.preview(60)), true);
  assert.deepEqual(h.calls.map(([name]) => name), ['state', 'seek', 'state']);
  assert.equal(h.api.view().transport, 'paused'); h.c.advance(5000);
  assert.equal(h.api.view().elapsed, 60);
});

test('unknown timing, nonsteady transport and stale reads cannot authorize seeking', async () => {
  const variants = [state(zone(), { track: null }), state(zone(), { track: { uri: 'stream' } }),
    state(zone(), { track: { uri: 'stream', position_secs: 0 } }),
    ...['stopped', 'transitioning', 'unknown'].map((transport_state) => state(zone(), { transport_state }))];
  for (const value of variants) {
    const h = seeking(); h.api.observe(value); assert.equal(h.api.view().seekable, false);
    assert.equal(h.api.preview(0), null); assert.equal(h.api.nudge(15), null);
    assert.equal(await h.api.confirm({ target: 0 }), false); assert.equal(h.calls.length, 0);
  }
  const h = seeking(), selected = h.api.preview(60); h.api.stale();
  assert.equal(await h.api.confirm(selected), false); assert.equal(h.calls.length, 0);
});

test('natural playback progress and cosmetic metadata preserve a deliberate draft', async () => {
  const h = seeking(), selected = h.api.preview(80); h.c.advance(10000);
  h.value.track.position_secs = 31; h.value.track.title = 'Updated title'; h.api.observe(h.value);
  assert.equal(h.api.view().review, selected); assert.equal(await h.api.confirm(selected), true);
  assert.equal(writes(h)[0][1].body.to_secs, 80);
});

test('changed track, duplicate queue slot, duration or transport invalidate old selection', async () => {
  for (const change of [v => v.track.uri = 'renderer:other', v => v.track.queue_position++,
    v => v.track.duration_secs++, v => v.transport_state = 'paused', v => delete v.track.position_secs]) {
    const h = seeking(), selected = h.api.preview(80); change(h.value); h.api.observe(h.value);
    assert.equal(h.api.view().review, null); assert.equal(await h.api.confirm(selected), false);
    assert.equal(h.calls.length, 0);
  }
});

test('copied, replaced, canceled and refreshed reviews cannot confirm another selection', async () => {
  const h = seeking(), old = h.api.preview(30), next = h.api.preview(70);
  for (const value of [old, clone(next), null, undefined]) assert.equal(await h.api.confirm(value), false);
  h.api.cancelReview(); assert.equal(await h.api.confirm(next), false);
  const fresh = h.api.preview(90); await h.api.refresh();
  assert.equal(await h.api.confirm(fresh), false); assert.equal(writes(h).length, 0);
});

test('preflight detects membership, identity and duplicate-slot changes before any seek', async () => {
  for (const change of [v => v.zone.members.push('Other'), v => v.zone.household = 'S1',
    v => v.track.uri = 'renderer:other', v => v.track.queue_position++,
    v => v.track.duration_secs = 60, v => v.transport_state = 'paused']) {
    const h = seeking(), selected = h.api.preview(80); change(h.value);
    assert.equal(await h.api.confirm(selected), false); assert.equal(writes(h).length, 0);
    assert.equal(h.api.view().result.status, 'not_sent'); assert.equal(h.api.view().fresh, false);
  }
});

test('malformed and failed preflight reads never fall back to an unprotected seek', async () => {
  for (const value of [null, {}, { ...state(), track: { uri: 'missing-timing' } }]) {
    const h = seeking(); h.custom = async () => value;
    assert.equal(await h.api.confirm(h.api.preview(40)), false); assert.equal(writes(h).length, 0);
  }
  const h = seeking(); h.custom = async () => { throw Object.assign(new Error('offline'), { hint: 'Reconnect this room.' }); };
  assert.equal(await h.api.confirm(h.api.preview(40)), false);
  assert.match(h.api.view().result.detail, /Reconnect this room/); assert.equal(writes(h).length, 0);
});

test('double confirmation and edits cannot overlap a preflight or sent seek', async () => {
  const h = seeking(), preflight = deferred(), sent = deferred(); let reads = 0;
  h.custom = (name) => name === 'seek' ? sent.promise : ++reads === 1 ? preflight.promise : clone(h.value);
  const selected = h.api.preview(40), action = h.api.confirm(selected);
  assert.equal(await h.api.confirm(selected), false); assert.equal(h.api.preview(50), null);
  assert.equal(await h.api.refresh(), false); assert.equal(h.api.view().pending, true);
  preflight.resolve(state()); await settle(); assert.equal(writes(h).length, 1);
  assert.equal(await h.api.confirm(selected), false); assert.equal(h.api.preview(50), null);
  sent.resolve({ done: 'seek', changed: true }); assert.equal(await action, true);
  assert.equal(writes(h).length, 1);
});

test('cancellation, event invalidation, hiding and disposal stop an unsent preflight', async () => {
  for (const interrupt of [h => h.api.cancelReview(), h => h.api.stale(), h => h.api.suspend(),
    h => { h.api.suspend(); h.api.resume(); }, h => h.api.dispose()]) {
    const h = seeking(), read = deferred(); h.custom = () => read.promise;
    const action = h.api.confirm(h.api.preview(40)); interrupt(h); read.resolve(state());
    assert.equal(await action, false); assert.equal(writes(h).length, 0); assert.equal(h.api.view().pending, false);
  }
});

test('a changed shared observation during preflight wins even when the old read returns last', async () => {
  const h = seeking(), read = deferred(); h.custom = () => read.promise;
  const action = h.api.confirm(h.api.preview(40)); const next = state(); next.track.queue_position = 4;
  h.api.observe(next); read.resolve(state()); assert.equal(await action, false); assert.equal(writes(h).length, 0);
});

test('lost or malformed seek replies are never replayed or reenabled by passive state updates', async () => {
  for (const reply of [null, {}, { done: 'seek' }, { done: 'seek', changed: 'true' },
    { done: 'seek', changed: true, notes: [null] }, new Error('reply lost')]) {
    const h = seeking(); h.custom = async (name) => {
      if (name !== 'seek') return clone(h.value);
      if (reply instanceof Error) throw reply;
      return reply;
    };
    const selected = h.api.preview(40); assert.equal(await h.api.confirm(selected), false);
    assert.equal(writes(h).length, 1); assert.equal(h.api.view().uncertain, true);
    assert.equal(h.api.view().result.status, 'uncertain');
    h.api.observe(state()); h.c.advance(1000);
    assert.equal(h.api.view().seekable, false); assert.equal(h.api.preview(50), null);
    assert.equal(await h.api.confirm(selected), false);
    assert.equal(await h.api.refresh(), true); assert.equal(h.api.view().uncertain, false);
    assert.equal(h.api.view().seekable, true); assert.equal(writes(h).length, 1);
    assert.equal(await h.api.confirm(selected), false);
  }
});

test('failed explicit recovery cannot clear uncertainty or resurrect a consumed review', async () => {
  const h = seeking(); h.custom = async (name) => { if (name === 'seek') throw new Error('lost'); return state(); };
  const selected = h.api.preview(40); await h.api.confirm(selected);
  h.custom = async () => { throw new Error('still offline'); }; await h.api.refresh();
  assert.equal(h.api.view().uncertain, true); assert.equal(h.api.view().seekable, false);
  assert.equal(await h.api.confirm(selected), false); assert.equal(writes(h).length, 1);
});

test('accepted seeks use observed readback rather than painting the requested position as verified', async () => {
  const h = seeking(); h.custom = async (name) => name === 'seek'
    ? { done: 'Seek accepted', changed: true } : state();
  assert.equal(await h.api.confirm(h.api.preview(80)), true);
  assert.equal(h.api.view().elapsed, 20); assert.equal(h.api.view().result.status, 'accepted');
  assert.equal(writes(h).length, 1);
});

test('a failed read after acknowledgement preserves acceptance and does not repeat the seek', async () => {
  const h = seeking(); let read = 0;
  h.custom = async (name) => {
    if (name === 'seek') return { done: 'Seek accepted', changed: true };
    if (++read > 1) throw new Error('readback offline'); return state();
  };
  assert.equal(await h.api.confirm(h.api.preview(80)), true);
  assert.equal(h.api.view().result.status, 'accepted'); assert.equal(h.api.view().fresh, false);
  assert.equal(writes(h).length, 1);
});

test('closing a sent seek neither aborts nor repeats it and performs no hidden readback', async () => {
  for (const close of [h => h.api.suspend(), h => h.api.dispose()]) {
    const h = seeking(), pending = deferred(); h.custom = (name) => name === 'seek' ? pending.promise : state();
    const action = h.api.confirm(h.api.preview(80)); await settle(); close(h);
    pending.resolve({ done: 'seek', changed: true }); assert.equal(await action, true);
    assert.deepEqual(h.calls.map(([name]) => name), ['state', 'seek']); assert.equal(h.c.timers.size, 0);
  }
});

test('a seek lost after hide/show remains uncertain until an explicit fresh read', async () => {
  const h = seeking(), pending = deferred(); h.custom = (name) => name === 'seek' ? pending.promise : state();
  const action = h.api.confirm(h.api.preview(80)); await settle(); h.api.suspend(); h.api.resume();
  pending.reject(new Error('lost')); assert.equal(await action, false); h.api.observe(state());
  assert.equal(h.api.view().uncertain, true); assert.equal(h.api.view().seekable, false);
  assert.equal(await h.api.refresh(), true); assert.equal(h.api.view().seekable, true);
});

test('same-name rooms in separate households send only their selected qualified destination', async () => {
  for (const home of ['S1', 'S2']) {
    const h = seeking({ zone: zone(home) }); await h.api.confirm(h.api.preview(70));
    assert.equal(writes(h)[0][1].body.zone, `Den@${home}`);
  }
});

test('observer exceptions cannot turn a confirmed seek into a failed operation', async () => {
  const h = seeking({ changed() { throw new Error('render'); } });
  assert.equal(await h.api.confirm(h.api.preview(60)), true); assert.equal(writes(h).length, 1);
});

test('a view that disposes itself before dispatch cannot send a seek', async () => {
  let api;
  const h = seeking({ changed(state) { if (state.pending) api?.dispose(); } }); api = h.api;
  assert.equal(await h.api.confirm(h.api.preview(60)), false); assert.equal(writes(h).length, 0);
});

test('the actual slider previews without dispatch and requires its own confirmation button', async () => {
  const h = browser(); await h.api.loadZones(); const panel = h.parts().panels[0];
  const range = descendants(panel.node).find(node => node.tag === 'input');
  assert.equal(range.disabled, false); range.value = '80'; range.emit('input');
  assert.equal(h.calls.filter(([, method]) => method === 'POST').length, 0);
  const apply = button(panel.node, 'Seek to 1:20'); assert.ok(apply);
  h.c.advance(1000); assert.equal(range.value, '80'); assert.equal(button(panel.node, 'Seek to 1:20'), apply);
  apply.emit('click'); await settle();
  assert.deepEqual(h.calls.filter(([, method]) => method === 'POST'), [['/seek', 'POST', { zone: 'Den@S2', to_secs: 80 }]]);
  apply.emit('click'); await settle(); assert.equal(h.calls.filter(([, method]) => method === 'POST').length, 1);
});

test('native card actions and seek confirmation share the same in-flight exclusion', async () => {
  const h = browser(); await h.api.loadZones(); const p = h.parts(), panel = p.panels[0], pending = deferred();
  h.custom = (url, init) => init.method === 'POST' ? pending.promise : undefined;
  const action = panel.confirm(panel.preview(80)); await settle();
  assert.equal(p.writing, true); assert.equal(p.next.disabled, true);
  p.next.emit('click'); p.play.emit('click'); await settle();
  assert.equal(h.calls.filter(([, method]) => method === 'POST').length, 1);
  pending.resolve({ done: 'seek', changed: true }); await action; await settle();
  assert.equal(p.writing, false); assert.equal(panel.view().result.status, 'accepted');
  const other = deferred(); h.custom = (url, init) => init.method === 'POST' ? other.promise : undefined;
  const selected = panel.preview(30); p.next.emit('click');
  assert.equal(await panel.confirm(selected), false); assert.equal(panel.preview(40), null);
  other.resolve({ done: 'next', changed: true }); await settle();
  assert.deepEqual(h.calls.filter(([, method]) => method === 'POST').map(([url]) => url), ['/seek', '/next']);
});

test('a detached seek confirmation cannot approve a new draft or act on a replaced card', async () => {
  const h = browser(); await h.api.loadZones(); const panel = h.parts().panels[0];
  panel.preview(60); const old = button(panel.node, 'Seek to 1:00');
  panel.preview(90); old.emit('click'); await settle();
  assert.equal(h.calls.filter(([, method]) => method === 'POST').length, 0);
  const newer = button(panel.node, 'Seek to 1:30');
  h.zones = [zone('S2', 'Other')]; await h.api.loadZones(); newer.emit('click'); await settle();
  assert.equal(h.calls.filter(([, method]) => method === 'POST').length, 0);
});

test('actual same-origin HTTP carries the native seek JSON once and reads the updated paused position', async (t) => {
  const received = [], z = zone('S1', 'Björk & A/?#'); let position = 20;
  const server = http.createServer(async (req, res) => {
    const chunks = []; for await (const chunk of req) chunks.push(chunk);
    const body = chunks.length ? JSON.parse(Buffer.concat(chunks)) : null;
    received.push([req.method, req.url, body]); res.setHeader('Content-Type', 'application/json');
    let data;
    if (req.url === '/seek' && req.method === 'POST') {
      assert.equal(req.headers['content-type'], 'application/json'); position = body.to_secs;
      data = { done: 'Seek accepted; playback stays paused', changed: true };
    } else if (req.url === '/zones') data = [z];
    else if (req.url.endsWith('/state')) { data = state(z, { transport_state: 'paused' }); data.track.position_secs = position; }
    else data = { running: false };
    res.end(JSON.stringify(data));
  });
  server.listen(0, '127.0.0.1'); await once(server, 'listening');
  t.after(() => { server.closeAllConnections(); server.close(); });
  const base = `http://127.0.0.1:${server.address().port}`;
  const h = browser({ fetcher: (url, init) => fetch(base + url, init) }); await h.api.loadZones();
  const panel = h.parts().panels[0]; assert.equal(await panel.confirm(panel.preview(70)), true);
  await h.api.refreshZone(h.api.house.zones[0]);
  assert.equal(panel.view().elapsed, 70); assert.equal(panel.view().transport, 'paused');
  assert.deepEqual(received.filter(([method]) => method === 'POST'), [
    ['POST', '/seek', { zone: 'Björk & A/?#@S1', to_secs: 70 }],
  ]);
  assert.ok(received.filter(([, url]) => url.endsWith('/state')).every(([, url]) =>
    url === '/zones/Bj%C3%B6rk%20%26%20A%2F%3F%23%40S1/state'));
});

test('a lost real HTTP seek reply remains uncertain even when the shared refresh observes its effect', async (t) => {
  const received = []; let position = 20;
  const server = http.createServer(async (req, res) => {
    const chunks = []; for await (const chunk of req) chunks.push(chunk);
    received.push([req.method, req.url]);
    if (req.method === 'POST') { position = JSON.parse(Buffer.concat(chunks)).to_secs; req.socket.destroy(); return; }
    res.setHeader('Content-Type', 'application/json');
    let data = req.url === '/zones' ? [zone()] : { running: false };
    if (req.url.endsWith('/state')) { data = state(); data.track.position_secs = position; }
    res.end(JSON.stringify(data));
  });
  server.listen(0, '127.0.0.1'); await once(server, 'listening');
  t.after(() => { server.closeAllConnections(); server.close(); });
  const base = `http://127.0.0.1:${server.address().port}`;
  const h = browser({ fetcher: (url, init) => fetch(base + url, init) }); await h.api.loadZones();
  const panel = h.parts().panels[0], selected = panel.preview(75);
  assert.equal(await panel.confirm(selected), false);
  await h.api.refreshZone(h.api.house.zones[0]);
  assert.equal(panel.view().elapsed, 75); assert.equal(panel.view().uncertain, true);
  assert.equal(panel.view().seekable, false); assert.equal(await panel.confirm(selected), false);
  assert.equal(await panel.refresh(), true); assert.equal(panel.view().seekable, true);
  assert.deepEqual(received.filter(([method]) => method === 'POST'), [['POST', '/seek']]);
});

test('a hidden late failure keeps its recovery instruction without publishing to the hidden view', async () => {
  const h = seeking(), pending = deferred(); h.custom = name => name === 'seek' ? pending.promise : state();
  const action = h.api.confirm(h.api.preview(80)); await settle(); h.api.suspend(); const count = h.views.length;
  pending.reject(new Error('lost')); await action;
  assert.equal(h.views.length, count); h.api.resume(); h.api.observe(state());
  assert.match(h.api.view().result.detail, /Refresh position/);
  assert.equal(h.api.view().seekable, false); assert.equal(writes(h).length, 1);
});
