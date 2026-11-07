'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const Sleep = require('../crates/fsonos-api/src/remote/dashboard-sleep.js');
const T = require('../crates/fsonos-api/src/remote/timers.js');
const copy = value => structuredClone(value);
const deferred = () => { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; };
const settled = async predicate => {
  for (let n = 0; n < 160 && !predicate(); n++) await Promise.resolve();
  assert.ok(predicate(), 'operation reached the expected boundary');
};
const receipt = (remaining = 2700, extra = {}) => ({ zone: 'Synthetic room',
  ends: '2030-01-01T01:00:00Z', remaining_secs: remaining, fades: true, fading: false,
  done: 'Sleep timer changed', changed: true, ...extra });

function documentFixture() {
  const nodes = new Map();
  const doc = { activeElement: null, getElementById(id) {
    if (!nodes.has(id)) {
      const listeners = new Map();
      nodes.set(id, { id, open: false, hidden: false, disabled: false, textContent: '', value: '',
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
function fixture({ timer = null, demo = false, dom = false } = {}) {
  const zones = ['S2', 'S1'].map(household => ({ household, coordinator_room: 'Synthetic room',
    members: ['Synthetic room', 'Synthetic companion'], transport_state: 'playing' }));
  const rows = zones.map(zone => ({ zone, transport_state: 'playing', volume: 20,
    track: { uri: 'fixture:track', title: 'Synthetic song', queue_position: 2, position_secs: 30, duration_secs: 180 } }));
  const timers = [copy(timer), null], calls = [], doc = dom ? documentFixture() : null;
  let time = 1000, reader = null, writer = null, sleep;
  const normalRead = url => {
    if (url === '/zones') return copy(zones);
    if (url.startsWith('/sleep?zone=')) {
      const target = decodeURIComponent(url.slice('/sleep?zone='.length));
      const index = zones.findIndex(zone => C.key(zone) === target);
      assert.ok(index >= 0); return timers[index] ? [copy(timers[index])] : [];
    }
    const index = zones.findIndex(zone => url === '/zones/' + encodeURIComponent(C.key(zone)) + '/state');
    if (index >= 0) return copy(rows[index]);
    throw new Error('Unexpected fixture read: ' + url);
  };
  const normalWrite = (url, body) => {
    if (url !== '/sleep') return { done: 'Transport changed', changed: true };
    const index = zones.findIndex(zone => C.key(zone) === body.zone);
    assert.ok(index >= 0);
    if (body.cancel) {
      timers[index] = null;
      return receipt(null, { ends: null, remaining_secs: null, done: 'Sleep timer cancelled' });
    }
    const seconds = Number(body.duration.slice(0, -1)) * 60 + (body.extend ? timers[index].remaining_secs : 0);
    timers[index] = receipt(seconds, { ends: new Date(Date.parse('2030-01-01T00:00:00Z') + seconds * 1000).toISOString(),
      done: body.extend ? 'Sleep timer extended' : 'Sleep timer set' });
    return copy(timers[index]);
  };
  const call = async (url, body, options) => {
    calls.push({ url, body, options });
    if (body !== undefined) return writer ? writer(url, body, normalWrite) : normalWrite(url, body);
    return reader ? reader(url, options, normalRead) : normalRead(url);
  };
  const ctl = C.model({ call, demo, now: () => time, changed: () => sleep?.observe() });
  const options = { call, controller: ctl, now: () => time };
  sleep = dom ? Sleep.panel({ ...options, document: doc }) : Sleep.model(options);
  return { ctl, sleep, rows, zones, timers, calls, doc, normalRead, normalWrite,
    writes: () => calls.filter(row => row.body !== undefined).map(({ url, body }) => ({ url, body })),
    read(fn) { reader = fn; }, write(fn) { writer = fn; }, time(value) { time = value; },
    async ready() { assert.equal(await ctl.refresh(), true); ctl.connection(true); return this; } };
}

test('sleep timers are lazy and read the exact household-qualified group with topology on both sides', async () => {
  const f = await fixture().ready();
  assert.equal(f.calls.some(row => row.url.startsWith('/sleep')), false);
  const before = f.calls.length;
  assert.equal(await f.sleep.open(), true);
  assert.deepEqual(f.calls.slice(before).map(row => row.url), ['/zones', '/sleep?zone=Synthetic%20room%40S2', '/zones']);
  assert.equal(f.sleep.state.loaded, true); assert.equal(f.sleep.state.timer, null);
  assert.match(f.sleep.state.label, /Synthetic companion.*S2/);
  assert.equal(f.writes().length, 0);
});

test('set, extend and cancel use the native request shapes and refresh the current timer once', async () => {
  for (const action of ['set', 'extend', 'cancel']) {
    const f = await fixture({ timer: action === 'set' ? null : receipt() }).ready();
    await f.sleep.open(); f.sleep.minutes('15');
    const pending = f.sleep.review(action);
    assert.ok(Object.isFrozen(pending)); assert.ok(Object.isFrozen(pending.body));
    assert.equal(f.writes().length, 0);
    const before = f.calls.length;
    assert.equal(await f.sleep.apply(), true, action);
    assert.deepEqual(f.writes(), [{ url: '/sleep', body: action === 'cancel'
      ? { zone: 'Synthetic room@S2', cancel: true }
      : { zone: 'Synthetic room@S2', duration: '15m', ...(action === 'extend' ? { extend: true } : {}) } }]);
    assert.deepEqual(f.calls.slice(before).map(row => row.url), [
      '/zones', '/sleep?zone=Synthetic%20room%40S2', '/zones', '/sleep',
      '/zones/Synthetic%20room%40S2/state', '/zones', '/sleep?zone=Synthetic%20room%40S2', '/zones']);
    assert.equal(f.sleep.state.result.action, action);
    assert.equal(f.sleep.state.stale, false);
    assert.equal(f.sleep.remaining(), action === 'cancel' ? null : action === 'extend' ? 3600 : 900);
    assert.equal(await f.sleep.apply(), false);
  }
});

test('setting an existing timer explicitly reviews replacement and cancellation does not resume music', async () => {
  const f = await fixture({ timer: receipt(45, { fading: true }) }).ready(); await f.sleep.open();
  assert.match(f.sleep.review('set').detail, /Replace the existing timer/);
  assert.match(f.sleep.review('cancel').detail, /does not restart music/);
  assert.equal(await f.sleep.apply(), true);
  assert.deepEqual(f.writes().map(row => row.url), ['/sleep']);
});

test('duration choices are bounded whole minutes and empty timers cannot be extended or cancelled', async () => {
  const f = await fixture().ready(); await f.sleep.open();
  for (const input of ['0', '-1', '1.5', '1e2', '1381', '9007199254740992', '', '1h', null]) {
    assert.equal(Sleep.minutesOf(input), null);
    if (input !== null) { f.sleep.minutes(input); assert.equal(f.sleep.review('set'), null); }
  }
  for (const input of ['1', '45', '1380', ' 15 ']) assert.ok(Sleep.minutesOf(input));
  f.sleep.minutes('1380'); assert.ok(f.sleep.review('set'));
  assert.equal(f.sleep.review('extend'), null); assert.equal(f.sleep.review('cancel'), null);
  assert.equal(f.sleep.review('reset'), null); assert.equal(f.writes().length, 0);
});

test('extension ceiling is checked again against the freshly reported remaining time', async () => {
  const f = await fixture({ timer: receipt(23 * 3600 - 600) }).ready(); await f.sleep.open();
  f.sleep.minutes('15'); assert.equal(f.sleep.canReview('extend'), false);
  f.sleep.minutes('10'); assert.ok(f.sleep.review('extend'));
  f.timers[0].remaining_secs += 1;
  assert.equal(await f.sleep.apply(), false); assert.equal(f.writes().length, 0);
  assert.match(f.ctl.state.message, /exceed 23 hours/);
});

test('countdown uses reported remaining time and monotonic elapsed time without polling or claiming playback stopped', async () => {
  const f = await fixture({ timer: receipt(90) }).ready(); await f.sleep.open();
  const before = f.calls.length;
  f.time(31000); f.sleep.observe(); assert.equal(f.sleep.remaining(), 60);
  f.time(101000); f.sleep.observe(); assert.equal(f.sleep.remaining(), 0);
  assert.equal(f.sleep.state.stale, true); assert.ok(f.sleep.state.timer);
  assert.equal(f.calls.length, before); assert.equal(f.writes().length, 0);
  assert.equal(f.rows[0].transport_state, 'playing');
});

test('aged timer observations remain visible but cannot authorize a change until explicitly read again', async () => {
  const f = await fixture({ timer: receipt() }).ready(); await f.sleep.open(); f.sleep.review('cancel');
  f.time(1000 + Sleep.MAX_AGE); f.sleep.observe();
  assert.ok(f.sleep.state.timer); assert.equal(f.sleep.state.stale, true); assert.equal(f.sleep.state.pending, null);
  assert.equal(await f.sleep.apply(), false); assert.equal(f.sleep.review('cancel'), null);
  assert.equal(await f.sleep.read(), true); assert.ok(f.sleep.review('cancel'));
});

test('a delayed refresh is not cancelled by the age of the previous observation', async () => {
  const f = await fixture({ timer: receipt() }).ready(); await f.sleep.open();
  f.time(1000 + Sleep.MAX_AGE); f.sleep.observe();
  const gate = deferred(); let waiting = false;
  f.read((url, options, normal) => {
    if (url.startsWith('/sleep')) { waiting = true; return gate.promise; }
    return normal(url);
  });
  const read = f.sleep.read(); await settled(() => waiting); f.sleep.observe();
  gate.resolve([receipt(2600)]); assert.equal(await read, true); assert.equal(f.sleep.remaining(), 2600);
});

test('preflight rejects a replaced, missing, appeared, differently managed or newly fading timer', async () => {
  for (const change of [f => { f.timers[0].ends = '2030-01-01T01:00:01Z'; }, f => { f.timers[0] = null; },
    f => { f.timers[0].fades = false; }, f => { f.timers[0].fading = true; }]) {
    const f = await fixture({ timer: receipt() }).ready(); await f.sleep.open(); f.sleep.review('cancel'); change(f);
    assert.equal(await f.sleep.apply(), false); assert.equal(f.writes().length, 0); assert.equal(f.ctl.state.uncertain, false);
  }
  const f = await fixture().ready(); await f.sleep.open(); f.sleep.review('set'); f.timers[0] = receipt();
  assert.equal(await f.sleep.apply(), false); assert.equal(f.writes().length, 0);
});

test('normal countdown and up to two seconds of speaker-only deadline rounding do not invalidate a review', async () => {
  for (const [fades, jitter] of [[true, 0], [false, 2000]]) {
    const f = await fixture({ timer: receipt(2700, { fades }) }).ready(); await f.sleep.open(); f.sleep.review('cancel');
    f.timers[0].remaining_secs -= 10;
    f.timers[0].ends = new Date(Date.parse(f.timers[0].ends) + jitter).toISOString();
    assert.equal(await f.sleep.apply(), true);
  }
  assert.equal(T.sameTimer(T.timerOf(receipt(50, { fades: false }), { coordinator: 'Synthetic room' }),
    T.timerOf(receipt(50, { fades: false, ends: '2030-01-01T01:00:03Z' }), { coordinator: 'Synthetic room' })), false);
});

test('an extension can adopt a speaker backstop into a daemon timer without assuming a deadline formula', async () => {
  const f = await fixture({ timer: receipt(3600, { fades: false }) }).ready(); await f.sleep.open();
  f.sleep.minutes('15'); f.sleep.review('extend');
  f.write(() => receipt(4200, { fades: true, ends: '2030-01-01T01:10:00Z' }));
  assert.equal(await f.sleep.apply(), true); assert.equal(f.ctl.state.uncertain, false);
});

test('editing a duration, going back or choosing another action retires the exact captured review', async () => {
  for (const edit of [f => f.sleep.minutes('30'), f => f.sleep.cancelReview(), f => f.sleep.review('extend')]) {
    const f = await fixture({ timer: receipt() }).ready(); await f.sleep.open(); const old = f.sleep.review('set'); edit(f);
    assert.equal(await f.sleep.apply(old), false); assert.equal(f.writes().length, 0);
  }
});

test('group membership, coordinator or household changes are rejected before any sleep write', async () => {
  for (const change of [f => f.zones[0].members.push('Synthetic guest'), f => { f.zones[0].coordinator_room = 'Synthetic companion'; },
    f => { f.zones[0].household = 'Other'; }]) {
    const f = await fixture({ timer: receipt() }).ready(); await f.sleep.open(); f.sleep.review('cancel'); change(f);
    assert.equal(await f.sleep.apply(), false); assert.equal(f.writes().length, 0);
  }
});

test('topology changed during the timer read is rejected on both initial read and preflight', async () => {
  for (const writing of [false, true]) {
    const f = await fixture({ timer: receipt() }).ready();
    if (writing) { await f.sleep.open(); f.sleep.review('cancel'); }
    f.read((url, options, normal) => {
      const value = normal(url); if (url.startsWith('/sleep')) f.zones[0].members.push('Synthetic guest'); return value;
    });
    assert.equal(await (writing ? f.sleep.apply() : f.sleep.open()), false);
    assert.equal(f.writes().length, 0);
    assert.equal(f.sleep.state.stale, true);
  }
});

test('a timer review that expires during preflight cannot dispatch without a clock render', async () => {
  const f = await fixture({ timer: receipt() }).ready(); await f.sleep.open(); f.sleep.review('cancel');
  f.read((url, options, normal) => { if (url.startsWith('/sleep')) f.time(1000 + Sleep.MAX_AGE); return normal(url); });
  assert.equal(await f.sleep.apply(), false); assert.equal(f.writes().length, 0); assert.equal(f.ctl.state.uncertain, false);
});

test('switching identically named groups in another household discards old timer authority and display', async () => {
  const f = await fixture({ timer: receipt() }).ready(); await f.sleep.open(); const old = f.sleep.review('cancel');
  f.ctl.select('Synthetic room@S1');
  assert.equal(f.sleep.state.timer, null); assert.equal(f.sleep.state.loaded, false);
  assert.equal(await f.sleep.apply(old), false); assert.equal(await f.sleep.read(), true);
  f.sleep.review('set'); assert.equal(await f.sleep.apply(), true);
  assert.equal(f.writes()[0].body.zone, 'Synthetic room@S1');
});

test('closing, hiding, invalidation and disposal during preflight prevent dispatch', async () => {
  for (const retire of [f => f.sleep.close(), f => f.sleep.suspend(), f => f.ctl.invalidate(),
    f => f.sleep.dispose()]) {
    const f = await fixture({ timer: receipt() }).ready(); await f.sleep.open(); f.sleep.review('cancel');
    const gate = deferred(); let waiting = false;
    f.read((url, options, normal) => {
      if (url.startsWith('/sleep')) { waiting = true; return gate.promise; }
      return normal(url);
    });
    const applying = f.sleep.apply(); await settled(() => waiting); retire(f); gate.resolve([receipt()]);
    assert.equal(await applying, false); assert.equal(f.writes().length, 0); assert.equal(f.ctl.state.uncertain, false);
  }
});

test('obsolete read-only requests are aborted and a late response cannot re-open a closed dialog', async () => {
  const f = await fixture().ready(), gate = deferred(); let signal;
  f.read((url, options, normal) => {
    if (url.startsWith('/sleep')) { signal = options.signal; return gate.promise; }
    return normal(url);
  });
  const reading = f.sleep.open(); await settled(() => !!signal); f.sleep.close();
  assert.equal(signal.aborted, true); gate.resolve([receipt()]);
  assert.equal(await reading, false); assert.equal(f.sleep.state.open, false); assert.equal(f.sleep.state.timer, null);
});

test('malformed timer observations fail closed without polling or retries', async () => {
  for (const value of [null, {}, [receipt(), receipt()], [receipt(1, { zone: 'Other room' })],
    [receipt(-1)], [receipt(10, { ends: '2030-01-01T01:00:00' })], [receipt(10, { fades: false, fading: true })],
    [receipt(null, { ends: null, fading: true })], [receipt(10, { changed: null })]]) {
    const f = await fixture().ready(); f.read((url, options, normal) => url.startsWith('/sleep') ? value : normal(url));
    assert.equal(await f.sleep.open(), false);
    for (let n = 0; n < 4; n++) f.sleep.observe();
    assert.equal(f.sleep.review('set'), null); assert.equal(f.writes().length, 0);
    assert.equal(f.calls.filter(row => row.url.startsWith('/sleep')).length, 1);
  }
});

test('all timer commands share the global lane and a second click or transport command cannot overlap', async () => {
  const f = await fixture().ready(); await f.sleep.open(); f.sleep.review('set');
  const gate = deferred(); let sent = false;
  f.write(() => { sent = true; return gate.promise; });
  const applying = f.sleep.apply(); await settled(() => sent);
  assert.equal(await f.sleep.apply(), false); assert.equal(await f.ctl.command('next'), false);
  assert.equal(f.ctl.select('Synthetic room@S1'), false);
  assert.equal(await f.sleep.read(), false); gate.resolve(receipt());
  assert.equal(await applying, true); assert.equal(f.writes().length, 1);
});

test('lost and malformed sent acknowledgements preserve global uncertainty across reads and lifecycle changes', async () => {
  for (const value of [null, receipt(2700, { notes: 'not notes' }), receipt(2700, { changed: false }),
    receipt(2700, { fading: true }), receipt(1), receipt(null, { ends: null })]) {
    const f = await fixture().ready(); await f.sleep.open(); f.sleep.review('set');
    f.write(() => { if (value === null) throw new Error('Lost acknowledgement'); return value; });
    assert.equal(await f.sleep.apply(), false); assert.equal(f.ctl.state.uncertain, true);
    assert.equal(f.sleep.state.result, null); assert.equal(f.writes().length, 1);
    f.sleep.suspend(); f.sleep.resume(); await f.ctl.refresh(); await f.sleep.open();
    assert.equal(f.ctl.state.uncertain, true); assert.equal(f.sleep.review('set'), null);
    assert.equal(await f.ctl.refresh(true), true); assert.equal(f.ctl.state.uncertain, false);
  }
});

test('a sent accepted command is recorded after close without reopening, re-reading or resending it', async () => {
  const f = await fixture().ready(); await f.sleep.open(); f.sleep.review('set');
  const gate = deferred(); let sent = false;
  f.write(() => { sent = true; return gate.promise; });
  const applying = f.sleep.apply(); await settled(() => sent); f.sleep.close(); gate.resolve(receipt());
  assert.equal(await applying, true); assert.equal(f.sleep.state.open, false);
  assert.equal(f.sleep.state.result.action, 'set'); assert.equal(f.sleep.state.timer, null);
  assert.equal(f.calls.filter(row => row.url.startsWith('/sleep?')).length, 2);
  assert.equal(f.writes().length, 1);
});

test('a failed read after acceptance retains the accepted result and never repeats the write', async () => {
  const f = await fixture().ready(); await f.sleep.open(); f.sleep.review('set');
  f.write((url, body, normal) => {
    const result = normal(url, body);
    f.read((url, options, normal) => { if (url.startsWith('/sleep')) throw new Error('Timer read unavailable'); return normal(url); });
    return result;
  });
  assert.equal(await f.sleep.apply(), true); assert.equal(f.sleep.state.result.action, 'set');
  assert.match(f.sleep.state.message, /Sleep timer set.*Timer read unavailable/);
  assert.equal(f.sleep.state.stale, true); assert.equal(f.ctl.state.uncertain, false); assert.equal(f.writes().length, 1);
});

test('demo and stale topology can inspect no actionable timer changes', async () => {
  const f = await fixture({ demo: true, timer: receipt() }).ready(); await f.sleep.open();
  for (const action of ['set', 'extend', 'cancel']) assert.equal(f.sleep.review(action), null);
  assert.equal(f.writes().length, 0);
  const g = await fixture().ready(); await g.sleep.open(); g.ctl.state.topologyFresh = false;
  assert.equal(g.sleep.review('set'), null); assert.equal(await g.sleep.read(), false);
});

test('the real panel handlers select presets, review effects, confirm and restore keyboard focus', async () => {
  const f = await fixture({ dom: true }).ready(), $ = id => f.doc.getElementById(id);
  $('open-sleep').focus(); $('open-sleep').fire('click'); await settled(() => f.sleep.state.loaded);
  assert.equal($('sleep-dialog').open, true); assert.match($('sleep-target').textContent, /Synthetic companion.*S2/);
  assert.match($('sleep-current').textContent, /No active/); assert.equal($('sleep-extend').disabled, true);
  $('sleep-preset-30').fire('click'); assert.equal($('sleep-minutes').value, '30');
  $('sleep-set').focus(); $('sleep-set').fire('click');
  assert.equal($('sleep-preview').hidden, false); assert.equal(f.doc.activeElement, $('sleep-preview'));
  assert.match($('sleep-preview-title').textContent, /30 minutes/); assert.match($('sleep-preview-detail').textContent, /whole group/);
  $('sleep-back').fire('click'); assert.equal($('sleep-preview').hidden, true); assert.equal(f.doc.activeElement, $('sleep-set'));
  $('sleep-set').fire('click'); $('sleep-confirm').fire('click');
  await settled(() => !f.sleep.state.writing && !f.sleep.state.loading && f.writes().length === 1);
  assert.match($('sleep-message').textContent, /Sleep timer set/); assert.match($('sleep-countdown').textContent, /30:00/);
  assert.match($('sleep-deadline').textContent, /your time/); assert.equal($('sleep-extend').disabled, false);
  $('close-sleep').fire('click'); assert.equal($('sleep-dialog').open, false); assert.equal(f.doc.activeElement, $('open-sleep'));
});

test('the panel exposes fade mode, due-now and stale observations without implying a local pause', async () => {
  const f = await fixture({ dom: true, timer: receipt(30, { fading: true }) }).ready(), $ = id => f.doc.getElementById(id);
  await f.sleep.open(); assert.match($('sleep-current').textContent, /Fading now/); assert.match($('sleep-fade').textContent, /daemon fades/);
  f.time(100000); f.sleep.observe();
  assert.match($('sleep-countdown').textContent, /Due now/); assert.match($('sleep-freshness').textContent, /Last observation/);
  assert.equal($('sleep-cancel').disabled, true);
  f.timers[0] = receipt(300, { fades: false }); await f.sleep.read();
  assert.match($('sleep-fade').textContent, /without a fade/);
  $('sleep-minutes').value = '2.5'; $('sleep-minutes').fire('input');
  assert.equal($('sleep-set').disabled, true); assert.match($('sleep-duration-help').textContent, /whole minutes/);
  assert.equal($('sleep-cancel').disabled, false);
  let prevented = false;
  $('sleep-dialog').fire('cancel', { preventDefault() { prevented = true; } });
  assert.equal(prevented, true); assert.equal(f.sleep.state.open, false); assert.equal(f.writes().length, 0);
});

test('shared timer helpers load on the dashboard without mounting the full remote timer panel', () => {
  const source = fs.readFileSync(require.resolve('../crates/fsonos-api/src/remote/timers.js'), 'utf8');
  const context = { document: { getElementById() { throw new Error('Dashboard must not auto-mount the full remote panel'); } } };
  vm.runInNewContext(source + '\nglobalThis.helpers = { timerOf: TimersRemote.timerOf, sameTimer: TimersRemote.sameTimer };', context);
  assert.equal(typeof context.helpers.timerOf, 'function'); assert.equal(typeof context.helpers.sameTimer, 'function');
});

test('the full remote still registers native sleep routes and mounts its existing timer panel', () => {
  const source = fs.readFileSync(require.resolve('../crates/fsonos-api/src/remote/timers.js'), 'utf8');
  let mounted = null;
  const el = (tag, attributes = {}, ...children) => ({ tag, ...attributes, children,
    contains() { return false; }, setAttribute() {}, replaceChildren(...next) { this.children = next; },
    append(...next) { this.children.push(...next); }, querySelectorAll() { return []; }, addEventListener() {} });
  const context = { ROUTES: {}, call() {}, el, document: { activeElement: null,
    getElementById(id) { assert.equal(id, 'scenes-section'); return { before(node) { mounted = node; } }; } } };
  vm.runInNewContext(source, context);
  assert.deepEqual([...context.ROUTES.sleepRead], ['GET', '/sleep']);
  assert.deepEqual([...context.ROUTES.sleepWrite], ['POST', '/sleep']);
  assert.equal(mounted.tag, 'details'); assert.equal(mounted.children[0].text, 'Sleep timers');
});
