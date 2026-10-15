// node --test scripts/test-remote-transport.cjs; native action lifetime regressions.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const zone = { household: 'S2', coordinator_room: 'Den', members: ['Den', 'Office'] };
function transportFixture() {
  const fs = require('node:fs'), path = require('node:path'), vm = require('node:vm');
  const source = fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/remote/remote.js'), 'utf8');
  const reads = [];
  const api = vm.runInNewContext(source.replace(/\nstart\(\);\s*$/, '')
    + '\nrefreshZone = async (zone) => read(zone); ({house, beginAction, endAction, actionControls, transportBusy, fillFavorites, zoneKey});',
  { setTimeout, clearTimeout, read: (zone) => reads.push(zone),
    document: { createElement: () => ({ setAttribute(key, value) { this[key] = value; } }) } });
  const part = (group) => {
    const parts = { zone: group, writing: false, disposed: false, readVersion: 0,
      panels: [{ block(value) { this.blocked = value; } }] };
    for (const key of ['play', 'next', 'previous', 'favorite', 'dj', 'skip', 'mood', 'like', 'dislike']) parts[key] = {};
    parts.favorite = { value: '', replaceChildren() {}, append() {} };
    api.house.cards.set(api.zoneKey(group), parts);
    api.actionControls(parts, api.transportBusy(parts));
    return parts;
  };
  const remove = (parts) => { parts.disposed = true; api.house.cards.delete(api.zoneKey(parts.zone)); };
  return { api, part, remove, reads };
}

test('a group split preserves in-flight transport exclusion across replacement cards', () => {
  const h = transportFixture(), old = h.part(zone);
  assert.equal(h.api.beginAction(old), true); h.remove(old);
  const den = h.part({ ...zone, members: ['Den'] });
  const office = h.part({ ...zone, coordinator_room: 'Office', members: ['Office'] });
  const otherHome = h.part({ ...zone, household: 'S1', members: ['Den'] });
  assert.equal(den.next.disabled, true); assert.equal(office.panels[0].blocked, true);
  assert.equal(h.api.beginAction(den), false); assert.equal(h.api.beginAction(office), false);
  assert.equal(h.api.beginAction(otherHome), true, 'same-name rooms in another household remain independent');
  h.api.endAction(old);
  assert.equal(den.next.disabled, false); assert.equal(office.next.disabled, false);
  assert.equal(otherHome.next.disabled, true);
  assert.ok(den.readVersion > 0 && office.readVersion > 0);
  assert.deepEqual(h.reads.map(z => z.coordinator_room).sort(), ['Den', 'Office']);
  h.api.endAction(otherHome);
});

test('a merged group remains blocked until both preceding group actions have settled', () => {
  const h = transportFixture();
  const den = h.part({ ...zone, members: ['Den'] });
  const office = h.part({ ...zone, coordinator_room: 'Office', members: ['Office'] });
  h.api.beginAction(den); h.api.beginAction(office); h.remove(den); h.remove(office);
  const merged = h.part(zone); assert.equal(merged.next.disabled, true);
  h.api.endAction(den); assert.equal(merged.next.disabled, true); assert.equal(h.reads.length, 0);
  assert.equal(h.api.beginAction(merged), false);
  h.api.endAction(office); assert.equal(merged.next.disabled, false); assert.equal(h.reads.length, 1);
  assert.equal(h.api.beginAction(merged), true); h.api.endAction(merged);
});

test('explicitly hidden room controls stay hidden despite flex row styling', () => {
  const fs = require('node:fs'), path = require('node:path');
  const css = fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/remote/remote.css'), 'utf8');
  assert.match(css, /\[hidden\]\s*\{\s*display:\s*none\s*!important;/);
});

test('catalog refresh cannot unlock favorites while a regrouped transport command is pending', () => {
  const h = transportFixture();
  h.api.house.favorites.set('S2', [{ id: 'FV:1', title: 'Radio' }]);
  const old = h.part(zone); h.api.beginAction(old); h.remove(old);
  const current = h.part({ ...zone, members: ['Den'] });
  h.api.fillFavorites(current);
  assert.equal(current.favorite.disabled, true);
  h.api.endAction(old); assert.equal(current.favorite.disabled, false);
  h.api.house.favorites.set('S2', []);
  h.api.actionControls(current, false); assert.equal(current.favorite.disabled, true);
});

const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
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
function zoneDto(home = 'S2', name = 'Den', members = [name]) {
  return { household: home, coordinator_room: name, members, transport_state: 'playing', degraded: [] };
}
function state(z = zoneDto(), overrides = {}) {
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
  const win = new Events(); const h = { c, calls, doc, win, zones: [zoneDto()], custom: null };
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

const descendants = (node) => [node, ...(node.children || []).flatMap((child) => typeof child === 'object' ? descendants(child) : [])];
const writes = (h) => h.calls.filter(([, method]) => method === 'POST');

test('paused seek controls disable immediately when another native action starts', async () => {
  const h = browser();
  h.custom = (url) => url.endsWith('/state') ? state(h.zones[0], { transport_state: 'paused' }) : undefined;
  await h.api.loadZones(); const p = h.parts(), panel = p.panels[0];
  const range = descendants(panel.node).find(n => n.tag === 'input');
  assert.equal(range.disabled, false);
  const hold = deferred(); h.custom = (url, init) => init.method === 'POST' ? hold.promise : undefined;
  p.next.emit('click');
  assert.equal(range.disabled, true, 'no animation tick or network refresh is required');
  assert.equal(panel.preview(60), null);
  hold.resolve({ done: 'next', changed: true }); await settle();
  assert.equal(range.disabled, false); assert.equal(writes(h).length, 1);
});

test('a dispatched seek excludes both replacement groups until its reply settles', async () => {
  const h = browser(); h.zones = [zoneDto('S2', 'Den', ['Den', 'Office'])];
  await h.api.loadZones(); const old = h.parts(), hold = deferred();
  h.custom = (url, init) => init.method === 'POST' ? hold.promise : undefined;
  const action = old.panels[0].confirm(old.panels[0].preview(80)); await settle();
  assert.equal(writes(h).length, 1);
  h.zones = [zoneDto('S2', 'Den'), zoneDto('S2', 'Office')]; await h.api.loadZones();
  for (const p of h.api.house.cards.values()) {
    assert.equal(p.next.disabled, true);
    assert.equal(p.panels[0].view().seekable, false);
    p.next.emit('click'); assert.equal(p.panels[0].preview(60), null);
  }
  await settle(); assert.equal(writes(h).length, 1);
  hold.resolve({ done: 'seek', changed: true }); await action; await settle();
  for (const p of h.api.house.cards.values()) {
    assert.equal(p.next.disabled, false); assert.equal(p.panels[0].view().seekable, true);
  }
  assert.equal(old.panels[0].view().fresh, false);
});

test('a merged group waits for both preceding native commands, not just the first completion', async () => {
  const h = browser(); h.zones = [zoneDto('S2', 'Den'), zoneDto('S2', 'Office')];
  await h.api.loadZones(); const waits = [deferred(), deferred()]; let index = 0;
  h.custom = (url, init) => init.method === 'POST' ? waits[index++].promise : undefined;
  for (const p of h.api.house.cards.values()) p.next.emit('click'); await settle();
  assert.equal(writes(h).length, 2);
  h.zones = [zoneDto('S2', 'Den', ['Den', 'Office'])]; await h.api.loadZones();
  const p = h.parts(); assert.equal(p.play.disabled, true); assert.equal(p.panels[0].view().seekable, false);
  waits[0].resolve({ done: 'next', changed: true }); await settle();
  assert.equal(p.play.disabled, true); p.play.emit('click'); await settle();
  assert.equal(writes(h).length, 2);
  waits[1].resolve({ done: 'next', changed: true }); await settle();
  assert.equal(p.play.disabled, false); assert.equal(p.panels[0].view().seekable, true);
});

test('same-name native controls in another household can proceed independently', async () => {
  const h = browser(); h.zones = [zoneDto('S1'), zoneDto('S2')]; await h.api.loadZones();
  const holds = [deferred(), deferred()]; let index = 0;
  h.custom = (url, init) => init.method === 'POST' ? holds[index++].promise : undefined;
  const [first, second] = h.api.house.cards.values(); first.next.emit('click');
  assert.equal(second.next.disabled, false); second.next.emit('click'); await settle();
  assert.deepEqual(writes(h).map(([, , body]) => body.zone), ['Den@S1', 'Den@S2']);
  holds[0].resolve({ done: 'next', changed: true }); await settle();
  assert.equal(first.next.disabled, false); assert.equal(second.next.disabled, true);
  holds[1].resolve({ done: 'next', changed: true }); await settle();
});

test('a lost reply releases inherited exclusion with read-only reconciliation, not a replay', async () => {
  const h = browser(); h.zones = [zoneDto('S2', 'Den', ['Den', 'Office'])]; await h.api.loadZones();
  const hold = deferred(); h.custom = (url, init) => init.method === 'POST' ? hold.promise : undefined;
  h.parts().next.emit('click'); await settle();
  h.zones = [zoneDto('S2', 'Office', ['Den', 'Office'])]; await h.api.loadZones();
  assert.equal(h.parts().play.disabled, true);
  hold.reject(new Error('reply lost')); await settle();
  assert.equal(h.parts().play.disabled, false); assert.equal(writes(h).length, 1);
  assert.ok(h.calls.some(([url, method]) => method === 'GET' && url === '/zones/Office%40S2/state'));
});

test('failed post-action observations leave the seek panel stale rather than enabling an old destination', async () => {
  const h = browser(); await h.api.loadZones(); const p = h.parts(), hold = deferred();
  h.custom = (url, init) => init.method === 'POST' ? hold.promise : url.endsWith('/state') ? null : undefined;
  p.next.emit('click'); hold.resolve({ done: 'next', changed: true }); await settle();
  assert.equal(p.writing, false); assert.equal(p.panels[0].view().fresh, false);
  assert.equal(p.panels[0].view().seekable, false); assert.equal(p.panels[0].preview(60), null);
  assert.equal(writes(h).length, 1);
});

test('the regression gate runs both serialized volume and regrouped transport suites', () => {
  const ci = fs.readFileSync(path.join(root, '.github/workflows/linux-speech.yml'), 'utf8');
  assert.match(ci, /run: node --test scripts\/test-remote-transport.cjs scripts\/test-remote-volume.cjs/);
});
