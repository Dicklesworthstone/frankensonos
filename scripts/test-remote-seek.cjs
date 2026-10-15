// node --test scripts/test-remote-seek.cjs; exact-time and cross-card seek ownership.
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const Playback = require('../crates/fsonos-api/src/remote/playback.js');
const clone = (v) => JSON.parse(JSON.stringify(v));
const settle = async () => { for (let i = 0; i < 60; i++) await Promise.resolve(); };
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
function clock() {
  let now = 10000, serial = 0;
  const timers = new Map();
  return { now: () => now, timers,
    schedule(fn, ms) { timers.set(++serial, { fn, at: now + ms }); return serial; },
    cancel(id) { timers.delete(id); },
    async tick(ms) {
      const end = now + ms; await settle();
      for (let n = 0; n < 10000; n++) {
        const next = [...timers].filter(([, t]) => t.at <= end).sort((a, b) => a[1].at - b[1].at)[0];
        if (!next) { now = end; await settle(); return; }
        now = next[1].at; timers.delete(next[0]); next[1].fn(); await settle();
      }
      throw new Error('timer loop');
    } };
}
const zone = (household = 'S2', room = 'Den') => ({ household, coordinator_room: room, members: [room] });
const state = (z = zone()) => ({ zone: clone(z), transport_state: 'paused',
  track: { uri: 'renderer:synthetic-track', duration_secs: 14400, position_secs: 20, queue_position: 3 } });
function harness(t, options = {}) {
  const time = clock(), calls = [], views = [];
  const h = { time, calls, views, value: state(options.zone), handler: null, lanes: options.lanes || new Map() };
  h.api = Playback.model({ zone: options.zone || zone(), now: time.now, schedule: time.schedule, cancel: time.cancel,
    lanes: h.lanes, ...options,
    call: async (route, request) => {
      calls.push([route, clone(request)]);
      if (h.handler) return h.handler(route, request);
      if (route === 'state') return clone(h.value);
      h.value.track.position_secs = request.body.to_secs;
      return { done: 'Seek accepted', changed: true, volume: null, notes: [] };
    }, changed: (v) => views.push(v) });
  t.after(() => h.api.dispose()); h.api.observe(h.value); return h;
}
const writes = (h) => h.calls.filter(([name]) => name !== 'state');

test('time entry accepts seconds and [hours:]minutes:seconds without overflow or guessing', () => {
  for (const [s, expected] of [['0', 0], [' 90 ', 90], ['1:30', 90], ['01:02:03', 3723], ['120:00', 7200], ['4294967295', 4294967295]]) {
    assert.equal(Playback.parseTime(s), expected);
  }
  for (const s of ['', ' ', null, 30, '-1', '1.5', '1:60', '1:2', '1:00:60', '1:60:00', '1:2:03', '1:02:03:04', '4294967296', '9'.repeat(100)]) {
    assert.equal(Playback.parseTime(s), null, String(s));
  }
});

test('a replacement card cannot overlap the old card\'s in-flight seek', async (t) => {
  const lanes = new Map(), old = harness(t, { lanes }), wait = deferred();
  old.handler = (route) => route === 'state' ? old.value : wait.promise;
  const operation = old.api.confirm(old.api.preview(90)); await settle(); old.api.dispose();
  const replacement = harness(t, { lanes });
  assert.equal(replacement.api.view().pending, true); assert.equal(replacement.api.view().seekable, false);
  assert.equal(replacement.api.preview(150), null); assert.equal(writes(replacement).length, 0);
  wait.resolve({ done: 'Seek accepted', changed: true }); await operation;
  assert.equal(replacement.api.view().pending, false); assert.equal(replacement.api.view().fresh, false);
  assert.equal(await replacement.api.refresh(), true); assert.equal(replacement.api.view().seekable, true);
});

test('two present cards share one seek lane and invalidate the other card\'s prior review', async (t) => {
  const lanes = new Map(), a = harness(t, { lanes }), b = harness(t, { lanes }), wait = deferred();
  const stale = b.api.preview(150); a.handler = (route) => route === 'state' ? a.value : wait.promise;
  const operation = a.api.confirm(a.api.preview(90)); await settle();
  assert.equal(b.api.view().review, null); assert.equal(await b.api.confirm(stale), false);
  assert.equal(await b.api.refresh(), false, 'cannot read a pending command into false certainty');
  wait.resolve({ done: 'Seek accepted', changed: true }); await operation;
  assert.equal(writes(a).length, 1); assert.equal(writes(b).length, 0);
});

test('a lost reply survives card disposal, an empty layout and later recreation', async (t) => {
  const lanes = new Map(), old = harness(t, { lanes }), wait = deferred();
  old.handler = (route) => route === 'state' ? old.value : wait.promise;
  const operation = old.api.confirm(old.api.preview(90)); await settle(); old.api.dispose();
  wait.reject(new Error('reply lost')); assert.equal(await operation, false);
  assert.equal(lanes.size, 1, 'retain unresolved ownership while no card is mounted');
  const next = harness(t, { lanes });
  assert.equal(next.api.view().uncertain, true); assert.equal(next.api.view().seekable, false);
  assert.match(next.api.view().result.detail, /reply lost/);
  next.api.observe(next.value); assert.equal(next.api.view().uncertain, true);
  next.api.suspend(); next.api.resume(); next.api.observe(next.value);
  assert.equal(next.api.view().uncertain, true);
  assert.equal(await next.api.refresh(), true); assert.equal(next.api.view().uncertain, false);
  assert.equal(writes(next).length, 0); next.api.dispose(); assert.equal(lanes.size, 0);
});

test('failed explicit recovery cannot clear shared uncertainty', async (t) => {
  const lanes = new Map(), old = harness(t, { lanes });
  old.handler = async (route) => { if (route === 'state') return old.value; throw new Error('lost'); };
  await old.api.confirm(old.api.preview(90)); old.api.dispose();
  const next = harness(t, { lanes }); next.handler = async () => { throw new Error('offline'); };
  assert.equal(await next.api.refresh(), false); assert.equal(next.api.view().uncertain, true);
  next.api.dispose(); const third = harness(t, { lanes }); assert.equal(third.api.view().uncertain, true);
});

test('malformed acknowledgements retain uncertainty for replacement cards', async (t) => {
  for (const reply of [null, {}, { done: 'Done' }, { done: 'Done', changed: true, notes: ['invalid'] }]) {
    const lanes = new Map(), old = harness(t, { lanes });
    old.handler = async (route) => route === 'state' ? old.value : reply;
    assert.equal(await old.api.confirm(old.api.preview(90)), false); old.api.dispose();
    const next = harness(t, { lanes }); assert.equal(next.api.view().uncertain, true);
    assert.equal(next.api.preview(150), null);
  }
});

test('different households do not share a same-name room\'s seek lane', async (t) => {
  const lanes = new Map(), a = harness(t, { lanes, zone: zone('S1') }), b = harness(t, { lanes }), wait = deferred();
  a.handler = () => wait.promise; const one = a.api.confirm(a.api.preview(90));
  assert.equal(await b.api.confirm(b.api.preview(120)), true); assert.equal(writes(b).length, 1);
  wait.reject(new Error('offline')); await one;
});

test('a skipped card action releases ownership without authorizing its old review', async (t) => {
  const lanes = new Map(), a = harness(t, { lanes, execute: async () => {} });
  const selected = a.api.preview(90); assert.equal(await a.api.confirm(selected), false);
  assert.equal(a.api.view().pending, false); assert.equal(await a.api.confirm(selected), false);
  assert.equal(a.calls.length, 0); a.api.dispose(); assert.equal(lanes.size, 0);
});

test('disposed preflight cannot send a seek and does not leave its replacement busy', async (t) => {
  const lanes = new Map(), a = harness(t, { lanes }), wait = deferred(); a.handler = () => wait.promise;
  const operation = a.api.confirm(a.api.preview(90)); a.api.dispose();
  const next = harness(t, { lanes }); wait.resolve(a.value); await operation;
  assert.equal(writes(a).length, 0); assert.equal(next.api.view().pending, false);
});

test('acknowledged commands finishing while hidden retain their receipt without fabricating a clock', async (t) => {
  const h = harness(t), wait = deferred(); h.handler = (route) => route === 'state' ? h.value : wait.promise;
  const operation = h.api.confirm(h.api.preview(90)); await settle(); h.api.suspend();
  wait.resolve({ done: 'Seek accepted while hidden', changed: true }); assert.equal(await operation, true); h.api.resume();
  assert.equal(h.api.view().result.status, 'accepted'); assert.match(h.api.view().result.detail, /while hidden/);
  assert.equal(h.api.view().fresh, false); assert.equal(h.calls.length, 2);
});

test('a successful explicit read clears uncertainty in another view but makes its old sample stale', async (t) => {
  const lanes = new Map(), a = harness(t, { lanes }), b = harness(t, { lanes });
  a.handler = async (route) => { if (route === 'state') return a.value; throw new Error('lost'); };
  await a.api.confirm(a.api.preview(90)); b.api.observe(b.value); assert.equal(b.api.view().uncertain, true);
  a.handler = null; await a.api.refresh();
  assert.equal(b.api.view().uncertain, false); assert.equal(b.api.view().fresh, false);
  assert.equal(b.api.preview(150), null); assert.equal(writes(b).length, 0);
});

test('existing seek contract still preserves paused playback and rechecks duplicate track slots', async (t) => {
  const h = harness(t); assert.equal(await h.api.confirm(h.api.preview(90)), true);
  assert.equal(h.api.view().elapsed, 90); assert.equal(h.api.view().transport, 'paused');
  assert.deepEqual(writes(h), [['seek', { body: { zone: 'Den@S2', to_secs: 90 } }]]);
  const review = h.api.preview(150); h.value.track.queue_position++;
  assert.equal(await h.api.confirm(review), false); assert.equal(writes(h).length, 1);
});

class Element {
  constructor(tag, attrs = {}, ...children) {
    this.tag = tag; this.attrs = { ...attrs }; this.handlers = new Map(); this.children = children;
    this.disabled = Boolean(attrs.disabled); this.hidden = Boolean(attrs.hidden); this.value = attrs.value ?? '';
    this._text = attrs.text || ''; this.textWrites = 0;
  }
  set textContent(value) { this._text = value; this.textWrites++; }
  get textContent() { return this._text; }
  setAttribute(key, value) { this.attrs[key] = String(value); }
  addEventListener(key, fn) { this.handlers.set(key, fn); }
  emit(key, event = {}) { this.handlers.get(key)?.({ preventDefault() {}, ...event }); }
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.children = children; }
  all() { return [this, ...this.children.flatMap((child) => child instanceof Element ? child.all() : [])]; }
}
function panelHarness(t) {
  const time = clock(), calls = [], value = state();
  const panel = Playback.panel({ zone: value.zone, now: time.now, schedule: time.schedule, cancel: time.cancel,
    el: (tag, attrs, ...children) => new Element(tag, attrs, ...children),
    call: async (route, request) => {
      calls.push([route, clone(request)]); if (route === 'state') return clone(value);
      value.track.position_secs = request.body.to_secs; return { done: 'Seek accepted', changed: true };
    } });
  t.after(() => panel.dispose()); panel.observe(value);
  return { panel, time, calls, value, find: (predicate) => panel.node.all().find(predicate),
    input: panel.node.all().find(n => n.attrs.type === 'text') };
}

test('an exact time creates the existing immutable review without writing', async (t) => {
  const h = panelHarness(t); assert.ok(h.input);
  h.input.value = '1:02:03'; h.input.emit('input');
  assert.equal(h.panel.view().review.target, 3723); assert.equal(h.calls.length, 0);
  assert.equal(h.find(n => n.textContent === 'Seek to 1:02:03').disabled, false);
  h.input.emit('keydown', { key: 'Enter' }); await settle();
  assert.deepEqual(h.calls[1], ['seek', { body: { zone: 'Den@S2', to_secs: 3723 } }]);
});

test('invalid exact-time input invalidates an older valid review and cannot submit it', async (t) => {
  const h = panelHarness(t); h.input.value = '1:30'; h.input.emit('input');
  const old = h.find(n => n.textContent === 'Seek to 1:30');
  for (const s of ['1:99', '-1', '', '4:00:00', 'nonsense']) {
    h.input.value = s; h.input.emit('input'); assert.equal(h.panel.view().review, null);
    h.input.emit('keydown', { key: 'Enter' }); old.emit('click'); await settle();
  }
  assert.equal(h.calls.length, 0);
});

test('partial typed time survives clock ticks but the draft is cleared by changed track identity', async (t) => {
  const h = panelHarness(t); h.value.transport_state = 'playing'; h.panel.observe(h.value);
  h.input.value = '1:'; h.input.emit('input'); await h.time.tick(5000); assert.equal(h.input.value, '1:');
  h.input.value = '01:30'; h.input.emit('input'); h.value.track.position_secs++; h.panel.observe(h.value);
  assert.equal(h.input.value, '01:30'); assert.equal(h.panel.view().review.target, 90);
  h.value.track.queue_position++; h.panel.observe(h.value);
  assert.equal(h.input.value, ''); assert.equal(h.panel.view().review, null);
});

test('Escape cancels a typed review and cannot leave a detached confirmation active', async (t) => {
  const h = panelHarness(t); h.input.value = '1:30'; h.input.emit('input');
  const old = h.find(n => n.textContent === 'Seek to 1:30');
  h.input.emit('keydown', { key: 'Escape' }); assert.equal(h.panel.view().review, null); assert.equal(h.input.value, '');
  old.emit('click'); await settle(); assert.equal(h.calls.length, 0);
});

test('an exact-time field is disabled for unknown timing and after the page is suspended', (t) => {
  const h = panelHarness(t); h.value.track.duration_secs = 0; h.panel.observe(h.value);
  assert.equal(h.input.disabled, true);
  h.panel.observe(state()); assert.equal(h.input.disabled, false); h.panel.suspend(); h.panel.resume();
  assert.equal(h.input.disabled, true);
});

test('status and input guidance are not re-announced on every clock tick', async (t) => {
  const h = panelHarness(t); h.value.transport_state = 'playing'; h.panel.observe(h.value);
  const status = h.find(n => n.attrs.role === 'status'); const before = status.textWrites;
  await h.time.tick(10000); assert.equal(status.textWrites, before);
});

test('disposing an old model twice cannot retire the replacement model\'s shared lane', async (t) => {
  const lanes = new Map(), old = harness(t, { lanes }); old.api.dispose();
  const next = harness(t, { lanes }); old.api.dispose(); assert.equal(lanes.size, 1);
  const third = harness(t, { lanes }), wait = deferred();
  next.handler = (route) => route === 'state' ? next.value : wait.promise;
  const operation = next.api.confirm(next.api.preview(90)); await settle();
  assert.equal(third.api.view().pending, true); assert.equal(third.api.preview(150), null);
  wait.resolve({ done: 'Seek accepted', changed: true }); await operation;
});

test('standalone clients do not share commands across separate daemon instances', async (t) => {
  const a = harness(t, { lanes: undefined }), b = harness(t, { lanes: undefined }), wait = deferred();
  a.handler = (route) => route === 'state' ? a.value : wait.promise;
  const operation = a.api.confirm(a.api.preview(90)); await settle();
  assert.equal(b.api.view().pending, false); assert.equal(await b.api.confirm(b.api.preview(150)), true);
  wait.resolve({ done: 'Seek accepted', changed: true }); await operation;
});

test('changing a track or losing state clears exact-time review guidance along with the selection', (t) => {
  const h = panelHarness(t); h.input.value = '1:30'; h.input.emit('input');
  const guidance = h.find(n => /Time previewed/.test(n.textContent)); assert.ok(guidance);
  h.value.track.queue_position++; h.panel.observe(h.value);
  assert.equal(guidance.textContent, ''); assert.equal(h.panel.view().review, null);
  h.input.value = '2:30'; h.input.emit('input'); h.panel.stale();
  assert.equal(guidance.textContent, ''); assert.equal(h.input.value, '');
});

const grouped = (coordinator_room, members, household = 'S2') => ({ household, coordinator_room, members });

test('coordinator re-election retains a pending seek and its lost acknowledgement', async (t) => {
  const lanes = new Map(), wait = deferred();
  const old = harness(t, { lanes, zone: grouped('Den', ['Den', 'Office']) });
  old.handler = (route) => route === 'state' ? old.value : wait.promise;
  const operation = old.api.confirm(old.api.preview(90)); await settle(); old.api.dispose();
  const replacement = harness(t, { lanes, zone: grouped('Office', ['Office', 'Den']) });
  assert.equal(replacement.api.view().pending, true);
  assert.equal(replacement.api.preview(150), null);
  assert.equal(await replacement.api.refresh(), false);
  wait.reject(new Error('accepted seek lost its reply during re-election'));
  assert.equal(await operation, false);
  assert.equal(replacement.api.view().uncertain, true);
  assert.match(replacement.api.view().result.detail, /re-election/);
  assert.equal(await replacement.api.refresh(), true);
  assert.equal(replacement.api.view().seekable, true);
  assert.equal(writes(old).length, 1); assert.equal(writes(replacement).length, 0);
});

test('an absent coordinator cannot erase unresolved ownership of the remaining members', async (t) => {
  const lanes = new Map();
  const old = harness(t, { lanes, zone: grouped('Den', ['Den', 'Office', 'Hall']) });
  old.handler = async (route) => { if (route === 'state') return old.value; throw new Error('reply lost'); };
  await old.api.confirm(old.api.preview(90)); old.api.dispose();
  const remaining = harness(t, { lanes, zone: grouped('Office', ['Hall', 'Office']) });
  assert.equal(remaining.api.view().uncertain, true);
  assert.equal(remaining.api.preview(150), null);
  assert.equal(await remaining.api.refresh(), true); remaining.api.dispose();
  // The absent room still needs an observation when it returns as a separate group.
  const returned = harness(t, { lanes, zone: zone('S2', 'Den') });
  assert.equal(returned.api.view().uncertain, true);
  assert.equal(await returned.api.refresh(), true); returned.api.dispose();
  assert.equal(lanes.size, 0);
});

test('a split keeps both groups blocked while their former group seek is pending', async (t) => {
  const lanes = new Map(), wait = deferred();
  const old = harness(t, { lanes, zone: grouped('Den', ['Den', 'Office']) });
  old.handler = (route) => route === 'state' ? old.value : wait.promise;
  const operation = old.api.confirm(old.api.preview(90)); await settle(); old.api.dispose();
  const a = harness(t, { lanes, zone: zone('S2', 'Den') });
  const b = harness(t, { lanes, zone: zone('S2', 'Office') });
  for (const h of [a, b]) {
    assert.equal(h.api.view().pending, true); assert.equal(h.api.preview(150), null);
    assert.equal(await h.api.refresh(), false); assert.equal(h.calls.length, 0);
  }
  wait.resolve({ done: 'Seek accepted', changed: true }); await operation;
  for (const h of [a, b]) {
    assert.equal(h.api.view().pending, false); assert.equal(h.api.view().fresh, false);
    assert.equal(h.api.view().result.status, 'accepted');
    assert.equal(await h.api.refresh(), true);
  }
});

test('observing one split group cannot clear uncertainty in its former sibling', async (t) => {
  const lanes = new Map();
  const old = harness(t, { lanes, zone: grouped('Den', ['Den', 'Office']) });
  old.handler = async (route) => { if (route === 'state') return old.value; throw new Error('lost'); };
  await old.api.confirm(old.api.preview(90)); old.api.dispose();
  const a = harness(t, { lanes, zone: zone('S2', 'Den') });
  const b = harness(t, { lanes, zone: zone('S2', 'Office') });
  assert.equal(await a.api.refresh(), true);
  assert.equal(a.api.view().seekable, true);
  assert.equal(b.api.view().uncertain, true); assert.equal(b.api.preview(150), null);
  b.api.observe(b.value); assert.equal(b.api.view().uncertain, true);
  b.api.dispose(); const returned = harness(t, { lanes, zone: zone('S2', 'Office') });
  assert.equal(returned.api.view().uncertain, true);
  assert.equal(await returned.api.refresh(), true);
  a.api.dispose(); returned.api.dispose(); assert.equal(lanes.size, 0);
});

test('merging independent groups retains both in-flight owners until each settles', async (t) => {
  const lanes = new Map(), aw = deferred(), bw = deferred();
  const a = harness(t, { lanes, zone: zone('S2', 'Den') });
  const b = harness(t, { lanes, zone: zone('S2', 'Office') });
  a.handler = (route) => route === 'state' ? a.value : aw.promise;
  b.handler = (route) => route === 'state' ? b.value : bw.promise;
  const ar = a.api.confirm(a.api.preview(90)), br = b.api.confirm(b.api.preview(120));
  await settle(); a.api.dispose(); b.api.dispose();
  const merged = harness(t, { lanes, zone: grouped('Den', ['Den', 'Office']) });
  assert.equal(merged.api.view().pending, true);
  aw.resolve({ done: 'First seek accepted', changed: true }); await ar;
  assert.equal(merged.api.view().pending, true, 'the second member still has a pending command');
  assert.equal(await merged.api.refresh(), false); assert.equal(merged.api.preview(150), null);
  bw.reject(new Error('second acknowledgement lost')); await br;
  assert.equal(merged.api.view().pending, false); assert.equal(merged.api.view().uncertain, true);
  assert.match(merged.api.view().result.detail, /second acknowledgement lost/);
  assert.equal(await merged.api.refresh(), true); assert.equal(merged.api.view().seekable, true);
  assert.equal(writes(merged).length, 0);
});

test('a newly joined room inherits exclusion through an old member, not its new coordinator name', async (t) => {
  const lanes = new Map(), wait = deferred();
  const old = harness(t, { lanes, zone: grouped('Den', ['Den', 'Office']) });
  old.handler = (route) => route === 'state' ? old.value : wait.promise;
  const operation = old.api.confirm(old.api.preview(90)); await settle(); old.api.dispose();
  const merged = harness(t, { lanes, zone: grouped('Hall', ['Hall', 'Office']) });
  assert.equal(merged.api.view().pending, true); assert.equal(merged.api.preview(150), null);
  wait.reject(new Error('lost')); await operation;
  assert.equal(merged.api.view().uncertain, true); assert.equal(await merged.api.refresh(), true);
});

test('member-scoped recovery is isolated between households and unrelated groups', async (t) => {
  const lanes = new Map(), wait = deferred();
  const a = harness(t, { lanes, zone: grouped('Den', ['Den', 'Office'], 'S1') });
  a.handler = (route) => route === 'state' ? a.value : wait.promise;
  const operation = a.api.confirm(a.api.preview(90)); await settle();
  const otherHouse = harness(t, { lanes, zone: grouped('Office', ['Office', 'Den'], 'S2') });
  const otherGroup = harness(t, { lanes, zone: zone('S1', 'Hall') });
  for (const h of [otherHouse, otherGroup]) {
    assert.equal(h.api.view().pending, false);
    assert.equal(await h.api.confirm(h.api.preview(150)), true);
  }
  wait.resolve({ done: 'Seek accepted', changed: true }); await operation;
});

test('an obsolete preflight releases every affected member without inventing uncertainty', async (t) => {
  const lanes = new Map(), wait = deferred();
  const old = harness(t, { lanes, zone: grouped('Den', ['Den', 'Office']) });
  old.handler = () => wait.promise;
  const operation = old.api.confirm(old.api.preview(90)); await settle(); old.api.dispose();
  const next = harness(t, { lanes, zone: grouped('Office', ['Office', 'Den']) });
  assert.equal(next.api.view().pending, true);
  wait.resolve(old.value); assert.equal(await operation, false);
  assert.equal(next.api.view().pending, false); assert.equal(next.api.view().uncertain, false);
  assert.equal(writes(old).length, 0); assert.equal(await next.api.refresh(), true);
});

test('retiring a former coordinator twice leaves replacement members registered', async (t) => {
  const lanes = new Map();
  const old = harness(t, { lanes, zone: grouped('Den', ['Den', 'Office']) });
  const next = harness(t, { lanes, zone: grouped('Office', ['Office', 'Den']) });
  old.api.dispose(); old.api.dispose(); assert.equal(lanes.size, 2);
  const wait = deferred(); next.handler = (route) => route === 'state' ? next.value : wait.promise;
  const operation = next.api.confirm(next.api.preview(90)); await settle();
  const sibling = harness(t, { lanes, zone: zone('S2', 'Den') });
  assert.equal(sibling.api.view().pending, true);
  wait.resolve({ done: 'Seek accepted', changed: true }); await operation;
  next.api.dispose(); sibling.api.dispose(); assert.equal(lanes.size, 0);
});
