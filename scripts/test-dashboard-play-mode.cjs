'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const Mode = require('../crates/fsonos-api/src/remote/dashboard-play-mode.js');
const copy = value => structuredClone(value);
const deferred = () => { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; };
const settled = async predicate => {
  for (let n = 0; n < 80 && !predicate(); n++) await Promise.resolve();
  assert.ok(predicate(), 'operation reached the expected boundary');
};

function documentFixture() {
  const nodes = new Map();
  const doc = { activeElement: null, getElementById(id) {
    if (!nodes.has(id)) {
      const listeners = new Map();
      nodes.set(id, { id, open: false, disabled: false, textContent: '', value: '',
        options: Object.keys(Mode.MODES).map(value => ({ value, disabled: false })),
        attributes: {}, setAttribute(name, value) { this.attributes[name] = value; },
        addEventListener(name, listener) { listeners.set(name, listener); },
        fire(name, event = {}) { return listeners.get(name)?.({ target: this, preventDefault() {}, ...event }); },
        showModal() { this.open = true; }, close() { this.open = false; this.fire('close'); },
        focus() { doc.activeElement = this; } });
    }
    return nodes.get(id);
  } };
  return doc;
}
function fixture({ mode = 'NORMAL', queue = true, dj = false, demo = false, dom = false } = {}) {
  const zones = ['S2', 'S1'].map(household => ({ household, coordinator_room: 'Synthetic room',
    members: ['Synthetic room'], transport_state: 'playing' }));
  const rows = zones.map(zone => ({ zone, transport_state: 'playing', volume: 20,
    track: { uri: 'fixture:track', title: 'Synthetic song', queue_position: 2, position_secs: 30, duration_secs: 180 } }));
  const modes = zones.map(zone => ({ zone: zone.coordinator_room, mode, queue_active: queue, dj_active: dj }));
  let time = 1000, reader = null, writer = null, playMode;
  const calls = [], doc = dom ? documentFixture() : null;
  const normalRead = url => {
    if (url === '/zones') return copy(zones);
    const index = zones.findIndex(zone => url.startsWith('/zones/' + encodeURIComponent(C.key(zone)) + '/'));
    if (index < 0) throw new Error('Unexpected fixture read: ' + url);
    if (url.endsWith('/state')) return copy(rows[index]);
    if (url.endsWith('/play-mode')) return copy(modes[index]);
    throw new Error('Unexpected fixture read: ' + url);
  };
  const call = async (url, body, options) => {
    calls.push({ url, body, options });
    if (body !== undefined) {
      if (writer) return writer(url, body);
      if (url === '/play-mode') {
        const index = zones.findIndex(zone => C.key(zone) === body.zone);
        assert.ok(index >= 0);
        assert.equal(body.expected_mode, modes[index].mode);
        modes[index].mode = body.mode;
        return { done: 'Playback order changed', changed: true, recovered: false, state: copy(modes[index]) };
      }
      return { done: 'Transport changed', changed: true };
    }
    return reader ? reader(url, options, normalRead) : normalRead(url);
  };
  const ctl = C.model({ call, demo, now: () => time, changed: () => playMode?.observe() });
  const options = { call, controller: ctl, now: () => time };
  playMode = dom ? Mode.panel({ ...options, document: doc }) : Mode.model(options);
  return { ctl, playMode, rows, zones, modes, calls, doc, normalRead,
    writes: () => calls.filter(row => row.body !== undefined).map(({ url, body }) => ({ url, body })),
    read(fn) { reader = fn; }, write(fn) { writer = fn; }, time(value) { time = value; },
    async ready() { assert.equal(await ctl.refresh(), true); ctl.connection(true); return this; } };
}

test('playback order is read lazily for the household-qualified selected group', async () => {
  const f = await fixture().ready();
  assert.equal(f.calls.some(row => row.url.endsWith('/play-mode')), false);
  assert.equal(await f.playMode.open(), true);
  assert.equal(f.calls.at(-1).url, '/zones/Synthetic%20room%40S2/play-mode');
  assert.equal(f.playMode.state.current.mode, 'NORMAL');
  assert.equal(f.playMode.choose('SHUFFLE'), true);
  assert.ok(Object.isFrozen(f.playMode.state.pending));
  assert.equal(f.writes().length, 0);
});

test('every native order mode uses one conditional command without restarting playback', async () => {
  for (const mode of Object.keys(Mode.MODES).filter(value => value !== 'NORMAL')) {
    const f = await fixture().ready(); await f.playMode.open();
    assert.equal(f.playMode.choose(mode), true);
    const before = f.calls.length;
    assert.equal(await f.playMode.apply(), true, mode);
    assert.deepEqual(f.writes(), [{ url: '/play-mode', body: {
      zone: 'Synthetic room@S2', mode, expected_mode: 'NORMAL',
    } }]);
    assert.deepEqual(f.calls.slice(before).map(row => row.url), [
      '/zones/Synthetic%20room%40S2/state', '/zones/Synthetic%20room%40S2/play-mode',
      '/play-mode', '/zones/Synthetic%20room%40S2/state']);
    assert.equal(f.playMode.state.result.mode, mode);
    assert.equal(f.playMode.state.current, null);
    assert.equal(await f.playMode.apply(), false);
  }
});

test('unknown values and unchanged selections cannot dispatch', async () => {
  const f = await fixture().ready(); await f.playMode.open();
  for (const value of ['__proto__', 'constructor', 'SHUFFLE_ALL', '', null]) {
    assert.equal(f.playMode.choose(value), false);
  }
  assert.equal(f.playMode.choose('NORMAL'), true);
  assert.equal(f.playMode.canApply(), false);
  assert.equal(await f.playMode.apply(), false);
  assert.equal(f.writes().length, 0);
});

test('non-queue sources stay read-only and DJ ownership permits only normal-order repair', async () => {
  for (const options of [{ queue: false }, { dj: true }, { demo: true }]) {
    const f = await fixture(options).ready(); await f.playMode.open();
    assert.equal(f.playMode.choose('SHUFFLE'), false);
    assert.equal(await f.playMode.apply(), false);
    assert.equal(f.writes().length, 0);
  }
  const f = await fixture({ mode: 'SHUFFLE', dj: true }).ready(); await f.playMode.open();
  assert.equal(f.playMode.choose('NORMAL'), true);
  assert.equal(await f.playMode.apply(), true);
  assert.deepEqual(f.writes()[0].body, { zone: 'Synthetic room@S2', mode: 'NORMAL', expected_mode: 'SHUFFLE' });
});

test('the reviewed existing mode is the native expected_mode precondition', async () => {
  const f = await fixture({ mode: 'REPEAT_ALL' }).ready(); await f.playMode.open();
  f.playMode.choose('SHUFFLE_NOREPEAT');
  assert.equal(await f.playMode.apply(), true);
  assert.equal(f.writes()[0].body.expected_mode, 'REPEAT_ALL');
});

test('preflight rejects a changed mode, queue source or DJ owner without a write', async () => {
  for (const change of [{ mode: 'REPEAT_ONE' }, { queue_active: false }, { dj_active: true }]) {
    const f = await fixture().ready(); await f.playMode.open(); f.playMode.choose('SHUFFLE');
    Object.assign(f.modes[0], change);
    assert.equal(await f.playMode.apply(), false);
    assert.equal(f.writes().length, 0);
    assert.equal(f.ctl.state.uncertain, false);
    assert.match(f.ctl.state.message, /No command was sent/);
  }
});

test('preflight rejects changed group membership or a substituted household', async () => {
  for (const change of [row => row.zone.members.push('Synthetic guest'), row => { row.zone.household = 'S1'; }]) {
    const f = await fixture().ready(); await f.playMode.open(); f.playMode.choose('SHUFFLE');
    change(f.rows[0]);
    assert.equal(await f.playMode.apply(), false);
    assert.equal(f.writes().length, 0);
  }
});

test('a retained review cannot replace a more recent selection', async () => {
  const f = await fixture().ready(); await f.playMode.open(); f.playMode.choose('SHUFFLE');
  const old = f.playMode.state.pending;
  f.playMode.choose('REPEAT_ONE');
  assert.equal(await f.playMode.apply(old), false);
  assert.equal(f.writes().length, 0);
  assert.equal(await f.playMode.apply(), true);
  assert.equal(f.writes()[0].body.mode, 'REPEAT_ONE');
});

test('switching identically named rooms in another household retires the old observation', async () => {
  const f = await fixture().ready(); await f.playMode.open(); f.playMode.choose('SHUFFLE');
  const old = f.playMode.state.pending;
  assert.equal(f.ctl.select('Synthetic room@S1'), true);
  assert.equal(f.playMode.state.current, null);
  assert.equal(await f.playMode.apply(old), false);
  assert.equal(await f.playMode.read(), true);
  f.playMode.choose('REPEAT_ALL');
  assert.equal(await f.playMode.apply(), true);
  assert.equal(f.writes()[0].body.zone, 'Synthetic room@S1');
});

test('expired observations require another explicit read', async () => {
  const f = await fixture().ready(); await f.playMode.open(); f.playMode.choose('SHUFFLE');
  f.time(1000 + Mode.MAX_AGE); f.playMode.observe();
  assert.equal(f.playMode.state.current, null);
  assert.equal(await f.playMode.apply(), false);
  assert.equal(f.writes().length, 0);
  assert.equal(await f.playMode.read(), true);
  assert.equal(f.playMode.choose('SHUFFLE'), true);
});

test('closing, hiding, invalidating or disposing during preflight prevents dispatch', async () => {
  for (const retire of [f => f.playMode.close(), f => f.playMode.suspend(),
    f => f.ctl.invalidate(['Synthetic room@S2']), f => f.playMode.dispose()]) {
    const f = await fixture().ready(); await f.playMode.open(); f.playMode.choose('SHUFFLE');
    const gate = deferred(); let waiting = false;
    f.read((url, options, normal) => {
      if (url.endsWith('/state')) { waiting = true; return gate.promise; }
      return normal(url);
    });
    const pending = f.playMode.apply();
    await settled(() => waiting); retire(f); gate.resolve(copy(f.rows[0]));
    assert.equal(await pending, false);
    assert.equal(f.writes().length, 0);
    assert.equal(f.ctl.state.uncertain, false);
  }
});

test('closing an in-flight mode read aborts it and discards a late body', async () => {
  const f = await fixture().ready(), gate = deferred(); let signal;
  f.read((url, options, normal) => {
    if (url.endsWith('/play-mode')) { signal = options.signal; return gate.promise; }
    return normal(url);
  });
  const pending = f.playMode.open(); await settled(() => !!signal);
  f.playMode.close(); assert.equal(signal.aborted, true);
  gate.resolve(copy(f.modes[0]));
  assert.equal(await pending, false);
  assert.equal(f.playMode.state.open, false);
  assert.equal(f.playMode.state.current, null);
  assert.equal(f.playMode.state.loading, false);
});

test('malformed or future mode responses stay read-only without automatic retries', async () => {
  for (const value of [null, {}, { mode: 'NORMAL' }, { zone: 'Synthetic room', mode: 'FUTURE',
    queue_active: true, dj_active: false }, { zone: 'Other room', mode: 'NORMAL', queue_active: true, dj_active: false }]) {
    const f = await fixture().ready();
    f.read((url, options, normal) => url.endsWith('/play-mode') ? value : normal(url));
    assert.equal(await f.playMode.open(), false);
    for (let n = 0; n < 5; n++) f.playMode.observe();
    assert.equal(f.calls.filter(row => row.url.endsWith('/play-mode')).length, 1);
    assert.equal(f.playMode.choose('SHUFFLE'), false);
    assert.equal(f.writes().length, 0);
  }
});

test('mode changes share the global command lane and cannot double-submit', async () => {
  const f = await fixture().ready(); await f.playMode.open(); f.playMode.choose('SHUFFLE');
  const gate = deferred(); let sent = false;
  f.write(() => { sent = true; return gate.promise; });
  const pending = f.playMode.apply(); await settled(() => sent);
  assert.equal(await f.playMode.apply(), false);
  assert.equal(await f.ctl.command('next'), false);
  gate.resolve({ done: 'Playback order changed', changed: true, recovered: false,
    state: { ...f.modes[0], mode: 'SHUFFLE' } });
  assert.equal(await pending, true);
  assert.equal(f.writes().length, 1);
});

test('lost or malformed write acknowledgements keep uncertainty until an explicit full refresh', async () => {
  for (const reply of [null, { done: 'Changed', changed: true, recovered: false,
    state: { zone: 'Synthetic room', mode: 'REPEAT_ONE', queue_active: true, dj_active: false } }]) {
    const f = await fixture().ready(); await f.playMode.open(); f.playMode.choose('SHUFFLE');
    f.write(() => { if (reply === null) throw new Error('Lost acknowledgement'); return reply; });
    assert.equal(await f.playMode.apply(), false);
    assert.equal(f.ctl.state.uncertain, true);
    assert.equal(f.playMode.state.result, null);
    assert.equal(f.writes().length, 1);
    await f.ctl.refresh(); await f.playMode.read();
    assert.equal(f.ctl.state.uncertain, true);
    assert.equal(f.playMode.choose('REPEAT_ALL'), false);
    assert.equal(await f.ctl.refresh(true), true);
    assert.equal(f.ctl.state.uncertain, false);
  }
});

test('a malformed shared notes acknowledgement cannot publish a successful mode result', async () => {
  const f = await fixture().ready(); await f.playMode.open(); f.playMode.choose('SHUFFLE');
  f.write(() => ({ done: 'Changed', changed: true, recovered: false, notes: 'invalid notes',
    state: { ...f.modes[0], mode: 'SHUFFLE' } }));
  assert.equal(await f.playMode.apply(), false);
  assert.equal(f.ctl.state.uncertain, true);
  assert.equal(f.playMode.state.result, null);
  assert.match(f.playMode.state.message, /Outcome uncertain/);
  assert.equal(f.writes().length, 1);
});

test('an accepted sent command is recorded after closing without replaying it', async () => {
  const f = await fixture().ready(); await f.playMode.open(); f.playMode.choose('SHUFFLE');
  const gate = deferred(); let sent = false;
  f.write(() => { sent = true; return gate.promise; });
  const pending = f.playMode.apply(); await settled(() => sent);
  f.playMode.close();
  gate.resolve({ done: 'Playback order changed', changed: true, recovered: true,
    state: { ...f.modes[0], mode: 'SHUFFLE' } });
  assert.equal(await pending, true);
  assert.equal(f.playMode.state.open, false);
  assert.equal(f.playMode.state.result.mode, 'SHUFFLE');
  assert.equal(f.playMode.state.result.recovered, true);
  assert.equal(await f.playMode.apply(), false);
  assert.equal(f.writes().length, 1);
});

test('the panel reads, renders, confirms and restores keyboard focus through its real handlers', async () => {
  const f = await fixture({ dom: true }).ready(), $ = id => f.doc.getElementById(id);
  $('open-play-mode').focus(); $('open-play-mode').fire('click');
  await settled(() => f.playMode.state.current !== null);
  assert.equal($('play-mode-dialog').open, true);
  assert.match($('play-mode-target').textContent, /S2/);
  assert.match($('play-mode-current').textContent, /In order/);
  assert.equal($('play-mode-apply').disabled, true);
  $('play-mode-choice').value = 'SHUFFLE'; $('play-mode-choice').fire('change');
  assert.equal($('play-mode-apply').disabled, false);
  $('play-mode-apply').fire('click');
  await settled(() => !f.playMode.state.writing && f.writes().length === 1);
  assert.match($('play-mode-message').textContent, /Playback order changed/);
  assert.equal($('play-mode-apply').disabled, true);
  $('close-play-mode').fire('click');
  assert.equal($('play-mode-dialog').open, false);
  assert.equal(f.doc.activeElement, $('open-play-mode'));
});

test('the panel disables all DJ-owned shuffle and repeat options and all non-queue changes', async () => {
  const f = await fixture({ dom: true, dj: true }).ready(), $ = id => f.doc.getElementById(id);
  await f.playMode.open();
  assert.ok($('play-mode-choice').options.filter(option => option.value !== 'NORMAL').every(option => option.disabled));
  assert.match($('play-mode-description').textContent, /whole works/);
  f.modes[0].queue_active = false; await f.playMode.read();
  assert.equal($('play-mode-choice').disabled, true);
  assert.match($('play-mode-description').textContent, /Another source/);
  assert.equal(f.writes().length, 0);
});

test('dialog cancellation retires the chosen mode without writing', async () => {
  const f = await fixture({ dom: true }).ready(); await f.playMode.open();
  f.playMode.choose('SHUFFLE'); let prevented = false;
  f.doc.getElementById('play-mode-dialog').fire('cancel', { preventDefault() { prevented = true; } });
  assert.equal(prevented, true);
  assert.equal(f.playMode.state.open, false);
  assert.equal(await f.playMode.apply(), false);
  assert.equal(f.writes().length, 0);
});
