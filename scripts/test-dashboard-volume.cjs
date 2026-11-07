'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const Volume = require('../crates/fsonos-api/src/remote/dashboard-volume.js');
const copy = value => structuredClone(value);
const deferred = () => { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; };
const settled = async predicate => {
  for (let n = 0; n < 200 && !predicate(); n++) await Promise.resolve();
  assert.ok(predicate(), 'operation reached the expected boundary');
};
function documentFixture() {
  const nodes = new Map();
  const doc = { activeElement: null, createElement(tag) {
    const listeners = new Map();
    return { tag, children: [], open: false, hidden: false, disabled: false, textContent: '', value: '',
      attributes: {}, setAttribute(name, value) { this.attributes[name] = value; },
      append(...children) { this.children.push(...children); }, replaceChildren(...children) { this.children = children; },
      addEventListener(name, listener) { listeners.set(name, listener); },
      fire(name, event = {}) { return listeners.get(name)?.({ target: this, preventDefault() {}, ...event }); },
      showModal() { this.open = true; }, close() { this.open = false; this.fire('close'); },
      focus() { doc.activeElement = this; } };
  }, getElementById(id) {
    if (!nodes.has(id)) nodes.set(id, { ...doc.createElement('div'), id });
    return nodes.get(id);
  } };
  return doc;
}
function fixture({ demo = false, dom = false, count = 2 } = {}) {
  const names = ['Main room', ...Array.from({ length: count - 1 }, (_, i) => i === 0 ? 'Side room' : 'Room ' + (i + 1))];
  const zones = ['S2', 'S1'].map(household => ({ household, coordinator_room: 'Main room', members: [...names], transport_state: 'playing' }));
  const rows = new Map(zones.flatMap(zone => zone.members.map((name, i) => [name + '@' + zone.household, {
    zone, transport_state: 'playing', volume: 20 + i * 5, mute: false,
    track: { uri: 'fixture:track', title: 'Synthetic music', queue_position: 2, position_secs: 30, duration_secs: 180 },
  }])));
  let time = 1000, reader = null, writer = null, mixer;
  const calls = [], doc = dom ? documentFixture() : null;
  const normalRead = url => {
    if (url === '/zones') return copy(zones);
    if (url.startsWith('/zones/') && url.endsWith('/state')) {
      const target = decodeURIComponent(url.slice(7, -6));
      assert.ok(rows.has(target), 'qualified physical room exists: ' + target);
      return copy(rows.get(target));
    }
    throw new Error('Unexpected fixture read: ' + url);
  };
  const normalWrite = (url, body) => {
    const row = rows.get(body.zone); assert.ok(row);
    if (url === '/volume') { row.volume = body.volume; return { done: 'Room volume changed', changed: true, volume: body.volume }; }
    if (url === '/mute') { row.mute = body.mute; return { done: body.mute ? 'Room muted' : 'Room unmuted', changed: true }; }
    return { done: 'Transport changed', changed: true };
  };
  const call = async (url, body, options) => {
    calls.push({ url, body, options });
    if (body !== undefined) return writer ? writer(url, body, normalWrite) : normalWrite(url, body);
    return reader ? reader(url, options, normalRead) : normalRead(url);
  };
  const ctl = C.model({ call, demo, now: () => time, changed: () => mixer?.observe() });
  const options = { call, controller: ctl, now: () => time };
  mixer = dom ? Volume.panel({ ...options, document: doc }) : Volume.model(options);
  return { ctl, mixer, zones, rows, calls, doc, normalRead, normalWrite,
    writes: () => calls.filter(row => row.body !== undefined).map(({ url, body }) => ({ url, body })),
    read(fn) { reader = fn; }, write(fn) { writer = fn; }, time(value) { time = value; },
    async ready() { assert.equal(await ctl.refresh(), true); ctl.connection(true); return this; } };
}
async function review(f, room = 'Side room@S2', value = '40') {
  await f.mixer.open(); assert.equal(f.mixer.choose(room, value), true);
  const pending = f.mixer.review(room, 'volume'); assert.ok(pending); return pending;
}
function rowNodes(f, index = 1) {
  const node = f.doc.getElementById('room-volume-rooms').children[index];
  return { node, status: node.children[1], range: node.children[2].children[0], output: node.children[0].children[1],
    apply: node.children[3].children[0], mute: node.children[3].children[1] };
}

test('mixer reads physical room levels lazily and preserves independent mute and volume values', async () => {
  const f = await fixture().ready(); f.rows.get('Side room@S2').mute = true;
  assert.equal(f.calls.some(row => row.url.includes('Side%20room')), false);
  const before = f.calls.length;
  assert.equal(await f.mixer.open(), true);
  assert.deepEqual(f.calls.slice(before).map(row => row.url), ['/zones', '/zones/Main%20room%40S2/state', '/zones/Side%20room%40S2/state', '/zones']);
  assert.deepEqual(f.mixer.state.rows.map(row => [row.target, row.volume, row.mute]),
    [['Main room@S2', 20, false], ['Side room@S2', 25, true]]);
  assert.equal(f.writes().length, 0);
});

test('room reads are bounded to four in flight even for a large group', async () => {
  const f = await fixture({ count: 12 }).ready(), waiting = [];
  let active = 0, max = 0;
  f.read(async (url, options, normal) => {
    if (!url.endsWith('/state')) return normal(url);
    const gate = deferred(); waiting.push(gate); max = Math.max(max, ++active);
    await gate.promise; --active; return normal(url);
  });
  const opening = f.mixer.open();
  for (let batch = 0; batch < 3; batch++) {
    await settled(() => waiting.length === (batch + 1) * 4);
    assert.equal(active, 4);
    for (const gate of waiting.slice(batch * 4, batch * 4 + 4)) gate.resolve();
  }
  assert.equal(await opening, true); assert.equal(max, 4); assert.equal(f.mixer.state.rows.length, 12);
});

test('absolute room volume targets the selected member without group fanout and refreshes once', async () => {
  const f = await fixture().ready(); const pending = await review(f);
  assert.ok(Object.isFrozen(pending)); assert.ok(Object.isFrozen(pending.body)); assert.ok(Object.isFrozen(pending.room));
  const before = f.calls.length;
  assert.equal(await f.mixer.apply(), true);
  assert.deepEqual(f.writes(), [{ url: '/volume', body: { zone: 'Side room@S2', volume: 40 } }]);
  assert.equal(f.rows.get('Main room@S2').volume, 20); assert.equal(f.rows.get('Side room@S1').volume, 25);
  assert.equal(f.mixer.state.result.volume, 40); assert.equal(f.mixer.desired('Side room@S2'), 40);
  assert.deepEqual(f.calls.slice(before).map(row => row.url), ['/zones', '/zones/Side%20room%40S2/state', '/zones',
    '/volume', '/zones/Main%20room%40S2/state', '/zones', '/zones/Main%20room%40S2/state', '/zones/Side%20room%40S2/state', '/zones']);
  assert.equal(await f.mixer.apply(), false);
});

test('mute and unmute use explicit native booleans without playback or volume commands', async () => {
  for (const muted of [false, true]) {
    const f = await fixture().ready(); f.rows.get('Side room@S2').mute = muted; await f.mixer.open();
    assert.ok(f.mixer.review('Side room@S2', 'mute')); assert.equal(await f.mixer.apply(), true);
    assert.deepEqual(f.writes(), [{ url: '/mute', body: { zone: 'Side room@S2', mute: !muted } }]);
    assert.equal(f.rows.get('Main room@S2').mute, false); assert.equal(f.rows.get('Side room@S2').volume, 25);
  }
});

test('policy-clamped levels and notes are accepted as the actual result without retrying toward the request', async () => {
  const f = await fixture().ready(); await review(f);
  f.write((url, body, normal) => ({ ...normal(url, { ...body, volume: 30 }), notes: [{ code: 'VOLUME_CLAMPED', detail: 'House limit 30 applied.' }] }));
  assert.equal(await f.mixer.apply(), true); assert.equal(f.mixer.state.result.volume, 30);
  assert.equal(f.mixer.state.result.requested, 40); assert.match(f.mixer.state.message, /House limit 30 applied/);
  assert.equal(f.mixer.desired('Side room@S2'), 30); assert.equal(f.ctl.state.uncertain, false); assert.equal(f.writes().length, 1);
});

test('a sent level acknowledgement is distinct from a later observed level', async () => {
  const f = await fixture().ready(); await review(f);
  f.write(() => ({ done: 'Volume accepted', changed: true, volume: 40 }));
  assert.equal(await f.mixer.apply(), true);
  assert.equal(f.mixer.state.result.volume, 40); assert.equal(f.mixer.desired('Side room@S2'), 25);
});

test('unknown volume and mute remain independently read-only, including an explicit unmute with unknown volume', async () => {
  const f = await fixture().ready();
  f.rows.get('Main room@S2').mute = null; f.rows.get('Side room@S2').volume = null; f.rows.get('Side room@S2').mute = true;
  await f.mixer.open();
  assert.equal(f.mixer.choose('Main room@S2', '30'), true); assert.ok(f.mixer.review('Main room@S2', 'volume'));
  assert.equal(f.mixer.review('Main room@S2', 'mute'), null);
  assert.equal(f.mixer.choose('Side room@S2', '30'), false);
  const pending = f.mixer.review('Side room@S2', 'mute'); assert.ok(pending);
  assert.match(pending.detail, /existing volume/); assert.equal(await f.mixer.apply(), true);
  assert.deepEqual(f.writes(), [{ url: '/mute', body: { zone: 'Side room@S2', mute: false } }]);
});

test('one unreadable member does not disable valid controls for another member', async () => {
  const f = await fixture().ready();
  f.read((url, options, normal) => { if (url.includes('Side%20room')) throw new Error('Speaker unavailable'); return normal(url); });
  assert.equal(await f.mixer.open(), true); assert.match(f.mixer.state.rows[1].error, /Speaker unavailable/);
  assert.equal(f.mixer.choose('Side room@S2', '30'), false); assert.equal(f.mixer.review('Side room@S2', 'mute'), null);
  f.mixer.choose('Main room@S2', '30'); assert.ok(f.mixer.review('Main room@S2', 'volume'));
  assert.equal(await f.mixer.apply(), true); assert.equal(f.writes()[0].body.zone, 'Main room@S2');
});

test('invalid levels and unobserved rooms cannot create a write or preserve an older review', async () => {
  const f = await fixture().ready(); await review(f);
  for (const value of ['-1', '101', '1.5', '1e2', '', '1234', null]) {
    assert.equal(Volume.levelOf(value), null);
    assert.equal(f.mixer.choose('Side room@S2', value), false); assert.equal(f.mixer.state.pending, null);
  }
  for (const value of ['0', '100', ' 30 ']) assert.equal(Volume.levelOf(value), Number(value));
  assert.equal(f.mixer.choose('Unknown room@S2', '30'), false);
  assert.equal(f.mixer.review('Side room@S1', 'mute'), null); assert.equal(f.mixer.review('Main room@S2', 'unmute'), null);
  assert.equal(f.mixer.choose('Side room@S2', '25'), true); assert.equal(f.mixer.review('Side room@S2', 'volume'), null);
  assert.equal(f.writes().length, 0);
});

test('dragging selects a reviewable level without writes and a later edit retires a captured confirmation', async () => {
  const f = await fixture().ready(), old = await review(f);
  for (let n = 30; n < 50; n++) f.mixer.choose('Side room@S2', String(n));
  assert.equal(f.writes().length, 0); assert.equal(await f.mixer.apply(old), false);
  const next = f.mixer.review('Side room@S2', 'volume'); assert.equal(next.want, 49);
  f.mixer.cancelReview(); assert.equal(await f.mixer.apply(next), false);
  f.mixer.review('Side room@S2', 'mute'); assert.equal(await f.mixer.apply(old), false);
});

test('a changed volume, mute or known/missing field transition rejects preflight without dispatch', async () => {
  for (const change of [{ volume: 26 }, { mute: true }, { volume: null }, { mute: null }]) {
    const f = await fixture().ready(); await review(f); Object.assign(f.rows.get('Side room@S2'), change);
    assert.equal(await f.mixer.apply(), false); assert.equal(f.writes().length, 0); assert.equal(f.ctl.state.uncertain, false);
  }
});

test('regrouping, coordinator changes and substituted households invalidate the room review', async () => {
  for (const change of [f => f.zones[0].members.push('New room'), f => { f.zones[0].coordinator_room = 'Side room'; },
    f => { f.zones[0].household = 'Other'; }]) {
    const f = await fixture().ready(); await review(f); change(f);
    assert.equal(await f.mixer.apply(), false); assert.equal(f.writes().length, 0);
  }
});

test('topology is rechecked after physical room observations on read and confirmation', async () => {
  for (const applying of [false, true]) {
    const f = await fixture().ready(); if (applying) await review(f);
    f.read((url, options, normal) => {
      const result = normal(url); if (url.includes('Side%20room')) f.zones[0].members.push('New room'); return result;
    });
    assert.equal(await (applying ? f.mixer.apply() : f.mixer.open()), false); assert.equal(f.writes().length, 0);
  }
});

test('age uses the oldest successful room observation and is enforced during delayed preflight', async () => {
  const f = await fixture().ready(); await review(f);
  f.read((url, options, normal) => { if (url.includes('Side%20room')) f.time(1000 + Volume.MAX_AGE); return normal(url); });
  assert.equal(await f.mixer.apply(), false); assert.equal(f.writes().length, 0);
  const g = await fixture().ready(); await g.mixer.open(); g.time(1000 + Volume.MAX_AGE); g.mixer.observe();
  assert.equal(g.mixer.state.stale, true); assert.equal(g.mixer.state.rows[1].volume, 25); assert.equal(g.mixer.choose('Side room@S2', '30'), false);
  assert.equal(await g.mixer.read(), true); assert.equal(g.mixer.choose('Side room@S2', '30'), true);
});

test('same-name members in different households use their exact qualified addresses', async () => {
  const f = await fixture().ready(); const old = await review(f);
  f.ctl.select('Main room@S1'); assert.equal(f.mixer.state.rows, null); assert.equal(await f.mixer.apply(old), false);
  await f.mixer.read(); f.mixer.choose('Side room@S1', '30'); f.mixer.review('Side room@S1', 'volume');
  assert.equal(await f.mixer.apply(), true); assert.equal(f.writes()[0].body.zone, 'Side room@S1');
});

test('normal playback progress does not invalidate an independent room-volume review', async () => {
  const f = await fixture().ready(); await review(f);
  Object.assign(f.rows.get('Side room@S2'), { transport_state: 'paused', track: null });
  assert.equal(await f.mixer.apply(), true); assert.deepEqual(f.writes().map(row => row.url), ['/volume']);
});

test('close, suspend, invalidation and disposal during preflight all suppress an unsent room command', async () => {
  for (const stop of [f => f.mixer.close(), f => f.mixer.suspend(), f => f.ctl.invalidate(), f => f.mixer.dispose()]) {
    const f = await fixture().ready(); await review(f); const gate = deferred(); let waiting = false;
    f.read((url, options, normal) => { if (url.includes('Side%20room')) { waiting = true; return gate.promise; } return normal(url); });
    const applying = f.mixer.apply(); await settled(() => waiting); stop(f); gate.resolve(copy(f.rows.get('Side room@S2')));
    assert.equal(await applying, false); assert.equal(f.writes().length, 0); assert.equal(f.ctl.state.uncertain, false);
  }
});

test('closing cancels all current read signals and discards a late member response', async () => {
  const f = await fixture().ready(), gate = deferred(); let signal;
  f.read((url, options, normal) => { if (url.includes('Side%20room')) { signal = options.signal; return gate.promise; } return normal(url); });
  const opening = f.mixer.open(); await settled(() => !!signal); f.mixer.close(); assert.equal(signal.aborted, true);
  gate.resolve(copy(f.rows.get('Side room@S2'))); assert.equal(await opening, false); assert.equal(f.mixer.state.rows, null);
});

test('room volume and mute share the global lane and cannot double-submit or overlap transport', async () => {
  const f = await fixture().ready(); await review(f); const gate = deferred(); let sent = false;
  f.write(() => { sent = true; return gate.promise; });
  const applying = f.mixer.apply(); await settled(() => sent);
  assert.equal(await f.mixer.apply(), false); assert.equal(await f.ctl.command('next'), false);
  assert.equal(await f.mixer.read(), false); assert.equal(f.mixer.review('Main room@S2', 'mute'), null);
  assert.equal(f.ctl.select('Main room@S1'), false);
  gate.resolve({ done: 'Accepted', changed: true, volume: 40 });
  assert.equal(await applying, true); assert.equal(f.writes().length, 1);
});

test('lost or malformed sent replies remain globally uncertain across passive refresh and reopening', async () => {
  for (const result of [null, { done: 'Accepted', changed: true }, { done: 'Accepted', changed: true, volume: 101 },
    { done: 'Accepted', changed: true, volume: 40, notes: 'invalid' }]) {
    const f = await fixture().ready(); await review(f);
    f.write(() => { if (result === null) throw new Error('Lost acknowledgement'); return result; });
    assert.equal(await f.mixer.apply(), false); assert.equal(f.ctl.state.uncertain, true); assert.equal(f.mixer.state.result, null);
    f.mixer.suspend(); f.mixer.resume(); await f.ctl.refresh(); await f.mixer.open();
    assert.equal(f.ctl.state.uncertain, true); assert.equal(f.mixer.choose('Side room@S2', '30'), false);
    assert.equal(await f.ctl.refresh(true), true); assert.equal(f.ctl.state.uncertain, false); assert.equal(f.writes().length, 1);
  }
});

test('an accepted write after closing stays recorded without reopening or resending', async () => {
  const f = await fixture().ready(); await review(f); const gate = deferred(); let sent = false;
  f.write(() => { sent = true; return gate.promise; });
  const applying = f.mixer.apply(); await settled(() => sent); f.mixer.close();
  gate.resolve({ done: 'Accepted', changed: true, volume: 35 });
  assert.equal(await applying, true); assert.equal(f.mixer.state.open, false); assert.equal(f.mixer.state.result.volume, 35);
  assert.equal(f.mixer.state.rows, null); assert.equal(f.writes().length, 1);
});

test('failed post-command observations do not erase accepted policy results or retry the command', async () => {
  const f = await fixture().ready(); await review(f);
  f.write((url, body, normal) => {
    const result = normal(url, body);
    f.read((url, options, normal) => { if (url === '/zones') throw new Error('Room inventory unavailable'); return normal(url); });
    return result;
  });
  assert.equal(await f.mixer.apply(), true); assert.equal(f.mixer.state.result.volume, 40);
  assert.match(f.mixer.state.message, /Room inventory unavailable/); assert.equal(f.writes().length, 1);
});

test('demo mode and stale group topology cannot authorize room changes', async () => {
  const f = await fixture({ demo: true }).ready(); await f.mixer.open();
  assert.equal(f.mixer.choose('Side room@S2', '30'), false); assert.equal(f.mixer.review('Side room@S2', 'mute'), null);
  const g = await fixture().ready(); await review(g); g.ctl.state.topologyFresh = false;
  assert.equal(await g.mixer.apply(), false); assert.equal(await g.mixer.read(), false);
});

test('the actual panel handlers retain sliders during edits, review a room level and display a policy-clamped receipt', async () => {
  const f = await fixture({ dom: true }).ready(), $ = id => f.doc.getElementById(id);
  $('open-room-volume').focus(); $('open-room-volume').fire('click'); await settled(() => f.mixer.state.rows !== null);
  assert.equal($('room-volume-dialog').open, true); const parts = rowNodes(f);
  assert.match(parts.range.attributes['aria-label'], /Side room@S2/);
  parts.range.value = '40'; parts.range.fire('input'); assert.equal(rowNodes(f).range, parts.range);
  assert.match(parts.output.textContent, /40.*not applied/); assert.equal(f.writes().length, 0);
  parts.apply.focus(); parts.apply.fire('click'); assert.equal(f.doc.activeElement, $('room-volume-preview'));
  assert.match($('room-volume-preview-title').textContent, /Side room: 25 → 40/);
  $('room-volume-back').fire('click'); assert.equal(f.doc.activeElement, parts.apply);
  parts.apply.fire('click');
  f.write((url, body, normal) => ({ ...normal(url, { ...body, volume: 30 }), notes: [{ detail: 'House limit 30 applied.' }] }));
  $('room-volume-confirm').fire('click'); await settled(() => !f.mixer.state.writing && !f.mixer.state.loading && f.writes().length === 1);
  assert.match($('room-volume-result').textContent, /accepted volume 30.*requested 40/);
  assert.match($('room-volume-message').textContent, /House limit 30/);
  assert.equal(parts.range.value, '30'); assert.equal(rowNodes(f).range, parts.range);
  $('close-room-volume').fire('click'); assert.equal($('room-volume-dialog').open, false); assert.equal(f.doc.activeElement, $('open-room-volume'));
});

test('the actual panel mute handler reflects native state and unknown fields disable only their own control', async () => {
  const f = await fixture({ dom: true }).ready(), $ = id => f.doc.getElementById(id);
  f.rows.get('Main room@S2').mute = null; f.rows.get('Side room@S2').volume = null;
  await f.mixer.open();
  assert.equal(rowNodes(f, 0).range.disabled, false); assert.equal(rowNodes(f, 0).mute.disabled, true);
  assert.equal(rowNodes(f).range.disabled, true); assert.equal(rowNodes(f).mute.disabled, false);
  rowNodes(f).mute.fire('click'); $('room-volume-confirm').fire('click');
  await settled(() => !f.mixer.state.writing && !f.mixer.state.loading && f.writes().length === 1);
  assert.equal(rowNodes(f).mute.textContent, 'Unmute'); assert.equal(rowNodes(f).mute.attributes['aria-pressed'], 'true');
  let prevented = false; $('room-volume-dialog').fire('cancel', { preventDefault() { prevented = true; } });
  assert.equal(prevented, true); assert.equal(f.mixer.state.open, false);
});
