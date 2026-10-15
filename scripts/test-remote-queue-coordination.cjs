// Shipped queue controls and native room actions in one runtime; no packages required.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.join(__dirname, '../crates/fsonos-api/src/remote');
const queueSource = fs.readFileSync(process.env.QUEUE_SOURCE || path.join(root, 'queue.js'), 'utf8');
const remoteSource = fs.readFileSync(process.env.REMOTE_SOURCE || path.join(root, 'remote.js'), 'utf8');
const Queue = vm.runInNewContext(queueSource + '\nQueueRemote;', { setTimeout, clearTimeout });
const settle = async () => { for (let n = 0; n < 80; n++) await Promise.resolve(); };
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
const zone = (room = 'Den', household = 'S2', members = [room]) => ({ household, coordinator_room: room, members, transport_state: 'playing' });
const page = (offset = 0, total = 4, revision = 'revision 1') => ({ total, revision, playing: total ? 1 : null,
  items: Array.from({ length: Math.min(25, Math.max(0, total - offset)) }, (_, n) => ({
    position: offset + n + 1, title: `Track ${offset + n + 1}`, uri: `renderer:${offset + n + 1}`,
  })) });
const result = (route, body) => route === 'queueRemove'
  ? { done: 'Removed', position: body.position, count: body.expected_uris.length, remaining: 4 - body.expected_uris.length, recovered: false, undoable: false }
  : route === 'queueMove'
    ? { done: 'Moved', position: body.insert_before > body.position ? body.insert_before - body.expected_uris.length : body.insert_before,
      count: body.expected_uris.length, total: 4, revision: 'revision 2', zone: 'Den', changed: true, recovered: false, undoable: false }
    : route === 'queueAdd' ? { done: 'Queued', first: 5, count: 1, playing_queue: true }
      : { done: 'Playing', changed: true };

function browser() {
  let serial = 0;
  const timers = new Map(), ids = new Map(), calls = [];
  class Element {
    constructor(tag) { this.tag = tag; this.children = []; this.attrs = {}; this.events = new Map(); this.value = ''; this.disabled = false; this.open = false; }
    setAttribute(k, v) { this.attrs[k] = String(v); if (k === 'id') ids.set(v, this); if (k === 'value') this.value = String(v); if (['hidden', 'disabled', 'checked'].includes(k)) this[k] = true; }
    getAttribute(k) { return this.attrs[k] ?? null; }
    removeAttribute(k) { delete this.attrs[k]; }
    addEventListener(k, fn) { if (!this.events.has(k)) this.events.set(k, new Set()); this.events.get(k).add(fn); }
    removeEventListener(k, fn) { this.events.get(k)?.delete(fn); }
    emit(k, event = {}) { for (const fn of [...(this.events.get(k) || [])]) fn({ target: this, ...event }); }
    append(...nodes) { this.children.push(...nodes); }
    replaceChildren(...nodes) { this.children = nodes; }
    contains(node) { return this === node || this.children.some(n => n && typeof n === 'object' && n.contains(node)); }
    descendants() { return [this, ...this.children.flatMap(n => n && typeof n === 'object' ? n.descendants() : [])]; }
    querySelectorAll(selector) { return this.descendants().filter(n => selector === '[data-focus]' ? n.attrs['data-focus'] != null : selector === '.queue-items' && n.className === 'queue-items'); }
    querySelector(selector) { return this.querySelectorAll(selector)[0] ?? null; }
    focus() { doc.activeElement = this; }
  }
  const doc = new Element('document'), win = new Element('window'); doc.hidden = false;
  doc.createElement = tag => new Element(tag); doc.getElementById = id => ids.get(id); doc.activeElement = null;
  for (const id of ['households', 'toast', 'live', 'pause-all']) { const node = new Element('div'); node.setAttribute('id', id); }
  const h = { calls, timers, doc, win, zones: [zone()], intercept: null };
  const network = async (route, args = {}) => {
    calls.push([route, JSON.parse(JSON.stringify(args))]);
    const override = h.intercept?.(route, args);
    if (override !== undefined) return override;
    if (route === 'zones') return h.zones;
    if (route === 'queue') return page(args.query.offset);
    if (route === 'state') {
      const z = h.zones.find(z => z.members.some(r => `${r}@${z.household}` === args.params.room));
      return { zone: z, transport_state: 'paused', volume: 20, mute: false,
        track: { uri: 'renderer:1', position_secs: 10, duration_secs: 120, queue_position: 1 } };
    }
    if (route === 'djStatus') return { running: false };
    return result(route, args.body || {});
  };
  const ctx = vm.createContext({ document: doc, window: win, network, performance: { now: () => 0 },
    URLSearchParams, AbortController, setTimeout(fn, delay) { timers.set(++serial, { fn, delay }); return serial; }, clearTimeout(id) { timers.delete(id); } });
  vm.runInContext(queueSource + '\n' + remoteSource.replace(/\nstart\(\);\s*$/, '') + '\ncall = network;', ctx);
  h.api = vm.runInContext('({house, render, refreshZone, beginAction, endAction, ownsCard, disposeCard, notifyCard})', ctx);
  h.render = () => { h.api.house.zones = h.zones; h.api.render(); };
  h.parts = () => [...h.api.house.cards.values()];
  h.render();
  h.open = async (p = h.parts()[0]) => { p.queue.node.open = true; p.queue.node.emit('toggle'); await settle(); };
  h.control = (p, key) => p.queue.node.descendants().find(n => n.attrs['data-focus'] === key);
  h.click = (p, key) => { const node = h.control(p, key); assert.ok(node, key); node.emit('click'); return node; };
  h.refresh = async (p = h.parts()[0]) => { h.click(p, 'Refresh queue'); await settle(); };
  h.posts = () => calls.filter(([, args]) => args.body);
  return h;
}

async function prepare(h, p, kind) {
  await h.open(p);
  if (kind === 'play') { const button = h.control(p, 'play-1'); return () => button.emit('click'); }
  if (kind === 'add' || kind === 'next') {
    const input = h.control(p, 'source'); input.value = 'spotify:track:0123456789abcdefghijkl'; input.emit('input');
    const button = h.control(p, kind === 'add' ? 'Add to end' : 'Queue next');
    return () => button.emit('click');
  }
  const select = h.control(p, 'select-1'); select.checked = true; select.emit('change');
  h.click(p, kind === 'remove' ? 'Review removal' : 'To end…');
  const button = h.control(p, kind === 'remove' ? 'Remove permanently' : 'Move entries');
  return () => button.emit('click');
}

for (const kind of ['play', 'add', 'next', 'remove', 'move']) {
  test(`${kind} owns the native transport lane until the one queue request settles`, async () => {
    const h = browser(), p = h.parts()[0], send = await prepare(h, p, kind), hold = deferred();
    h.intercept = (route, args) => args.body ? hold.promise : undefined;
    send(); await settle();
    assert.equal(p.writing, true); assert.equal(p.next.disabled, true); assert.equal(p.play.disabled, true);
    p.next.emit('click'); p.play.emit('click'); send(); await settle();
    assert.equal(h.posts().length, 1);
    const [route, args] = h.posts()[0]; hold.resolve(result(route, args.body)); await settle();
    assert.equal(p.writing, false); assert.equal(p.next.disabled, false);
    assert.equal(h.posts().length, 1); assert.ok(h.control(p, 'play-1'));
  });
}

for (const kind of ['play', 'add', 'remove', 'move']) {
  test(`a pending native action retires detached queue ${kind} handlers and reviews`, async () => {
    const h = browser(), p = h.parts()[0]; await prepare(h, p, kind);
    const key = { play: 'play-1', add: 'Add to end', remove: 'Remove permanently', move: 'Move entries' }[kind];
    const old = h.control(p, key), hold = deferred(); h.intercept = (route, args) => args.body ? hold.promise : undefined;
    p.next.emit('click'); old.emit('click'); await settle();
    assert.deepEqual(h.posts().map(([route]) => route), ['next']);
    hold.resolve({ done: 'Next', changed: true }); await settle(); old.emit('click'); await settle();
    assert.equal(h.posts().length, 1, 'old snapshot never regains authority');
    await h.refresh(p); assert.equal(h.control(p, 'play-1').disabled, false);
  });
}

test('queue writes retain exclusion across coordinator elections and group splits', async () => {
  const h = browser(); h.zones = [zone('Den', 'S2', ['Den', 'Office'])]; h.render();
  const old = h.parts()[0], send = await prepare(h, old, 'play'), hold = deferred();
  h.intercept = (route, args) => args.body ? hold.promise : undefined;
  send(); await settle(); h.zones = [zone('Office'), zone('Den')]; h.render();
  for (const p of h.parts()) { await h.open(p); assert.equal(p.play.disabled, true); assert.equal(h.control(p, 'Refresh queue').disabled, true); }
  hold.resolve({ done: 'Playing', changed: true }); await settle();
  for (const p of h.parts()) { await h.refresh(p); assert.equal(h.control(p, 'play-1').disabled, false); }
  assert.equal(h.posts().length, 1);
});

test('merging independently busy queue groups waits for both requests', async () => {
  const h = browser(); h.zones = [zone('Den'), zone('Office')]; h.render();
  const sends = []; for (const p of h.parts()) sends.push(await prepare(h, p, 'play'));
  const holds = [deferred(), deferred()]; let n = 0;
  h.intercept = (route, args) => args.body ? holds[n++].promise : undefined;
  sends.forEach(send => send()); await settle(); h.zones = [zone('Office', 'S2', ['Office', 'Den'])]; h.render();
  const p = h.parts()[0]; await h.open(p); assert.equal(p.next.disabled, true);
  holds[0].resolve({ done: 'Playing', changed: true }); await settle();
  assert.equal(p.next.disabled, true); assert.equal(h.control(p, 'Refresh queue').disabled, true);
  holds[1].resolve({ done: 'Playing', changed: true }); await settle();
  await h.refresh(p); assert.equal(p.next.disabled, false); assert.equal(h.posts().length, 2);
});

test('same-name rooms in separate households do not share the queue exclusion', async () => {
  const h = browser(); h.zones = [zone('Den', 'S1'), zone('Den', 'S2')]; h.render();
  const sends = []; for (const p of h.parts()) sends.push(await prepare(h, p, 'play'));
  const hold = deferred(); h.intercept = (route, args) => args.body ? hold.promise : undefined;
  sends.forEach(send => send()); await settle();
  assert.deepEqual(h.posts().map(([, args]) => args.body.zone).sort(), ['Den@S1', 'Den@S2']);
  hold.resolve({ done: 'Playing', changed: true }); await settle();
});

test('lost and malformed queue replies release native controls but never replay the edit', async () => {
  for (const reply of [null, new Error('lost response')]) {
    const h = browser(), p = h.parts()[0], send = await prepare(h, p, 'play');
    h.intercept = async (route, args) => args.body ? (reply instanceof Error ? Promise.reject(reply) : reply) : route === 'state'
      ? { zone: h.zones[0], transport_state: 'paused', volume: 20 } : page();
    send(); await settle(); assert.equal(p.writing, false); assert.equal(p.play.disabled, false);
    assert.equal(h.posts().length, 1); assert.equal(h.control(p, 'play-1'), undefined);
    assert.match(p.queue.node.descendants().map(n => n.textContent || '').join(' '), /not repeated/);
  }
});

test('a native action fences an earlier queue read before it can reenable editing', async () => {
  const h = browser(), p = h.parts()[0], reading = deferred(), writing = deferred();
  h.intercept = (route, args) => route === 'queue' ? reading.promise : args.body ? writing.promise : undefined;
  await h.open(p); p.next.emit('click'); reading.resolve(page()); await settle();
  assert.equal(h.control(p, 'play-1'), undefined);
  writing.resolve({ done: 'Next', changed: true }); await settle();
  assert.equal(h.control(p, 'play-1'), undefined); assert.equal(h.posts().length, 1);
});

test('declining execution does not send a queue command or replay the consumed intent', async () => {
  const calls = []; const c = Queue.model({ zone: 'Den@S2', execute: () => {}, call: async route => { calls.push(route); return page(); } });
  await c.load(); assert.equal(await c.play(1), false); assert.equal(await c.play(1), false);
  assert.deepEqual(calls, ['queue']); assert.match(c.state.message, /no queue command/);
});

test('closing or losing ownership before delayed execution sends no command', async () => {
  for (const interrupt of ['close', 'dispose', 'owner']) {
    let run, owned = true; const calls = [], hold = deferred();
    const c = Queue.model({ zone: 'Den@S2', current: () => owned,
      execute: work => { run = work; return hold.promise; }, call: async route => { calls.push(route); return page(); } });
    await c.load(); const action = c.play(1);
    if (interrupt === 'owner') owned = false; else c[interrupt]();
    await run(); hold.resolve(); assert.equal(await action, false); assert.deepEqual(calls, ['queue']);
  }
});

test('view callbacks cannot change the result or leak the action lane', async () => {
  let held = false, commands = 0;
  const c = Queue.model({ zone: 'Den@S2', changed() { throw new Error('broken view'); },
    execute: async work => { held = true; try { await work(); } finally { held = false; } },
    call: async route => { if (route === 'queue') return page(); commands++; return { done: 'Playing', changed: true }; } });
  await c.load(); assert.equal(await c.play(1), true); assert.equal(held, false); assert.equal(commands, 1);
});
