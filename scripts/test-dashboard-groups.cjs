'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const C = require('../crates/fsonos-api/src/remote/dashboard-core.js');
const Groups = require('../crates/fsonos-api/src/remote/dashboard-groups.js');
const copy = value => structuredClone(value);
const deferred = () => {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
};
const settled = async predicate => {
  for (let n = 0; n < 100 && !predicate(); n++) await Promise.resolve();
  assert.ok(predicate(), 'operation reached the expected boundary');
};
function documentFixture() {
  const nodes = new Map();
  const doc = { activeElement: null, createElement(tag) {
    const listeners = new Map();
    return { tagName: tag.toUpperCase(), children: [], disabled: false, hidden: false, open: false,
      value: '', textContent: '', attributes: {},
      get options() { return this.children; },
      append(...items) { this.children.push(...items); },
      replaceChildren(...items) { this.children = [...items]; },
      setAttribute(name, value) { this.attributes[name] = value; },
      addEventListener(name, listener) { listeners.set(name, listener); },
      fire(name, event = {}) { return listeners.get(name)?.({ target: this, preventDefault() {}, ...event }); },
      showModal() { this.open = true; }, close() { this.open = false; this.fire('close'); },
      focus() { doc.activeElement = this; },
    };
  }, getElementById(id) {
    if (!nodes.has(id)) { const node = this.createElement('div'); node.id = id; nodes.set(id, node); }
    return nodes.get(id);
  } };
  return doc;
}
function fixture({ demo = false, dom = false } = {}) {
  const zones = [
    { household: 'Fixture S2', coordinator_room: 'Synthetic main',
      members: ['Synthetic main', 'Synthetic companion'], transport_state: 'playing', degraded: [] },
    { household: 'Fixture S2', coordinator_room: 'Synthetic kitchen',
      members: ['Synthetic kitchen', 'Synthetic nook'], transport_state: 'paused', degraded: [] },
    { household: 'Fixture S1', coordinator_room: 'Synthetic main',
      members: ['Synthetic main'], transport_state: 'stopped', degraded: [] },
  ];
  const tracks = new Map(zones.map(zone => [C.key(zone), { uri: 'fixture:track', title: 'Synthetic song',
    queue_position: 2, position_secs: 30, duration_secs: 180 }]));
  let time = 1000, reader = null, writer = null, groups;
  const calls = [], doc = dom ? documentFixture() : null;
  const normalRead = url => {
    if (url === '/zones') return copy(zones);
    const zone = zones.find(row => url === '/zones/' + encodeURIComponent(C.key(row)) + '/state');
    if (!zone) throw new Error('Unexpected fixture read: ' + url);
    return { zone: copy(zone), transport_state: zone.transport_state, volume: 20,
      track: copy(tracks.has(C.key(zone)) ? tracks.get(C.key(zone)) : { uri: 'fixture:new', queue_position: 1 }) };
  };
  const call = async (url, body, options) => {
    calls.push({ url, body, options });
    if (body !== undefined) return writer ? writer(url, body) : { done: 'Grouping changed', changed: true };
    return reader ? reader(url, options, normalRead) : normalRead(url);
  };
  const ctl = C.model({ call, demo, now: () => time, changed: () => groups?.observe() });
  const options = { call, controller: ctl, now: () => time };
  groups = dom ? Groups.panel({ ...options, document: doc }) : Groups.model(options);
  return { ctl, groups, zones, tracks, calls, doc, normalRead,
    writes: () => calls.filter(row => row.body !== undefined).map(({ url, body }) => ({ url, body })),
    read(fn) { reader = fn; }, write(fn) { writer = fn; }, time(value) { time = value; },
    async ready() { assert.equal(await ctl.refresh(), true); ctl.connection(true); return this; },
    async reviewed(action = 'join', room = action === 'move' ? 'Synthetic main@Fixture S2' : 'Synthetic kitchen@Fixture S2',
      destination = action === 'replay' ? 'Synthetic main@Fixture S1' : 'Synthetic kitchen@Fixture S2') {
      assert.equal(await groups.open(), true);
      assert.equal(groups.chooseAction(action), true);
      if (!['party', 'replay'].includes(action)) assert.equal(groups.chooseRoom(room), true);
      if (Groups.transferring(action)) assert.equal(groups.chooseDestination(destination), true);
      assert.ok(await groups.review());
      return groups.state.pending;
    },
  };
}

test('group inventory is lazy, household qualified and immutable', async () => {
  const f = await fixture().ready(), before = f.calls.length;
  assert.equal(f.groups.state.groups, null);
  assert.equal(await f.groups.open(), true);
  assert.deepEqual(f.calls.slice(before).map(row => row.url), ['/zones']);
  assert.equal(f.groups.state.target, 'Synthetic main@Fixture S2');
  assert.deepEqual(f.groups.choices().map(row => row.target), [
    'Synthetic kitchen@Fixture S2', 'Synthetic nook@Fixture S2',
  ]);
  assert.ok(Object.isFrozen(f.groups.state.groups));
  assert.ok(Object.isFrozen(f.groups.state.groups[0].members));
  assert.equal(f.writes().length, 0);
});

test('joining a coordinator sends one room-specific group command and refreshes the whole house', async () => {
  const f = await fixture().ready(), pending = await f.reviewed();
  assert.ok(Object.isFrozen(pending));
  assert.ok(Object.isFrozen(pending.body));
  assert.ok(Object.isFrozen(pending.affected));
  assert.match(pending.detail, /currently coordinates/);
  const before = f.calls.length;
  assert.equal(await f.groups.apply(pending), true);
  assert.deepEqual(f.writes(), [{ url: '/group', body: {
    zone: 'Synthetic kitchen@Fixture S2', to: 'Synthetic main@Fixture S2',
  } }]);
  assert.deepEqual(f.calls.slice(before).map(row => row.url), [
    '/zones', '/group', '/zones',
    '/zones/Synthetic%20main%40Fixture%20S2/state',
    '/zones/Synthetic%20kitchen%40Fixture%20S2/state',
    '/zones/Synthetic%20main%40Fixture%20S1/state',
  ]);
  assert.equal(f.groups.state.result.action, 'join');
  assert.equal(f.groups.state.groups, null);
  assert.equal(await f.groups.apply(pending), false);
  assert.equal(f.writes().length, 1);
});

test('adding a member names that room rather than moving its entire source group', async () => {
  const f = await fixture().ready(), pending = await f.reviewed('join', 'Synthetic nook@Fixture S2');
  assert.doesNotMatch(pending.detail, /currently coordinates/);
  assert.equal(await f.groups.apply(), true);
  assert.equal(f.writes()[0].body.zone, 'Synthetic nook@Fixture S2');
});

test('separating a member or the coordinator uses the exact reviewed room', async () => {
  for (const room of ['Synthetic main', 'Synthetic companion']) {
    const f = await fixture().ready(), pending = await f.reviewed('leave', room + '@Fixture S2');
    assert.equal(pending.affected.length, 1);
    assert.equal(pending.detail.includes('currently coordinates'), room === 'Synthetic main');
    assert.equal(await f.groups.apply(), true);
    assert.deepEqual(f.writes(), [{ url: '/ungroup', body: { zone: room + '@Fixture S2' } }]);
  }
});

test('whole-house playback reviews every group in only the selected household', async () => {
  const f = await fixture().ready(), pending = await f.reviewed('party');
  assert.equal(pending.affected.length, 2);
  assert.ok(pending.affected.every(zone => zone.household === 'Fixture S2'));
  assert.match(pending.detail, /All 4 rooms/);
  assert.match(pending.detail, /multi-step/);
  assert.equal(await f.groups.apply(), true);
  assert.deepEqual(f.writes(), [{ url: '/party', body: { zone: 'Synthetic main@Fixture S2' } }]);
});

test('unchanged standalone groups, foreign households and unsupported actions cannot dispatch', async () => {
  const f = await fixture().ready(); await f.groups.open();
  assert.equal(f.groups.chooseRoom('Synthetic main@Fixture S1'), false);
  assert.equal(f.groups.review(), null);
  for (const action of ['__proto__', 'constructor', 'copy', 'unsupported', null]) {
    assert.equal(f.groups.chooseAction(action), false);
  }
  f.ctl.select('Synthetic main@Fixture S1'); await f.groups.read();
  assert.deepEqual(f.groups.choices(), []);
  f.groups.chooseAction('leave'); f.groups.chooseRoom('Synthetic main@Fixture S1');
  assert.match(f.groups.reason(), /already plays on its own/);
  assert.equal(f.groups.review(), null);
  f.groups.chooseAction('party');
  assert.match(f.groups.reason(), /already has one group/);
  assert.equal(f.groups.review(), null);
  assert.equal(await f.groups.apply(), false);
  assert.equal(f.writes().length, 0);
});

test('changing or clearing the selection retires the previous immutable review', async () => {
  const f = await fixture().ready(), old = await f.reviewed();
  f.groups.chooseRoom('');
  assert.equal(f.groups.state.pending, null);
  assert.equal(await f.groups.apply(old), false);
  assert.equal(f.groups.canReview(), false);
  f.groups.chooseRoom('Synthetic nook@Fixture S2'); f.groups.review();
  assert.equal(await f.groups.apply(old), false);
  assert.equal(await f.groups.apply(), true);
  assert.equal(f.writes()[0].body.zone, 'Synthetic nook@Fixture S2');
});

test('invalid selection events cannot leave an older room change armed', async () => {
  for (const change of [
    groups => groups.chooseRoom('missing@Fixture S2'),
    groups => groups.chooseAction('unsupported'),
  ]) {
    const f = await fixture().ready(), old = await f.reviewed();
    assert.equal(change(f.groups), false);
    assert.equal(f.groups.state.pending, null);
    assert.equal(await f.groups.apply(old), false);
    assert.equal(f.writes().length, 0);
  }
});

test('another household with the same coordinator name never inherits a review', async () => {
  const f = await fixture().ready(), old = await f.reviewed();
  assert.equal(f.ctl.select('Synthetic main@Fixture S1'), true);
  assert.equal(f.groups.state.target, 'Synthetic main@Fixture S1');
  assert.equal(f.groups.state.groups, null);
  assert.equal(await f.groups.apply(old), false);
  assert.equal(f.writes().length, 0);
});

test('cancelled and expired reviews require another observation or review', async () => {
  const f = await fixture().ready(), old = await f.reviewed();
  assert.equal(f.groups.cancel(), true);
  assert.equal(await f.groups.apply(old), false);
  assert.ok(f.groups.review());
  f.time(1000 + Groups.MAX_AGE);
  assert.equal(f.groups.canApply(), false);
  f.groups.observe();
  assert.equal(f.groups.state.groups, null);
  assert.equal(await f.groups.apply(), false);
  assert.equal(f.writes().length, 0);
});

test('preflight rejects changed membership, coordinators, health and removed source rooms', async () => {
  for (const change of [
    zones => zones[0].members.push('Synthetic extra'),
    zones => { zones[1].coordinator_room = 'Synthetic nook'; },
    zones => zones[1].degraded.push('Synthetic nook'),
    zones => { zones[1].members = ['Synthetic nook']; zones[1].coordinator_room = 'Synthetic nook'; },
  ]) {
    const f = await fixture().ready(); await f.reviewed(); change(f.zones);
    assert.equal(await f.groups.apply(), false);
    assert.equal(f.writes().length, 0);
    assert.equal(f.ctl.state.uncertain, false);
    assert.match(f.ctl.state.message, /No command was sent/);
  }
});

test('party preflight includes rooms discovered after the preview', async () => {
  const f = await fixture().ready(); await f.reviewed('party');
  f.zones.push({ household: 'Fixture S2', coordinator_room: 'Synthetic new',
    members: ['Synthetic new'], transport_state: 'stopped', degraded: [] });
  assert.equal(await f.groups.apply(), false);
  assert.equal(f.writes().length, 0);
});

test('unrelated household changes do not substitute the reviewed native target', async () => {
  const f = await fixture().ready(); await f.reviewed();
  f.zones[2].members.push('Synthetic other');
  assert.equal(await f.groups.apply(), true);
  assert.equal(f.writes()[0].body.to, 'Synthetic main@Fixture S2');
});

test('a stale affected group blocks a fresh review and retires an existing one', async () => {
  const f = await fixture().ready(); await f.groups.open();
  f.ctl.invalidate(['Synthetic kitchen@Fixture S2']);
  f.groups.chooseRoom('Synthetic kitchen@Fixture S2');
  assert.equal(f.groups.review(), null);
  assert.match(f.groups.reason(), /every affected group/);
  assert.equal(f.writes().length, 0);
  await f.ctl.refresh(); await f.groups.read();
  f.groups.chooseRoom('Synthetic kitchen@Fixture S2'); assert.ok(f.groups.review());
  f.ctl.invalidate(['Synthetic kitchen@Fixture S2']);
  assert.equal(f.groups.state.pending, null);
  assert.equal(await f.groups.apply(), false);
});

test('closing, hiding and invalidating either affected group during preflight prevents dispatch', async () => {
  for (const interrupt of [
    f => f.groups.close(),
    f => f.groups.suspend(),
    f => f.ctl.invalidate(),
    f => f.ctl.invalidate(['Synthetic kitchen@Fixture S2']),
    f => f.ctl.suspend(),
    f => f.groups.dispose(),
  ]) {
    const f = await fixture().ready(); await f.reviewed();
    const wait = deferred();
    f.read(url => { assert.equal(url, '/zones'); return wait.promise; });
    const task = f.groups.apply();
    await settled(() => f.ctl.state.busy);
    interrupt(f);
    wait.resolve(copy(f.zones));
    assert.equal(await task, false);
    assert.equal(f.writes().length, 0);
    assert.equal(f.ctl.state.uncertain, false);
  }
});

test('an obsolete inventory read is aborted and cannot replace a reopened view', async () => {
  const f = await fixture().ready(), wait = deferred();
  let signal, first = true;
  f.read((url, options, normal) => {
    if (first) { first = false; signal = options.signal; return wait.promise; }
    return normal(url);
  });
  const old = f.groups.open();
  await settled(() => !!signal);
  f.groups.close();
  assert.equal(signal.aborted, true);
  assert.equal(await f.groups.open(), true);
  const current = f.groups.state.groups;
  wait.resolve([]);
  assert.equal(await old, false);
  assert.equal(f.groups.state.groups, current);
  assert.equal(f.groups.state.loading, false);
});

test('an inventory read that disagrees with the dashboard must refresh before reviewing', async () => {
  const f = await fixture().ready();
  f.zones[0].members.push('Synthetic unexpected');
  assert.equal(await f.groups.open(), false);
  assert.equal(f.groups.state.groups, null);
  assert.match(f.groups.state.message, /Refresh playback/);
  assert.equal(f.writes().length, 0);
});

test('malformed and ambiguous inventories never create review authority', async () => {
  const f = await fixture().ready();
  for (const value of [null, {}, [{ ...copy(f.zones[0]), members: [] }],
    [copy(f.zones[0]), copy(f.zones[0])],
    [{ household: 'c', coordinator_room: 'a@b', members: ['a@b'], transport_state: 'stopped' },
      { household: 'b@c', coordinator_room: 'a', members: ['a'], transport_state: 'stopped' }],
  ]) assert.throws(() => Groups.inventory(value));
  f.read(() => ({ groups: [] }));
  assert.equal(await f.groups.open(), false);
  assert.equal(f.groups.state.groups, null);
  assert.equal(f.groups.review(), null);
});

test('preview and disconnected stale state never issue group commands', async () => {
  const demo = await fixture({ demo: true }).ready(); await demo.groups.open();
  assert.equal(demo.groups.chooseRoom('Synthetic kitchen@Fixture S2'), false);
  assert.equal(demo.groups.review(), null);
  assert.equal(await demo.groups.apply(), false);
  assert.equal(demo.writes().length, 0);
  const f = await fixture().ready(); await f.reviewed();
  f.ctl.connection(false);
  assert.equal(await f.groups.apply(), false);
  assert.equal(f.writes().length, 0);
});

test('group commands serialize with transport and cannot be submitted twice', async () => {
  const f = await fixture().ready(), pending = await f.reviewed(), wait = deferred();
  f.write(() => wait.promise);
  const task = f.groups.apply(pending);
  await settled(() => f.writes().length === 1);
  assert.equal(await f.ctl.command('pause'), false);
  assert.equal(await f.groups.apply(pending), false);
  assert.equal(f.ctl.select('Synthetic main@Fixture S1'), false);
  wait.resolve({ done: 'Grouping changed', changed: true });
  assert.equal(await task, true);
  assert.equal(f.writes().length, 1);
});

test('unreadable acknowledgements preserve global uncertainty and never report local success', async () => {
  for (const receipt of [null, { done: '', changed: true }, { done: 'Changed' },
    { done: 'Changed', changed: true, notes: [{ detail: 42 }] }]) {
    const f = await fixture().ready(); await f.reviewed();
    f.write(() => receipt);
    assert.equal(await f.groups.apply(), false);
    assert.equal(f.groups.state.result, null);
    assert.equal(f.ctl.state.uncertain, true);
    assert.equal(f.writes().length, 1);
    assert.match(f.groups.state.message, /no retry/);
  }
});

test('a lost acknowledgement keeps the global lock through reopening and ordinary refresh', async () => {
  const f = await fixture().ready(); await f.reviewed();
  f.write(() => { throw new Error('Connection lost after dispatch'); });
  assert.equal(await f.groups.apply(), false);
  f.groups.suspend(); f.ctl.suspend(); f.ctl.resume(); f.groups.resume();
  await f.ctl.refresh(false);
  assert.equal(await f.groups.open(), true);
  assert.equal(f.ctl.state.uncertain, true);
  assert.equal(f.groups.chooseRoom('Synthetic kitchen@Fixture S2'), false);
  assert.equal(await f.ctl.command('pause'), false);
  assert.equal(f.writes().length, 1);
  await f.ctl.refresh(true);
  assert.equal(f.ctl.state.uncertain, false);
});

test('closing after dispatch keeps the command tracked and does not retry it', async () => {
  const f = await fixture().ready(); await f.reviewed();
  const wait = deferred(); f.write(() => wait.promise);
  const task = f.groups.apply();
  await settled(() => f.writes().length === 1);
  f.groups.close();
  assert.equal(f.ctl.state.busy, true);
  wait.resolve({ done: 'Grouping changed', changed: true, notes: [{ detail: 'One room remains unavailable.' }] });
  assert.equal(await task, true);
  assert.equal(f.groups.state.open, false);
  assert.match(f.ctl.state.message, /One room remains unavailable/);
  assert.equal(f.writes().length, 1);
});

test('successful topology changes refresh the selected group and retire the prior review', async () => {
  const f = await fixture().ready(); await f.reviewed('party');
  f.write(() => {
    f.zones[0].members.push(...f.zones[1].members); f.zones.splice(1, 1);
    return { done: 'Household grouped', changed: true };
  });
  assert.equal(await f.groups.apply(), true);
  assert.equal(f.ctl.selected().zone.members.length, 4);
  assert.equal(f.ctl.state.items.length, 2);
  assert.equal(f.groups.state.pending, null);
  assert.equal(f.groups.state.groups, null);
  assert.equal(f.groups.state.result.action, 'party');
});

test('real panel handlers add a room and expose an immutable affected-group preview', async () => {
  const f = await fixture({ dom: true }).ready(), $ = id => f.doc.getElementById(id);
  $('open-groups').focus(); $('open-groups').fire('click');
  await settled(() => !f.groups.state.loading);
  assert.equal($('groups-dialog').open, true);
  assert.equal($('open-groups').attributes['aria-expanded'], 'true');
  assert.equal($('groups-room').options.length, 3);
  assert.equal($('groups-confirm').disabled, true);
  $('groups-room').value = 'Synthetic nook@Fixture S2'; $('groups-room').fire('change');
  $('groups-review').fire('click');
  assert.equal($('groups-preview').hidden, false);
  assert.equal($('groups-affected').children.length, 2);
  assert.equal(f.doc.activeElement, $('groups-preview'));
  assert.equal($('groups-confirm').disabled, false);
  $('groups-confirm').fire('click');
  await settled(() => f.writes().length === 1 && !f.groups.state.writing);
  assert.deepEqual(f.writes()[0], { url: '/group', body: {
    zone: 'Synthetic nook@Fixture S2', to: 'Synthetic main@Fixture S2',
  } });
  assert.equal($('groups-preview').hidden, true);
  $('close-groups').fire('click');
  assert.equal($('groups-dialog').open, false);
  assert.equal(f.doc.activeElement, $('open-groups'));
});

test('real panel action, cancellation and household handlers cannot reuse an earlier selection', async () => {
  const f = await fixture({ dom: true }).ready(), $ = id => f.doc.getElementById(id);
  $('open-groups').fire('click'); await settled(() => !f.groups.state.loading);
  $('groups-action').value = 'leave'; $('groups-action').fire('change');
  $('groups-room').value = 'Synthetic companion@Fixture S2'; $('groups-room').fire('change');
  $('groups-review').fire('click');
  assert.equal($('groups-confirm').textContent, 'Separate a room');
  $('groups-cancel').fire('click');
  $('groups-confirm').fire('click');
  assert.equal(f.writes().length, 0);
  $('groups-action').value = 'party'; $('groups-action').fire('change');
  assert.equal($('groups-room-field').hidden, true);
  $('groups-review').fire('click');
  assert.equal($('groups-confirm').textContent, 'Group the household');
  $('groups-confirm').fire('click');
  await settled(() => f.writes().length === 1 && !f.groups.state.writing);
  assert.equal(f.writes()[0].url, '/party');
});

test('real panel Escape, stale-state and lifecycle handlers retire reviewed actions', async () => {
  for (const interrupt of [
    f => f.doc.getElementById('groups-dialog').fire('cancel'),
    f => f.ctl.invalidate(),
    f => f.groups.suspend(),
  ]) {
    const f = await fixture({ dom: true }).ready(), $ = id => f.doc.getElementById(id);
    await f.reviewed();
    interrupt(f);
    $('groups-confirm').fire('click');
    assert.equal($('groups-confirm').disabled, true);
    assert.equal(f.writes().length, 0);
    assert.equal(f.groups.state.pending, null);
  }
});


test('native movement offers source rooms in this group and destinations in this household', async () => {
  const f = await fixture().ready(); await f.groups.open(); f.groups.chooseAction('move');
  assert.deepEqual(f.groups.choices().map(choice => choice.target), [
    'Synthetic companion@Fixture S2', 'Synthetic main@Fixture S2',
  ]);
  f.groups.chooseRoom('Synthetic main@Fixture S2');
  assert.ok(f.groups.destinations().some(choice => choice.target === 'Synthetic companion@Fixture S2'));
  assert.equal(f.groups.destinations().some(choice => choice.target === 'Synthetic main@Fixture S2'), false);
  assert.equal(f.groups.destinations().some(choice => choice.target.endsWith('@Fixture S1')), false);
  assert.equal(f.groups.chooseDestination('Synthetic main@Fixture S1'), false);
  assert.equal(f.groups.review(), null);
  f.groups.chooseAction('replay');
  assert.equal(f.groups.choices().length, 0);
  assert.deepEqual(f.groups.destinations().map(choice => choice.target), [
    'Synthetic kitchen@Fixture S2', 'Synthetic main@Fixture S1',
  ]);
});

test('moving a coordinator captures both groups and sends one native move command', async () => {
  const f = await fixture().ready(), pending = await f.reviewed('move');
  assert.ok(Object.isFrozen(pending.audio));
  assert.ok(Object.isFrozen(pending.audio.source));
  assert.equal(pending.audio.source.target, 'Synthetic main@Fixture S2');
  assert.equal(pending.audio.destination.target, 'Synthetic kitchen@Fixture S2');
  assert.equal(pending.audio.source.position, 2);
  assert.match(pending.detail, /Coordination moves first/);
  assert.match(pending.detail, /destination currently coordinates/);
  assert.equal(f.writes().length, 0);
  assert.equal(await f.groups.apply(), true);
  assert.deepEqual(f.writes(), [{ url: '/move', body: {
    zone: 'Synthetic main@Fixture S2', to: 'Synthetic kitchen@Fixture S2', copy: false,
  } }]);
  assert.equal(f.groups.state.result.action, 'move');
});

test('moving a member reads its coordinator while the native request names the actual rooms', async () => {
  const f = await fixture().ready(), pending = await f.reviewed('move',
    'Synthetic companion@Fixture S2', 'Synthetic nook@Fixture S2');
  assert.equal(pending.audio.source.target, 'Synthetic main@Fixture S2');
  assert.equal(pending.audio.destination.target, 'Synthetic kitchen@Fixture S2');
  assert.equal(await f.groups.apply(), true);
  assert.deepEqual(f.writes()[0].body, {
    zone: 'Synthetic companion@Fixture S2', to: 'Synthetic nook@Fixture S2', copy: false,
  });
});

test('a move within one group reads that playback once and separates the selected source room', async () => {
  const f = await fixture().ready(), before = f.calls.length;
  const pending = await f.reviewed('move', 'Synthetic companion@Fixture S2', 'Synthetic main@Fixture S2');
  assert.equal(pending.affected.length, 1);
  assert.equal(pending.audio.source, pending.audio.destination);
  assert.match(pending.detail, /already shares this music/);
  assert.equal(f.calls.slice(before).filter(row => row.url.endsWith('/state')).length, 1);
  assert.equal(await f.groups.apply(), true);
  assert.deepEqual(f.writes(), [{ url: '/move', body: {
    zone: 'Synthetic companion@Fixture S2', to: 'Synthetic main@Fixture S2', copy: false,
  } }]);
});

test('replay across households explicitly replaces the destination and stops the entire source group', async () => {
  const f = await fixture().ready(), pending = await f.reviewed('replay');
  assert.match(pending.title, /Replay on .*Fixture S1.* and stop .*Fixture S2/);
  assert.match(pending.detail, /stops the entire source group/);
  assert.match(pending.detail, /not synchronized/);
  assert.equal(pending.affected.length, 2);
  assert.equal(await f.groups.apply(), true);
  assert.deepEqual(f.writes(), [{ url: '/move', body: {
    zone: 'Synthetic main@Fixture S2', to: 'Synthetic main@Fixture S1', copy: true,
  } }]);
});

test('replay to a different group in the same household keeps copy semantics', async () => {
  const f = await fixture().ready(); await f.reviewed('replay', '', 'Synthetic kitchen@Fixture S2');
  assert.equal(await f.groups.apply(), true);
  assert.equal(f.writes()[0].body.copy, true);
  assert.equal(f.writes()[0].body.to, 'Synthetic kitchen@Fixture S2');
});

test('identical rooms, same-group replay and cross-household native moves cannot be reviewed', async () => {
  const f = await fixture().ready(), groups = Groups.inventory(f.zones);
  assert.throws(() => Groups.proposal(groups, 'Synthetic main@Fixture S2', 'move',
    'Synthetic main@Fixture S2', 'Synthetic main@Fixture S2'), /different destination/);
  assert.throws(() => Groups.proposal(groups, 'Synthetic main@Fixture S2', 'move',
    'Synthetic main@Fixture S2', 'Synthetic main@Fixture S1'), /one household/);
  assert.throws(() => Groups.proposal(groups, 'Synthetic main@Fixture S2', 'replay',
    '', 'Synthetic companion@Fixture S2'), /already share/);
  await f.groups.open(); f.groups.chooseAction('replay');
  assert.equal(f.groups.chooseDestination('Synthetic main@Fixture S2'), false);
  assert.equal(f.groups.review(), null);
  assert.equal(f.writes().length, 0);
});

test('paused sources and direct streams can transfer without an invented resume command', async () => {
  for (const stream of [false, true]) {
    const f = await fixture().ready();
    f.zones[0].transport_state = 'paused';
    if (stream) f.tracks.set('Synthetic main@Fixture S2', { uri: 'fixture:radio', title: 'Synthetic radio' });
    f.tracks.set('Synthetic main@Fixture S1', null);
    const pending = await f.reviewed('replay');
    assert.equal(pending.audio.source.transport, 'paused');
    assert.equal(pending.audio.source.position, stream ? null : 2);
    assert.equal(pending.audio.destination.label, 'Nothing loaded');
    assert.match(pending.detail, /Paused music is loaded silently/);
    assert.equal(await f.groups.apply(), true);
    assert.deepEqual(f.writes().map(row => row.url), ['/move']);
  }
});

test('empty or unstable sources and unreadable destination playback create no transfer review', async () => {
  for (const change of [
    f => f.tracks.set('Synthetic main@Fixture S2', null),
    f => { f.tracks.get('Synthetic main@Fixture S2').uri = 'NOT_IMPLEMENTED'; },
    f => { f.zones[0].transport_state = 'transitioning'; },
    f => { f.zones[0].transport_state = 'unknown'; },
    f => { f.zones[1].transport_state = 'transitioning'; },
  ]) {
    const f = await fixture().ready();
    await f.groups.open(); f.groups.chooseAction('move');
    f.groups.chooseRoom('Synthetic main@Fixture S2'); f.groups.chooseDestination('Synthetic kitchen@Fixture S2');
    change(f);
    assert.equal(await f.groups.review(), null);
    assert.equal(await f.groups.apply(), false);
    assert.equal(f.groups.state.pending, null);
    assert.equal(f.writes().length, 0);
    assert.equal(f.ctl.state.uncertain, false);
  }
});

test('invalid queue occurrences in either group are rejected before creating transfer authority', async () => {
  for (const target of ['Synthetic main@Fixture S2', 'Synthetic kitchen@Fixture S2']) {
    for (const position of [0, -1, 2.5, 4294967296, '2']) {
      const f = await fixture().ready();
      await f.groups.open(); f.groups.chooseAction('move');
      f.groups.chooseRoom('Synthetic main@Fixture S2'); f.groups.chooseDestination('Synthetic kitchen@Fixture S2');
      f.tracks.get(target).queue_position = position;
      assert.equal(await f.groups.review(), null);
      assert.equal(f.writes().length, 0);
    }
  }
});

test('preflight fences source and destination URI, duplicate occurrence, duration and transport changes', async () => {
  for (const target of ['Synthetic main@Fixture S2', 'Synthetic kitchen@Fixture S2']) {
    for (const change of [
      f => { f.tracks.get(target).uri = 'fixture:changed'; },
      f => { f.tracks.get(target).queue_position = 3; },
      f => { delete f.tracks.get(target).queue_position; },
      f => { f.tracks.get(target).duration_secs = 200; },
      f => { f.zones.find(zone => C.key(zone) === target).transport_state = 'stopped'; },
    ]) {
      const f = await fixture().ready(); await f.reviewed('move'); change(f);
      assert.equal(await f.groups.apply(), false);
      assert.equal(f.writes().length, 0);
      assert.equal(f.ctl.state.uncertain, false);
    }
  }
});

test('an absent occurrence becoming known and an empty destination becoming loaded require a new review', async () => {
  for (const change of [
    f => { f.tracks.get('Synthetic main@Fixture S2').queue_position = 2; },
    f => { f.tracks.set('Synthetic main@Fixture S1', { uri: 'fixture:new', queue_position: 1 }); },
  ]) {
    const f = await fixture().ready();
    delete f.tracks.get('Synthetic main@Fixture S2').queue_position;
    f.tracks.set('Synthetic main@Fixture S1', null);
    await f.reviewed('replay'); change(f);
    assert.equal(await f.groups.apply(), false);
    assert.equal(f.writes().length, 0);
  }
});

test('elapsed progress and cosmetic title changes do not block the same reviewed transfer', async () => {
  const f = await fixture().ready(); await f.reviewed('move');
  for (const target of ['Synthetic main@Fixture S2', 'Synthetic kitchen@Fixture S2']) {
    Object.assign(f.tracks.get(target), { position_secs: 45, title: 'Updated display title' });
  }
  assert.equal(await f.groups.apply(), true);
  assert.equal(f.writes().length, 1);
});

test('topology is checked again after both playback observations are captured', async () => {
  const f = await fixture().ready();
  await f.groups.open(); f.groups.chooseAction('move');
  f.groups.chooseRoom('Synthetic main@Fixture S2'); f.groups.chooseDestination('Synthetic kitchen@Fixture S2');
  f.read((url, options, normal) => {
    if (url === '/zones') { const groups = copy(f.zones); groups[1].members.push('Synthetic arrival'); return groups; }
    return normal(url);
  });
  assert.equal(await f.groups.review(), null);
  assert.equal(f.writes().length, 0);
  assert.match(f.groups.state.message, /changed while their music was being read/);
});

test('closing, hiding or selecting another group while transfer review loads cancels its authority', async () => {
  for (const interrupt of [
    f => f.groups.close(),
    f => f.groups.suspend(),
    f => f.ctl.select('Synthetic main@Fixture S1'),
    f => f.ctl.invalidate(['Synthetic kitchen@Fixture S2']),
  ]) {
    const f = await fixture().ready();
    await f.groups.open(); f.groups.chooseAction('move');
    f.groups.chooseRoom('Synthetic main@Fixture S2'); f.groups.chooseDestination('Synthetic kitchen@Fixture S2');
    const wait = deferred(); let signal;
    f.read((url, options, normal) => {
      if (url.endsWith('/state')) { signal = options.signal; return wait.promise.then(() => normal(url)); }
      return normal(url);
    });
    const pending = f.groups.review();
    await settled(() => !!signal);
    interrupt(f); wait.resolve();
    assert.equal(await pending, null);
    assert.equal(f.groups.state.pending, null);
    assert.equal(f.writes().length, 0);
  }
});

test('an altered destination cannot reuse the previously captured playback review', async () => {
  const f = await fixture().ready(), old = await f.reviewed('replay');
  f.groups.chooseDestination('Synthetic kitchen@Fixture S2');
  assert.equal(f.groups.state.pending, null);
  assert.equal(await f.groups.apply(old), false);
  assert.ok(await f.groups.review());
  assert.equal(await f.groups.apply(), true);
  assert.equal(f.writes()[0].body.to, 'Synthetic kitchen@Fixture S2');
});

test('a transfer review that expires during delayed preflight cannot dispatch without observe running', async () => {
  const f = await fixture().ready(); await f.reviewed('move');
  f.time(1000 + Groups.MAX_AGE - 1);
  const wait = deferred(); let reads = 0;
  f.read((url, options, normal) => {
    if (url.endsWith('/state')) { reads++; return wait.promise.then(() => normal(url)); }
    return normal(url);
  });
  const task = f.groups.apply();
  await settled(() => reads === 2);
  f.time(1000 + Groups.MAX_AGE);
  wait.resolve();
  assert.equal(await task, false);
  assert.equal(f.writes().length, 0);
  assert.equal(f.ctl.state.uncertain, false);
});

test('membership changes during transfer preflight are detected after the audio reads', async () => {
  const f = await fixture().ready(); await f.reviewed('move');
  let topologyReads = 0;
  f.read((url, options, normal) => {
    if (url === '/zones' && ++topologyReads === 2) {
      const groups = copy(f.zones); groups[1].members.push('Synthetic arrival'); return groups;
    }
    return normal(url);
  });
  assert.equal(await f.groups.apply(), false);
  assert.equal(f.writes().length, 0);
});

test('transfers keep the shared mutation lane while checking both groups and never repeat uncertain writes', async () => {
  const f = await fixture().ready(); await f.reviewed('replay');
  const wait = deferred(); let reads = 0;
  f.read((url, options, normal) => {
    if (url.endsWith('/state')) { reads++; return wait.promise.then(() => normal(url)); }
    return normal(url);
  });
  f.write(() => { throw new Error('Replay accepted but reply lost'); });
  const task = f.groups.apply();
  await settled(() => reads === 2);
  assert.equal(await f.ctl.command('next'), false);
  assert.equal(await f.groups.apply(), false);
  wait.resolve();
  assert.equal(await task, false);
  assert.deepEqual(f.writes().map(row => row.url), ['/move']);
  assert.equal(f.ctl.state.uncertain, true);
  assert.equal(f.groups.state.result, null);
  f.groups.close(); f.groups.resume();
  assert.equal(await f.groups.apply(), false);
  assert.equal(f.writes().length, 1);
});

test('accepted replay refreshes source and destination states across households', async () => {
  const f = await fixture().ready(); await f.reviewed('replay');
  f.write(() => {
    f.zones[0].transport_state = 'stopped'; f.zones[2].transport_state = 'playing';
    f.tracks.set('Synthetic main@Fixture S1', { ...copy(f.tracks.get('Synthetic main@Fixture S2')), title: 'Transferred fixture' });
    return { done: 'Replayed and source stopped', changed: true };
  });
  const before = f.calls.length;
  assert.equal(await f.groups.apply(), true);
  assert.equal(f.ctl.state.items[0].state.transport_state, 'stopped');
  assert.equal(f.ctl.state.items[2].state.track.title, 'Transferred fixture');
  assert.equal(f.ctl.state.items[2].state.transport_state, 'playing');
  const sent = f.calls.findIndex((row, index) => index >= before && row.body !== undefined);
  assert.deepEqual(f.calls.slice(sent + 1).map(row => row.url), [
    '/zones', '/zones/Synthetic%20main%40Fixture%20S2/state',
    '/zones/Synthetic%20kitchen%40Fixture%20S2/state', '/zones/Synthetic%20main%40Fixture%20S1/state',
  ]);
});

test('real panel transfer handlers show source and destination audio and dispatch the reviewed operation', async () => {
  for (const action of ['move', 'replay']) {
    const f = await fixture({ dom: true }).ready(), $ = id => f.doc.getElementById(id);
    $('open-groups').fire('click'); await settled(() => !f.groups.state.loading);
    $('groups-action').value = action; $('groups-action').fire('change');
    assert.equal($('groups-destination-field').hidden, false);
    assert.equal($('groups-room-field').hidden, action === 'replay');
    if (action === 'move') {
      assert.equal($('groups-room-label').textContent, 'Room the music leaves');
      $('groups-room').value = 'Synthetic companion@Fixture S2'; $('groups-room').fire('change');
    }
    $('groups-destination').value = action === 'move' ? 'Synthetic nook@Fixture S2' : 'Synthetic main@Fixture S1';
    $('groups-destination').fire('change'); $('groups-review').fire('click');
    await settled(() => !!f.groups.state.pending);
    assert.equal($('groups-audio').hidden, false);
    assert.equal($('groups-audio').children.length, 2);
    assert.match($('groups-audio').children[0].textContent, /Source: .*playing.*queue #2/);
    assert.equal($('groups-confirm').disabled, false);
    $('groups-confirm').fire('click');
    await settled(() => f.writes().length === 1 && !f.groups.state.writing);
    assert.equal(f.writes()[0].url, '/move');
    assert.equal(f.writes()[0].body.copy, action === 'replay');
  }
});

test('clearing a destination through the real panel retires its captured transfer review', async () => {
  const f = await fixture({ dom: true }).ready(), $ = id => f.doc.getElementById(id);
  await f.reviewed('replay');
  $('groups-destination').value = ''; $('groups-destination').fire('change');
  assert.equal(f.groups.state.pending, null);
  assert.equal($('groups-preview').hidden, true);
  assert.equal($('groups-confirm').disabled, true);
  $('groups-confirm').fire('click');
  assert.equal(f.writes().length, 0);
});
