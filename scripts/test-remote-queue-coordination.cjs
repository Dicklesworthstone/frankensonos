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
  let serial = 0, now = 10000;
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
  const h = { calls, timers, doc, win, zones: [zone()], intercept: null, feeds: [] };
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
    if (route === 'favorites') return [];
    if (route === 'moods') return { moods: [] };
    return result(route, args.body || {});
  };
  class Feed extends Element {
    constructor() { super('feed'); this.readyState = 0; h.feeds.push(this); }
    close() { this.readyState = 2; }
  }
  win.EventSource = Feed;
  const ctx = vm.createContext({ document: doc, window: win, network, EventSource: Feed,
    Date: { now: () => now, parse: Date.parse }, performance: { now: () => now },
    URLSearchParams, AbortController, setTimeout(fn, delay) { timers.set(++serial, { fn, delay, at: now + delay }); return serial; }, clearTimeout(id) { timers.delete(id); } });
  vm.runInContext(queueSource + '\n' + remoteSource.replace(/\nstart\(\);\s*$/, '') + '\ncall = network;', ctx);
  h.api = vm.runInContext('({house, render, refreshZone, beginAction, endAction, ownsCard, disposeCard, notifyCard, loadZones, listen, LiveRemote})', ctx);
  h.render = () => { h.api.house.zones = h.zones; h.api.render(); };
  h.parts = () => [...h.api.house.cards.values()];
  h.render();
  h.open = async (p = h.parts()[0]) => { p.queue.node.open = true; p.queue.node.emit('toggle'); await settle(); };
  h.control = (p, key) => p.queue.node.descendants().find(n => n.attrs['data-focus'] === key);
  h.click = (p, key) => { const node = h.control(p, key); assert.ok(node, key); node.emit('click'); return node; };
  h.refresh = async (p = h.parts()[0]) => { h.click(p, 'Refresh queue'); await settle(); };
  h.posts = () => calls.filter(([, args]) => args.body);
  h.tick = async (ms) => {
    const end = now + ms; await settle();
    for (let i = 0; i < 10000; i++) {
      const next = [...timers].filter(([,t])=>t.at<=end).sort((a,b)=>a[1].at-b[1].at)[0];
      if (!next) { now=end; await settle(); return; }
      timers.delete(next[0]); now=next[1].at; next[1].fn(); await settle();
    }
    throw new Error('timer loop');
  };
  h.event = (kind, data = {}) => {
    const feed = h.feeds.at(-1);
    if (kind === 'open') feed.readyState = 1;
    feed.emit(kind, { data: JSON.stringify(data) });
  };
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
  h.click(p, kind === 'remove' ? 'Review removal' : 'move-selection-end');
  const button = h.control(p, kind === 'remove' ? 'Remove entries' : 'Move entries');
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

test('row movement shortcuts retain exact occurrence, review and native command ownership', async () => {
  const h = browser(), p = h.parts()[0], waiting = deferred();
  await h.open(p);
  h.click(p, 'move-4');
  assert.equal(h.posts().length, 0, 'opening placement options is read-only');
  h.click(p, 'move-4-earlier');
  assert.equal(h.posts().length, 0, 'choosing a destination prepares a review');
  h.intercept = (route, args) => route === 'queueMove' ? waiting.promise : undefined;
  const confirmation = h.click(p, 'Move entries');
  await settle();
  assert.deepEqual(h.posts().map(([route, args]) => [route, args.body]), [['queueMove', {
    zone: 'Den@S2', position: 4, insert_before: 3,
    expected_uris: ['renderer:4'], expected_revision: 'revision 1',
  }]]);
  assert.equal(p.writing, true);
  confirmation.emit('click'); await settle();
  assert.equal(h.posts().length, 1, 'repeated confirmation cannot dispatch twice');
  waiting.resolve(result('queueMove', h.posts()[0][1].body)); await settle();
  assert.equal(p.writing, false);
});

test('detached contextual move buttons cannot retarget a refreshed queue page', async () => {
  const h = browser(), p = h.parts()[0];
  await h.open(p); h.click(p, 'move-4');
  const old = h.control(p, 'move-4-earlier');
  await h.refresh(p);
  old.emit('click'); await settle();
  assert.equal(h.control(p, 'Move entries'), undefined);
  assert.equal(h.posts().length, 0);
  h.click(p, 'move-4'); h.click(p, 'move-4-earlier');
  assert.ok(h.control(p, 'Move entries'), 'the new snapshot can be deliberately reviewed');
  h.click(p, 'Cancel move');
  assert.equal(h.posts().length, 0);
});

for (const kind of ['play', 'add', 'remove', 'move']) {
  test(`a pending native action retires detached queue ${kind} handlers and reviews`, async () => {
    const h = browser(), p = h.parts()[0]; await prepare(h, p, kind);
    const key = { play: 'play-1', add: 'Add to end', remove: 'Remove entries', move: 'Move entries' }[kind];
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
  assert.equal(h.calls.filter(([r]) => r === 'queue').length, 2, 'a fresh post-action read replaces the fenced response');
  assert.equal(h.control(p, 'play-1').disabled, false); assert.equal(h.posts().length, 1);
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

// Open queues reconcile inside the same bounded household refresh, never a
// parallel event feed. These tests execute that wiring and the actual DOM.
async function live(h) {
  const controller=h.api.listen(); h.event('open'); await h.tick(300); return controller;
}
const queueReads = h => h.calls.filter(([r])=>r==='queue');

test('a coalesced live event burst refreshes only open queues and retains their source draft', async () => {
  const h=browser(); h.zones=[zone('Den'),zone('Office')]; h.render(); const controller=await live(h);
  assert.equal(queueReads(h).length,0); const p=h.parts()[0]; await h.open(p);
  const source=h.control(p,'source'); source.value='my unsent Spotify source'; source.emit('input');
  h.intercept=(r,a)=>r==='queue'?{...page(a.query.offset,6,'new'),playing:3}:undefined;
  for(let n=0;n<100;n++)h.event('zone.state',{room:'Den',queue_length:6});
  assert.equal(h.control(p,'play-1').disabled,true); await h.tick(300);
  assert.equal(queueReads(h).length,2); assert.equal(h.control(p,'source').value,'my unsent Spotify source');
  assert.equal(p.queue.node.descendants().find(n=>n.attrs['aria-current']==='true').children[0].children[0].textContent,'▶ Track 3');
  assert.equal(h.feeds.length,1); assert.equal(h.posts().length,0);
  await h.tick(180000); assert.equal(queueReads(h).length,2,'healthy quiet queues do not poll'); controller.dispose();
});

test('live refresh retains exact selection but consumes old confirmation buttons', async () => {
  const h=browser(); const controller=await live(h),p=h.parts()[0]; await h.open(p);
  const select=h.control(p,'select-1');select.checked=true;select.emit('change');h.click(p,'Review removal');
  const old=h.control(p,'Remove entries');h.event('zone.state',{room:'Den',volume:22}); await h.tick(300);
  assert.equal(h.control(p,'select-1').checked,true); assert.equal(h.control(p,'Remove entries'),undefined);
  old.emit('click');await settle();assert.equal(h.posts().length,0);controller.dispose();
});

test('detached play and selection handlers cannot act on a freshly reconciled page', async () => {
  const h=browser();const controller=await live(h),p=h.parts()[0];await h.open(p);
  const play=h.control(p,'play-1'),select=h.control(p,'select-1');
  h.intercept=(r,a)=>r==='queue'?page(a.query.offset,4,'new'):undefined;
  h.event('zone.state',{room:'Den'});await h.tick(300);play.emit('click');select.checked=true;select.emit('change');await settle();
  assert.equal(h.posts().length,0);assert.notEqual(h.control(p,'select-1').checked,true);
  h.click(p,'play-1');await settle();assert.equal(h.posts()[0][1].body.expected_revision,'new');controller.dispose();
});

test('same-name household notifications refresh each open queue with its exact qualified address', async () => {
  const h=browser();h.zones=[zone('Den','S1'),zone('Den','S2')];h.render();const controller=await live(h);
  for(const p of h.parts())await h.open(p);h.event('zone.state',{room:'Den'});await h.tick(300);
  assert.deepEqual(queueReads(h).slice(2).map(([,a])=>a.params.room),['Den@S1','Den@S2']);
  assert.equal(h.posts().length,0);controller.dispose();
});

test('a failing queue is isolated and later healthy rooms still receive their live snapshots', async () => {
  const h=browser();h.zones=[zone('Den'),zone('Office')];h.render();const controller=await live(h);
  for(const p of h.parts())await h.open(p);
  h.intercept=(r,a)=>r==='queue'?(a.params.room==='Den@S2'?Promise.reject(new Error('offline')):page(0,5,'new')):undefined;
  h.event('zone.state',{room:'Den'});h.event('zone.state',{room:'Office'});await h.tick(300);
  assert.equal(h.doc.getElementById('live').textContent,'State refresh failed; retrying…');
  assert.equal(h.control(h.parts()[0],'play-1').disabled,true);assert.equal(h.control(h.parts()[1],'play-1').disabled,false);
  const reads=queueReads(h).length;await h.tick(10000);assert.equal(queueReads(h).length,reads);
  h.intercept=null;await h.tick(10300);assert.equal(h.doc.getElementById('live').textContent,'Live');controller.dispose();
});

test('reset, action log and reconnect events all reconcile open queues without replaying actions', async () => {
  const h=browser();const controller=await live(h);await h.open();
  for(const kind of ['events.reset','action.logged','open']) {
    const before=queueReads(h).length;h.event(kind);await h.tick(300);assert.equal(queueReads(h).length,before+1);
  }
  assert.equal(h.feeds.length,1);assert.equal(h.posts().length,0);controller.dispose();
});

test('an uncertain queue write remains blocked through automatic post-action and event refreshes', async () => {
  const h=browser();const controller=await live(h),p=h.parts()[0];await h.open(p);
  h.intercept=(r,a)=>a.body?Promise.reject(new Error('reply lost')):undefined;
  h.click(p,'play-1');await settle();const count=queueReads(h).length;
  h.event('zone.state',{room:'Den'});await h.tick(300);assert.equal(queueReads(h).length,count);assert.equal(h.control(p,'play-1'),undefined);
  assert.match(p.queue.node.descendants().map(n=>n.textContent||'').join(' '),/not repeated/);
  h.intercept=null;await h.refresh(p);assert.equal(h.control(p,'play-1').disabled,false);assert.equal(h.posts().length,1);controller.dispose();
});

test('closed queues ignore updates; hidden queues wait for the shared resume snapshot', async () => {
  const h=browser();const controller=await live(h),p=h.parts()[0];await h.open(p);
  p.queue.node.open=false;p.queue.node.emit('toggle');h.event('zone.state',{room:'Den'});await h.tick(300);assert.equal(queueReads(h).length,1);
  await h.open(p);h.doc.hidden=true;h.doc.emit('visibilitychange');const count=queueReads(h).length;
  await h.tick(60000);assert.equal(queueReads(h).length,count);
  h.doc.hidden=false;h.doc.emit('visibilitychange');assert.equal(queueReads(h).length,count);
  h.event('open');await h.tick(300);assert.equal(queueReads(h).length,count+1);controller.dispose();
});

test('queue lifecycle listeners are released when topology removes their cards', async () => {
  const h=browser();await h.open();assert.equal(h.doc.events.get('visibilitychange').size,1);
  h.zones=[];h.render();assert.equal(h.doc.events.get('visibilitychange').size,0);
  assert.equal(h.win.events.get('pagehide').size,0);assert.equal(h.win.events.get('pageshow').size,0);assert.equal(h.timers.size,0);
});

test('native actions trigger automatic open-queue recovery without needing a new SSE event', async () => {
  const h=browser(),p=h.parts()[0];await h.open(p);h.intercept=(r,a)=>r==='queue'?page(a.query.offset,5,'after native action'):undefined;
  p.next.emit('click');await settle();assert.equal(queueReads(h).length,2);assert.equal(h.control(p,'play-1').disabled,false);
  assert.deepEqual(h.posts().map(([r])=>r),['next']);
});
