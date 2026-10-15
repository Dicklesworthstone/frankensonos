// node --test scripts/test-remote-catalogs.cjs; shipped catalog recovery, fake time/network.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const source = fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/remote/remote.js'), 'utf8');
const settle = async () => { for (let n = 0; n < 60; n++) await Promise.resolve(); };
const plain = (value) => JSON.parse(JSON.stringify(value));
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
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
        const next = [...timers].filter(([, t]) => t.at <= end).sort((a, b) => a[1].at - b[1].at)[0];
        if (!next) break;
        timers.delete(next[0]); now = next[1].at; next[1].fn(); await settle();
      }
      now = end; await settle();
    },
  };
}
class Element {
  value = ''; disabled = false; hidden = false; textContent = ''; children = [];
  setAttribute(key, value) { if (key === 'value') this.value = value; }
  replaceChildren(...children) { this.children = children; this.value = children[0]?.value ?? ''; }
  append(child) { this.children.push(child); }
  addEventListener() {}
}
const zone = (room = 'Den', household = 'S2') => ({ household, coordinator_room: room, members: [room] });
function runtime(network = async () => []) {
  const time = clock();
  const context = { setTimeout: time.schedule, clearTimeout: time.cancel, Date: { now: time.now },
    document: { createElement: () => new Element() }, network };
  const api = vm.runInNewContext(source.replace(/\nstart\(\);\s*$/, '')
    + '\ncall = network; ({ CatalogRemote, catalogs, house, loadZones, loadFavorites, loadMoods, fillFavorites, fillMoods, zoneKey, groupShape, replacePlayback: (fn) => { refreshZone = fn; } });', context);
  return { ...api, time };
}
function harness(read = async (s) => s.zone) {
  const h = runtime(), reads = [], values = [], phases = [], removed = [];
  const model = h.CatalogRemote.model({
    read(s) { reads.push({ ...plain(s), at: h.time.now() }); return read(s); },
    publish(s, v) { values.push({ source: plain(s), value: v }); },
    status(s, phase) { phases.push({ source: plain(s), phase }); },
    removed(s) { removed.push(plain(s)); },
    schedule: h.time.schedule, cancel: h.time.cancel, now: h.time.now,
  });
  return { ...h, model, reads, values, phases, removed };
}
async function start(h, zones = [zone()]) { h.model.reconcile(zones); h.model.resume(); await settle(); }

test('favorites use one qualified source per household; topology order cannot reshuffle it', () => {
  const h = harness(), zones = [zone('Office', 'S2'), zone('Den', 'S1'), zone('Den', 'S2')];
  const sources = plain(h.CatalogRemote.sources(zones));
  assert.deepEqual(sources, plain(h.CatalogRemote.sources([...zones].reverse())));
  assert.deepEqual(sources.filter(s => s.kind === 'favorites').map(s => s.zone), ['Den@S1', 'Den@S2']);
  assert.equal(sources.filter(s => s.kind === 'moods').length, 1);
});

test('healthy catalogs load once, with no quiet-state polling or leftover timer', async () => {
  const h = harness(); await start(h); await h.time.tick(600000);
  assert.equal(h.reads.length, 2); assert.equal(h.values.length, 2); assert.equal(h.time.pending, 0);
  h.model.reconcile([zone()]); await settle(); assert.equal(h.reads.length, 2);
});

test('failed catalogs recover automatically with capped exponential backoff', async () => {
  let attempts = 0;
  const h = harness(async (s) => { if (s.kind === 'favorites' && ++attempts < 7) throw new Error('offline'); return ['usable']; });
  await start(h);
  assert.equal(h.phases.at(-1).phase, 'ready');
  for (const ms of [5000, 10000, 20000, 40000, 60000, 60000]) await h.time.tick(ms);
  assert.deepEqual(h.reads.filter(r => r.kind === 'favorites').map(r => r.at), [0, 5000, 15000, 35000, 75000, 135000, 195000]);
  assert.equal(h.reads.filter(r => r.kind === 'moods').length, 1);
  assert.equal(h.values.length, 2); assert.equal(h.time.pending, 0);
});

test('one failed household does not reread healthy households or reset on state events', async () => {
  const h = harness(async (s) => { if (s.kind === 'favorites' && s.household === 'S1') throw new Error('offline'); return []; });
  await start(h, [zone('Den', 'S1'), zone('Den', 'S2')]);
  for (let i = 0; i < 50; i++) { h.model.reconcile([zone('Den', 'S2'), zone('Den', 'S1')]); await h.time.tick(100); }
  assert.equal(h.reads.filter(r => r.kind === 'favorites' && r.household === 'S1').length, 2);
  assert.equal(h.reads.filter(r => r.kind === 'favorites' && r.household === 'S2').length, 1);
  assert.equal(h.time.pending, 1);
});

test('a valid empty catalog is success, not a reason to keep retrying', async () => {
  const h = harness(async () => []); await start(h); await h.time.tick(600000);
  assert.equal(h.values.length, 2); assert.equal(h.reads.length, 2); assert.equal(h.time.pending, 0);
});

test('new households load without restarting the page or rereading existing favorites', async () => {
  const h = harness(); await start(h, [zone('Den', 'A')]);
  h.model.reconcile([zone('Den', 'A'), zone('Den', 'B')]); await settle();
  assert.equal(h.reads.filter(r => r.kind === 'favorites' && r.household === 'A').length, 1);
  assert.equal(h.reads.filter(r => r.kind === 'favorites' && r.household === 'B').length, 1);
});

test('replacing a source fences its old response and waits for the existing read to settle', async () => {
  const waiting = deferred();
  const h = harness((s) => s.zone.startsWith('Den@') ? waiting.promise : Promise.resolve('new'));
  await start(h); h.model.reconcile([zone('Kitchen')]); await settle();
  assert.equal(h.reads.length, 2, 'no overlapping reads for replaced slots');
  waiting.resolve('old'); await settle();
  assert.equal(h.reads.length, 4); assert.equal(h.values.length, 2);
  assert.ok(h.values.every(v => v.value === 'new' && v.source.zone === 'Kitchen@S2'));
});

test('removed households cannot publish late replies and lose their cached slots', async () => {
  const waiting = deferred(), h = harness(() => waiting.promise); await start(h);
  h.model.reconcile([]); waiting.resolve('old'); await settle();
  assert.equal(h.values.length, 0); assert.equal(h.removed.length, 2); assert.equal(h.time.pending, 0);
});

test('suspension stops retries; resumption rechecks catalogs without overlapping old reads', async () => {
  const waiting = deferred(); let calls = 0;
  const h = harness(async () => ++calls <= 2 ? waiting.promise : 'fresh');
  await start(h); h.model.suspend(); await h.time.tick(600000); h.model.resume(); await settle();
  assert.equal(h.reads.length, 2);
  waiting.resolve('stale'); await settle();
  assert.equal(h.reads.length, 4); assert.ok(h.values.every(v => v.value === 'fresh'));
  h.model.suspend(); assert.equal(h.time.pending, 0);
});

test('explicit refresh invalidates a pending observation without overlapping its request', async () => {
  const waiting = deferred(); let calls = 0;
  const h = harness(async () => ++calls <= 2 ? waiting.promise : 'fresh'); await start(h);
  for (let i = 0; i < 50; i++) h.model.refresh();
  assert.equal(h.reads.length, 2); waiting.resolve('stale'); await settle();
  assert.equal(h.reads.length, 4); assert.ok(h.values.every(v => v.value === 'fresh'));
});

test('catalog fan-out remains bounded, including across a hidden-page lifecycle', async () => {
  const waiting = [];
  const h = harness(() => { const d = deferred(); waiting.push(d); return d.promise; });
  await start(h, Array.from({ length: 20 }, (_, i) => zone(`Room ${i}`, `House ${i}`)));
  assert.equal(waiting.length, 4); assert.equal(h.time.pending, 0);
  h.model.suspend(); h.model.resume(); await settle(); assert.equal(waiting.length, 4);
  waiting[0].resolve('old'); await settle(); assert.equal(waiting.length, 5);
  h.model.dispose(); for (const d of waiting) d.resolve('old'); await settle();
  assert.equal(h.values.length, 0); assert.equal(h.time.pending, 0);
});

for (const error of [Object.assign(new Error('denied'), { status: 403 }),
  Object.assign(new Error('unsupported'), { code: 'NOT_IMPLEMENTED' })]) {
  test(`${error.message} catalogs wait for explicit refresh rather than hammering the daemon`, async () => {
    let failed = true;
    const h = harness(async () => { if (failed) throw error; return []; });
    await start(h); await h.time.tick(600000);
    assert.equal(h.reads.length, 2); assert.equal(h.time.pending, 0);
    assert.ok(h.phases.filter(p => p.phase !== 'loading').every(p => p.phase === 'unavailable'));
    failed = false; h.model.refresh(); await settle(); assert.equal(h.values.length, 2);
  });
}

test('disposing a controller cancels retry timers and prevents revival', async () => {
  const h = harness(async () => { throw new Error('offline'); }); await start(h);
  assert.equal(h.time.pending, 1); h.model.dispose();
  h.model.resume(); h.model.refresh(); h.model.reconcile([zone('Other')]); await h.time.tick(600000);
  assert.equal(h.reads.length, 2); assert.equal(h.time.pending, 0);
});

function addCard(h, z = zone()) {
  const parts = { zone: z, favorite: new Element(), mood: new Element(), writing: false, djRunning: false };
  h.house.cards.set(h.zoneKey(z), parts);
  return parts;
}
const favorite = { id: 'FV:1', title: 'Evening radio', kind: 'radio' };

test('the shipped loaders never interpret failure or malformed data as an empty catalog', async () => {
  for (const value of [null, {}, [{ id: '', title: '', kind: 'track' }], [favorite, favorite]]) {
    const h = runtime(async () => value);
    await assert.rejects(h.loadFavorites('Den@S2'), /favorites list/);
  }
  for (const value of [null, {}, { moods: [null] }, { moods: [{ name: 'Calm' }, { name: 'Calm' }] }]) {
    const h = runtime(async () => value);
    await assert.rejects(h.loadMoods('Den@S2'), /mood list/);
  }
  const h = runtime(async () => { throw new Error('offline'); });
  await assert.rejects(h.loadFavorites('Den@S2'), /offline/);
  await assert.rejects(h.loadMoods('Den@S2'), /offline/);
});

test('actual recovery keeps last-known favorites and mood drafts, then applies valid fresh choices', async () => {
  let online = false;
  const calls = [];
  const h = runtime(async (name, options) => {
    calls.push({ name, options: plain(options) });
    assert.ok(['favorites', 'moods'].includes(name), 'catalog recovery sends reads only');
    if (!online) throw new Error('offline');
    return name === 'favorites' ? [favorite] : { moods: [{ name: 'Calm' }, { name: 'Bright' }] };
  });
  const parts = addCard(h);
  h.house.favorites.set('S2', [{ ...favorite, title: 'Previously read favorite' }]);
  h.house.moods = ['Calm']; parts.mood.value = 'Calm';
  h.catalogs.reconcile([zone()]); h.catalogs.resume(); await settle();
  assert.equal(h.house.favorites.get('S2')[0].title, 'Previously read favorite');
  assert.equal(parts.favorite.hidden, false); assert.equal(parts.favorite.disabled, false);
  assert.match(parts.favorite.children[0].textContent, /retrying/);
  assert.equal(parts.mood.value, 'Calm');
  online = true; await h.time.tick(5000);
  assert.equal(h.house.favorites.get('S2')[0].title, 'Evening radio');
  assert.equal(parts.favorite.children[0].textContent, 'Play a favorite…');
  assert.equal(parts.mood.value, 'Calm'); assert.deepEqual(plain(h.house.moods), ['Calm', 'Bright']);
  assert.ok(calls.every(c => c.options.query.zone === 'Den@S2'));
  assert.equal(h.time.pending, 0);
});

test('empty favorites hide only after a successful read; an outage remains visible', async () => {
  let online = false;
  const h = runtime(async name => { if (!online) throw new Error('offline'); return name === 'moods' ? { moods: [] } : []; });
  const parts = addCard(h); h.catalogs.reconcile([zone()]); h.catalogs.resume(); await settle();
  assert.equal(parts.favorite.hidden, false); assert.equal(parts.favorite.disabled, true);
  assert.match(parts.favorite.children[0].textContent, /retrying/);
  online = true; await h.time.tick(5000); assert.equal(parts.favorite.hidden, true);
});

test('late mood choices restore a running DJ mood without changing an in-flight user draft', async () => {
  const h = runtime(async name => name === 'moods' ? { moods: [{ name: 'Calm' }, { name: 'Bright' }] } : []);
  const parts = addCard(h); parts.djRunning = true; parts.djMood = 'Calm';
  h.catalogs.reconcile([zone()]); h.catalogs.resume(); await settle();
  assert.equal(parts.mood.value, 'Calm');
  parts.writing = true; parts.mood.value = 'Bright'; h.catalogs.refresh(); await settle();
  assert.equal(parts.mood.value, 'Bright');
});

test('a failed playback refresh cannot prevent discovery from scheduling catalog reads', async () => {
  const h = runtime(async name => name === 'zones' ? [zone()]
    : name === 'favorites' ? [favorite] : { moods: [{ name: 'Calm' }] });
  const parts = addCard(h); h.house.layout = JSON.stringify([h.groupShape(zone())]);
  h.replacePlayback(async () => { throw new Error('speaker state unavailable'); });
  h.catalogs.resume(); await assert.rejects(h.loadZones(), /speaker state unavailable/); await settle();
  assert.equal(parts.favorite.children[1].textContent, 'Evening radio');
  assert.deepEqual(plain(h.house.moods), ['Calm']);
});

test('departed households are removed from both visible catalog caches and status maps', async () => {
  const h = runtime(async name => name === 'favorites' ? [favorite] : { moods: [] });
  addCard(h); h.catalogs.reconcile([zone()]); h.catalogs.resume(); await settle();
  h.catalogs.reconcile([]);
  assert.equal(h.house.favorites.size, 0); assert.equal(h.house.catalogStatus.size, 0);
  assert.equal(h.house.moods, null); assert.equal(h.time.pending, 0);
});

test('catalog retries honor long Retry-After hints even across visibility changes and reconnects', async () => {
  let attempts = 0;
  const h = harness(async (s) => {
    if (s.kind === 'favorites' && ++attempts === 1) throw Object.assign(new Error('rate limited'), { status: 429, retryAfter: 180000 });
    return [];
  });
  await start(h); await h.time.tick(30000);
  h.model.suspend(); h.model.resume(); h.model.refresh(); await settle();
  await h.time.tick(149999); assert.equal(attempts, 1);
  await h.time.tick(1); assert.equal(attempts, 2); assert.equal(h.time.pending, 0);
});

test('unrepresentable Retry-After hints cannot overflow into a retry storm', async () => {
  const h = harness(() => { throw Object.assign(new Error('rate limited'), { retryAfter: Infinity }); });
  await start(h); await h.time.tick(600000);
  h.model.suspend(); h.model.resume(); await h.time.tick(600000);
  assert.equal(h.reads.length, 2); assert.equal(h.time.pending, 0);
});

test('synchronously thrown read failures still have exactly one future recovery timer', async () => {
  const h = harness(() => { throw new Error('synchronous outage'); }); await start(h);
  assert.equal(h.time.pending, 1);
  await h.time.tick(5000); assert.equal(h.reads.length, 4); assert.equal(h.time.pending, 1);
});
