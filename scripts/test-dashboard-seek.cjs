'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const Seek = require('../crates/fsonos-api/src/remote/dashboard-seek.js');
const copy = value => structuredClone(value);
const deferred = () => { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; };
const settled = async predicate => { for (let n = 0; n < 40 && !predicate(); n++) await Promise.resolve(); assert.ok(predicate(), 'operation settled'); };

function fixture({ state = 'playing', track = {}, demo = false, dom = false } = {}) {
  const zones = ['S2', 'S1'].map(household => ({ household, coordinator_room: 'Synthetic room',
    members: ['Synthetic room'], transport_state: household === 'S2' ? state : 'paused' }));
  const rows = zones.map((zone, index) => ({ zone, transport_state: index ? 'paused' : state,
    volume: 20, track: { uri: 'fixture:track', title: 'Synthetic song', queue_position: 2,
      position_secs: 30, duration_secs: 180, ...track } }));
  let time = 1000, reader = null, writer = null, seek;
  const calls = [];
  const call = async (url, body) => {
    calls.push({ url, body });
    if (body !== undefined) {
      if (writer) return writer(url, body);
      const row = rows.find(value => C.key(value.zone) === body.zone);
      if (url === '/seek') row.track.position_secs = body.to_secs;
      return { done: 'Playback position changed', changed: true };
    }
    if (url === '/zones') return copy(zones);
    if (reader) return reader(url);
    const row = rows.find(value => url === '/zones/' + encodeURIComponent(C.key(value.zone)) + '/state');
    if (!row) throw new Error('Unexpected fixture read: ' + url);
    return copy(row);
  };
  const ctl = C.model({ call, demo, now: () => time, changed: () => seek?.observe() });
  const doc = dom ? documentFixture() : null;
  seek = dom ? Seek.create({ call, controller: ctl, now: () => time, document: doc })
    : Seek.model({ call, controller: ctl, now: () => time });
  return { ctl, seek, doc, rows, zones, calls,
    writes: () => calls.filter(row => row.body !== undefined),
    read(fn) { reader = fn; }, write(fn) { writer = fn; }, time(value) { time = value; },
    async ready() { assert.equal(await ctl.refresh(), true); ctl.connection(true); return this; } };
}

test('position review is immutable, household-qualified and read-only until confirmed', async () => {
  const f = await fixture().ready(), choice = f.seek.begin(80);
  assert.equal(choice.target, 'Synthetic room@S2');
  assert.ok(Object.isFrozen(choice));
  assert.throws(() => { choice.to_secs = 90; }, TypeError);
  const next = f.seek.choose(95);
  assert.notEqual(next, choice); assert.equal(choice.to_secs, 80); assert.equal(next.to_secs, 95);
  assert.equal(await f.seek.confirm(choice), false);
  assert.equal(f.writes().length, 0);
});

test('current position and 15-second preview steps stay inside the track', async () => {
  const f = await fixture().ready(); f.time(3500);
  assert.equal(f.seek.begin().to_secs, 32);
  f.seek.choose(5); assert.equal(f.seek.step(-15).to_secs, 0);
  f.seek.choose(175); assert.equal(f.seek.step(15).to_secs, 179);
  for (const value of [-1, 180, 0.5, NaN, Infinity, 0x100000000]) assert.equal(f.seek.choose(value), null);
  assert.equal(f.seek.step(1), null); assert.equal(f.writes().length, 0);
});

test('unknown positions, unbounded streams and non-seekable transports cannot create reviews', async () => {
  for (const options of [{ track: { duration_secs: null } }, { track: { duration_secs: 0 } },
    { track: { position_secs: null } }, { track: { uri: 'NOT_IMPLEMENTED' } },
    { state: 'stopped' }, { state: 'transitioning' }, { state: 'unknown' }, { demo: true }]) {
    const f = await fixture(options).ready();
    assert.equal(f.seek.available(), false, JSON.stringify(options));
    assert.equal(f.seek.begin(0), null); assert.equal(await f.seek.confirm(), false);
    assert.equal(f.writes().length, 0);
  }
});

test('a one-second track can seek only to zero', async () => {
  const f = await fixture({ track: { duration_secs: 1, position_secs: 0 } }).ready();
  assert.equal(f.seek.begin().to_secs, 0); assert.equal(f.seek.step(15).to_secs, 0);
  assert.equal(f.seek.begin(1), null);
  assert.equal(await f.seek.confirm(), true);
  assert.deepEqual(f.writes(), [{ url: '/seek', body: { zone: 'Synthetic room@S2', to_secs: 0 } }]);
});

test('paused seeking sends exactly one native seek and reads the affected group afterward', async () => {
  const f = await fixture({ state: 'paused' }).ready(), choice = f.seek.begin(80), before = f.calls.length;
  assert.equal(await f.seek.confirm(choice), true);
  assert.deepEqual(f.calls.slice(before).map(value => value.url), [
    '/zones/Synthetic%20room%40S2/state', '/seek', '/zones/Synthetic%20room%40S2/state']);
  assert.deepEqual(f.writes(), [{ url: '/seek', body: { zone: 'Synthetic room@S2', to_secs: 80 } }]);
  assert.equal(f.ctl.selected().state.transport_state, 'paused');
  assert.equal(f.ctl.selected().state.track.position_secs, 80);
  assert.equal(f.seek.state.open, false);
});

test('normal elapsed progress does not retarget the reviewed absolute position', async () => {
  const f = await fixture().ready(), choice = f.seek.begin(80);
  f.rows[0].track.position_secs = 40;
  assert.equal(await f.seek.confirm(choice), true);
  assert.equal(f.writes()[0].body.to_secs, 80);
});

test('fresh preflight rejects a different URI, duplicate occurrence, duration or transport', async () => {
  for (const change of [row => { row.track.uri = 'fixture:next'; }, row => { row.track.queue_position = 3; },
    row => { row.track.duration_secs = 240; }, row => { row.transport_state = 'paused'; },
    row => { row.track.duration_secs = null; }]) {
    const f = await fixture().ready(), choice = f.seek.begin(80);
    change(f.rows[0]);
    assert.equal(await f.seek.confirm(choice), false);
    assert.equal(f.writes().length, 0); assert.equal(f.ctl.state.uncertain, false);
    assert.match(f.ctl.state.message, /No command was sent/);
    assert.equal(f.seek.state.pending, null);
  }
});

test('preflight cannot substitute changed group membership or another household', async () => {
  for (const change of [row => { row.zone.members.push('Synthetic guest'); },
    row => { row.zone.household = 'S1'; }]) {
    const f = await fixture().ready(), choice = f.seek.begin(80), raw = copy(f.rows[0]);
    change(raw); f.read(() => raw);
    assert.equal(await f.seek.confirm(choice), false); assert.equal(f.writes().length, 0);
  }
});

test('selection and selected-group invalidation permanently retire a pending review', async () => {
  for (const change of [f => f.ctl.select('Synthetic room@S1'), f => f.ctl.invalidate(),
    f => f.ctl.invalidate(['Synthetic room@S2'])]) {
    const f = await fixture().ready(), choice = f.seek.begin(80);
    change(f); assert.equal(f.seek.state.pending, null);
    await f.ctl.refresh();
    assert.equal(await f.seek.confirm(choice), false); assert.equal(f.writes().length, 0);
  }
});

test('an unrelated household event leaves the selected group review intact', async () => {
  const f = await fixture().ready(), choice = f.seek.begin(80);
  f.ctl.invalidate(['Synthetic room@S1']);
  assert.equal(f.seek.state.pending, choice);
  assert.equal(await f.seek.confirm(choice), true);
});

test('a disconnected snapshot expiring after 30 seconds cannot keep an old review alive', async () => {
  const f = await fixture().ready(); f.ctl.connection(false); await f.ctl.refresh();
  const choice = f.seek.begin(80); f.time(32000); f.seek.observe();
  assert.equal(f.seek.state.pending, null); assert.equal(await f.seek.confirm(choice), false);
});

test('cancel, hide and disposal retire a held preflight without releasing the shared command lane early', async () => {
  for (const retire of [seek => seek.close(), seek => seek.suspend(), seek => seek.dispose()]) {
    const f = await fixture().ready(), held = deferred(), choice = f.seek.begin(80);
    f.read(() => held.promise);
    const running = f.seek.confirm(choice);
    assert.equal(f.ctl.state.busy, true);
    assert.equal(await f.ctl.command('next'), false);
    assert.equal(await f.seek.confirm(choice), false);
    retire(f.seek); assert.equal(f.ctl.state.busy, true);
    held.resolve(copy(f.rows[0]));
    assert.equal(await running, false); assert.equal(f.ctl.state.busy, false);
    assert.equal(f.writes().length, 0);
  }
});

test('controller suspension or an intervening selected-room event also fences a held preflight', async () => {
  for (const retire of [ctl => ctl.suspend(), ctl => ctl.invalidate(['Synthetic room@S2'])]) {
    const f = await fixture().ready(), held = deferred(), choice = f.seek.begin(80);
    f.read(() => held.promise); const running = f.seek.confirm(choice);
    retire(f.ctl); held.resolve(copy(f.rows[0]));
    assert.equal(await running, false); assert.equal(f.writes().length, 0);
  }
});

test('hiding and reopening cannot resurrect a retained confirmation callback', async () => {
  const f = await fixture().ready(), choice = f.seek.begin(80);
  f.seek.suspend(); f.seek.resume(); f.seek.observe();
  assert.equal(await f.seek.confirm(choice), false);
  const next = f.seek.begin(90); assert.notEqual(next, choice);
  assert.equal(await f.seek.confirm(choice), false); assert.equal(await f.seek.confirm(next), true);
});

test('lost or malformed acknowledgements lock the shared lane against retries until explicit refresh', async () => {
  for (const reply of [() => { throw new Error('Reply lost after application'); }, () => ({ done: 'Applied' }),
    () => ({ done: 'Applied', changed: true, notes: [{}] })]) {
    const f = await fixture().ready(), choice = f.seek.begin(80); f.write(reply);
    assert.equal(await f.seek.confirm(choice), false); assert.equal(f.ctl.state.uncertain, true);
    assert.match(f.ctl.state.message, /Outcome uncertain; no retry was sent/);
    assert.equal(await f.ctl.command('next'), false); assert.equal(await f.seek.confirm(choice), false);
    await f.ctl.refresh(); assert.equal(f.ctl.state.uncertain, true);
    await f.ctl.refresh(true); assert.equal(f.ctl.state.uncertain, false);
    assert.equal(await f.seek.confirm(choice), false); assert.equal(f.writes().length, 1);
  }
});

test('a confirmed command can finish after hiding without a second write or hidden refresh', async () => {
  const f = await fixture().ready(), held = deferred(), choice = f.seek.begin(80);
  f.write(() => held.promise); const running = f.seek.confirm(choice);
  await settled(() => f.writes().length === 1);
  const calls = f.calls.length; f.seek.suspend(); f.ctl.suspend();
  held.resolve({ done: 'Seek accepted', changed: true });
  assert.equal(await running, true); assert.equal(f.calls.length, calls);
  assert.equal(f.ctl.state.uncertain, false); assert.equal(f.seek.state.open, false);
  assert.equal(f.seek.state.writing, false);
});

// The browser host below tests production event handlers and focus semantics.
// It does not stand in for layout or a Chromium rendering check.
function documentFixture() {
  const elements = new Map(), doc = { activeElement: null };
  class Element {
    constructor(id) { this.id = id; this.listeners = {}; this.disabled = false; this.open = false;
      this.isConnected = true; this.hidden = false; this.value = '0'; this.attrs = {}; }
    addEventListener(type, fn) { (this.listeners[type] ||= []).push(fn); }
    emit(type) { const event = { defaultPrevented: false, preventDefault() { this.defaultPrevented = true; } };
      for (const fn of this.listeners[type] || []) fn(event); return event; }
    setAttribute(name, value) { this.attrs[name] = String(value); }
    focus() { doc.activeElement = this; }
    closest() { return null; }
    showModal() { this.open = true; }
    close() { this.open = false; this.emit('close'); }
  }
  doc.getElementById = id => { if (!elements.has(id)) elements.set(id, new Element(id)); return elements.get(id); };
  doc.querySelector = selector => selector === 'dialog[open]' ? [...elements.values()].find(element => element.open) || null : null;
  return doc;
}

test('dock button and dialog keyboard range preview without writes, then submit the exact chosen time', async () => {
  const f = await fixture({ dom: true, state: 'paused' }).ready(), $ = id => f.doc.getElementById(id);
  $('open-seek').focus(); $('open-seek').emit('click');
  assert.equal($('seek-dialog').open, true); assert.equal(f.doc.activeElement.id, 'seek-range');
  assert.match($('seek-note').textContent, /stays paused/);
  $('seek-range').value = '90'; $('seek-range').emit('input'); $('seek-back').emit('click');
  assert.equal(f.seek.state.pending.to_secs, 75); assert.equal(f.writes().length, 0);
  assert.equal($('seek-form').emit('submit').defaultPrevented, true);
  await settled(() => !f.seek.state.writing);
  assert.deepEqual(f.writes(), [{ url: '/seek', body: { zone: 'Synthetic room@S2', to_secs: 75 } }]);
  assert.equal($('seek-dialog').open, false); assert.equal(f.doc.activeElement.id, 'open-seek');
});

test('Cancel and Escape close the position dialog and restore its opener without dispatch', async () => {
  const f = await fixture({ dom: true }).ready(), $ = id => f.doc.getElementById(id);
  for (const event of [() => $('seek-cancel').emit('click'), () => $('seek-dialog').emit('cancel')]) {
    $('open-seek').focus(); $('open-seek').emit('click'); event();
    assert.equal(f.seek.state.pending, null); assert.equal($('seek-dialog').open, false);
    assert.equal(f.doc.activeElement.id, 'open-seek');
  }
  assert.equal(f.writes().length, 0);
});

test('a scrubbed dock position survives a clock paint and becomes only a review on release', async () => {
  const f = await fixture({ dom: true }).ready(), $ = id => f.doc.getElementById(id);
  $('progress').focus(); $('progress').value = '90'; $('progress').emit('input');
  f.time(5000); f.seek.observe(); assert.equal(Number($('progress').value), 90);
  $('progress').emit('change');
  assert.equal(f.seek.state.pending.to_secs, 90); assert.equal(f.writes().length, 0);
  assert.equal($('seek-dialog').open, true);
});

test('a retired scrub gesture cannot create a review for a new group or track', async () => {
  for (const change of [f => f.ctl.select('Synthetic room@S1'), f => f.ctl.invalidate(['Synthetic room@S2'])]) {
    const f = await fixture({ dom: true }).ready(), $ = id => f.doc.getElementById(id);
    $('progress').value = '90'; $('progress').emit('input'); change(f);
    await f.ctl.refresh(); $('progress').emit('change');
    assert.equal(f.seek.state.pending, null); assert.equal($('seek-dialog').open, false);
    assert.equal(f.writes().length, 0);
  }
});

test('unknown duration disables both dock paths and suspension closes an open dialog', async () => {
  const stream = await fixture({ dom: true, track: { duration_secs: null } }).ready();
  for (const id of ['progress', 'open-seek']) assert.equal(stream.doc.getElementById(id).disabled, true);
  stream.doc.getElementById('open-seek').emit('click'); assert.equal(stream.seek.state.open, false);
  const f = await fixture({ dom: true }).ready(); f.doc.getElementById('open-seek').emit('click');
  f.seek.suspend(); assert.equal(f.doc.getElementById('seek-dialog').open, false);
  assert.equal(f.doc.getElementById('open-seek').disabled, true); assert.equal(f.writes().length, 0);
});
