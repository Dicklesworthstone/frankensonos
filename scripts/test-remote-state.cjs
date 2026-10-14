// Execute actual remote reconciliation and controls with a minimal DOM shell.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const source = fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/remote/remote.js'), 'utf8');
const clone = (v) => JSON.parse(JSON.stringify(v));
const settle = async () => { for (let n = 0; n < 50; n++) await Promise.resolve(); };
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
function zone(home = 'S2', name = 'Den', members = [name]) {
  return { household: home, coordinator_room: name, members, transport_state: 'playing', degraded: [] };
}
function state(z, title = 'Current', volume = 20) {
  return { zone: z, transport_state: 'playing', volume, mute: false,
    track: { uri: `renderer:${title}`, title } };
}
function harness() {
  const ids = new Map(), timers = new Map(), calls = []; let serial = 0;
  const doc = { activeElement: null, getElementById: (id) => ids.get(id), createElement: (tag) => new Element(tag) };
  class Element {
    constructor(tag) { this.tag = tag; this.children = []; this.attributes = {}; this.handlers = new Map(); this.disabled = false; this.textContent = ''; }
    setAttribute(key, value) {
      this.attributes[key] = value;
      if (key === 'id') ids.set(value, this);
      if (key === 'value') this.value = value;
      if (key === 'disabled') this.disabled = true;
      if (key === 'hidden') this.hidden = true;
    }
    getAttribute(key) { return this.attributes[key] ?? null; }
    removeAttribute(key) { delete this.attributes[key]; }
    addEventListener(key, fn) { this.handlers.set(key, fn); }
    emit(key) { return this.handlers.get(key)?.({ target: this }); }
    append(...children) {
      for (const child of children) {
        if (typeof child === 'object') {
          if (child.parent) child.parent.children = child.parent.children.filter((c) => c !== child);
          child.parent = this;
        }
        this.children.push(child);
      }
    }
    replaceChildren(...children) {
      if (this.contains(doc.activeElement)) doc.activeElement = null;
      for (const child of this.children) if (typeof child === 'object') child.parent = null;
      this.children = []; this.append(...children);
    }
    contains(node) { return this === node || this.children.some((child) => typeof child === 'object' && child.contains(node)); }
    focus() { doc.activeElement = this; }
  }
  for (const id of ['households', 'toast', 'live', 'pause-all']) { const e = new Element('div'); e.setAttribute('id', id); }
  const h = { zones: [zone()], calls, timers, doc, custom: null,
    async fetch(url, init) {
      calls.push([url, init.method, init.body ? JSON.parse(init.body) : null]);
      const custom = h.custom?.(url, init);
      let value;
      if (custom !== undefined) value = await custom;
      else if (url === '/zones') value = clone(h.zones);
      else if (url.endsWith('/state')) {
        const name = decodeURIComponent(url.slice('/zones/'.length, -'/state'.length));
        const z = h.zones.find((z) => z.members.some((room) => `${room}@${z.household}` === name));
        value = state(clone(z));
      } else if (url.endsWith('/dj')) value = { running: false };
      else value = { done: 'Changed', changed: true };
      return { ok: true, text: async () => JSON.stringify(value) };
    },
    async fire(delay) {
      const found = [...timers].filter(([, t]) => t.delay === delay);
      for (const [id, timer] of found) { if (timers.delete(id)) timer.fn(); }
      await settle();
    },
  };
  const ctx = {
    document: doc, AbortController, URLSearchParams,
    fetch: h.fetch,
    setTimeout: (fn, delay) => { timers.set(++serial, { fn, delay }); return serial; },
    clearTimeout: (id) => timers.delete(id),
    QueueRemote: { panel({ el }) { const node = el('details'); node.draft = el('input'); node.append(node.draft);
      return { node, dispose() { node.disposed = true; } }; } },
  };
  h.api = vm.runInNewContext(source.replace(/\nstart\(\);\s*$/, '')
    + '\n({house, loadZones, render, refreshZone, refreshVolume, act, readEach, checkedZones, ownsCard, roomName, zoneKey, groupShape});', ctx);
  return h;
}
const parts = (h, name = 'Den', home = 'S2') => h.api.house.cards.get(h.api.zoneKey(zone(home, name)));

test('an unchanged topology preserves card nodes, open queues, drafts and keyboard focus', async () => {
  const h = harness(); await h.api.loadZones(); const before = parts(h);
  before.queue.node.open = true; before.queue.node.draft.value = 'My selection'; before.queue.node.draft.focus();
  h.zones[0].transport_state = 'paused'; await h.api.loadZones();
  assert.equal(parts(h), before); assert.equal(before.queue.node.open, true);
  assert.equal(before.queue.node.draft.value, 'My selection'); assert.equal(h.doc.activeElement, before.queue.node.draft);
  assert.equal(before.queue.node.disposed, undefined);
});

test('changed memberships dispose only the affected card and retain other groups', async () => {
  const h = harness(); h.zones.push(zone('S1', 'Office')); await h.api.loadZones();
  const den = parts(h), office = parts(h, 'Office', 'S1'); office.queue.node.draft.focus();
  h.zones[0].members.push('Kitchen'); await h.api.loadZones();
  assert.equal(den.disposed, true); assert.equal(den.queue.node.disposed, true);
  assert.notEqual(parts(h), den); assert.equal(parts(h, 'Office', 'S1'), office);
  assert.equal(h.doc.activeElement, office.queue.node.draft);
});

test('older household reads cannot restore a superseded topology', async () => {
  const h = harness(), first = deferred(), second = deferred(); let n = 0;
  h.custom = (url) => url === '/zones' ? (++n === 1 ? first.promise : second.promise) : undefined;
  const a = h.api.loadZones(), b = h.api.loadZones();
  h.zones = [zone('S1', 'New room')]; second.resolve(clone(h.zones)); await b;
  first.resolve([zone('S2', 'Old room')]); await a;
  assert.equal(h.api.house.zones[0].coordinator_room, 'New room');
  assert.ok(!h.calls.some(([url]) => url.includes('Old%20room')));
});

test('out-of-order playback reads cannot overwrite newer track observations', async () => {
  const h = harness(); await h.api.loadZones(); const old = deferred(), next = deferred(); let n = 0;
  h.custom = (url) => url.endsWith('/state') ? (++n === 1 ? old.promise : next.promise) : undefined;
  const a = h.api.refreshZone(h.zones[0]), b = h.api.refreshZone(h.zones[0]);
  next.resolve(state(h.zones[0], 'New')); await b; old.resolve(state(h.zones[0], 'Old')); await a;
  assert.equal(parts(h).title.textContent, 'New');
});

test('a disposed card ignores a late response and starts no additional reads', async () => {
  const h = harness(); await h.api.loadZones(); const before = parts(h), waiting = deferred();
  h.custom = (url) => url.endsWith('/state') ? waiting.promise : undefined;
  const read = h.api.refreshZone(h.zones[0]); h.api.house.zones = []; h.api.render();
  const count = h.calls.length; waiting.resolve(state(h.zones[0], 'Old')); await read;
  assert.equal(before.title.textContent, 'Current'); assert.equal(h.calls.length, count);
});

test('a delayed slider command is cancelled when its group card is replaced', async () => {
  const h = harness(); await h.api.loadZones(); const entry = parts(h).sliders.get('Den');
  entry.input.value = '30'; entry.input.emit('input');
  h.api.house.zones = []; h.api.render(); await h.fire(250);
  assert.ok(!h.calls.some(([, method]) => method === 'POST'));
  entry.input.emit('input'); await h.fire(250);
  assert.ok(!h.calls.some(([, method]) => method === 'POST'));
});

test('background level reads do not cancel a deliberate debounced volume change', async () => {
  const h = harness(); h.zones = [zone('S2', 'Den', ['Den', 'Kitchen'])]; await h.api.loadZones();
  const card = parts(h), entry = card.sliders.get('Kitchen');
  entry.input.value = '35'; entry.input.emit('input'); await h.api.refreshVolume(entry, card); await h.fire(250);
  assert.deepEqual(h.calls.filter(([, method]) => method === 'POST').map((c) => c[2]), [{ zone: 'Kitchen@S2', volume: 35 }]);
});

test('card controls acquire ownership before creating a request and reject double taps', async () => {
  const h = harness(); await h.api.loadZones(); const p = parts(h), waiting = deferred();
  h.custom = (url, init) => init.method === 'POST' ? waiting.promise : undefined;
  p.next.emit('click'); p.next.emit('click'); p.play.emit('click'); await settle();
  assert.equal(h.calls.filter(([, method]) => method === 'POST').length, 1);
  assert.equal(p.play.disabled, true); waiting.resolve({ done: 'Next', changed: true }); await settle();
  assert.equal(p.play.disabled, false); assert.equal(p.writing, false);
});

test('detached card controls cannot send a request', async () => {
  const h = harness(); await h.api.loadZones(); const p = parts(h);
  h.api.house.zones = []; h.api.render(); p.next.emit('click'); p.play.emit('click'); p.dj.emit('click');
  p.sliders.get('Den').mute.emit('click'); await settle();
  assert.equal(h.calls.filter(([, method]) => method === 'POST').length, 0);
});

test('failed writes are not replayed and old reads cannot paint over the post-action refresh', async () => {
  const h = harness(); await h.api.loadZones(); const p = parts(h), old = deferred(); let stateReads = 0;
  h.custom = (url, init) => {
    if (init.method === 'POST') return Promise.reject(new Error('reply lost'));
    if (url.endsWith('/state')) return ++stateReads === 1 ? old.promise : state(h.zones[0], 'Fresh');
  };
  const read = h.api.refreshZone(h.zones[0]); p.next.emit('click'); await settle();
  old.resolve(state(h.zones[0], 'Old')); await read; await settle();
  assert.equal(h.calls.filter(([, method]) => method === 'POST').length, 1);
  assert.equal(p.title.textContent, 'Fresh');
});

test('unreadable topology preserves the last valid cards rather than deleting them', async () => {
  const h = harness(); await h.api.loadZones(); const p = parts(h);
  for (const value of [null, {}, [zone('S2', 'Den', [])], [zone(), zone()]]) {
    h.custom = (url) => url === '/zones' ? Promise.resolve(value) : undefined;
    await assert.rejects(h.api.loadZones()); assert.equal(parts(h), p);
  }
});

test('households with duplicate room names remain independently addressed', async () => {
  const h = harness(); h.zones = [zone('S1'), zone('S2')]; await h.api.loadZones();
  assert.notEqual(parts(h, 'Den', 'S1'), parts(h));
  assert.ok(h.calls.some(([url]) => url === '/zones/Den%40S1/state'));
  assert.ok(h.calls.some(([url]) => url === '/zones/Den%40S2/state'));
  assert.equal(h.api.roomName('Office', 'S1'), 'Office@S1');
});

test('topology identities cannot collide through slashes in household or room names', () => {
  const h = harness();
  assert.notEqual(h.api.zoneKey(zone('a/b', 'c')), h.api.zoneKey(zone('a', 'b/c')));
});

test('read fan-out is bounded and one failure does not hide other results', async () => {
  const h = harness(); let active = 0, peak = 0; const visited = [];
  await assert.rejects(h.api.readEach(Array.from({ length: 30 }, (_, i) => i), async (i) => {
    peak = Math.max(peak, ++active); await settle(); active--; visited.push(i);
    if (i === 4) throw new Error('one offline');
  }), /one offline/);
  assert.equal(peak, 4); assert.equal(visited.length, 30); assert.equal(active, 0);
});

test('a superseded read batch does not launch more queued room requests', async () => {
  const h = harness(); let current = true; const visited = [];
  await h.api.readEach(Array.from({ length: 30 }, (_, i) => i), async (i) => { visited.push(i); current = false; }, () => current);
  assert.equal(visited.length, 1);
});

test('failed or mismatched playback reads are visible without inventing stopped playback', async () => {
  const h = harness(); await h.api.loadZones(); const p = parts(h);
  for (const value of [null, {}, { ...state(h.zones[0]), track: {} }, state(zone('S2', 'Other'))]) {
    h.custom = (url) => url.endsWith('/state') ? Promise.resolve(value) : undefined;
    await assert.rejects(h.api.refreshZone(h.zones[0]));
    assert.equal(p.title.textContent, 'Current'); assert.match(p.state.textContent, /unavailable/);
  }
});

test('a passive stale volume response cannot overwrite a newer deliberate value', async () => {
  const h = harness(); await h.api.loadZones(); const p = parts(h), e = p.sliders.get('Den'), waiting = deferred();
  h.custom = (url) => url.endsWith('/state') ? waiting.promise : undefined;
  const read = h.api.refreshVolume(e, p); e.input.value = '42'; e.input.emit('input');
  waiting.resolve(state(h.zones[0], 'Current', 5)); await read;
  assert.equal(e.input.value, '42');
});

test('mute writes are serialized independently and a read cannot undo optimistic mute', async () => {
  const h = harness(); await h.api.loadZones(); const p = parts(h), e = p.sliders.get('Den'), waiting = deferred();
  h.custom = (url, init) => init.method === 'POST' ? waiting.promise : undefined;
  e.mute.emit('click'); e.mute.emit('click'); await h.api.refreshZone(h.zones[0]);
  assert.equal(e.muted, true); assert.equal(h.calls.filter(([, m]) => m === 'POST').length, 1);
  waiting.resolve({ done: 'Muted', changed: true }); await settle(); assert.equal(e.mute.disabled, false);
});
